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
//!   read the admission state. `probeAdmission(identity)` and `messageDelivery(msgId)` tell
//!   whether that identity admits this one.
//! - `requests()`: the one-time requests this identity holds as payer; `payeeActivity()`: the
//!   requests it sent and the payments it received as payee; `payments()`: the payer's
//!   payments, with their receipts; `requestReceipts(id)`: the receipts of one request.
//! - Payee: `sendRequest(request)` signs and sends a Payment Request, to endpoints the host
//!   supplies (such as a fresh address) or to any of the identity's Private Payment Lists;
//!   `verifyPayment(record)` checks a received proof with the host's `rail.verify`;
//!   `confirmPayment(record)` verifies it and sends the payer a receipt;
//!   `cancelSentRequest(record)`.
//! - Payer: `payRequest(id)` pays a request through the host's `rail.pay` and sends the proof;
//!   `rejectRequest(id)`; `resumePayments()` finishes payments left open, which the feed loop
//!   also does after a start and whenever a slow `pay` answers.
//! - Private Payment Lists: `setListEndpoints(endpoints)` sets the reusable payment details
//!   this identity shares; `publishListTo(identity)` shares them with a contact, and
//!   `withdrawListFrom(identity)` stops; `sharedList()` reads what is shared and with whom;
//!   `receivedLists()` reads the lists contacts shared with this identity. A request with
//!   destination `{type: "any"}` is paid to one of the payee's lists.
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
//! `identity_mismatch`, `signer`, `enrolment`, `paykit`, `invalid_input`, `homeserver`,
//! `refused`, `rail` and `conflict` (`error.rs`). The `Error` carries the code as `code`, and
//! for `refused` the refusal's name as `reason`. Calls made while another runs wait their
//! turn; `busy` means the engine is starting or stopping.
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
mod flights;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod homeserver;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod host;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod inputs;
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
    use wasm_bindgen::prelude::{wasm_bindgen, JsValue};

    use crate::engine::{destroy_store, Engine, SIGNER_FILES};
    use crate::error::EngineError;
    use crate::feed::{self, RoundError};
    use crate::inputs;
    use crate::options::{
        app_store_name, signer_store_name, HomeserverCallbacks, ResetOptions, StartOptions,
    };
    use crate::turn::Turn;
    use crate::views::{self, Admission};
    use paykit_sdk::{ReceiptsOf, ResendSelection};
    use pubky_dispatch_core::enc::random_bytes;

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
  /** The endpoint's payload as the payee signed it; `null` for a request with destination
   * `any` or `app`, whose payment the method checks against its own endpoints for
   * `endpointId`, such as the addresses in its Private Payment List. */
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
  /** How long a payment waits for `pay`, in ms: 1,000 to 3,600,000, default 60,000. Past it
   * `pay` goes on by itself, the attempt stays open (`payRequest` resolves `pending`), and
   * the engine finishes the payment when `pay` answers. */
  payLimitMs?: number;
  /** Must answer `{status: "paid"}` or `{status: "failed"}` once the payment is decided, and
   * may take as long as the method needs. The engine never calls it twice at once for one
   * `attemptId`, and a repeat with an `attemptId` already paid must return that payment. */
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

/** A Payment Request to send (`sendRequest`). */
export interface RequestInput {
  /** The payer's Pubky ID. The payer must admit this identity (its `allowConnect`). */
  to: string;
  amount: RailAmount;
  /** The payer sees it with the request, and it is copied into the receipt: 1 to 256
   * characters, no control characters. */
  paymentReference: string;
  /** Where to pay: endpoints the host supplies, such as `[{identifier: "btc-regtest-p2tr",
   * payload: "{\"value\":\"<address>\"}"}]`; or `any`: an endpoint of the Private Payment
   * List this identity shared with the payer, or its public endpoints. */
  destination: RequestDestination;
  /** The last time the payer may act on it, in Unix ms. */
  proposalExpiresAtMs?: number;
  /** The deadline of the payment itself, in Unix ms. */
  paymentDeadlineMs?: number;
}

/** A request just sent. `delivery`: `accepted` (in the payer's inbox), `queued`, `kept`,
 * `waitingForAdmission` (the payer does not admit this identity yet), ... */
