//! # paykit-wasm
//!
//! A Paykit engine for a browser wallet: the SDK and the identity's signer, compiled to
//! WebAssembly, with a JavaScript API. Load it in a **dedicated worker**: its stores live in
//! the Origin Private File System, which only a dedicated worker can open for synchronous
//! access, and each store is held by one worker at a time. One engine runs per worker.
//!
//! The API passes plain JSON-shaped values in and out, and takes the host's callbacks for the
//! two things the engine does not do itself: Homeserver reads and writes, and payments.
//! `paykit_wasm.d.ts` declares the types. Names follow paykit-ffi's `NativeWallet` calls in
//! camelCase, so a web host and a native host drive Paykit the same way.
//!
//! - `start(options)`: opens the stores, runs the in-process signer, enrols the identity and
//!   joins a new app on the first start, or reloads both on later starts, then starts the feed
//!   loop (unless `autoReceive` is `false`). Resolves to `{identity, appId, enrolment: "new" |
//!   "loaded"}`; `identity` is the identity's Pubky ID in z-base-32, without a prefix, and
//!   `appId` is base64url (Dispatch wire 1.2).
//! - `identity()`, `appId()`: the started engine's.
//! - `allowConnect(identity)`: lets that identity start a conversation, as a payer does for a
//!   payee it expects requests from (Dispatch spec 7). `admits(identity)` and `contacts()`
//!   read the admission state.
//! - `requests()`: the one-time requests this identity holds as payer; `payeeActivity()`: the
//!   requests it sent and the payments it received as payee.
//! - `receive()`: one receive round, for a host that drives the feed itself.
//! - `signerTick()`: the signer's due work, which the feed loop also runs once a minute.
//! - `stop()`: closes the signer and the app, releasing their stores.
//! - `resetIdentity(options?)`: for tests, stops the engine and deletes its stores and the
//!   signer's files on the Homeserver.
//! - `version()`: this crate's version.
//!
//! The engine tells the host what changes through the `onEvent` callback (`events.rs`).
//!
//! A failed call rejects (or throws) an `Error` whose message is `<code>: <text>`, with the
//! codes `bad_options`, `already_started`, `not_started`, `busy`, `store`,
//! `identity_mismatch`, `signer`, `enrolment`, `paykit`, `invalid_input` and `homeserver`
//! (`error.rs`). Calls made while another runs wait their turn; `busy` means the engine is
//! starting or stopping.
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
mod events;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod feed;
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
mod turn;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod vault;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod views;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use api::*;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod api {
    use std::cell::RefCell;
    use std::rc::Rc;

    use js_sys::{Array, JSON};
    use pubky_dispatch_core::enc::{b64, pubky_id};
    use serde_json::{json, Value};
    use wasm_bindgen::prelude::{wasm_bindgen, JsError, JsValue};

