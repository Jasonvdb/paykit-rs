//! The engine: one identity's Paykit App and its signer, in this worker.
//!
//! `start` opens two stores in the Origin Private File System, `<prefix>-app` for the app and
//! `<prefix>-signer` for the signer, each sealed under a key the origin holds
//! (`WrappingKey::Origin`). Then:
//!
//! - **First start** (the app's store holds no engine record): the signer is set up for the
//!   identity and enrols it at the Dispatch, which publishes its `identity.json` on the
//!   identity's Homeserver; or, if an earlier start stopped after setting it up, the signer
//!   reopens and finishes. The signer then approves a new app, with fresh keys, for the pay
//!   and request scopes; the app joins the Dispatch; and the engine record is written.
//! - **Later starts**: the signer reopens from its store and resumes, and the app loads from
//!   its store. Nothing is enrolled or approved again.
//!
//! The signer stays open in the worker beside the app, as it stores the scope keys the app
//! waits for when it is next online, and renews the app's certificate when due
//! (`signerTick`, and the feed loop once a minute).
//!
//! **Restarts.** A worker that ends without `stop`, such as one the host terminated or one a
//! panic aborted, leaves both stores as their last commit left them, and the browser releases
//! their files once the worker is gone. The next start opens them again, waiting a few seconds
//! for a release still under way, and loads: nothing is enrolled or approved again, and the
//! Dispatch client hands over again what the last read did not finish.
//!
//! What the stores hold is sealed, but the identity secret comes from the host on every start:
//! see `vault.rs`.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::fetch::{FetchConfig, FetchTransport};
use paykit_browser::store::{OpenError, OpfsStore, WrappingKey};
use paykit_browser::timer::JsTimer;
use paykit_sdk::{AppEnv, AppScopes, JoinRequest, Paykit, PaykitConfig, Rail, Timer as _};
use pubky_dispatch_client::store::{StateStore, StoreHandle};
use pubky_dispatch_client::transport::Transport;
use pubky_dispatch_core::enc::random_bytes;
use pubky_dispatch_core::keys::AppKeys;
use pubky_dispatch_core::records::{
    app_record_path, AppRequest, AppRequestBody, ApprovalClaim, AuthRequest,
};
use pubky_dispatch_signer::{
    IdentityKeyring, IdentitySecret, Protocols, Refusal, Signer, SignerEnv, SignerError,
    SignerStatus,
};
use zeroize::Zeroizing;

use crate::error::EngineError;
use crate::events::EventSink;
use crate::feed::Observed;
use crate::flights::Flights;
use crate::homeserver::{JsHomeserver, SignerHomeserver};
use crate::options::StartOptions;
use crate::rail::JsRail;
use crate::record::EngineRecord;
use crate::turn::{StopSignal, TurnState};
use crate::vault::MemoryVault;

/// How long a start waits for a store that another worker, such as one just terminated,
/// still holds, in milliseconds.
const STORE_IN_USE_WAIT_MS: u64 = 10_000;
/// The pause between two tries to open a store in use, in milliseconds.
const STORE_IN_USE_RETRY_MS: u64 = 250;

/// The signer's files on the identity's Homeserver: its published `identity.json` and its
/// backup (pubky-dispatch-signer's Homeserver paths).
pub const SIGNER_FILES: [&str; 2] = [
    "/pub/pubky-dispatch/v1/identity.json",
    "/priv/pubky-dispatch/v1/signer-backup.json",
];

/// How `start` got the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enrolment {
    /// The identity was enrolled, or its enrolment finished, and a new app joined.
    New,
    /// The app and its signer were loaded from their stores.
    Loaded,
}

impl Enrolment {
    /// The name JavaScript sees.
    pub fn as_str(self) -> &'static str {
        match self {
            Enrolment::New => "new",
            Enrolment::Loaded => "loaded",
        }
    }
}

/// A started engine.
pub struct Engine {
    /// The identity's public key.
    pub identity: [u8; 32],
    /// The app's App ID.
    pub app_id: [u8; 16],
    /// The app and its signer, taken out for the length of each call that uses them
    /// ([`crate::turn::Turn`]), so no borrow is held across an await.
    turns: TurnState,
    /// The host's event callback.
    pub events: EventSink,
    /// Raised when the engine stops, to end the feed loop's wait.
    pub stop: StopSignal,
    /// What the feed loop last saw, to tell the host what changed.
    pub observed: RefCell<Observed>,
    /// The host's Homeserver, as the engine's identity's.
    pub homeserver: Arc<JsHomeserver>,
    /// The names of the app's and the signer's stores.
    pub store_names: [String; 2],
    /// When the signer's due work runs next, on the worker's wall clock in Unix ms.
    pub signer_due_at: Cell<f64>,
    /// The host's `pay` calls in progress and their late answers.
    pub flights: Flights,
    /// A payment may be open: the feed loop resumes the payments at its next turn.
    pub payments_open: Cell<bool>,
}