export interface SentRequest {
  msgId: string;
  delivery: string;
  request: RequestId | null;
}

/** The options of `payRequest`. */
export interface PayRequestOptions {
  /** The Payment Endpoint Identifier to pay; the first usable one otherwise. */
  endpoint?: string;
  /** For a request whose destination is `any` or `app`: the App ID (base64url) of the payee
   * app whose Private Payment List to pay; any current one otherwise. */
  listApp?: string;
  /** For a request whose destination is `any` or `app`: `private` (the default) pays an
   * endpoint of the payee's Private Payment Lists; `public` pays the payee's public
   * endpoints, and is passed only when the user chose them. */
  mode?: "private" | "public";
}

/** A reusable Payment Endpoint of a Private Payment List. */
export interface ListEndpoint {
  /** The Payment Endpoint Identifier, such as `btc-regtest-p2tr`. */
  identifier: string;
  /** The payload, such as `{"value":"<address>"}`. */
  payload: string;
}

/** A Private Payment List's version: greater for each newer list of one app to one
 * contact. */
export interface ListVersion {
  generation: number;
  counter: number;
}

/** A Private Payment List a contact shared with this identity: its latest version from one
 * of the contact's apps. */
export interface ReceivedList {
  /** The contact's Pubky ID. */
  payee: string;
  /** The contact's app that published it (base64url): `payRequest`'s `listApp`. */
  appId: string;
  version: ListVersion;
  /** Empty once the contact withdrew its details. */
  endpoints: ListEndpoint[];
  /** When this identity's inbox accepted it, in Unix ms. */
  acceptedAtMs: number | null;
  /** From this instant (Unix ms) no payment uses it: the earlier of its expiry and 30 days
   * after it was accepted. */
  usableUntilMs: number;
  /** It has endpoints and has not expired, as of the read. A payment checks it again. */
  usable: boolean;
}

/** The Private Payment List this identity shares, and with whom. */
export interface SharedList {
  /** As `setListEndpoints` last set them; `null` before it was called. */
  endpoints: ListEndpoint[] | null;
  /** The contacts it is shared with (Pubky IDs): chosen with `publishListTo` and not
   * withdrawn since. Each is sent the current list, now or at a later round. */
  recipients: string[];
}

/** What became of a payment (paykit-ffi `PaymentOutcome`): `paid` (the proof is sent or
 * queued), `pending` (the method has not answered yet; the engine finishes it when it does),
 * `failed` (definitely not paid), `uncertain` (may be paid: the user decides), and the rarer
 * `paidUnreported`, `paidOutcomeUnwritten`, `recordHeld`, `recordDivergent`, `resolved`. */
export interface PaymentOutcome {
  outcome: string;
  attemptId: string;
  proofMessageId?: string | null;
  reason?: string;
}