    use crate::engine::{destroy_store, Engine, SIGNER_FILES};
    use crate::error::EngineError;
    use crate::feed::{self, RoundError};
    use crate::options::{
        app_store_name, signer_store_name, HomeserverCallbacks, ResetOptions, StartOptions,
    };
    use crate::turn::Turn;
    use crate::views::{self, Admission};

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
  /** Whether the engine runs its own feed loop until `stop`. Default `true`; with `false`
   * the host calls `receive` itself. */
  autoReceive?: boolean;
  /** Called with each event, in order. A callback that throws is ignored. */
  onEvent?: (event: EngineEvent) => void;
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

/** A request's ID: the payee's Pubky ID and the request hash in hex. */
export interface RequestId {
  payee: string;
  requestHash: string;
}

/** Where a request is paid, as its signed body names it. */
export type RequestDestination =
  | { type: "endpoints"; endpoints: { identifier: string; payload: string }[] }
  | { type: "app"; appId: string }
  | { type: "any" };

/** A receipt the payer holds. */
export interface Receipt {
  record: string;
  payee: string;
  requestHash: string | null;
  attemptId: string;
  messageId: string;
  acceptedAtMs: number | null;
  paymentReference: string | null;
}

/** What the payer sees of a request (paykit-ffi `OneTimeRequestStatus`). */
export type OneTimeRequestStatus =
  | "open" | "beingPaidHere" | "beingHandledByOtherWallet" | "mayBePaid" | "paid"
  | "rejected" | "cancelled" | "expired" | "held" | "unrecorded" | "blocked" | "divergent"
  | string;

/** A one-time request the payer holds (paykit-ffi `OneTimeRequest`). */
export interface OneTimeRequest {
  id: RequestId;
  record: string;
  amount: RailAmount;
  paymentReference: string;
  label: string | null;
  proposalExpiresAtMs: number | null;
  status: OneTimeRequestStatus;
  held: boolean;
  manualOnly: boolean;
  divergent: boolean;
  receipt: Receipt | null;
  destination: RequestDestination;
}

/** What the payee sees (paykit-ffi `PayeeStatus`). */
export type PayeeStatus =
  | "requested" | "accepted" | "rejected" | "cancelled" | "proofReceived" | "receipted"
  | string;

/** A request the payee sent, or a payment it received without one (paykit-ffi
 * `PayeeRequest`). */
export interface PayeeRequest {
  record: string;
  payer: string;
  requestHash: string | null;
  amount: RailAmount | null;
  label: string | null;
  attemptId: string | null;
  status: PayeeStatus;
  divergent: boolean;
  recurring: boolean;
}

/** A contact's admission: whether it may start a conversation with this identity. */
export interface Admission {
  identity: string;
  admitted: boolean;
  kept: boolean;
  reply: boolean;
}

/** The identity's allow list and deny list, as its Dispatch holds them. */
export interface Contacts {
  allowed: Admission[];
  denied: string[];
}

/** What a receive round did. */
export interface ReceiveResult {
  generation: number;
  sequence: number;
  more: boolean;
  stopped: boolean;
  scopeKeysAwaited: boolean;
  recordsUnlisted: string | null;
  accountGone: boolean;
  /** The message IDs whose effects the round applied. */
  applied: string[];
}

/** An event of the engine, given to `onEvent`. `atMs` is the worker's clock. */
export type EngineEvent = { atMs: number } & (
  | ({ type: "received" } & ReceiveResult)
  | { type: "messageReceived"; kind: string; msgId: string; sender: string; seq: number }
  | ({ type: "admissionChanged" } & Admission)
  | { type: "requestReceived"; request: OneTimeRequest }
  | { type: "requestUpdated"; request: OneTimeRequest; previousStatus: string }
  | { type: "receiptReceived"; request: OneTimeRequest; receipt: Receipt }
  | { type: "proofReceived"; activity: PayeeRequest }
  | { type: "activityUpdated"; activity: PayeeRequest; previousStatus: string | null }
  | { type: "error"; during: string; code: string; message: string }
  | { type: "feedStopped"; reason: string }
);

/** What `signerTick` did. */
export interface SignerTick {
  /** More work was due than one run does: tick again soon. */
  moreDue: boolean;
}

/** The options of `resetIdentity` when no engine runs in the worker. */
export interface ResetOptions {
  /** The prefix of the stores to destroy. Default `paykit`. */
  storePrefix?: string;
  /** The Homeserver to delete the signer's files from; omitted, they stay. */
  homeserver?: EngineHomeserver;
}
"#;

    /// The engine of this worker.
    enum Slot {
        /// None started.
        Empty,
        /// `start`, `stop` or `resetIdentity` is in progress.
        Changing,
        /// Started.
        Ready(Rc<Engine>),
    }

    thread_local! {
        static SLOT: RefCell<Slot> = const { RefCell::new(Slot::Empty) };
    }

    /// The started engine.
    fn engine() -> Result<Rc<Engine>, EngineError> {
        SLOT.with_borrow(|slot| match slot {
            Slot::Ready(engine) => Ok(Rc::clone(engine)),
            Slot::Changing => Err(EngineError::busy()),
            Slot::Empty => Err(EngineError::not_started()),
        })
    }

    /// A JavaScript value of `value`.
    fn to_js(value: &Value) -> JsValue {
        JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
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

    /// Starts the engine of this worker with `options` (`StartOptions` in the declarations),
    /// then its feed loop unless `autoReceive` is `false`.
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
        let auto_receive = options.auto_receive;
        let started = Engine::start(options).await;
        let (engine, enrolment) = match started {
            Ok(started) => started,
            Err(error) => {
                SLOT.set(Slot::Empty);
                return Err(error.into());
            }
        };
        let engine = Rc::new(engine);
        feed::baseline(&engine).await;
        let result = json!({
            "identity": pubky_id(&engine.identity),
            "appId": b64(&engine.app_id),
            "enrolment": enrolment.as_str(),
        });
        SLOT.set(Slot::Ready(Rc::clone(&engine)));
        if auto_receive {
            feed::spawn(engine);
        }
        Ok(to_js(&result))
    }

    /// The started engine's Pubky ID, z-base-32, without a prefix.
    ///
    /// # Errors
    ///
    /// `not_started`, or `busy` while the engine starts or stops.
    #[wasm_bindgen]
    pub fn identity() -> Result<String, JsError> {
        Ok(pubky_id(&engine()?.identity))
    }

