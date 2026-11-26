//! The engine's own record in the app's store: which identity the store belongs to and which
//! app keeps its state there.
//!
//! It is kept under the engine's namespace ([`ENGINE_NAMESPACE`]), apart from the Dispatch
//! client's (`client/<App ID>`) and the SDK's, as the browser rig's harness keeps its App IDs.
//! It is written once the app has joined, so a later start loads the app instead of enrolling
//! again, and a start with another identity's secret is refused before anything is sent.
//!
//! If a start stops between the join and this write, the next start finds no record and
//! approves and joins a new app for the identity; the first one stays approved and unused.

use pubky_dispatch_client::store::{StateStore, WriteBatch};
use pubky_dispatch_client::StoreError;

use crate::error::EngineError;

/// The engine's namespace in the app's store.
const ENGINE_NAMESPACE: &str = "paykit-wasm";
/// The key of the identity's public key.
const IDENTITY_KEY: &[u8] = b"identity";
/// The key of the App ID.
const APP_ID_KEY: &[u8] = b"app_id";

/// What the app's store holds of the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineRecord {
    /// The identity's public key.
    pub identity: [u8; 32],
    /// The App ID of the app that keeps its state in the store.
    pub app_id: [u8; 16],
}

impl EngineRecord {
    /// The record `store` holds, if any.
    pub fn read(store: &dyn StateStore) -> Result<Option<Self>, EngineError> {
        let identity = read_key(store, IDENTITY_KEY)?;
        let app_id = read_key(store, APP_ID_KEY)?;
        match (identity, app_id) {
            (None, None) => Ok(None),
            (Some(identity), Some(app_id)) => Ok(Some(Self {
                identity: fixed(identity)?,
                app_id: fixed(app_id)?,
            })),
            _ => Err(corrupt()),
        }
    }

    /// Writes the record to `store`, in one commit.
    pub fn write(&self, store: &dyn StateStore) -> Result<(), EngineError> {
        let batch_error =
            |error| EngineError::store("batch the engine's record", &StoreError::from(error));
        let mut batch = WriteBatch::new();
        batch
            .put(
                ENGINE_NAMESPACE,
                IDENTITY_KEY.to_vec(),
                self.identity.to_vec(),
            )
            .map_err(batch_error)?;
        batch
            .put(ENGINE_NAMESPACE, APP_ID_KEY.to_vec(), self.app_id.to_vec())
            .map_err(batch_error)?;
        store
            .commit(batch)
            .map_err(|error| EngineError::store("keep the engine's record", &error))
    }
}

fn read_key(store: &dyn StateStore, key: &[u8]) -> Result<Option<Vec<u8>>, EngineError> {
    store
        .get(ENGINE_NAMESPACE, key)
        .map_err(|error| EngineError::store("read the engine's record", &error))
}

fn fixed<const N: usize>(bytes: Vec<u8>) -> Result<[u8; N], EngineError> {
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| corrupt())
}

fn corrupt() -> EngineError {
    EngineError::store(
        "read the engine's record",
        &"the app's store holds a malformed engine record",
    )
}
