//! DEMO ONLY: the identity's signer (`pubky-dispatch-signer`) run inside the host app, so a
//! wallet can approve its own app enrollment on a local development Dispatch.
//!
//! A real wallet never does this. The identity key belongs in a separate signer (Pubky Ring or
//! another signer install), and the wallet only ever sees the approval claim it returns. Here
//! the host hands its 32-byte identity secret to the bindings for the length of one call, and
//! the signer approves whatever app request it is given, without asking a user. That is a
//! shortcut for a demo on one machine, and nothing else. This module is compiled only with the
//! `demo-signer` feature, and every Dispatch request it makes goes through the local
//! development transport (`local_dispatch`), which reaches only a service on this machine.
//!
//! What the signer works with:
//!
//! - its identity key: the secret given to each call, held in memory for that call only;
//! - its records: the host's [`WalletStore`], in the signer's own namespace
//!   (`signer/<identity>`), apart from every namespace the wallet uses;
//! - its Homeserver: the host's [`WalletHomeserver`], for the identity's own files. The host's
//!   session for the identity must be able to write `/pub/pubky-dispatch/v1/` and
//!   `/priv/pubky-dispatch/v1/signer-backup.json` (a root session does);
//! - its protocols: Paykit's alone;
//! - its clock: the device clock.
//!
//! Each call opens the signer, does its work and closes it again, so no signer stays open
//! between calls and the store's namespace is free in between.

use crate::{
    environment::bounded, runtime, StorageError, StoreWrite, WalletHomeserver, WalletStore,
};
use pubky_dispatch_client::{
    clock::{Clock, SystemClock},
    store::{
        check_owned_batch, AcquireOutcome, CommitOutcome, KeyValue, StateStore, StoreError,
        StoreHandle, StoreLease, StoreOwner, Write, WriteBatch,
    },
    transport::BoxFuture,
    TransportError,
};
use pubky_dispatch_core::{records::AuthRequest, sig::SignedDoc};
use pubky_dispatch_signer::{
    state::signer_namespace, AddOutcome, Homeserver, IdentityKeyring, IdentitySecret, Protocols,
    Refusal, SecretVault, Signer, SignerEnv, SignerError, SignerStatus, VaultError,
};
use std::{
    future::Future,
    sync::{Arc, Mutex, PoisonError},
};
use zeroize::Zeroize;

/// Why a demo signer call failed.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum DemoSignerError {
    /// A value the host passed has the wrong form. Only the field's name is given.
    #[error("invalid {field}")]
    InvalidInput {
        /// The field.
        field: String,
    },
    /// The identity has no signer records in this store yet: approve an app first.
    #[error("the signer is not set up in this store")]
    NotSetUp,
    /// The signer is set up but doesn't sign now, for example because the Dispatch already
    /// holds an account for this identity that another install or an earlier store created.
    /// A demo starts again with a fresh identity or a reset Dispatch database.
    #[error("the signer does not sign: {status}")]
    NotSigning {
        /// The signer's status, as its debug text.
        status: String,
    },
    /// The signer failed: a Dispatch, Homeserver, store or key error, or a refusal.
    #[error("the signer failed: {reason}")]
    Failed {
        /// The signer's error text. It holds no key material.
        reason: String,
    },
    /// The call was made from inside a synchronous host callback, which it would wait for.
    #[error("called from inside a host callback")]
    Reentrant,
    /// The bindings could not start the threads they run this work on.
    #[error("the runtime is unavailable")]
    RuntimeUnavailable,
}

impl From<SignerError> for DemoSignerError {
    fn from(error: SignerError) -> Self {
        Self::Failed {
            reason: error.to_string(),
        }
    }
}

impl DemoSignerError {
    fn invalid(field: &str) -> Self {
        Self::InvalidInput {
            field: field.into(),
        }
    }
}

/// What [`demo_approve_own_app`] returns: the two values
/// `WalletEnrollment::accept_approval` takes besides the reference.
#[derive(uniffi::Record)]
pub struct DemoApproval {
    /// The identity's public key, 32 bytes: `accept_approval`'s `identity`.
    pub identity: Vec<u8>,
    /// The companion claim's plaintext (Dispatch wire 3.10): `accept_approval`'s `claim`.
    pub claim: Vec<u8>,
}

