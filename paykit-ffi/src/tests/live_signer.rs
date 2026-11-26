//! The first start of a demo host, through the exported calls alone, against a live local
//! development Dispatch and the local fake Homeserver: enrollment, the in-process signer's
//! approval (`demo_approve_own_app`), the wallet's join and first receive, a later start
//! (`demo_signer_run_due`, `WalletHost::load`), then two such hosts admitting each other and
//! exchanging a Payment Request. Ignored by default: it needs both services on this machine.
//!
//! ```text
//! cargo test -p paykit-ffi --features demo-signer --lib live_signer -- --ignored --nocapture
//! ```
//!
//! `PAYKIT_LOCAL_DISPATCH_ORIGIN`, `PAYKIT_LOCAL_DISPATCH_LISTENER` and
//! `PAYKIT_FAKE_HOMESERVER` override the service's signed origin, its plain HTTP listener and
//! the fake Homeserver's address. Each run enrols two new identities on the service.

use super::{method::Account, storage::DeviceStore};
use crate::*;
use pubky_dispatch_client::{
    clock::SystemClock,
    transport::{HttpConfig, HttpTransport, Request, TimeoutClass, Transport},
};
use pubky_dispatch_core::enc::{random_bytes, zbase32_encode};
use std::{error::Error, sync::Arc};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const ENDPOINT: &str = "btc-lightning-bolt11";

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// The fake Homeserver's API: `GET`, `PUT` and `DELETE` on `/{identity z32}{path}`, no auth.
struct FakeHomeserver {
    http: HttpTransport,
    base: String,
}

impl FakeHomeserver {
    fn new(base: String) -> TestResult<Self> {
        let http = HttpTransport::without_address_checks_for_loopback_tests(HttpConfig::default())
            .map_err(|e| format!("{e:?}"))?;
        Ok(Self { http, base })
    }

    async fn call(
        &self,
        method: &str,
        identity: &[u8],
        path: &str,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), HomeserverError> {
        let request = Request {
            method: method.into(),
            origin: self.base.clone(),
            path_and_query: format!("/{}{path}", zbase32_encode(identity)),
            headers: Vec::new(),
            body,
            timeout: TimeoutClass::Bulk,
        };
        let response = self
            .http
            .send(request)
            .await
            .map_err(|_| HomeserverError::Unavailable)?;
        Ok((response.status, response.body))
    }
}

#[async_trait::async_trait]
impl WalletHomeserver for FakeHomeserver {
    async fn get(&self, id: Vec<u8>, p: String) -> Result<Option<Vec<u8>>, HomeserverError> {
        match self.call("GET", &id, &p, Vec::new()).await? {
            (200, body) => Ok(Some(body)),
            (404, _) => Ok(None),
            _ => Err(HomeserverError::Unavailable),
        }
    }
    async fn put(&self, id: Vec<u8>, p: String, b: Vec<u8>) -> Result<(), HomeserverError> {
        match self.call("PUT", &id, &p, b).await?.0 {
            200..=299 => Ok(()),
            _ => Err(HomeserverError::Unavailable),
        }
    }
    async fn delete(&self, id: Vec<u8>, p: String) -> Result<(), HomeserverError> {
        match self.call("DELETE", &id, &p, Vec::new()).await?.0 {
            200..=299 | 404 => Ok(()),
            _ => Err(HomeserverError::Unavailable),
        }
    }
}

/// The values a host keeps from launch config.
#[derive(Clone)]
struct Config {
    origin: String,
    listener: String,
    homeserver: String,
}

/// One demo host after its first start.
struct Party {
    name: &'static str,
    secret: Vec<u8>,
    identity: Vec<u8>,
    store: Arc<dyn WalletStore>,
    home: Arc<dyn WalletHomeserver>,
    host: Arc<WalletHost>,
    wallet: Arc<NativeWallet>,
    _root: tempfile::TempDir,
}

impl Party {
    async fn run_due(&self, config: &Config) -> TestResult<bool> {
        let run = demo_signer_run_due(
            self.secret.clone(),
            config.origin.clone(),
            config.listener.clone(),
            self.home.clone(),
            self.store.clone(),
        )
        .await?;
        Ok(run.more_due)
    }

