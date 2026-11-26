//! # paykit-wasm
//!
//! A Paykit engine for a browser wallet: the SDK and the identity's signer, compiled to
//! WebAssembly, with a JavaScript API. Load it in a **dedicated worker**: its stores live in
//! the Origin Private File System, which only a dedicated worker can open for synchronous
//! access, and each store is held by one worker at a time. One engine runs per worker.
//!
//! The API passes plain JSON-shaped values in and out, and takes the host's callbacks for the
//! two things the engine does not do itself: Homeserver reads and writes, and payments.
//! `paykit_wasm.d.ts` declares the types.
//!
//! - `start(options)`: opens the stores, runs the in-process signer, enrols the identity and
//!   joins a new app on the first start, or reloads both on later starts. Resolves to
//!   `{identity, appId, enrolment: "new" | "loaded"}`; `identity` is the identity's Pubky ID in
//!   z-base-32, without a prefix, and `appId` is base64url (Dispatch wire 1.2).
//! - `identity()`: the started engine's Pubky ID.
//! - `stop()`: closes the signer and the app, releasing their stores.
//! - `version()`: this crate's version.
//!
//! A failed call rejects (or throws) an `Error` whose message is `<code>: <text>`, with the
//! codes `bad_options`, `already_started`, `not_started`, `busy`, `store`,
//! `identity_mismatch`, `signer`, `enrolment` and `paykit` (`error.rs`).
//!
//! **Demo shortcut.** The host passes the identity's secret key to `start`, and the signer runs
//! in the same worker as the app. Pubky's direction keeps identity keys out of apps; the engine
//! accepts the key because the host, a wallet that derives it from its own seed, holds it
//! already.
//!
//! On every target but `wasm32-unknown-unknown` the crate is empty.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