/// What one [`demo_signer_run_due`] did.
#[derive(uniffi::Record)]
pub struct DemoSignerRun {
    /// A bound cut the run short: run it again soon, for example in a minute.
    pub more_due: bool,
}

/// DEMO ONLY: approve this host's own app request with the identity's signer, run in
/// process against the local development Dispatch at `dispatch_origin`, reached at
/// `dispatch_listener` (as `WalletHost::new_local_development` takes them).
///
/// On the identity's first call the signer is set up in `store` and enrolls the identity at
/// the Dispatch: the service creates its account, and the signer writes the identity's
/// `identity.json` and its backup through `homeserver`. Later calls reopen it from `store`.
/// The signer then checks the request against the auth request it is bound to
/// (`client_id`, `client_key`, `capabilities` and `auth_secret`, exactly as given to
/// `WalletEnrollment::prepare_enrollment`), approves every scope it asks for without asking
/// anyone, signs the app's certificate and publishes the new `identity.json`.
///
/// `signed_request` is `EnrollmentRequest::signed_request` from `prepare_enrollment`. Pass
/// the answer to `WalletEnrollment::accept_approval` at once: the claim counts as delivered
/// when this call answers, and the signer refuses the same request again until the app has
/// joined (`WalletHost::complete_enrollment`).
///
/// The identity secret is the 32-byte Ed25519 secret key of the Pubky identity. It is held
/// in memory for this call only and wiped after it. Never use this in a release build: a
/// real wallet gets its approval from a separate signer.
///
/// # Errors
///
/// [`DemoSignerError::InvalidInput`] for a value of the wrong form,
/// [`DemoSignerError::NotSigning`] when the signer can't sign for this identity, and
/// [`DemoSignerError::Failed`] for a failure or refusal of the signer.
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn demo_approve_own_app(
    identity_secret: Vec<u8>,
    dispatch_origin: String,
    dispatch_listener: String,
    homeserver: Arc<dyn WalletHomeserver>,
    store: Arc<dyn WalletStore>,
    signed_request: Vec<u8>,
    client_id: String,
    client_key: Vec<u8>,
    capabilities: String,
    auth_secret: Vec<u8>,
) -> Result<DemoApproval, DemoSignerError> {
    let parts = SignerParts::new(
        identity_secret,
        &dispatch_origin,
        &dispatch_listener,
        homeserver,
        store,
    )?;
    let request: SignedDoc = serde_json::from_slice(&signed_request)
        .map_err(|_| DemoSignerError::invalid("signed request"))?;
    let mut auth = AuthRequest {
        client_id,
        client_key: fixed(client_key, "client key")?,
        caps: capabilities,
        secret: fixed(auth_secret, "auth secret")?,
    };
    on_runtime(async move {
        let identity = parts.identity;
        let mut signer = Box::pin(parts.open(true)).await?;
        let approved = Box::pin(async {
            let review = signer.review(&request, &auth)?;
            let approved = Box::pin(signer.approve(review)).await?;
            // The claim goes straight to the caller, in this process: it is delivered once
            // this call answers.
            Box::pin(signer.claim_delivered(&approved.request)).await?;
            Ok::<_, SignerError>(approved)
        })
        .await;
        auth.secret.zeroize();
        let closed = Box::pin(signer.close()).await;
        let approved = approved?;
        closed?;
        Ok(DemoApproval {
            identity: identity.to_vec(),
            claim: approved.claim,
        })
    })
    .await
}

