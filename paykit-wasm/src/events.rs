//! Events for the host: plain objects passed to the `onEvent` callback of `start`.
//!
//! Each event is `{type, atMs, ...fields}`, `atMs` the worker's wall clock in Unix
//! milliseconds. The types (`EngineEvent` in the declarations):
//!
//! | `type` | Fields | When |
//! | --- | --- | --- |
//! | `received` | `generation`, `sequence`, `applied` (message IDs), `more`, `stopped`, `scopeKeysAwaited`, `recordsUnlisted` | a receive round finished |
//! | `messageReceived` | `kind`, `msgId`, `sender`, `seq` | the Dispatch delivered a message (`AppEvent::MessageReceived`) |
//! | `admissionChanged` | `identity`, `admitted`, `kept`, `reply` | what this identity allows of a contact changed |
//! | `requestReceived` | `request` (a `OneTimeRequest`) | the payer holds a new request |
//! | `requestUpdated` | `request`, `previousStatus` | a request's status changed |
//! | `receiptReceived` | `request`, `receipt` | the payer holds the payee's receipt for a request |
//! | `proofReceived` | `activity` (a `PayeeRequest`) | the payee holds a proof to verify |
//! | `activityUpdated` | `activity`, `previousStatus` (`null` when new) | a request sent, or a payment received, changed |
//! | `paymentsResumed` | `outcomes` (`PaymentOutcome[]`) | payments left open were resumed: after a start, or when a `pay` answered after its limit |
//! | `error` | `during`, `code`, `message` | a step of the feed loop failed; the loop goes on |
//! | `feedStopped` | `reason` | the feed loop ended |
//!
//! The engine compares each read, and the state after each call that changes it, with what it
//! saw before, from the state it had at `start`:
//! a host reads the current state with `requests()` and `payeeActivity()` at start, and the
//! events tell it what changed after. No event carries a key or a secret.

use js_sys::{Date, Function, JSON};
use serde_json::{Map, Value};
use wasm_bindgen::JsValue;

/// The host's `onEvent` callback, if it gave one.
#[derive(Clone, Default)]
pub struct EventSink(Option<Function>);

impl EventSink {
    /// A sink calling `callback`.
    pub fn new(callback: Option<Function>) -> Self {
        Self(callback)
    }

    /// Calls the host with `{type: kind, atMs, ...fields}`. A callback that throws is ignored:
    /// the engine's state does not depend on the host taking an event.
    pub fn emit(&self, kind: &str, fields: Value) {
        let Some(callback) = &self.0 else {
            return;
        };
        let mut event = Map::new();
        event.insert("type".into(), Value::String(kind.to_owned()));
        event.insert("atMs".into(), Value::from(Date::now() as u64));
        if let Value::Object(fields) = fields {
            event.extend(fields);
        }
        let Ok(object) = JSON::parse(&Value::Object(event).to_string()) else {
            return;
        };
        let _ = callback.call1(&JsValue::NULL, &object);
    }
}