    /// The started engine's App ID, base64url.
    ///
    /// # Errors
    ///
    /// `not_started`, or `busy` while the engine starts or stops.
    #[wasm_bindgen(js_name = appId)]
    pub fn app_id() -> Result<String, JsError> {
        Ok(b64(&engine()?.app_id))
    }

    /// Allows `identity` (a Pubky ID) to start a conversation with this one: a reply grant at
    /// this identity's Dispatch, which also allows a pending connect request from it
    /// (Dispatch spec 7). A payer allows each payee it expects requests from. Emits
    /// `admissionChanged` when the admission changed.
    ///
    /// # Errors
    ///
    /// `invalid_input` for a malformed Pubky ID, `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = allowConnect)]
    pub async fn allow_connect(identity: String) -> Result<(), JsError> {
        let peer = views::identity_key(&identity)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        Box::pin(turn.paykit.allow_connect(&peer))
            .await
            .map_err(|error| EngineError::paykit("allow the identity to connect", &error))?;
        let admitted = Box::pin(turn.paykit.app().admits(&peer))
            .await
            .map_err(|error| EngineError::paykit("read the admission", &error))?;
        drop(turn);
        let known = feed::admission_of(&engine, &peer);
        feed::observe_admission(
            &engine,
            &peer,
            Admission {
                admitted,
                reply: true,
                ..known
            },
        );
        Ok(())
    }

    /// Whether `identity` may start a conversation with this one, as this app's copy of the
    /// allow and deny lists shows it.
    ///
    /// # Errors
    ///
    /// `invalid_input`, `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen]
    pub async fn admits(identity: String) -> Result<bool, JsError> {
        let peer = views::identity_key(&identity)?;
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let admitted = Box::pin(turn.paykit.app().admits(&peer))
            .await
            .map_err(|error| EngineError::paykit("read the admission", &error))?;
        Ok(admitted)
    }

    /// The identity's allow list and deny list, read from its Dispatch (`Contacts` in the
    /// declarations). Emits `admissionChanged` for each contact whose admission changed.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(unchecked_return_type = "Contacts")]
    pub async fn contacts() -> Result<JsValue, JsError> {
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let list = Box::pin(turn.paykit.app_mut().allow_list())
            .await
            .map_err(|error| EngineError::paykit("read the allow list", &error))?;
        drop(turn);
        let mut allowed = Vec::new();
        for peer in &list.peers {
            let admission = Admission {
                admitted: !list.deny.contains(&peer.pubky),
                kept: peer.kept,
                reply: peer.reply.is_some(),
            };
            feed::observe_admission(&engine, &peer.pubky, admission);
            allowed.push(admission.to_json(&peer.pubky));
        }
        for denied in &list.deny {
            if !list.peers.iter().any(|peer| peer.pubky == *denied) {
                feed::observe_admission(&engine, denied, Admission::default());
            }
        }
        let denied: Vec<String> = list.deny.iter().map(pubky_id).collect();
        Ok(to_js(&json!({ "allowed": allowed, "denied": denied })))
    }

    /// The one-time requests this identity holds as payer (`OneTimeRequest[]`).
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit` (also while the app's first listing is not done).
    #[wasm_bindgen(unchecked_return_type = "OneTimeRequest[]")]
    pub async fn requests() -> Result<JsValue, JsError> {
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let views = feed::requests(&turn).await?;
        drop(turn);
        let list: Vec<Value> = views.iter().map(views::request).collect();
        Ok(to_js(&Value::Array(list)))
    }

    /// The requests this identity sent and the payments it received as payee
    /// (`PayeeRequest[]`).
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = payeeActivity, unchecked_return_type = "PayeeRequest[]")]
    pub async fn payee_activity() -> Result<JsValue, JsError> {
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let activity = Box::pin(turn.paykit.payee_activity())
            .await
            .map_err(|error| EngineError::paykit("read the payee activity", &error))?;
        drop(turn);
        let list: Vec<Value> = activity.iter().map(views::payee_activity).collect();
        Ok(to_js(&Value::Array(list)))
    }

    /// Runs one receive round now (`ReceiveResult`), emitting its events, as the feed loop
    /// does: for a host that started with `autoReceive: false`, or wants a read at once.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit` when the read failed.
    #[wasm_bindgen(unchecked_return_type = "ReceiveResult")]
    pub async fn receive() -> Result<JsValue, JsError> {
        let engine = engine()?;
        match feed::round(&engine).await {
            Ok(summary) => Ok(to_js(&summary)),
            Err(RoundError::Stopped) => Err(EngineError::not_started().into()),
            Err(RoundError::Failed(error)) => Err(error.into()),
        }
    }