/// What an engine call works on.
pub struct Running {
    /// The identity's Paykit App.
    pub paykit: Paykit,
    /// The identity's signer.
    pub signer: Signer,
}

impl Engine {
    /// Starts the engine of `options`: see the module docs.
    pub async fn start(options: StartOptions) -> Result<(Engine, Enrolment), EngineError> {
        let secret = Zeroizing::new(*options.secret);
        let keyring = || {
            let vault = MemoryVault::holding(IdentitySecret::from_bytes(*secret));
            IdentityKeyring::open(Arc::new(vault))
                .map_err(|error| EngineError::signer("open the identity's key", &error))
        };
        let identity = keyring()?.public_key();

        let transport: Arc<dyn Transport> = match &options.dispatch_listener {
            Some(listener) => Arc::new(
                FetchTransport::for_local_development(
                    FetchConfig::default(),
                    &options.dispatch_origin,
                    listener,
                )
                .map_err(|error| {
                    EngineError::bad_options(format!(
                        "dispatchOrigin and dispatchListener are refused: {error}"
                    ))
                })?,
            ),
            None => Arc::new(FetchTransport::new(FetchConfig::default())),
        };

        let app_store = open_store(&options.app_store_name(), "open the app's store").await?;
        let record = EngineRecord::read(app_store.as_ref())?;
        if record.is_some_and(|record| record.identity != identity) {
            return Err(EngineError::identity_mismatch());
        }
        let signer_store =
            open_store(&options.signer_store_name(), "open the signer's store").await?;

        let homeserver = Arc::new(JsHomeserver::new(identity, options.homeserver.clone()));
        let own_homeserver = Arc::clone(&homeserver);
        let signer_env = SignerEnv {
            store: StoreHandle::inline(signer_store),
            clock: Arc::new(JsClock),
            transport: Arc::clone(&transport),
            homeserver: Arc::new(SignerHomeserver(Arc::clone(&homeserver))),
            protocols: Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])
                .map_err(|error| EngineError::signer("register Paykit's protocol", &error))?,
        };
        let app_env = AppEnv {
            store: StoreHandle::inline(Arc::clone(&app_store)),
            transport,
            clock: Arc::new(JsClock),
            homeserver: homeserver.clone(),
            homeserver_writer: Some(homeserver),
        };
        let flights = Flights::default();
        let rail: Arc<dyn Rail> = Arc::new(JsRail::new(
            options.endpoints.clone(),
            options.rail.clone(),
            options.pay_limit_ms,
            flights.clone(),
        ));

        let (paykit, signer, enrolment) = match record {
            Some(record) => {
                let mut signer = Box::pin(Signer::open(signer_env, keyring()?))
                    .await
                    .map_err(|error| EngineError::signer("reopen the signer", &error))?;
                resume(&mut signer).await?;
                let paykit = Box::pin(Paykit::load(
                    app_env,
                    PaykitConfig::default(),
                    record.app_id,
                    rail,
                    Arc::new(JsTimer),
                ))
                .await
                .map_err(|error| EngineError::paykit("load the app from its store", &error))?;
                (paykit, signer, Enrolment::Loaded)
            }
            None => {
                let mut signer = Box::pin(open_or_create(
                    signer_env,
                    keyring,
                    &options.dispatch_origin,
                ))
                .await?;
                resume(&mut signer).await?;
                let join =
                    Box::pin(approve_new_app(&mut signer, identity, &options.client_id)).await?;
                let paykit = Box::pin(Paykit::join(
                    app_env,
                    PaykitConfig::default(),
                    join,
                    rail,
                    Arc::new(JsTimer),
                ))
                .await
                .map_err(|error| EngineError::paykit("join the app", &error))?;
                EngineRecord {
                    identity,
                    app_id: paykit.app_id(),
                }
                .write(app_store.as_ref())?;
                (paykit, signer, Enrolment::New)
            }
        };

        let engine = Engine {
            identity,
            app_id: paykit.app_id(),
            turns: TurnState::holding(Running { paykit, signer }),
            events: EventSink::new(options.on_event.clone()),
            stop: StopSignal::default(),
            observed: RefCell::new(Observed::default()),
            homeserver: own_homeserver,
            store_names: [options.app_store_name(), options.signer_store_name()],
            signer_due_at: Cell::new(0.0),
            flights,
            // A start resumes whatever payment an earlier run left open.
            payments_open: Cell::new(true),
        };
        Ok((engine, enrolment))
    }

    /// Where the app and the signer are, for [`crate::turn::Turn`].
    pub fn turns(&self) -> &TurnState {
        &self.turns
    }

    /// Stops the engine: closes the signer, then drops the app. Each store closes once the
    /// last handle to it is dropped, which releases its files for another worker.
    pub async fn stop(running: Running) -> Result<(), EngineError> {
        let Running { paykit, signer } = running;
        let closed = Box::pin(signer.close()).await;
        drop(paykit);
        closed.map_err(|error| EngineError::signer("close the signer", &error))
    }
}

