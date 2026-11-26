//! The engine's Homeserver: the host's callbacks, shaped like paykit-ffi's `WalletHomeserver`.
//!
//! - `get(identityZ32, path) -> Promise<Uint8Array | null>` reads any identity's public file;
//!   `null` (or `undefined`) means the file is absent.
//! - `put(path, bytes) -> Promise<void>` and `delete(path) -> Promise<void>` write the engine's
//!   own identity's Homeserver, through the host's authenticated session for it. A write
//!   resolves only once it is durable.
//!
//! One [`JsHomeserver`] serves three traits: the app's `HomeserverReader` and
//! `HomeserverWriter`, and, through [`SignerHomeserver`], the signer's `Homeserver`. The signer
//! writes `/pub/pubky-dispatch/v1/identity.json` and its backup under `/priv/`, so the host's
//! session needs write access to both. The engine asks to write only its own identity's
//! files: a write for another identity is refused before the host is called.
//!
//! Paths are absolute (`/pub/...`, `/priv/...`) and passed to the host as the SDK and the
//! signer give them.

use std::sync::Arc;

use js_sys::{Array, Uint8Array};
use pubky_dispatch_client::app::{HomeserverReader, HomeserverWriter};
use pubky_dispatch_client::transport::BoxFuture;
use pubky_dispatch_client::TransportError;
use pubky_dispatch_core::enc::pubky_id;
use pubky_dispatch_signer::Homeserver;
use wasm_bindgen::{JsCast, JsValue};

use crate::host::{self, CallError};
use crate::options::HomeserverCallbacks;

/// How long a Homeserver callback may take before the call counts as timed out, in
/// milliseconds. A write that times out has an unknown outcome, as for any transport timeout.
const HOMESERVER_CALL_LIMIT_MS: u64 = 30_000;

/// The host's Homeserver callbacks, for the engine's identity.
pub struct JsHomeserver {
    own: [u8; 32],
    callbacks: HomeserverCallbacks,
}

impl JsHomeserver {
    /// The host's callbacks, writing as `own`.
    pub fn new(own: [u8; 32], callbacks: HomeserverCallbacks) -> Self {
        Self { own, callbacks }
    }

    fn read(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'static, Result<Option<Vec<u8>>, TransportError>> {
        let get = self.callbacks.get.clone();
        let call = host::sendable(async move {
            let args = Array::of2(&JsValue::from_str(&pubky_id(&identity)), &path.into());
            let value = host::call(&get, &args, HOMESERVER_CALL_LIMIT_MS).await?;
            if value.is_null() || value.is_undefined() {
                return Ok(None);
            }
            value
                .dyn_into::<Uint8Array>()
                .map(|bytes| Some(bytes.to_vec()))
                .map_err(|_| CallError::Failed("get gave neither a Uint8Array nor null".into()))
        });
        Box::pin(async move { call.await.map_err(|error| transport_error("get", error)) })
    }

    fn write(
        &self,
        identity: [u8; 32],
        path: String,
        bytes: Option<Vec<u8>>,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        if identity != self.own {
            return Box::pin(std::future::ready(Err(TransportError::Refused(
                "the engine writes its own identity's Homeserver only".into(),
            ))));
        }
        let (name, function) = match bytes {
            Some(_) => ("put", self.callbacks.put.clone()),
            None => ("delete", self.callbacks.delete.clone()),
        };
        let call = host::sendable(async move {
            let args = Array::of1(&path.into());
            if let Some(bytes) = bytes {
                args.push(&Uint8Array::from(bytes.as_slice()));
            }
            host::call(&function, &args, HOMESERVER_CALL_LIMIT_MS)
                .await
                .map(|_| ())
        });
        Box::pin(async move { call.await.map_err(|error| transport_error(name, error)) })
    }
}

/// The transport error of a failed callback. The host's own text is kept, after the
/// callback's name.
fn transport_error(name: &str, error: CallError) -> TransportError {
    match error {
        CallError::TimedOut => TransportError::Timeout,
        CallError::Failed(text) => {
            TransportError::Connect(format!("the host's homeserver {name} failed: {text}"))
        }
        CallError::Dropped => {
            TransportError::Other(format!("the host's homeserver {name} was dropped"))
        }
    }
}

impl HomeserverReader for JsHomeserver {
    fn get(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        self.read(identity, path)
    }
}

impl HomeserverWriter for JsHomeserver {
    fn put(
        &self,
        identity: [u8; 32],
        path: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        self.write(identity, path, Some(bytes))
    }

    fn delete(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        self.write(identity, path, None)
    }
}

/// The signer's own Homeserver: the same callbacks, on the engine's identity.
pub struct SignerHomeserver(pub Arc<JsHomeserver>);

impl Homeserver for SignerHomeserver {
    fn get(&self, path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        self.0.read(self.0.own, path.to_owned())
    }

    fn put(&self, path: &str, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TransportError>> {
        self.0.write(self.0.own, path.to_owned(), Some(bytes))
    }

    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), TransportError>> {
        self.0.write(self.0.own, path.to_owned(), None)
    }
}
