//! A wallet enrolled and joined on a live local development Dispatch through
//! `WalletHost::new_local_development`. Ignored by default: it needs the service running on
//! this machine. Run it with
//!
//! ```text
//! cargo test -p paykit-ffi live_dispatch -- --ignored
//! ```
//!
//! `PAYKIT_LOCAL_DISPATCH_ORIGIN` and `PAYKIT_LOCAL_DISPATCH_LISTENER` override the service's
//! signed origin and its plain HTTP listener. Each run enrols a new identity on the service,
//! and its Homeserver is in memory for the run.

use super::{method::Account, storage::DeviceStore};
use crate::*;
use pubky_dispatch_client::{
    app::{HomeserverReader, HomeserverWriter},
    clock::{Clock, SystemClock},
    store::{MemoryStore, StoreHandle},
    testing::{Seeds, TestHomeserver, TestIdentity},
    transport::BoxFuture,
    TransportError,
};
use pubky_dispatch_core::records::AuthRequest;
use pubky_dispatch_signer::{
    AddOutcome, Homeserver, IdentityKeyring, IdentitySecret, Protocols, SecretVault, Signer,
    SignerEnv, SignerStatus, VaultError,
};
use std::{
    error::Error,
    sync::{Arc, Mutex, PoisonError},
};

type TestResult = Result<(), Box<dyn Error>>;

const DEFAULT_ORIGIN: &str = "https://localhost:18580";
const DEFAULT_LISTENER: &str = "http://127.0.0.1:18580";

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

/// The wallet's Homeserver callbacks, on the run's in-memory Homeservers.
struct Home(Arc<TestHomeserver>);
#[async_trait::async_trait]
impl WalletHomeserver for Home {
    async fn get(&self, id: Vec<u8>, p: String) -> Result<Option<Vec<u8>>, HomeserverError> {
        let id = id.try_into().map_err(|_| HomeserverError::Refused)?;
        HomeserverReader::get(self.0.as_ref(), id, p)
            .await
            .map_err(|_| HomeserverError::Unavailable)
    }
    async fn put(&self, id: Vec<u8>, p: String, b: Vec<u8>) -> Result<(), HomeserverError> {
        let id = id.try_into().map_err(|_| HomeserverError::Refused)?;
        HomeserverWriter::put(self.0.as_ref(), id, p, b)
            .await
            .map_err(|_| HomeserverError::Unavailable)
    }
    async fn delete(&self, id: Vec<u8>, p: String) -> Result<(), HomeserverError> {
        let id = id.try_into().map_err(|_| HomeserverError::Refused)?;
        HomeserverWriter::delete(self.0.as_ref(), id, p)
            .await
            .map_err(|_| HomeserverError::Unavailable)
    }
}

/// The signer's own Homeserver, on the same in-memory Homeservers.
struct OwnHomeserver {
    identity: [u8; 32],
    files: Arc<TestHomeserver>,
}
impl Homeserver for OwnHomeserver {
    fn get(&self, path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        HomeserverReader::get(&*self.files, self.identity, path.to_owned())
    }
    fn put(&self, path: &str, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TransportError>> {
        self.files.put(self.identity, path, bytes);
        Box::pin(std::future::ready(Ok(())))
    }
    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), TransportError>> {
        self.files.remove(self.identity, path);
        Box::pin(std::future::ready(Ok(())))
    }
}

/// The identity secret, in memory for the run.
struct MemoryVault(Mutex<Option<IdentitySecret>>);
impl SecretVault for MemoryVault {
    fn load(&self) -> Result<Option<IdentitySecret>, VaultError> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
    fn add(&self, secret: &IdentitySecret) -> Result<AddOutcome, VaultError> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if held.is_some() {
            return Ok(AddOutcome::Occupied);
        }
        *held = Some(secret.clone());
        Ok(AddOutcome::Stored)
    }
    fn delete(&self, expected: &IdentitySecret) -> Result<(), VaultError> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if held
            .as_ref()
            .is_some_and(|secret| secret.as_bytes() == expected.as_bytes())
        {
            *held = None;
        }
        Ok(())
    }
}

/// A seed prefix no earlier run used, so each run enrols a new identity.
fn fresh_seed_prefix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut bytes = nanos.to_be_bytes().to_vec();
    bytes.extend(std::process::id().to_be_bytes());
    let hash = pubky_dispatch_core::enc::hash(&bytes);
    hash[..16].iter().map(|b| format!("{b:02x}")).collect()
}