/// Opens the store `name`, its data key wrapped by a key the origin holds. A store another
/// worker holds is tried again for [`STORE_IN_USE_WAIT_MS`]: a worker the host has just
/// terminated releases its files shortly after.
async fn open_store(name: &str, step: &str) -> Result<Arc<dyn StateStore>, EngineError> {
    let mut waited = 0;
    loop {
        match OpfsStore::open(name, WrappingKey::Origin).await {
            Ok(store) => return Ok(Arc::new(store)),
            Err(OpenError::InUse) if waited < STORE_IN_USE_WAIT_MS => {
                JsTimer.sleep(STORE_IN_USE_RETRY_MS).await;
                waited += STORE_IN_USE_RETRY_MS;
            }
            Err(error) => return Err(EngineError::store(step, &error)),
        }
    }
}

/// Destroys the store `name`, waiting as [`open_store`] does for one still held.
pub async fn destroy_store(name: &str) -> Result<(), EngineError> {
    let mut waited = 0;
    loop {
        match OpfsStore::destroy(name).await {
            Ok(()) => return Ok(()),
            Err(OpenError::InUse) if waited < STORE_IN_USE_WAIT_MS => {
                JsTimer.sleep(STORE_IN_USE_RETRY_MS).await;
                waited += STORE_IN_USE_RETRY_MS;
            }
            Err(error) => return Err(EngineError::store("destroy a store", &error)),
        }
    }
}

/// The signer of the identity: reopened from its store if an earlier start set it up, or set
/// up now to enrol at `origin`.
async fn open_or_create(
    env: SignerEnv,
    keyring: impl Fn() -> Result<IdentityKeyring, EngineError>,
    origin: &str,
) -> Result<Signer, EngineError> {
    match Box::pin(Signer::open(env.clone(), keyring()?)).await {
        Ok(signer) => Ok(signer),
        Err(SignerError::Refused(Refusal::NotSetUp)) => {
            Box::pin(Signer::create(env, keyring()?, origin))
                .await
                .map_err(|error| EngineError::signer("set up the signer", &error))
        }
        Err(error) => Err(EngineError::signer("reopen the signer", &error)),
    }
}

/// Resumes `signer`, which finishes an enrolment or any flow a stop left, and checks that it
/// signs for the identity.
async fn resume(signer: &mut Signer) -> Result<(), EngineError> {
    Box::pin(signer.resume())
        .await
        .map_err(|error| EngineError::signer("resume the signer", &error))?;
    match signer.status() {
        SignerStatus::Signing => Ok(()),
        other => Err(EngineError::signer(
            "resume the signer",
            &format_args!("it stopped at {other:?}, not signing"),
        )),
    }
}

/// Has `signer` approve a new app with fresh keys for the pay and request scopes, and returns
/// the app's join request (Dispatch wire 3.9, 3.10).
///
/// The app's request is bound to an authentication request that never leaves the worker: the
/// signer is in-process, so there is no Pubky Auth exchange. It asks for the app's own record
/// path only, as Paykit adds no grant paths, and its client key and secret are random.
async fn approve_new_app(
    signer: &mut Signer,
    identity: [u8; 32],
    client_id: &str,
) -> Result<JoinRequest, EngineError> {
    let scopes = AppScopes::new()
        .pay()
        .request()
        .build()
        .map_err(|error| EngineError::enrolment("build the pay and request scopes", &error))?
        .scopes;
    let random = |error: pubky_dispatch_core::Error| {
        EngineError::enrolment("draw the app's random values", &error)
    };
    let keys = AppKeys::generate().map_err(random)?;
    let auth = AuthRequest {
        client_id: client_id.to_owned(),
        client_key: random_bytes::<32>().map_err(random)?,
        caps: format!("{}:rw", app_record_path(&keys.app_id_bytes())),
        secret: random_bytes::<32>().map_err(random)?,
    };
    let request = AppRequest::sign(&keys, AppRequestBody::new(&keys, scopes, &auth))
        .map_err(|error| EngineError::enrolment("sign the app's request", &error))?;
    let review = signer
        .review(request.signed(), &auth)
        .map_err(|error| EngineError::signer("review the app's request", &error))?;
    let approved = Box::pin(signer.approve(review))
        .await
        .map_err(|error| EngineError::signer("approve the app", &error))?;
    Box::pin(signer.claim_delivered(&approved.request))
        .await
        .map_err(|error| EngineError::signer("record the approval as delivered", &error))?;
    let claim = ApprovalClaim::from_plaintext(&approved.claim)
        .map_err(|error| EngineError::enrolment("read the approval", &error))?;
    let approval = claim
        .verify(&identity, &auth)
        .map_err(|error| EngineError::enrolment("verify the approval", &error))?
        .to_vec();
    Ok(JoinRequest {
        identity,
        keys,
        request,
        approval,
    })
}