/// DEMO ONLY: run the signer's due work once, as a signer does at start-up and now and then
/// (Dispatch spec 3.5, 4.3): finish a step a crash left, answer the apps' requests for full
/// key bundles, renew certificates past half their lifetime and repair the Homeserver's
/// `identity.json`. A wallet whose receive reports `scope_keys_awaited` waits for exactly
/// this. Arguments as for [`demo_approve_own_app`].
///
/// # Errors
///
/// [`DemoSignerError::NotSetUp`] before the identity's first approval in this store,
/// [`DemoSignerError::NotSigning`] when the signer can't sign for this identity, and
/// [`DemoSignerError::Failed`] for a failure of the run.
#[uniffi::export]
pub async fn demo_signer_run_due(
    identity_secret: Vec<u8>,
    dispatch_origin: String,
    dispatch_listener: String,
    homeserver: Arc<dyn WalletHomeserver>,
    store: Arc<dyn WalletStore>,
) -> Result<DemoSignerRun, DemoSignerError> {
    let parts = SignerParts::new(
        identity_secret,
        &dispatch_origin,
        &dispatch_listener,
        homeserver,
        store,
    )?;
    on_runtime(async move {
        let mut signer = Box::pin(parts.open(false)).await?;
        let ran = Box::pin(signer.run_due()).await;
        let closed = Box::pin(signer.close()).await;
        let ran = ran?;
        closed?;
        Ok(DemoSignerRun {
            more_due: ran.more_due(),
        })
    })
    .await
}

/// Run `work` on the bindings' runtime and wait for its answer.
async fn on_runtime<T: Send + 'static>(
    work: impl Future<Output = Result<T, DemoSignerError>> + Send + 'static,
) -> Result<T, DemoSignerError> {
    runtime::refuse_reentry().map_err(|_| DemoSignerError::Reentrant)?;
    runtime::spawn(work)
        .map_err(|_| DemoSignerError::RuntimeUnavailable)?
        .await
        .map_err(|_| DemoSignerError::Failed {
            reason: "the signer's task ended early".into(),
        })?
}

fn fixed(mut bytes: Vec<u8>, field: &str) -> Result<[u8; 32], DemoSignerError> {
    let fixed = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| DemoSignerError::invalid(field));
    bytes.zeroize();
    fixed
}

/// Everything a signer of the identity is opened with.
struct SignerParts {
    identity: [u8; 32],
    env: SignerEnv,
    vault: Arc<MemoryVault>,
    origin: String,
}

impl SignerParts {
    fn new(
        identity_secret: Vec<u8>,
        origin: &str,
        listener: &str,
        homeserver: Arc<dyn WalletHomeserver>,
        store: Arc<dyn WalletStore>,
    ) -> Result<Self, DemoSignerError> {
        let mut secret = fixed(identity_secret, "identity secret")?;
        let vault = Arc::new(MemoryVault(Mutex::new(Some(IdentitySecret::from_bytes(
            secret,
        )))));
        secret.zeroize();
        let identity = IdentityKeyring::open(vault.clone())
            .map_err(|_| DemoSignerError::invalid("identity secret"))?
            .public_key();
        let transport =
            crate::local_dispatch::transport(origin, listener).map_err(|error| match error {
                crate::WalletError::InvalidInput { field } => {
                    DemoSignerError::InvalidInput { field }
                }
                _ => DemoSignerError::invalid("dispatch"),
            })?;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])
            .map_err(|error| DemoSignerError::Failed {
                reason: error.to_string(),
            })?;
        let env = SignerEnv {
            store: StoreHandle::new(Arc::new(SignerStore {
                host: store,
                namespace: signer_namespace(&identity),
            })),
            clock,
            transport,
            homeserver: Arc::new(OwnHomeserver {
                identity,
                host: homeserver,
            }),
            protocols,
        };
        Ok(Self {
            identity,
            env,
            vault,
            origin: origin.to_owned(),
        })
    }

    fn keyring(&self) -> Result<IdentityKeyring, DemoSignerError> {
        IdentityKeyring::open(self.vault.clone())
            .map_err(|_| DemoSignerError::invalid("identity secret"))
    }

    /// The identity's signer from the store, set up for the origin first when `create` and
    /// the store holds none, with any step a crash left finished. It signs, or it is closed
    /// again and refused.
    async fn open(&self, create: bool) -> Result<Signer, DemoSignerError> {
        let mut signer = match Box::pin(Signer::open(self.env.clone(), self.keyring()?)).await {
            Ok(signer) => signer,
            Err(SignerError::Refused(Refusal::NotSetUp)) if create => {
                Box::pin(Signer::create(
                    self.env.clone(),
                    self.keyring()?,
                    &self.origin,
                ))
                .await?
            }
            Err(SignerError::Refused(Refusal::NotSetUp)) => return Err(DemoSignerError::NotSetUp),
            Err(error) => return Err(error.into()),
        };
        let resumed = Box::pin(signer.resume()).await;
        let status = signer.status();
        if resumed.is_ok() && status == SignerStatus::Signing {
            return Ok(signer);
        }
        let _ = Box::pin(signer.close()).await;
        resumed?;
        Err(DemoSignerError::NotSigning {
            status: format!("{status:?}"),
        })
    }
}