    /// Receive until the feed has nothing more, running the signer when the wallet waits
    /// for its scope keys, as the host's loop does.
    async fn drain(&self, config: &Config) -> TestResult {
        for _ in 0..32 {
            let report = self.wallet.receive().await?;
            eprintln!(
                "{}: receive more = {}, scope_keys_awaited = {}, records_unlisted = {:?}",
                self.name, report.more, report.scope_keys_awaited, report.records_unlisted
            );
            if report.scope_keys_awaited {
                let more = self.run_due(config).await?;
                eprintln!("{}: signer run_due, more due = {more}", self.name);
            }
            if let Some(id) = report.events.iter().rev().find_map(|e| e.id.clone()) {
                self.wallet.acknowledge_events(id).await?;
            }
            if !report.more && !report.scope_keys_awaited {
                return Ok(());
            }
        }
        Err(format!("{}: receive did not settle", self.name).into())
    }
}

/// The sequence a Swift host runs on its first start, then on a later start.
async fn first_start(name: &'static str, config: &Config) -> TestResult<Party> {
    let root = tempfile::tempdir()?;
    let store: Arc<dyn WalletStore> = Arc::new(DeviceStore::open(root.path()));
    let home: Arc<dyn WalletHomeserver> = Arc::new(FakeHomeserver::new(config.homeserver.clone())?);
    let secret = random_bytes::<32>()?.to_vec();

    // 1. A random reference, kept by the host.
    let reference = random_bytes::<16>()?.to_vec();
    let enrollment = WalletEnrollment::new(store.clone());
    let info = enrollment.begin_enrollment(reference.clone()).await?;
    // 2. The app request, bound to the host's auth request.
    let client_id = "wallet.example".to_owned();
    let client_key = random_bytes::<32>()?.to_vec();
    let auth_secret = random_bytes::<32>()?.to_vec();
    let capabilities = format!("{}:rw", info.app_record_path);
    let prepared = enrollment
        .prepare_enrollment(
            reference.clone(),
            client_id.clone(),
            client_key.clone(),
            capabilities.clone(),
            auth_secret.clone(),
            WalletScopes {
                pay: true,
                request: true,
                contacts: true,
                ..Default::default()
            },
        )
        .await?;
    // 3. The in-process signer enrols the identity and approves the request.
    let approval = demo_approve_own_app(
        secret.clone(),
        config.origin.clone(),
        config.listener.clone(),
        home.clone(),
        store.clone(),
        prepared.signed_request.clone(),
        client_id,
        client_key,
        capabilities,
        auth_secret,
    )
    .await?;
    let identity = approval.identity.clone();
    eprintln!("{name}: enrolled {}", zbase32_encode(&identity));
    // 4. The wallet keeps the approval.
    enrollment
        .accept_approval(reference.clone(), identity.clone(), approval.claim)
        .await?;
    // 5. The wallet joins the app on the service.
    let account = Arc::new(Account::open(root.path(), Arc::new(SystemClock)));
    let host = WalletHost::new_local_development(
        store.clone(),
        home.clone(),
        account,
        Account::declaration(true),
        vec![EndpointSupport {
            identifier: ENDPOINT.into(),
            fraction_digits: Some(8),
        }],
        config.origin.clone(),
        config.listener.clone(),
        None,
    )?;
    let wallet = Box::pin(host.complete_enrollment(reference)).await?;
    assert_eq!(wallet.identity().await?, identity);
    let app_id = wallet.app_id().await?;
    assert_eq!(app_id, prepared.app_id);
    eprintln!(
        "{name}: joined app {}",
        pubky_dispatch_core::enc::b64(&app_id)
    );

    let mut party = Party {
        name,
        secret,
        identity,
        store,
        home,
        host,
        wallet,
        _root: root,
    };
    // 6. The first receive, until the feed has nothing more, then one feed wait.
    party.drain(config).await?;
    let _wake = party.wallet.feed_wait(1).await?;
    eprintln!("{name}: feed_wait returned");

    // 7. A later start: the signer's due work, then the wallet from its App ID.
    party.wallet.shutdown().await?;
    let more = party.run_due(config).await?;
    eprintln!("{name}: later start, signer run_due, more due = {more}");
    party.wallet = party.host.load(app_id).await?;
    party.drain(config).await?;
    Ok(party)
}