async fn enrol_and_join(origin: &str, listener: &str) -> TestResult {
    let files = Arc::new(TestHomeserver::new());
    let identity = TestIdentity::new(&Seeds::new(&fresh_seed_prefix()), "local-wallet");
    let public = identity.public();
    eprintln!("enrolling identity {}", identity.key().id());

    // The identity's signer, on the same local transport the wallet host builds.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let env = SignerEnv {
        store: StoreHandle::inline(Arc::new(MemoryStore::new())),
        clock,
        transport: crate::local_dispatch::transport(origin, listener)?,
        homeserver: Arc::new(OwnHomeserver {
            identity: public,
            files: Arc::clone(&files),
        }),
        protocols: Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?,
    };
    let vault = MemoryVault(Mutex::new(Some(IdentitySecret::from_bytes(
        identity.key().secret_bytes(),
    ))));
    let keyring = IdentityKeyring::open(Arc::new(vault))?;
    let mut signer = Box::pin(Signer::create(env, keyring, origin)).await?;
    Box::pin(signer.resume()).await?;
    assert!(
        matches!(signer.status(), SignerStatus::Signing),
        "the signer stopped at {:?}",
        signer.status()
    );

    // The wallet's enrollment, through the exported API.
    let root = tempfile::tempdir()?;
    let enrollment = WalletEnrollment::new(Arc::new(DeviceStore::open(root.path())));
    let reference = vec![42; 16];
    let info = enrollment.begin_enrollment(reference.clone()).await?;
    let auth = AuthRequest {
        client_id: "wallet.example".into(),
        client_key: [8; 32],
        caps: format!("{}:rw", info.app_record_path),
        secret: [9; 32],
    };
    let prepared = enrollment
        .prepare_enrollment(
            reference.clone(),
            auth.client_id.clone(),
            auth.client_key.into(),
            auth.caps.clone(),
            auth.secret.into(),
            WalletScopes {
                pay: true,
                request: true,
                ..Default::default()
            },
        )
        .await?;
    let signed = serde_json::from_slice(&prepared.signed_request)?;
    let review = signer.review(&signed, &auth)?;
    let approved = Box::pin(signer.approve(review)).await?;
    enrollment
        .accept_approval(reference.clone(), public.into(), approved.claim.clone())
        .await?;
    Box::pin(signer.claim_delivered(&approved.request)).await?;

    // The host under test joins the app on the live service.
    let account = Arc::new(Account::open(root.path(), Arc::new(SystemClock)));
    let host = WalletHost::new_local_development(
        Arc::new(DeviceStore::open(root.path())),
        Arc::new(Home(Arc::clone(&files))),
        account,
        Account::declaration(true),
        vec![EndpointSupport {
            identifier: "btc-lightning-bolt11".into(),
            fraction_digits: Some(8),
        }],
        origin.to_owned(),
        listener.to_owned(),
        None,
    )?;
    let wallet = Box::pin(host.complete_enrollment(reference)).await?;
    assert_eq!(wallet.identity().await?, public.to_vec());
    assert_eq!(wallet.app_id().await?, prepared.app_id);
    eprintln!(
        "joined app {}",
        pubky_dispatch_core::enc::b64(&prepared.app_id)
    );

    // One more signed exchange with the service: read the app's inbox.
    let report = wallet.receive().await?;
    eprintln!("received from the live service: more = {}", report.more);
    wallet.shutdown().await?;
    Ok(())
}

#[test]
#[ignore = "needs a local development Dispatch running on this machine"]
fn test_local_development_host_joins_on_a_live_local_dispatch() {
    let origin = setting("PAYKIT_LOCAL_DISPATCH_ORIGIN", DEFAULT_ORIGIN);
    let listener = setting("PAYKIT_LOCAL_DISPATCH_LISTENER", DEFAULT_LISTENER);
    let thread = std::thread::Builder::new()
        .stack_size(super::JOURNEY_STACK_BYTES * 4)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(Box::pin(enrol_and_join(&origin, &listener)))
                .unwrap();
        })
        .unwrap();
    thread.join().unwrap();
}

#[test]
fn test_local_development_host_refuses_a_remote_listener() {
    let root = tempfile::tempdir().unwrap();
    let account = Arc::new(Account::open(root.path(), Arc::new(SystemClock)));
    let host = WalletHost::new_local_development(
        Arc::new(DeviceStore::open(root.path())),
        Arc::new(Home(Arc::new(TestHomeserver::new()))),
        account,
        Account::declaration(true),
        Vec::new(),
        DEFAULT_ORIGIN.into(),
        "http://dispatch.example:18580".into(),
        None,
    );
    assert!(
        matches!(host, Err(WalletError::InvalidInput { field }) if field == "dispatch listener")
    );
}
