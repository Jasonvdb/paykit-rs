//! One wallet: the identity's signer, its Paykit app and its rail, and the commands and the
//! event loop that drive them.
//!
//! Test tool only. Payments are made and receipts sent without a user: `--auto-pay` pays every
//! open one-time request, and `--auto-confirm` credits every proof the rail verifies.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _};
use paykit_lib::signer::PaykitProtocol;
use paykit_sdk::{
    AppEnv, AppScopes, JoinRequest, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig,
    PaymentEndpointIdentifier, PaymentReference, Rail, RequestDestination, RequestDraft,
    RequestStatus, Timer,
};
use pubky_dispatch_client::clock::{Clock, SystemClock};
use pubky_dispatch_client::store::{StateStore, StoreHandle};
use pubky_dispatch_client::transport::{BoxFuture, Transport};
use pubky_dispatch_core::enc::{pubky_id, pubky_key, random_bytes};
use pubky_dispatch_core::keys::AppKeys;
use pubky_dispatch_core::records::{
    app_record_path, AppRequest, AppRequestBody, ApprovalClaim, AuthRequest,
};
use pubky_dispatch_signer::{
    AddOutcome, IdentityKeyring, IdentitySecret, Protocols, SecretVault, Signer, SignerEnv,
    SignerStatus, VaultError,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::bitcoind::Bitcoind;
use crate::cli::{Command, Line};
use crate::events::{emit, variant};
use crate::homeserver::{FakeHomeserver, OwnHomeserver};
use crate::rail::{address_payload, amount_sats, btc_amount, RegtestRail, P2TR, P2WPKH};
use crate::state::{hex, unhex, PeerConfig, StateDir};
use crate::store::FileStore;
use crate::transport::LoopbackListener;

/// How often the running peer lets its signer do its due work (renewals, key deliveries).
const SIGNER_TICK: Duration = Duration::from_secs(30);
/// The most inbox pages one round reads before it handles what they brought.
const PAGES_PER_ROUND: usize = 8;

/// Where the peer's services are, from the command line.
#[derive(Clone, Debug)]
pub struct Settings {
    pub dispatch_url: String,
    pub origin: String,
    pub homeserver: String,
    pub bitcoind_url: String,
    pub rpc_user: String,
    pub rpc_password: String,
    pub wallet: Option<String>,
    pub funder_wallet: String,
}

/// The host's timer, on the tokio runtime.
struct TokioTimer;

impl Timer for TokioTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The identity secret, held in memory for the process (and in `peer.json` with `--state`).
struct MemoryVault(Mutex<Option<IdentitySecret>>);

impl SecretVault for MemoryVault {
    fn load(&self) -> Result<Option<IdentitySecret>, VaultError> {
        Ok(lock(&self.0).clone())
    }

    fn add(&self, secret: &IdentitySecret) -> Result<AddOutcome, VaultError> {
        let mut held = lock(&self.0);
        if held.is_some() {
            return Ok(AddOutcome::Occupied);
        }
        *held = Some(secret.clone());
        Ok(AddOutcome::Stored)
    }

    /// Deletes the secret only when it is `expected`, under the one lock.
    fn delete(&self, expected: &IdentitySecret) -> Result<(), VaultError> {
        let mut held = lock(&self.0);
        if held
            .as_ref()
            .is_some_and(|secret| secret.as_bytes() == expected.as_bytes())
        {
            *held = None;
        }
        Ok(())
    }
}

/// A Pubky ID as the commands take it: z-base-32, with or without a `pubky` or `pk:` prefix.
pub fn parse_id(text: &str) -> anyhow::Result<[u8; 32]> {
    let bare = text
        .strip_prefix("pubky")
        .or_else(|| text.strip_prefix("pk:"))
        .unwrap_or(text);
    pubky_key(bare).map_err(|_| anyhow!("{text:?} is not a Pubky ID"))
}

/// Splits a stdin command line into words at whitespace. Text in double or single quotes stays
/// one word, so a reference can hold spaces: `request <id> 15000 --reference "Concert tickets"`.
/// `None` for an unbalanced quote.
fn split_line(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for character in line.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => word.push(character),
            None if character == '"' || character == '\'' => {
                quote = Some(character);
                in_word = true;
            }
            None if character.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(character);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// A random payment reference.
fn random_reference() -> anyhow::Result<PaymentReference> {
    let bytes: [u8; 8] = random_bytes().map_err(|error| anyhow!("{error}"))?;
    Ok(PaymentReference::new(&format!("peer-{}", hex(&bytes)))?)
}

fn p2wpkh() -> PaymentEndpointIdentifier {
    // A constant that passes the identifier's checks.
    PaymentEndpointIdentifier::new(P2WPKH).unwrap_or_else(|_| unreachable!())
}

pub struct Peer {
    settings: Settings,
    dir: Option<StateDir>,
    config: PeerConfig,
    identity: [u8; 32],
    signer: Signer,
    wallet: Paykit,
    rail: Arc<RegtestRail>,
    /// Events already printed, so each is printed once per process.
    printed: BTreeSet<String>,
    /// Requests this process tried to pay, by request hash.
    tried: BTreeSet<[u8; 32]>,
    /// Payee records this process confirmed or gave up on.
    settled: BTreeSet<[u8; 32]>,
    /// Whether an attempt may be left open, for `resume_payments`.
    attempts_open: bool,
}

impl Peer {
    /// Open the peer in `dir` (or in memory): load its identity, signer and app, or create
    /// them, enrolling the identity at the Dispatch and joining a Paykit app approved for the
    /// pay and request scopes. Prints `ready`.
    pub async fn open(settings: Settings, dir: Option<StateDir>) -> anyhow::Result<Self> {
        let files = Arc::new(FakeHomeserver::new(&settings.homeserver)?);
        let transport: Arc<dyn Transport> = Arc::new(LoopbackListener::new(
            &settings.origin,
            &settings.dispatch_url,
        )?);
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let node = Bitcoind::new(
            &settings.bitcoind_url,
            &settings.rpc_user,
            &settings.rpc_password,
        )?;

        // The identity: the one in the state directory, or a new random secret.
        let loaded = match &dir {
            Some(dir) => dir.load_config()?,
            None => None,
        };
        let (config, fresh) = match loaded {
            Some(config) => {
                if config.origin != settings.origin {
                    bail!(
                        "this state directory's identity is enrolled at {}, not {}",
                        config.origin,
                        settings.origin
                    );
                }
                (config, false)
            }
            None => {
                let secret: [u8; 32] = random_bytes().map_err(|error| anyhow!("{error}"))?;
                let keyring = IdentityKeyring::open(Arc::new(MemoryVault(Mutex::new(Some(
                    IdentitySecret::from_bytes(secret),
                )))))?;
                let identity = pubky_id(&keyring.public_key());
                let wallet = settings
                    .wallet
                    .clone()
                    .unwrap_or_else(|| format!("paykit-peer-{}", &identity[..12]));
                let config = PeerConfig {
                    identity_secret: hex(&secret),
                    identity,
                    origin: settings.origin.clone(),
                    wallet,
                    app_id: None,
                };
                if let Some(dir) = &dir {
                    dir.save_config(&config)?;
                }
                (config, true)
            }
        };
        if let Some(wallet) = &settings.wallet {
            if *wallet != config.wallet {
                bail!(
                    "this state directory's bitcoind wallet is {}, not {wallet}",
                    config.wallet
                );
            }
        }
        let secret = unhex::<32>(&config.identity_secret)
            .ok_or_else(|| anyhow!("peer.json holds no identity secret"))?;
        let keyring = IdentityKeyring::open(Arc::new(MemoryVault(Mutex::new(Some(
            IdentitySecret::from_bytes(secret),
        )))))?;
        let identity = keyring.public_key();

        // The signer.
        let signer_store: Arc<dyn StateStore> = match &dir {
            Some(dir) => Arc::new(FileStore::open(dir.path("signer.json"))?),
            None => Arc::new(FileStore::in_memory()),
        };
        let signer_existed = dir
            .as_ref()
            .is_some_and(|dir| dir.path("signer.json").exists());
        let env = SignerEnv {
            store: StoreHandle::inline(signer_store),
            clock: Arc::clone(&clock),
            transport: Arc::clone(&transport),
            homeserver: Arc::new(OwnHomeserver {
                identity,
                files: Arc::clone(&files),
            }),
            protocols: Protocols::new(vec![Arc::new(PaykitProtocol)])
                .map_err(|error| anyhow!("could not register Paykit's protocol: {error}"))?,
        };
        let mut signer = if signer_existed {
            Box::pin(Signer::open(env, keyring))
                .await
                .context("could not open the signer")?
        } else {
            eprintln!(
                "paykit-peer: enrolling {} at {}",
                config.identity, config.origin
            );
            Box::pin(Signer::create(env, keyring, &config.origin))
                .await
                .context("could not enrol the identity at the Dispatch")?
        };
        Box::pin(signer.resume())
            .await
            .context("the signer could not resume")?;
        if !matches!(signer.status(), SignerStatus::Signing) {
            bail!("the signer stopped at {:?}", signer.status());
        }

        // The rail, on the peer's own bitcoind wallet.
        node.ensure_wallet(&config.wallet)
            .await
            .context("could not load the peer's bitcoind wallet")?;
        let rail = Arc::new(RegtestRail::new(
            node,
            config.wallet.clone(),
            dir.as_ref().map(|dir| dir.path("rail.json")),
        )?);

        // The Paykit app.
        let app_store: Arc<dyn StateStore> = match &dir {
            Some(dir) => Arc::new(FileStore::open(dir.path("app.json"))?),
            None => Arc::new(FileStore::in_memory()),
        };
        let env = AppEnv {
            store: StoreHandle::inline(app_store),
            transport,
            clock,
            homeserver: files.clone(),
            homeserver_writer: Some(files),
        };
        let as_rail: Arc<dyn Rail> = rail.clone();
        let timer: Arc<dyn Timer> = Arc::new(TokioTimer);
        let (wallet, joined) = match &config.app_id {
            Some(app_id) => {
                let app_id = unhex::<16>(app_id).ok_or_else(|| anyhow!("peer.json's app_id"))?;
                let wallet = Box::pin(Paykit::load(
                    env,
                    PaykitConfig::default(),
                    app_id,
                    as_rail,
                    timer,
                ))
                .await
                .context("could not load the Paykit app")?;
                (wallet, false)
            }
            None => {
                let request = Box::pin(approve_app(&mut signer, identity)).await?;
                let wallet = Box::pin(Paykit::join(
                    env,
                    PaykitConfig::default(),
                    request,
                    as_rail,
                    timer,
                ))
                .await
                .context("could not join the Paykit app")?;
                (wallet, true)
            }
        };
        let mut config = config;
        if joined {
            config.app_id = Some(hex(&wallet.app_id()));
            if let Some(dir) = &dir {
                dir.save_config(&config)?;
            }
        }
        let mut peer = Self {
            settings,
            dir,
            config,
            identity,
            signer,
            wallet,
            rail,
            printed: BTreeSet::new(),
            tried: BTreeSet::new(),
            settled: BTreeSet::new(),
            attempts_open: true,
        };
        if joined {
            Box::pin(peer.finish_listing()).await?;
        }
        emit(
            "ready",
            json!({
                "identity": peer.id(),
                "app_id": peer.config.app_id,
                "wallet": peer.config.wallet,
                "origin": peer.config.origin,
                "state": peer.dir.as_ref().map(|_| "dir").unwrap_or("memory"),
                "enrolled": if fresh { "new" } else { "loaded" },
            }),
        );
        Ok(peer)
    }

    /// The identity's Pubky ID, z-base-32 without a prefix.
    pub fn id(&self) -> String {
        pubky_id(&self.identity)
    }

    /// Read the inbox until the new app's first listing is done.
    async fn finish_listing(&mut self) -> anyhow::Result<()> {
        for _ in 0..8 {
            let read = Box::pin(self.wallet.receive(None)).await?;
            if !read.report.more && read.records_unlisted.is_none() {
                return Ok(());
            }
        }
        bail!("the app's first listing did not finish")
    }

    /// Fund the peer's bitcoind wallet up to `sats`. Prints `funded`.
    pub async fn fund(&mut self, sats: u64) -> anyhow::Result<()> {
        let node = self.rail.node().clone();
        let txid = node
            .fund(&self.config.wallet, &self.settings.funder_wallet, sats)
            .await?;
        let balance = node.balance_sats(&self.config.wallet).await?;
        emit(
            "funded",
            json!({ "wallet": self.config.wallet, "txid": txid, "balance_sats": balance }),
        );
        Ok(())
    }

    /// Run one command (not `run` or `id`).
    pub async fn command(&mut self, command: Command) -> anyhow::Result<()> {
        match command {
            Command::Init { fund } => {
                if let Some(sats) = fund {
                    Box::pin(self.fund(sats)).await?;
                }
                Ok(())
            }
            Command::Id => {
                emit("id", json!({ "identity": self.id() }));
                Ok(())
            }
            Command::Fund { sats } => Box::pin(self.fund(sats)).await,
            Command::Allow { id } => {
                let peer = parse_id(&id)?;
                Box::pin(self.wallet.allow_connect(&peer)).await?;
                emit("allowed", json!({ "id": pubky_id(&peer) }));
                Ok(())
            }
            Command::Request {
                id,
                sats,
                any,
                taproot,
                reference,
            } => Box::pin(self.request(&id, sats, any, taproot, reference)).await,
            Command::PublishList { id } => Box::pin(self.publish_list(&id)).await,
            Command::Run { .. } => bail!("run is not a command for a running peer"),
        }
    }

    /// Request `sats` from `id`. Prints `request_sent`.
    async fn request(
        &mut self,
        id: &str,
        sats: u64,
        any: bool,
        taproot: bool,
        reference: Option<String>,
    ) -> anyhow::Result<()> {
        let payer = parse_id(id)?;
        let reference = match reference {
            Some(text) => PaymentReference::new(&text)?,
            None => random_reference()?,
        };
        let endpoint_id = if taproot { P2TR } else { P2WPKH };
        let (destination, address) = if any {
            (RequestDestination::Any, None)
        } else {
            let node = self.rail.node();
            let address = if taproot {
                node.new_taproot_address(self.rail.wallet()).await?
            } else {
                node.new_address(self.rail.wallet()).await?
            };
            let identifier = PaymentEndpointIdentifier::new(endpoint_id)?;
            let endpoints = [(identifier, address_payload(&address)?)]
                .into_iter()
                .collect();
            (RequestDestination::Endpoints(endpoints), Some(address))
        };
        let draft = RequestDraft::new(btc_amount(sats)?, reference.clone(), destination);
        let sent = Box::pin(self.wallet.send_request(payer, draft)).await?;
        if let Err(error) = Box::pin(self.wallet.flush()).await {
            eprintln!("paykit-peer: flush after the request: {error}");
        }
        emit(
            "request_sent",
            json!({
                "to": pubky_id(&payer),
                "msg_id": sent.msg_id,
                "request_hash": sent.request.as_ref().map(|r| hex(&r.request_hash)),
                "sats": sats,
                "reference": reference.as_str(),
                "destination": if any { "any" } else { "endpoints" },
                "endpoint_id": if any { Value::Null } else { Value::from(endpoint_id) },
                "address": address,
                "delivery": variant(&sent.delivery),
            }),
        );
        Ok(())
    }

    /// Set this app's Private Payment List to a new address and publish it to `id`. Prints
    /// `list_published`.
    async fn publish_list(&mut self, id: &str) -> anyhow::Result<()> {
        let recipient = parse_id(id)?;
        let address = self.rail.node().new_address(self.rail.wallet()).await?;
        let endpoints: BTreeMap<_, _> = [(p2wpkh(), address_payload(&address)?)]
            .into_iter()
            .collect();
        Box::pin(self.wallet.set_list_endpoints(endpoints)).await?;
        Box::pin(self.wallet.publish_list_to(recipient)).await?;
        if let Err(error) = Box::pin(self.wallet.flush()).await {
            eprintln!("paykit-peer: flush after publishing the list: {error}");
        }
        emit(
            "list_published",
            json!({
                "to": pubky_id(&recipient),
                "endpoint_id": P2WPKH,
                "address": address,
            }),
        );
        Ok(())
    }

    /// Print `name` once per `key` in this process.
    fn once(&mut self, key: String, name: &str, fields: Value) {
        if self.printed.insert(key) {
            emit(name, fields);
        }
    }

    /// One round: read the inbox, send what is queued, then act as payer and as payee.
    async fn round(&mut self, auto_pay: bool, auto_confirm: bool) -> anyhow::Result<()> {
        for _ in 0..PAGES_PER_ROUND {
            let summary = Box::pin(self.wallet.receive(None)).await?;
            for event in &summary.report.events {
                emit("app_event", json!({ "kind": variant(event) }));
            }
            if !summary.report.more {
                break;
            }
        }
        if let Err(error) = Box::pin(self.wallet.flush()).await {
            self.once(
                format!("flush:{error}"),
                "warning",
                json!({ "during": "flush", "error": error.to_string() }),
            );
        }
        if self.attempts_open {
            match Box::pin(self.wallet.resume_payments()).await {
                Ok(outcomes) => {
                    self.attempts_open = false;
                    for outcome in outcomes {
                        self.attempts_open |=
                            !matches!(outcome, PayOutcome::Paid { .. } | PayOutcome::Failed { .. });
                    }
                }
                Err(error) => emit(
                    "warning",
                    json!({ "during": "resume_payments", "error": error.to_string() }),
                ),
            }
        }
        Box::pin(self.as_payer(auto_pay)).await?;
        Box::pin(self.as_payee(auto_confirm)).await?;
        Ok(())
    }

    /// Requests received, payments made, receipts received.
    async fn as_payer(&mut self, auto_pay: bool) -> anyhow::Result<()> {
        let requests = Box::pin(self.wallet.requests()).await?;
        for view in requests {
            let hash = hex(&view.request.request_hash);
            let sats = amount_sats(&view.amount);
            self.once(
                format!("request:{hash}"),
                "request_received",
                json!({
                    "from": pubky_id(&view.request.payee),
                    "request_hash": hash,
                    "sats": sats,
                    "amount": view.amount.value.to_string(),
                    "asset": view.amount.asset.as_str(),
                    "reference": view.payment_reference,
                    "status": variant(&view.status),
                    "recurring": view.recurring,
                    "manual_only": view.manual_only,
                }),
            );
            self.once(
                format!("request-status:{hash}:{:?}", view.status),
                "request_status",
                json!({ "request_hash": hash, "status": variant(&view.status) }),
            );
            let payable = auto_pay
                && view.status == RequestStatus::Open
                && !view.recurring
                && view.held.is_none()
                && !view.divergent;
            if payable && self.tried.insert(view.request.request_hash) {
                let result = Box::pin(
                    self.wallet
                        .pay_request(&view.request, PayOptions::default()),
                )
                .await;
                self.report_payment(&hash, &view.request.payee, result.outcome);
            }
        }
        let payments = Box::pin(self.wallet.payments()).await?;
        for payment in payments {
            let Some(receipt) = &payment.receipt else {
                continue;
            };
            let txid = self.rail.handoff(&payment.attempt_id).map(|h| h.txid);
            self.once(
                format!("receipt:{}", receipt.msg_id),
                "receipt_received",
                json!({
                    "payee": pubky_id(&payment.payee),
                    "request_hash": payment.request_hash.as_ref().map(|h| hex(h)),
                    "attempt_id": payment.attempt_id.to_text(),
                    "msg_id": receipt.msg_id,
                    "txid": txid,
                    "sats": amount_sats(&payment.amount),
                    "stage": variant(&payment.stage),
                }),
            );
        }
        Ok(())
    }

    fn report_payment(
        &mut self,
        hash: &str,
        payee: &[u8; 32],
        outcome: paykit_sdk::Result<PayOutcome>,
    ) {
        match outcome {
            Ok(PayOutcome::Paid {
                attempt_id,
                proof_msg_id,
                ..
            }) => {
                let handoff = self.rail.handoff(&attempt_id);
                emit(
                    "paid",
                    json!({
                        "payee": pubky_id(payee),
                        "request_hash": hash,
                        "attempt_id": attempt_id.to_text(),
                        "proof_msg_id": proof_msg_id,
                        "txid": handoff.as_ref().map(|h| h.txid.clone()),
                        "address": handoff.as_ref().map(|h| h.address.clone()),
                        "endpoint_id": handoff.as_ref().map(|h| h.endpoint_id.clone()),
                        "sats": handoff.as_ref().map(|h| h.sats),
                    }),
                );
            }
            Ok(other) => {
                self.attempts_open = true;
                emit(
                    "pay_outcome",
                    json!({
                        "payee": pubky_id(payee),
                        "request_hash": hash,
                        "outcome": variant(&other),
                        "attempt_id": other.attempt_id().to_text(),
                    }),
                );
            }
            Err(error) => {
                // Refused before any attempt (no destination known yet, for one): tried again
                // next round, and printed once per distinct error.
                if let Some(request_hash) = unhex::<32>(hash) {
                    self.tried.remove(&request_hash);
                }
                self.once(
                    format!("pay-failed:{hash}:{error}"),
                    "pay_failed",
                    json!({
                        "payee": pubky_id(payee),
                        "request_hash": hash,
                        "error": error.to_string(),
                    }),
                );
            }
        }
    }

    /// Requests sent, proofs received, payments verified and confirmed.
    async fn as_payee(&mut self, auto_confirm: bool) -> anyhow::Result<()> {
        let activity = Box::pin(self.wallet.payee_activity()).await?;
        for sale in activity {
            let record = hex(&sale.record);
            self.once(
                format!("sale:{record}:{:?}", sale.status),
                "payee_status",
                json!({
                    "payer": pubky_id(&sale.payer),
                    "record": record,
                    "request_hash": sale.request_hash.as_ref().map(|h| hex(h)),
                    "status": variant(&sale.status),
                }),
            );
            if sale.status != PayeeStatus::ProofReceived {
                continue;
            }
            let attempt = sale.attempt_id.map(|a| a.to_text());
            self.once(
                format!("proof:{record}"),
                "proof_received",
                json!({
                    "payer": pubky_id(&sale.payer),
                    "record": record,
                    "request_hash": sale.request_hash.as_ref().map(|h| hex(h)),
                    "attempt_id": attempt,
                    "sats": sale.amount.as_ref().and_then(amount_sats),
                }),
            );
            if !auto_confirm || self.settled.contains(&sale.record) {
                continue;
            }
            let verified = match Box::pin(self.wallet.verify_payment(&sale.record)).await {
                Ok(verified) => verified,
                Err(error) => {
                    // Not checkable now (the node doesn't know the transaction yet): next
                    // round.
                    self.once(
                        format!("verify-error:{record}:{error}"),
                        "warning",
                        json!({ "during": "verify", "record": record, "error": error.to_string() }),
                    );
                    continue;
                }
            };
            let seen = sale.attempt_id.and_then(|a| self.rail.verified(&a));
            let Some(paid_at_ms) = verified else {
                self.settled.insert(sale.record);
                emit(
                    "verify_failed",
                    json!({
                        "record": record,
                        "attempt_id": attempt,
                        "txid": seen.as_ref().map(|v| v.txid.clone()),
                        "sats_received": seen.as_ref().map(|v| v.sats_received),
                    }),
                );
                continue;
            };
            emit(
                "verified",
                json!({
                    "payer": pubky_id(&sale.payer),
                    "record": record,
                    "request_hash": sale.request_hash.as_ref().map(|h| hex(h)),
                    "attempt_id": attempt,
                    "txid": seen.as_ref().map(|v| v.txid.clone()),
                    "sats_received": seen.as_ref().map(|v| v.sats_received),
                    "paid_at_ms": paid_at_ms,
                }),
            );
            match Box::pin(self.wallet.confirm_payment(&sale.record)).await {
                Ok(Some(receipt)) => {
                    self.settled.insert(sale.record);
                    if let Err(error) = Box::pin(self.wallet.flush()).await {
                        eprintln!("paykit-peer: flush after the receipt: {error}");
                    }
                    emit(
                        "confirmed",
                        json!({
                            "payer": pubky_id(&sale.payer),
                            "record": record,
                            "request_hash": sale.request_hash.as_ref().map(|h| hex(h)),
                            "attempt_id": attempt,
                            "txid": seen.as_ref().map(|v| v.txid.clone()),
                            "receipt_msg_id": receipt,
                        }),
                    );
                }
                Ok(None) => self.once(
                    format!("confirm-none:{record}"),
                    "warning",
                    json!({ "during": "confirm", "record": record, "error": "not confirmed" }),
                ),
                Err(error) => self.once(
                    format!("confirm-error:{record}:{error}"),
                    "warning",
                    json!({ "during": "confirm", "record": record, "error": error.to_string() }),
                ),
            }
        }
        Ok(())
    }

    /// Run until stdin closes or reads `quit`: a round every `poll_ms`, commands from stdin
    /// in between, the signer's due work every 30 s.
    pub async fn run(
        &mut self,
        auto_pay: bool,
        auto_confirm: bool,
        poll_ms: u64,
    ) -> anyhow::Result<()> {
        let mut lines = stdin_lines();
        let mut signer_due = Instant::now() + SIGNER_TICK;
        let mut stdin_open = true;
        emit(
            "running",
            json!({ "auto_pay": auto_pay, "auto_confirm": auto_confirm, "poll_ms": poll_ms }),
        );
        loop {
            if let Err(error) = Box::pin(self.round(auto_pay, auto_confirm)).await {
                emit(
                    "warning",
                    json!({ "during": "round", "error": format!("{error:#}") }),
                );
            }
            if Instant::now() >= signer_due {
                signer_due = Instant::now() + SIGNER_TICK;
                if let Err(error) = Box::pin(self.signer.run_due()).await {
                    emit(
                        "warning",
                        json!({ "during": "signer", "error": error.to_string() }),
                    );
                }
            }
            let line = if stdin_open {
                tokio::select! {
                    line = lines.recv() => line.map(Some).unwrap_or_else(|| {
                        stdin_open = false;
                        None
                    }),
                    () = tokio::time::sleep(Duration::from_millis(poll_ms)) => None,
                }
            } else {
                tokio::time::sleep(Duration::from_millis(poll_ms)).await;
                None
            };
            if !stdin_open {
                emit("stopping", json!({ "why": "stdin closed" }));
                return Ok(());
            }
            let Some(line) = line else { continue };
            let Some(words) = split_line(&line) else {
                emit(
                    "command_failed",
                    json!({ "command": "", "line": line, "error": "unbalanced quote" }),
                );
                continue;
            };
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            match words.first() {
                None => continue,
                Some(&"quit" | &"exit") => {
                    emit("stopping", json!({ "why": "quit" }));
                    return Ok(());
                }
                Some(_) => {}
            }
            let parsed = <Line as clap::Parser>::try_parse_from(&words);
            let name = words.first().copied().unwrap_or_default().to_owned();
            let result = match parsed {
                Ok(Line {
                    command: Command::Id,
                }) => {
                    emit("id", json!({ "identity": self.id() }));
                    Ok(())
                }
                Ok(Line {
                    command: Command::Run { .. },
                }) => Err(anyhow!("already running")),
                Ok(Line { command }) => Box::pin(self.command(command)).await,
                Err(error) => Err(anyhow!("{}", error.render().to_string().trim())),
            };
            match result {
                Ok(()) => emit("command_ok", json!({ "command": name, "line": line })),
                Err(error) => emit(
                    "command_failed",
                    json!({ "command": name, "line": line, "error": format!("{error:#}") }),
                ),
            }
        }
    }

    /// Close the signer and the app cleanly.
    pub async fn close(self) -> anyhow::Result<()> {
        Box::pin(self.signer.close())
            .await
            .context("could not close the signer")?;
        drop(self.wallet);
        Ok(())
    }
}

/// Lines from stdin, read on a task of their own. The channel closes at end of input.
fn stdin_lines() -> mpsc::UnboundedReceiver<String> {
    use tokio::io::AsyncBufReadExt as _;
    let (send, receive) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    receive
}

/// Approve a new Paykit app of this identity for the pay and request scopes with the
/// in-process signer, as the rig's `dispatch-enroll` does, and return its join request.
async fn approve_app(signer: &mut Signer, identity: [u8; 32]) -> anyhow::Result<JoinRequest> {
    let scopes = AppScopes::new()
        .pay()
        .request()
        .build()
        .map_err(|error| anyhow!("could not build the pay and request scopes: {error}"))?
        .scopes;
    let keys = AppKeys::generate().map_err(|error| anyhow!("{error}"))?;
    // The auth request binds the app request; nothing else reads it here, as the signer and
    // the app are in one process. Its client key and secret are random per app.
    let auth = AuthRequest {
        client_id: "paykit-peer.local".into(),
        client_key: random_bytes().map_err(|error| anyhow!("{error}"))?,
        caps: format!("{}:rw", app_record_path(&keys.app_id_bytes())),
        secret: random_bytes().map_err(|error| anyhow!("{error}"))?,
    };
    let request = AppRequest::sign(&keys, AppRequestBody::new(&keys, scopes, &auth))
        .map_err(|error| anyhow!("could not sign the app request: {error}"))?;
    let checked = signer
        .review(request.signed(), &auth)
        .map_err(|error| anyhow!("the signer refused the app request: {error}"))?;
    let approved = Box::pin(signer.approve(checked))
        .await
        .map_err(|error| anyhow!("the signer could not approve the app: {error}"))?;
    Box::pin(signer.claim_delivered(&approved.request))
        .await
        .map_err(|error| anyhow!("{error}"))?;
    let claim =
        ApprovalClaim::from_plaintext(&approved.claim).map_err(|error| anyhow!("{error}"))?;
    let approval = claim
        .verify(&identity, &auth)
        .map_err(|error| anyhow!("the approval does not verify: {error}"))?;
    Ok(JoinRequest {
        identity,
        keys,
        request,
        approval: approval.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::split_line;

    #[test]
    fn test_split_line_keeps_quoted_words() {
        let words = split_line(r#"request pk:abc 15000 --reference "Concert tickets" --taproot"#);
        assert_eq!(
            words.unwrap(),
            [
                "request",
                "pk:abc",
                "15000",
                "--reference",
                "Concert tickets",
                "--taproot"
            ]
        );
        assert_eq!(split_line("  id  ").unwrap(), ["id"]);
        assert_eq!(
            split_line("request x 1 --reference ''").unwrap(),
            ["request", "x", "1", "--reference", ""]
        );
    }

    #[test]
    fn test_split_line_refuses_an_unbalanced_quote() {
        assert!(split_line(r#"request x 1 --reference "Concert"#).is_none());
    }
}