// Every item below carries `#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]`, the one
// target the engine runs on, so a native build of the crate compiles nothing. The attribute sits
// on each item rather than inside a macro so that rustfmt still follows the `mod` declarations.

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod engine;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod error;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod homeserver;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod host;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod options;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod rail;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod record;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod vault;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use api::*;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod api {
    use std::cell::RefCell;
    use std::rc::Rc;

    use js_sys::JSON;
    use pubky_dispatch_core::enc::{b64, pubky_id};
    use serde_json::json;
    use wasm_bindgen::prelude::{wasm_bindgen, JsError, JsValue};

    use crate::engine::Engine;
    use crate::error::EngineError;
    use crate::options::StartOptions;

    #[wasm_bindgen(typescript_custom_section)]
    const TYPES: &'static str = r#"
/** The host's Homeserver. `get` reads any identity's public file; `put` and `delete` write the
 * engine's own identity's Homeserver and resolve once the write is durable. Paths are absolute
 * (`/pub/...`, `/priv/...`). */
export interface EngineHomeserver {
  /** The file's bytes, or `null` when it is absent. */
  get(identityZ32: string, path: string): Promise<Uint8Array | null>;
  put(path: string, bytes: Uint8Array): Promise<void>;
  delete(path: string): Promise<void>;
}

/** A Payment Endpoint Identifier the host's method pays, with its fraction digits. */
export interface EndpointSupport {
  identifier: string;
  fractionDigits: number;
}

/** An amount: a decimal string in whole units of the asset. */
export interface RailAmount {
  value: string;
  asset: string;
}

/** A payment to make. `attemptId` is the idempotency key. */
export interface RailPayment {
  attemptId: string;
  endpointId: string;
  /** The endpoint's payload, as published, such as `{"value":"<address>"}`. */
  payload: string;
  amount: RailAmount;
  paymentReference: string | null;
}

/** A payment's outcome: paid with its proof, definitely failed, or not settled yet. */
export type RailOutcome =
  | { status: "paid"; proof: Record<string, unknown>; paidAtMs?: number }
  | { status: "failed" }
  | { status: "pending" };

/** A received proof for the payee's method to check. */
export interface RailProofCheck {
  endpointId: string;
  payload: string | null;
  amount: RailAmount;
  attemptId: string;
  proof: Record<string, unknown>;
}

/** The host's payment method. The engine answers what it can from `endpoints`; the callbacks
 * are called for the rest. A missing callback makes that call fail as unavailable (`pay`:
 * refused, nothing sent). */
export interface EngineRail {
  endpoints: EndpointSupport[];
  pay?(payment: RailPayment): Promise<RailOutcome>;
  /** `null` only when `pay` was never called with `attemptId`. */
  payment?(attemptId: string): Promise<RailOutcome | null>;
  /** The verified payment time in Unix milliseconds, or `null`. */
  verify?(check: RailProofCheck): Promise<number | null>;
}

/** The options of `start`. */
export interface StartOptions {
  /** The identity's 32-byte secret key, as 64 hex digits. */
  identitySecretHex: string;
  /** The Dispatch origin requests are signed for, such as `https://localhost:18580`. */
  dispatchOrigin: string;
  /** A plain HTTP loopback listener that requests for a `https://localhost` origin go to,
   * such as `http://127.0.0.1:18580`; omit it to reach the origin itself. */
  dispatchListener?: string;
  /** The prefix of the store names `<prefix>-app` and `<prefix>-signer`: 1 to 57 ASCII
   * letters, digits, `-` or `_`. Default `paykit`. It must not name the identity. */
  storePrefix?: string;
  /** The client ID of the app's authentication request. Default `paykit-wasm`. */
  clientId?: string;
  homeserver: EngineHomeserver;
  rail: EngineRail;
}

/** What `start` resolves to. */
export interface StartResult {
  /** The identity's Pubky ID, z-base-32, without a prefix. */
  identity: string;
  /** The app's App ID, base64url. */
  appId: string;
  /** `new`: enrolled and joined now; `loaded`: reopened from the stores. */
  enrolment: "new" | "loaded";
}
"#;

    /// The engine of this worker.
    enum Slot {
        /// None started.
        Empty,
        /// `start` or `stop` is in progress.
        Changing,
        /// Started.
        Ready(Rc<Engine>),
    }

    thread_local! {
        static SLOT: RefCell<Slot> = const { RefCell::new(Slot::Empty) };
    }

    /// Installs the panic hook when the module is instantiated: a panic is reported to the
    /// console, then aborts the instance, which the host replaces with a new worker.
    #[wasm_bindgen(start)]
    fn instantiated() {
        paykit_browser::panic::install_panic_hook();
    }

    /// This crate's version.
    #[wasm_bindgen]
    pub fn version() -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    /// Starts the engine of this worker with `options` (`StartOptions` in the declarations).
    ///
    /// # Errors
    ///
    /// `bad_options`, `already_started`, `busy`, `store`, `identity_mismatch`, `signer`,
    /// `enrolment` or `paykit`.
    #[wasm_bindgen(unchecked_return_type = "StartResult")]
    pub async fn start(
        #[wasm_bindgen(unchecked_param_type = "StartOptions")] options: JsValue,
    ) -> Result<JsValue, JsError> {
        let options = StartOptions::read(&options)?;
        SLOT.with_borrow_mut(|slot| match slot {
            Slot::Empty => {
                *slot = Slot::Changing;
                Ok(())
            }
            Slot::Changing => Err(EngineError::busy()),
            Slot::Ready(_) => Err(EngineError::already_started()),
        })?;
        let started = Engine::start(options).await;
        let (engine, enrolment) = match started {
            Ok(started) => started,
            Err(error) => {
                SLOT.set(Slot::Empty);
                return Err(error.into());
            }
        };
        let result = json!({
            "identity": pubky_id(&engine.identity),
            "appId": b64(&engine.app_id),
            "enrolment": enrolment.as_str(),
        });
        SLOT.set(Slot::Ready(Rc::new(engine)));
        Ok(JSON::parse(&result.to_string()).unwrap_or(JsValue::NULL))
    }

    /// The started engine's Pubky ID, z-base-32, without a prefix.
    ///
    /// # Errors
    ///
    /// `not_started`, or `busy` while the engine starts or stops.
    #[wasm_bindgen]
    pub fn identity() -> Result<String, JsError> {
        SLOT.with_borrow(|slot| match slot {
            Slot::Ready(engine) => Ok(pubky_id(&engine.identity)),
            Slot::Changing => Err(EngineError::busy().into()),
            Slot::Empty => Err(EngineError::not_started().into()),
        })
    }

    /// Stops the engine: closes the signer and the app, which releases their stores.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy` while another call is in progress, or `signer` when the signer
    /// could not close cleanly; the engine is stopped either way.
    #[wasm_bindgen]
    pub async fn stop() -> Result<(), JsError> {
        let running = SLOT.with_borrow_mut(|slot| {
            let engine = match slot {
                Slot::Ready(engine) => Rc::clone(engine),
                Slot::Changing => return Err(EngineError::busy()),
                Slot::Empty => return Err(EngineError::not_started()),
            };
            let running = engine.take()?;
            *slot = Slot::Changing;
            Ok(running)
        })?;
        let stopped = Engine::stop(running).await;
        SLOT.set(Slot::Empty);
        stopped.map_err(JsError::from)
    }
}
