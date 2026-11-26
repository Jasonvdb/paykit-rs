//! The engine's payment method: [`JsRail`], the SDK's `Rail` over the host's callbacks.
//!
//! The SDK calls some `Rail` methods synchronously, and the host's method may live outside the
//! worker, so those are answered here, from what the host declared at `start`:
//!
//! - the declaration: idempotent on the attempt ID, with a history that shows each handoff's
//!   outcome and survives a restore, completing within an hour, with paid times read within
//!   five minutes of the request's times;
//! - the identifiers it pays, each with its fraction digits;
//! - the endpoints it pays: those of a declared identifier whose payload is a JSON object
//!   with a non-empty string `value`, such as `{"value":"<address>"}`;
//! - what a payment transfers: the amount asked, the method's fee on top;
//! - the settlement ID of a proof: the 32 bytes of its `txid`, 64 lowercase hex digits.
//!
//! The rest calls the host, all values as JSON-shaped objects:
//!
//! - `pay(payment) -> Promise<RailOutcome>`, where `payment` is `{attemptId, endpointId,
//!   payload, amount: {value, asset}, paymentReference}`. The attempt ID is the idempotency
//!   key. A call that throws, rejects or takes longer than a minute leaves the attempt open:
//!   the SDK asks `payment` later.
//! - `payment(attemptId) -> Promise<RailOutcome | null>`: `null` only when `pay` was never
//!   called with that ID; `{status: "pending"}` for one in flight.
//! - `verify(check) -> Promise<number | null>`, where `check` is `{endpointId, payload,
//!   amount, attemptId, proof}`: the verified payment time in Unix milliseconds, or `null`.
//!
//! A `RailOutcome` is `{status: "paid", proof: {...}, paidAtMs?: number}`,
//! `{status: "failed"}` (definitely not paid) or `{status: "pending"}`.

use js_sys::{Array, Function, JSON};
use paykit_sdk::rail::ProofCheck;
use paykit_sdk::{
    AttemptId, EndpointSelection, PaymentAmount, PaymentEndpointCandidate,
    PaymentEndpointIdentifier, Rail, RailDeclaration, RailError, RailOutcome, RailPaid,
    RailPayment, Transfer,
};
use pubky_dispatch_client::transport::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use wasm_bindgen::JsValue;

use crate::host::{self, CallError};
use crate::options::RailCallbacks;

/// The longest a payment takes to complete, in milliseconds (Paykit spec 7.2).
const LONGEST_COMPLETION_MS: u64 = 3_600_000;
/// How far a verified paid time may lie outside a request's times, in milliseconds.
const PAYMENT_TIME_TOLERANCE_MS: u64 = 300_000;
/// How long `pay` may take before the attempt is left open, in milliseconds.
const PAY_LIMIT_MS: u64 = 60_000;
/// How long `payment` and `verify` may take, in milliseconds.
const LOOKUP_LIMIT_MS: u64 = 30_000;

/// The host's payment method.
pub struct JsRail {
    endpoints: Vec<(PaymentEndpointIdentifier, u32)>,
    callbacks: RailCallbacks,
}

impl JsRail {
    /// The host's method, paying `endpoints`, each with its fraction digits.
    pub fn new(endpoints: Vec<(PaymentEndpointIdentifier, u32)>, callbacks: RailCallbacks) -> Self {
        Self {
            endpoints,
            callbacks,
        }
    }

    fn pays(&self, identifier: &PaymentEndpointIdentifier) -> bool {
        self.endpoints.iter().any(|(known, _)| known == identifier)
    }

    /// Calls `callback` with `argument` as a JavaScript value, for at most `limit_ms`, and
    /// returns its value as JSON.
    fn call_host(
        callback: Option<&Function>,
        name: &'static str,
        argument: Value,
        limit_ms: u64,
    ) -> BoxFuture<'static, Result<Value, RailError>> {
        let Some(function) = callback.cloned() else {
            return Box::pin(std::future::ready(Err(RailError::Unavailable(format!(
                "the host gave no rail.{name} callback"
            )))));
        };
        let call = host::sendable(async move {
            let argument = JSON::parse(&argument.to_string())
                .map_err(|_| CallError::Failed("the argument does not convert".into()))?;
            let value = host::call(&function, &Array::of1(&argument), limit_ms).await?;
            to_json(&value).ok_or_else(|| CallError::Failed("its value is not JSON".into()))
        });
        Box::pin(async move {
            call.await.map_err(|error| match error {
                CallError::TimedOut => {
                    RailError::Unavailable(format!("rail.{name} did not answer in time"))
                }
                CallError::Failed(text) => {
                    RailError::Unavailable(format!("rail.{name} failed: {text}"))
                }
                CallError::Dropped => RailError::Unavailable(format!("rail.{name} was dropped")),
            })
        })
    }
}

/// A JavaScript value as JSON; `undefined` reads as `null`.
fn to_json(value: &JsValue) -> Option<Value> {
    if value.is_undefined() {
        return Some(Value::Null);
    }
    let text = JSON::stringify(value).ok()?.as_string()?;
    serde_json::from_str(&text).ok()
}

fn amount_json(amount: &PaymentAmount) -> Value {
    json!({ "value": amount.value.as_str(), "asset": amount.asset.as_str() })
}