/// The identity secret, in memory for one call.
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

/// The identity's own Homeserver files, through the host's session for the identity.
struct OwnHomeserver {
    identity: [u8; 32],
    host: Arc<dyn WalletHomeserver>,
}

impl Homeserver for OwnHomeserver {
    fn get(&self, path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(bounded(
            self.host.get(self.identity.to_vec(), path.to_owned()),
        ))
    }

    fn put(&self, path: &str, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(bounded(self.host.put(
            self.identity.to_vec(),
            path.to_owned(),
            bytes,
        )))
    }

    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(bounded(
            self.host.delete(self.identity.to_vec(), path.to_owned()),
        ))
    }
}

/// The signer's records in the host's store, in the one namespace the signer of this identity
/// owns. Any other namespace is refused, so the signer can't touch the wallet's records.
struct SignerStore {
    host: Arc<dyn WalletStore>,
    namespace: String,
}

impl SignerStore {
    fn check(&self, ns: &str) -> Result<(), StoreError> {
        if ns == self.namespace {
            Ok(())
        } else {
            Err(StoreError::Other("not the signer's namespace".into()))
        }
    }
}

impl StateStore for SignerStore {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.check(ns)?;
        Ok(runtime::in_callback(|| {
            self.host.get(ns.to_owned(), key.to_vec())
        })?)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        self.check(ns)?;
        Ok(runtime::in_callback(|| {
            self.host
                .scan(ns.to_owned(), prefix.to_vec(), after.map(Vec::from), limit)
        })?
        .into_iter()
        .map(|entry| (entry.key, entry.value))
        .collect())
    }

    fn commit(&self, _: WriteBatch) -> Result<(), StoreError> {
        // The signer writes only as the namespace's owner (`commit_if`).
        Err(StoreError::InUse)
    }

    fn acquire(&self, ns: &str, owner: StoreOwner) -> Result<AcquireOutcome, StoreError> {
        self.check(ns)?;
        let token = owner.as_bytes().to_vec();
        match runtime::in_callback(|| self.host.acquire(ns.to_owned(), token.clone())) {
            Ok(true) => Ok(AcquireOutcome::Acquired(Arc::new(SignerLease {
                host: self.host.clone(),
                namespace: ns.to_owned(),
                token,
            }))),
            Ok(false) => Ok(AcquireOutcome::Busy),
            Err(StorageError::Unknown) => {
                // The acquisition may have applied: release this exact token, which leaves
                // any other owner in place.
                let _ = runtime::in_callback(|| self.host.release(ns.to_owned(), token));
                Err(StoreError::Unknown)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: StoreOwner,
        batch: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        self.check(ns)?;
        check_owned_batch(ns, &batch)?;
        let writes = batch
            .into_writes()
            .into_iter()
            .map(|write| match write {
                Write::Put { key, value, .. } => StoreWrite::Put { key, value },
                Write::Delete { key, .. } => StoreWrite::Delete { key },
                Write::DeletePrefix { prefix, .. } => StoreWrite::DeletePrefix { prefix },
            })
            .collect();
        let applied = runtime::in_callback(|| {
            self.host
                .commit_if(ns.to_owned(), owner.as_bytes().to_vec(), writes)
        })?;
        Ok(if applied {
            CommitOutcome::Applied
        } else {
            CommitOutcome::NotOwner
        })
    }
}

/// The signer's ownership of its namespace, released when the signer's last handle on it
/// goes. The release runs on the thread that drops it, so the namespace is free again once
/// `Signer::close` returns and the next call can open the signer at once.
struct SignerLease {
    host: Arc<dyn WalletStore>,
    namespace: String,
    token: Vec<u8>,
}

impl StoreLease for SignerLease {}

impl Drop for SignerLease {
    fn drop(&mut self) {
        let _ = runtime::in_callback(|| {
            self.host
                .release(self.namespace.clone(), std::mem::take(&mut self.token))
        });
    }
}