async fn two_hosts_exchange_a_request(config: Config) -> TestResult {
    let payer = Box::pin(first_start("payer", &config)).await?;
    let payee = Box::pin(first_start("payee", &config)).await?;

    // The payer admits the payee (Dispatch spec 7), and the payee sends a request.
    payer.wallet.allow_connect(payee.identity.clone()).await?;
    let reference = format!("demo-{}", zbase32_encode(&random_bytes::<8>()?));
    let draft = RequestDraft {
        amount: Amount {
            asset: "btc".into(),
            value: "0.00012345".into(),
        },
        destination: RequestDestination::Endpoints {
            endpoints: vec![ListEndpoint {
                identifier: ENDPOINT.into(),
                payload: "lnbc1demo".into(),
            }],
        },
        payment_reference: reference.clone(),
        proposal_expires_at_ms: None,
        payment_deadline_ms: None,
        accepted_endpoint_identifiers: Some(vec![ENDPOINT.into()]),
        conversion: None,
        recurrence: None,
        period_deadline_s: None,
        per_period_endpoints: false,
        metadata_json: None,
        invite_code: None,
    };
    let sent = payee
        .wallet
        .send_request(payer.identity.clone(), draft)
        .await?;
    eprintln!("payee: sent request {}", sent.msg_id);

    // The payer's feed brings it.
    for _ in 0..12 {
        payer.drain(&config).await?;
        let requests = payer.wallet.requests().await?;
        if let Some(found) = requests.iter().find(|r| r.payment_reference == reference) {
            eprintln!(
                "payer: received request {} for {} {}",
                found.payment_reference, found.amount.value, found.amount.asset
            );
            assert_eq!(found.amount.value, "0.00012345");
            payer.wallet.shutdown().await?;
            payee.wallet.shutdown().await?;
            return Ok(());
        }
        let _wake = payer.wallet.feed_wait(5).await?;
        eprintln!("payer: no request yet, feed_wait returned");
    }
    Err("the payer never received the request".into())
}

#[test]
#[ignore = "needs a local development Dispatch and the fake Homeserver on this machine"]
fn test_demo_signer_first_start_and_request_on_live_services() {
    let config = Config {
        origin: setting("PAYKIT_LOCAL_DISPATCH_ORIGIN", "https://localhost:18580"),
        listener: setting("PAYKIT_LOCAL_DISPATCH_LISTENER", "http://127.0.0.1:18580"),
        homeserver: setting("PAYKIT_FAKE_HOMESERVER", "http://127.0.0.1:18590"),
    };
    let thread = std::thread::Builder::new()
        .stack_size(super::JOURNEY_STACK_BYTES * 4)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(Box::pin(two_hosts_exchange_a_request(config)))
                .unwrap();
        })
        .unwrap();
    thread.join().unwrap();
}

#[test]
fn test_demo_signer_refuses_bad_input() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let store: Arc<dyn WalletStore> = Arc::new(DeviceStore::open(root.path()));
    let home: Arc<dyn WalletHomeserver> =
        Arc::new(FakeHomeserver::new("http://127.0.0.1:1".into()).unwrap());
    let run = |secret: Vec<u8>, origin: &str, listener: &str| {
        runtime.block_on(demo_signer_run_due(
            secret,
            origin.into(),
            listener.into(),
            home.clone(),
            store.clone(),
        ))
    };
    let field = |result: Result<DemoSignerRun, DemoSignerError>| match result {
        Err(DemoSignerError::InvalidInput { field }) => field,
        _ => String::new(),
    };
    let origin = "https://localhost:18580";
    let listener = "http://127.0.0.1:18580";
    assert_eq!(field(run(vec![1; 31], origin, listener)), "identity secret");
    assert_eq!(
        field(run(vec![1; 32], "https://dispatch.example", listener)),
        "dispatch origin"
    );
    assert_eq!(
        field(run(vec![1; 32], origin, "http://10.0.0.1:18580")),
        "dispatch listener"
    );
    // Nothing is set up in a new store, and nothing was sent.
    assert!(matches!(
        run(vec![1; 32], origin, listener),
        Err(DemoSignerError::NotSetUp)
    ));
}