    /// Runs the signer's due work now (`Signer::run_due`): renewals of the app's certificate,
    /// the key bundles the app asked for, and the deletions owed. The feed loop runs it once
    /// a minute, and at once when a read waits for scope keys.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `signer`.
    #[wasm_bindgen(js_name = signerTick, unchecked_return_type = "SignerTick")]
    pub async fn signer_tick() -> Result<JsValue, JsError> {
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let more_due = feed::run_signer(&engine, &mut turn).await?;
        Ok(to_js(&json!({ "moreDue": more_due })))
    }

    /// Stops the engine: ends the feed loop, waits for the call in progress, then closes the
    /// signer and the app, which releases their stores.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy` while the engine starts or stops, or `signer` when the signer
    /// could not close cleanly; the engine is stopped either way.
    #[wasm_bindgen]
    pub async fn stop() -> Result<(), JsError> {
        let engine = take_engine()?;
        let stopped = close(&engine).await;
        SLOT.set(Slot::Empty);
        stopped.map_err(JsError::from)
    }

    /// For tests: removes this worker's engine and the identity's enrolment state, so the
    /// identity can be enrolled again, such as after the Dispatch's database was reset.
    ///
    /// It stops the engine if one runs, destroys its two stores, and deletes the signer's
    /// `identity.json` and backup from the identity's Homeserver. With no engine running, it
    /// destroys the stores of `options.storePrefix`, and deletes the signer's files through
    /// `options.homeserver` when given. The identity's account at the Dispatch is left as it
    /// is.
    ///
    /// # Errors
    ///
    /// `bad_options`, `busy`, `store` when a store is still held elsewhere, or `homeserver`
    /// when a delete failed; the stores are destroyed first, so calling it again with the
    /// Homeserver finishes it.
    #[wasm_bindgen(js_name = resetIdentity)]
    pub async fn reset_identity(
        #[wasm_bindgen(unchecked_param_type = "ResetOptions | undefined")] options: JsValue,
    ) -> Result<(), JsError> {
        let options = ResetOptions::read(&options)?;
        let running = SLOT.with_borrow_mut(|slot| match slot {
            Slot::Ready(engine) => {
                let engine = Rc::clone(engine);
                *slot = Slot::Changing;
                Ok(Some(engine))
            }
            Slot::Empty => {
                *slot = Slot::Changing;
                Ok(None)
            }
            Slot::Changing => Err(EngineError::busy()),
        })?;
        let reset = async {
            let (names, homeserver) = match &running {
                Some(engine) => {
                    // A signer that does not close cleanly is gone all the same.
                    let _ = close(engine).await;
                    (
                        engine.store_names.clone(),
                        Some(engine.homeserver.callbacks().clone()),
                    )
                }
                None => (
                    [
                        app_store_name(&options.store_prefix),
                        signer_store_name(&options.store_prefix),
                    ],
                    options.homeserver.clone(),
                ),
            };
            drop(running);
            for name in &names {
                destroy_store(name).await?;
            }
            if let Some(homeserver) = homeserver {
                delete_signer_files(&homeserver).await?;
            }
            Ok::<(), EngineError>(())
        };
        let result = reset.await;
        SLOT.set(Slot::Empty);
        result.map_err(JsError::from)
    }

    /// Takes the started engine out of the slot for a stop.
    fn take_engine() -> Result<Rc<Engine>, EngineError> {
        SLOT.with_borrow_mut(|slot| {
            let engine = match slot {
                Slot::Ready(engine) => Rc::clone(engine),
                Slot::Changing => return Err(EngineError::busy()),
                Slot::Empty => return Err(EngineError::not_started()),
            };
            *slot = Slot::Changing;
            Ok(engine)
        })
    }

    /// Ends the feed loop, waits for the turn in progress, and closes the engine.
    async fn close(engine: &Rc<Engine>) -> Result<(), EngineError> {
        engine.stop.raise();
        match Turn::take(engine).await {
            Ok(turn) => Engine::stop(turn.close()).await,
            Err(_) => Ok(()),
        }
    }

    /// Deletes the signer's files from the identity's Homeserver through the host.
    async fn delete_signer_files(homeserver: &HomeserverCallbacks) -> Result<(), EngineError> {
        for path in SIGNER_FILES {
            crate::host::call(&homeserver.delete, &Array::of1(&path.into()), 30_000)
                .await
                .map_err(|error| {
                    EngineError::homeserver(&format!("delete {path}"), &format_args!("{error:?}"))
                })?;
        }
        Ok(())
    }
}