/// An outcome as the host writes it.
#[derive(Deserialize)]
#[serde(
    tag = "status",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum HostOutcome {
    Paid {
        proof: Map<String, Value>,
        #[serde(default)]
        paid_at_ms: Option<u64>,
    },
    Failed,
    Pending,
}

fn outcome(value: Value, name: &str) -> Result<RailOutcome, RailError> {
    let outcome = serde_json::from_value::<HostOutcome>(value)
        .map_err(|_| RailError::Unavailable(format!("rail.{name} gave a malformed outcome")))?;
    Ok(match outcome {
        HostOutcome::Paid { proof, paid_at_ms } => {
            let paid = RailPaid::new(proof);
            RailOutcome::Paid(match paid_at_ms {
                Some(at) => paid.with_paid_at_ms(at),
                None => paid,
            })
        }
        HostOutcome::Failed => RailOutcome::Failed,
        HostOutcome::Pending => RailOutcome::Pending,
    })
}

/// Whether `payload` is a JSON object with a non-empty string `value`.
fn payload_has_value(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value.get("value")?.as_str().map(|text| !text.is_empty()))
        .unwrap_or(false)
}

impl Rail for JsRail {
    fn declaration(&self) -> RailDeclaration {
        RailDeclaration::new(LONGEST_COMPLETION_MS, PAYMENT_TIME_TOLERANCE_MS)
            .with_idempotent(true)
            .with_history(true)
            .with_history_survives_restore(true)
    }

    fn select_endpoints(
        &self,
        candidates: &[PaymentEndpointCandidate],
        _amount: Option<&PaymentAmount>,
    ) -> Result<EndpointSelection, RailError> {
        Ok(EndpointSelection::new(
            candidates
                .iter()
                .filter(|candidate| {
                    self.pays(&candidate.endpoint_id)
                        && payload_has_value(candidate.payload.as_str())
                })
                .map(|candidate| candidate.id)
                .collect(),
        ))
    }

    fn endpoint_identifiers(&self) -> Vec<PaymentEndpointIdentifier> {
        self.endpoints
            .iter()
            .map(|(identifier, _)| identifier.clone())
            .collect()
    }

    fn transfer(
        &self,
        _endpoint_id: &PaymentEndpointIdentifier,
        _payload: &paykit_sdk::PaymentEndpointPayload,
        amount: &PaymentAmount,
    ) -> Transfer {
        Transfer::new(Some(amount.clone()))
    }

    fn fraction_digits(&self, endpoint_id: &PaymentEndpointIdentifier) -> Option<u32> {
        self.endpoints
            .iter()
            .find(|(known, _)| known == endpoint_id)
            .map(|(_, digits)| *digits)
    }

    fn pay(&self, payment: RailPayment) -> BoxFuture<'_, Result<RailOutcome, RailError>> {
        if self.callbacks.pay.is_none() {
            // Nothing was submitted, and nothing will be.
            return Box::pin(std::future::ready(Err(RailError::Refused(
                "the host gave no rail.pay callback".into(),
            ))));
        }
        let argument = json!({
            "attemptId": payment.idempotency_key.to_text(),
            "endpointId": payment.endpoint_id.as_str(),
            "payload": payment.payload.as_str(),
            "amount": amount_json(&payment.amount),
            "paymentReference": payment.payment_reference,
        });
        let call = Self::call_host(self.callbacks.pay.as_ref(), "pay", argument, PAY_LIMIT_MS);
        Box::pin(async move { outcome(call.await?, "pay") })
    }

    fn payment(
        &self,
        idempotency_key: AttemptId,
    ) -> BoxFuture<'_, Result<Option<RailOutcome>, RailError>> {
        let argument = Value::String(idempotency_key.to_text());
        let call = Self::call_host(
            self.callbacks.payment.as_ref(),
            "payment",
            argument,
            LOOKUP_LIMIT_MS,
        );
        Box::pin(async move {
            match call.await? {
                Value::Null => Ok(None),
                value => outcome(value, "payment").map(Some),
            }
        })
    }

    fn verify(&self, check: ProofCheck) -> BoxFuture<'_, Result<Option<u64>, RailError>> {
        let argument = json!({
            "endpointId": check.endpoint_id.as_str(),
            "payload": check.payload.as_ref().map(|payload| payload.as_str()),
            "amount": amount_json(&check.amount),
            "attemptId": check.attempt_id.to_text(),
            "proof": check.proof,
        });
        let call = Self::call_host(
            self.callbacks.verify.as_ref(),
            "verify",
            argument,
            LOOKUP_LIMIT_MS,
        );
        Box::pin(async move {
            match call.await? {
                Value::Null => Ok(None),
                value => value.as_u64().map(Some).ok_or_else(|| {
                    RailError::Unavailable("rail.verify gave neither a time nor null".into())
                }),
            }
        })
    }

    fn settlement_id(
        &self,
        _endpoint_id: &PaymentEndpointIdentifier,
        proof: &Map<String, Value>,
    ) -> Option<Vec<u8>> {
        let txid = proof.get("txid")?.as_str()?.as_bytes();
        if txid.len() != 64 {
            return None;
        }
        txid.chunks_exact(2)
            .map(|pair| Some((lower_hex(pair[0])? << 4) | lower_hex(pair[1])?))
            .collect()
    }
}

fn lower_hex(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}