/** A payment in the payer's journal (paykit-ffi `PaymentEntry`). */
export interface PaymentEntry {
  attemptId: string;
  payee: string;
  requestHash: string | null;
  amount: RailAmount;
  endpoint: string;
  /** `recorded`, `attempted`, `paying`, `railPaid`, `railFailed`, `acceptanceOwed`,
   * `finished` or `abandoned`. */
  stage: string;
  paidByMethod: boolean;
  recordHeld: boolean;
  paid: RailAmount | null;
  receipt: Receipt | null;
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
  | { type: "paymentsResumed"; outcomes: PaymentOutcome[] }
  | { type: "listReceived"; list: ReceivedList; previousVersion: ListVersion | null }
  | { type: "sharedListUpdated"; list: SharedList }
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
    ) -> Result<JsValue, JsValue> {
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
    pub fn identity() -> Result<String, JsValue> {
        Ok(pubky_id(&engine()?.identity))
    }

    /// The started engine's App ID, base64url.
    ///
    /// # Errors
    ///
    /// `not_started`, or `busy` while the engine starts or stops.
    #[wasm_bindgen(js_name = appId)]
    pub fn app_id() -> Result<String, JsValue> {
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
    pub async fn allow_connect(identity: String) -> Result<(), JsValue> {
        let peer = views::identity_key(&identity)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        Box::pin(turn.paykit.allow_connect(&peer))
            .await
            .map_err(|error| EngineError::sdk("allow the identity to connect", &error))?;
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
    pub async fn admits(identity: String) -> Result<bool, JsValue> {
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
    pub async fn contacts() -> Result<JsValue, JsValue> {
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

    /// Sends `identity` a `resend_request` that selects nothing (one fresh, unknown message
    /// ID), so that its delivery tells whether `identity` admits this one: Paykit has no read
    /// of the other side's allow list. Resolves to the request's message ID; read it with
    /// `messageDelivery`. It stays `waitingForAdmission` until `identity` admits this one,
    /// and turns `accepted` at the first delivery try after. The recipient has nothing to
    /// re-send, and a `resend_request` adds no reply grant.
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused` (`scopeMissing` for an app approved without
    /// `paykit_contacts`, `heldForUser`); `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = probeAdmission)]
    pub async fn probe_admission(identity: String) -> Result<String, JsValue> {
        let peer = views::identity_key(&identity)?;
        let unknown = random_uuid_v4()?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let selection = ResendSelection::default().with_msg_ids(vec![unknown]);
        let msg_id = Box::pin(turn.paykit.request_resend(peer, selection))
            .await
            .map_err(|error| EngineError::sdk("probe the admission", &error))?;
        Ok(msg_id)
    }

    /// Where the message this app sent as `msgId` stands, after one try of the outbox:
    /// `accepted`, `queued`, `kept`, `waitingForAdmission`, `certificateNotCurrent` or
    /// `failed`; `null` when the client keeps nothing of it (accepted or ended long ago, or
    /// not sent from these stores).
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = messageDelivery, unchecked_return_type = "string | null")]
    pub async fn message_delivery(msg_id: String) -> Result<JsValue, JsValue> {
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        feed::deliver(&engine, &mut turn).await;
        let delivery = Box::pin(turn.paykit.message_delivery(&msg_id))
            .await
            .map_err(|error| EngineError::sdk("read the message's delivery", &error))?;
        Ok(match delivery {
            Some(delivery) => JsValue::from_str(&views::case_name(&delivery)),
            None => JsValue::NULL,
        })
    }

    /// A fresh lowercase UUID v4, as `resend_request` takes message IDs (Paykit wire 4.12).
    fn random_uuid_v4() -> Result<String, EngineError> {
        let mut bytes = random_bytes::<16>()
            .map_err(|error| EngineError::paykit("draw a message ID", &error))?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex = views::hex(&bytes);
        Ok(format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        ))
    }

    /// The one-time requests this identity holds as payer (`OneTimeRequest[]`).
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit` (also while the app's first listing is not done).
    #[wasm_bindgen(unchecked_return_type = "OneTimeRequest[]")]
    pub async fn requests() -> Result<JsValue, JsValue> {
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
    pub async fn payee_activity() -> Result<JsValue, JsValue> {
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let activity = Box::pin(turn.paykit.payee_activity())
            .await
            .map_err(|error| EngineError::sdk("read the payee activity", &error))?;
        drop(turn);
        let list: Vec<Value> = activity.iter().map(views::payee_activity).collect();
        Ok(to_js(&Value::Array(list)))
    }

    /// Signs and sends a Payment Request (`RequestInput`) as payee, records it, and tries to
    /// deliver it at once. Resolves to `SentRequest`; the payee activity shows the request
    /// from now on (`activityUpdated`, status `requested`).
    ///
    /// # Errors
    ///
    /// `invalid_input` for a malformed request; `refused` (such as `reason` `scopeMissing`,
    /// `heldForUser` or `notNow`); `not_started`, `busy`, or `paykit`. After a `paykit` error
    /// read `payeeActivity()` before sending it again: the request may have been kept.
    #[wasm_bindgen(js_name = sendRequest, unchecked_return_type = "SentRequest")]
    pub async fn send_request(
        #[wasm_bindgen(unchecked_param_type = "RequestInput")] request: JsValue,
    ) -> Result<JsValue, JsValue> {
        let input = inputs::request_input(&request)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let sent = Box::pin(turn.paykit.send_request(input.to, input.draft))
            .await
            .map_err(|error| EngineError::sdk("send the request", &error))?;
        feed::deliver(&engine, &mut turn).await;
        Ok(to_js(&views::sent_request(&sent)))
    }

    /// Pays the request `id` (`RequestId`) as payer: the SDK writes the attempt, the host's
    /// `rail.pay` pays it, and the proof is sent to the payee. Resolves to `PaymentOutcome`:
    /// `paid` once the method paid within the pay limit, `pending` when it has not answered
    /// yet (the engine finishes the payment when it does: `paymentsResumed`).
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused` when the request is not payable, nothing paid, with
    /// `reason` such as `alreadyPaid`, `paymentInProgress`, `rejected`, `cancelled`,
    /// `noDestination`, `unsupportedEndpoint` or `attemptFailed`, or for a request with
    /// destination `any` `waitingForList` (no usable list from the payee yet: the engine has
    /// asked it for one; try again after `listReceived`); `conflict`; `not_started`,
    /// `busy`, or `paykit`. A failure after money may have moved is never an error: it is the
    /// `pending` or `uncertain` outcome.
    #[wasm_bindgen(js_name = payRequest, unchecked_return_type = "PaymentOutcome")]
    pub async fn pay_request(
        #[wasm_bindgen(unchecked_param_type = "RequestId")] id: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "PayRequestOptions")] options: JsValue,
    ) -> Result<JsValue, JsValue> {
        let request = inputs::request_ref(&id)?;
        let options = inputs::pay_options(&options)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let result = Box::pin(turn.paykit.pay_request(&request, options)).await;
        let outcome = match result.outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                feed::observe(&engine, &turn).await;
                return Err(EngineError::sdk("pay the request", &error).into());
            }
        };
        if views::outcome_waits(&outcome) {
            engine.payments_open.set(true);
        }
        feed::deliver(&engine, &mut turn).await;
        Ok(to_js(&views::pay_outcome(&outcome)))
    }

    /// Rejects the request `id` as payer, with an optional `reason` the payee sees, and
    /// resolves to the rejection's message ID.
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused` (`notFound`, `alreadyPaid`, `paymentInProgress`, ...);
    /// `conflict`; `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = rejectRequest)]
    pub async fn reject_request(
        #[wasm_bindgen(unchecked_param_type = "RequestId")] id: JsValue,
        reason: Option<String>,
    ) -> Result<String, JsValue> {
        let request = inputs::request_ref(&id)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let msg_id = Box::pin(turn.paykit.reject_request(&request, reason))
            .await
            .map_err(|error| EngineError::sdk("reject the request", &error))?;
        feed::deliver(&engine, &mut turn).await;
        Ok(msg_id)
    }

    /// Checks the proof the payee activity record `record` (hex) holds with the host's
    /// `rail.verify`, against the request's destination, amount and times. Resolves to the
    /// verified payment time in Unix ms, or `null` when the method does not confirm it, or
    /// the settlement is credited to another payment already.
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused` (`notFound` for a record without a proof,
    /// `paymentTimeOutside`, `maybeCredited`, ...); `rail` when the method could not check it
    /// now; `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = verifyPayment, unchecked_return_type = "number | null")]
    pub async fn verify_payment(record: String) -> Result<JsValue, JsValue> {
        let record = inputs::record_key(&record)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let verified = Box::pin(turn.paykit.verify_payment(&record))
            .await
            .map_err(|error| EngineError::sdk("verify the payment", &error))?;
        Ok(verified.map_or(JsValue::NULL, |at| JsValue::from_f64(at as f64)))
    }

    /// Verifies the payment in the payee activity record `record` (hex) as `verifyPayment`
    /// does and, when the method confirms it, sends the payer a receipt. Resolves to the
    /// receipt's message ID (the same one on a repeat), or `null` when the payment is not
    /// confirmed. The record's status becomes `receipted`.
    ///
    /// # Errors
    ///
    /// As `verifyPayment`, and `refused` `divergent`.
    #[wasm_bindgen(js_name = confirmPayment, unchecked_return_type = "string | null")]
    pub async fn confirm_payment(record: String) -> Result<JsValue, JsValue> {
        let record = inputs::record_key(&record)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let receipt = Box::pin(turn.paykit.confirm_payment(&record))
            .await
            .map_err(|error| EngineError::sdk("confirm the payment", &error))?;
        feed::deliver(&engine, &mut turn).await;
        Ok(receipt.map_or(JsValue::NULL, |id| JsValue::from_str(&id)))
    }

    /// Cancels the request this identity sent whose payee activity record is `record` (hex),
    /// with an optional `reason`, and resolves to the cancellation's message ID. A proof that
    /// crosses it still counts.
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused`; `conflict`; `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = cancelSentRequest)]
    pub async fn cancel_sent_request(
        record: String,
        reason: Option<String>,
    ) -> Result<String, JsValue> {
        let record = inputs::record_key(&record)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let msg_id = Box::pin(turn.paykit.cancel_sent_request(&record, reason))
            .await
            .map_err(|error| EngineError::sdk("cancel the request", &error))?;
        feed::deliver(&engine, &mut turn).await;
        Ok(msg_id)
    }

    /// The payer's payments, from its journal (`PaymentEntry[]`), each with the payee's
    /// receipt once it came.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(unchecked_return_type = "PaymentEntry[]")]
    pub async fn payments() -> Result<JsValue, JsValue> {
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let payments = Box::pin(turn.paykit.payments())
            .await
            .map_err(|error| EngineError::sdk("read the payments", &error))?;
        drop(turn);
        let list: Vec<Value> = payments.iter().map(views::payment).collect();
        Ok(to_js(&Value::Array(list)))
    }

    /// Finishes the payments left open now (`PaymentOutcome[]`): one a restart interrupted,
    /// or one whose method answered after the pay limit. The feed loop does it by itself once
    /// after `start` and whenever a late answer comes; a host that drives the feed itself
    /// (`autoReceive: false`) calls it.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, `rail`, or `paykit`.
    #[wasm_bindgen(js_name = resumePayments, unchecked_return_type = "PaymentOutcome[]")]
    pub async fn resume_payments() -> Result<JsValue, JsValue> {
        let engine = engine()?;
        let outcomes = feed::resume(&engine).await?;
        Ok(to_js(&Value::Array(outcomes)))
    }

    /// The payee's receipts of the request `id` the payer holds (`Receipt[]`).
    ///
    /// # Errors
    ///
    /// `invalid_input`, `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = requestReceipts, unchecked_return_type = "Receipt[]")]
    pub async fn request_receipts(
        #[wasm_bindgen(unchecked_param_type = "RequestId")] id: JsValue,
    ) -> Result<JsValue, JsValue> {
        let request = inputs::request_ref(&id)?;
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let receipts = Box::pin(turn.paykit.receipts(ReceiptsOf::Request(request)))
            .await
            .map_err(|error| EngineError::sdk("read the receipts", &error))?;
        drop(turn);
        let list: Vec<Value> = receipts.iter().map(views::receipt).collect();
        Ok(to_js(&Value::Array(list)))
    }

    /// Sets the reusable Private Payment List this identity shares (`ListEndpoint[]`), such
    /// as `[{identifier: "btc-regtest-p2tr", payload: "{\"value\":\"<address>\"}"}]`. Each
    /// contact it is shared with is sent the changed list, now or at a later round; an empty
    /// list withdraws the details from all of them. Emits `sharedListUpdated`.
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused` (`scopeMissing`, `notNow`, ...); `not_started`, `busy`, or
    /// `paykit`.
    #[wasm_bindgen(js_name = setListEndpoints)]
    pub async fn set_list_endpoints(
        #[wasm_bindgen(unchecked_param_type = "ListEndpoint[]")] endpoints: JsValue,
    ) -> Result<(), JsValue> {
        let endpoints = inputs::list_endpoints(&endpoints)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let set = Box::pin(turn.paykit.set_list_endpoints(endpoints))
            .await
            .map_err(|error| EngineError::sdk("set the list's endpoints", &error));
        list_changed(&engine, &mut turn, set).await
    }

    /// Shares the list `setListEndpoints` set with the contact `identity` (a Pubky ID): a
    /// signed Private Payment List goes to its inbox, now or at a later round. The contact
    /// must admit this identity (its `allowConnect`). Emits `sharedListUpdated`.
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused`: `usage` before the list is set or for this identity
    /// itself, `heldForUser` while the deny list holds the contact, `notNow` while an
    /// earlier publication to it is unresolved (a later round sends it), or
    /// `listDeliveryUnknown`; `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = publishListTo)]
    pub async fn publish_list_to(identity: String) -> Result<(), JsValue> {
        let recipient = views::identity_key(&identity)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let published = Box::pin(turn.paykit.publish_list_to(recipient))
            .await
            .map_err(|error| EngineError::sdk("share the list", &error));
        list_changed(&engine, &mut turn, published).await
    }

    /// Stops sharing the list with the contact `identity`: an empty list supersedes what it
    /// holds. Emits `sharedListUpdated`.
    ///
    /// # Errors
    ///
    /// `invalid_input`; `refused` (`notNow`, `listDeliveryUnknown`); `not_started`, `busy`,
    /// or `paykit`.
    #[wasm_bindgen(js_name = withdrawListFrom)]
    pub async fn withdraw_list_from(identity: String) -> Result<(), JsValue> {
        let recipient = views::identity_key(&identity)?;
        let engine = engine()?;
        let mut turn = Turn::take(&engine).await?;
        let withdrawn = Box::pin(turn.paykit.withdraw_list_from(recipient))
            .await
            .map_err(|error| EngineError::sdk("stop sharing the list", &error));
        list_changed(&engine, &mut turn, withdrawn).await
    }

    /// After a list call: sends what it queued, tells the host what changed, including the
    /// shared list (the call may have changed it before it failed), and returns its result.
    async fn list_changed(
        engine: &Rc<Engine>,
        turn: &mut Turn,
        result: Result<(), EngineError>,
    ) -> Result<(), JsValue> {
        feed::deliver(engine, turn).await;
        if let Err(error) = feed::observe_shared(engine, turn).await {
            if result.is_ok() {
                return Err(error.into());
            }
        }
        result.map_err(JsValue::from)
    }

    /// The Private Payment List this identity shares, and the contacts it is shared with
    /// (`SharedList`).
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = sharedList, unchecked_return_type = "SharedList")]
    pub async fn shared_list() -> Result<JsValue, JsValue> {
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let view = feed::observe_shared(&engine, &turn).await?;
        Ok(to_js(&view))
    }

    /// The Private Payment Lists contacts shared with this identity (`ReceivedList[]`): the
    /// latest from each of their apps, a withdrawn one with no endpoints. `listReceived`
    /// tells each new one.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit`.
    #[wasm_bindgen(js_name = receivedLists, unchecked_return_type = "ReceivedList[]")]
    pub async fn received_lists() -> Result<JsValue, JsValue> {
        let engine = engine()?;
        let turn = Turn::take(&engine).await?;
        let lists = Box::pin(turn.paykit.received_lists())
            .await
            .map_err(|error| EngineError::sdk("read the received lists", &error))?;
        drop(turn);
        let now = feed::now_ms();
        let list: Vec<Value> = lists
            .iter()
            .map(|list| views::received_list(list, now))
            .collect();
        Ok(to_js(&Value::Array(list)))
    }

    /// Runs one receive round now (`ReceiveResult`), emitting its events, as the feed loop
    /// does: for a host that started with `autoReceive: false`, or wants a read at once.
    ///
    /// # Errors
    ///
    /// `not_started`, `busy`, or `paykit` when the read failed.
    #[wasm_bindgen(unchecked_return_type = "ReceiveResult")]
    pub async fn receive() -> Result<JsValue, JsValue> {
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
    pub async fn signer_tick() -> Result<JsValue, JsValue> {
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
    pub async fn stop() -> Result<(), JsValue> {
        let engine = take_engine()?;
        let stopped = close(&engine).await;
        SLOT.set(Slot::Empty);
        stopped.map_err(JsValue::from)
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
    ) -> Result<(), JsValue> {
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
        result.map_err(JsValue::from)
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
