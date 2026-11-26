//! The JSON shapes the engine hands the host: requests, payee activity, receipts and
//! contacts. Each follows paykit-ffi's record of the same name (`OneTimeRequest`,
//! `PayeeRequest`, `Receipt`) with camelCase field names, and with keys and hashes as text:
//!
//! - Pubky IDs in z-base-32 without a prefix (`pubky_id`);
//! - request hashes, record keys and other 32-byte values in lowercase hex;
//! - App IDs in base64url, as `start` gives them;
//! - amounts as `{value, asset}`, the value a decimal string in whole units;
//! - statuses as the FFI enum case in lowerCamelCase (`open`, `beingPaidHere`, `paid`, ...).
//!
//! None of them carries a key or a secret. A request's payment reference and its endpoints'
//! payloads are the host's to show; they are the counterparty's own words.

use std::fmt::Debug;

use paykit_sdk::{
    PayOutcome, PayeeActivity, PaymentAmount, PaymentView, ReceivedReceipt, RequestDestination,
    RequestView, SentRequest,
};
use pubky_dispatch_core::enc::{b64, pubky_id, pubky_key};
use serde_json::{json, Value};

use crate::error::EngineError;

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The public key of a Pubky ID: 52 z-base-32 characters, alone or after a `pubky` or `pk:`
/// prefix.
pub fn identity_key(text: &str) -> Result<[u8; 32], EngineError> {
    let bare = if text.len() == 52 {
        text
    } else {
        text.strip_prefix("pubky")
            .or_else(|| text.strip_prefix("pk:"))
            .unwrap_or(text)
    };
    pubky_key(bare).map_err(|_| EngineError::invalid_input("the identity is not a Pubky ID"))
}

/// The FFI's case name of an SDK enum value: its `Debug` name up to the first space, `(` or
/// `{`, with its first letter lowercased.
pub fn case_name(value: &impl Debug) -> String {
    let debug = format!("{value:?}");
    let name = debug.split([' ', '(', '{']).next().unwrap_or_default();
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `{value, asset}`.
pub fn amount(amount: &PaymentAmount) -> Value {
    json!({ "value": amount.value.as_str(), "asset": amount.asset.as_str() })
}

/// A request's destination: `{type: "endpoints", endpoints: [{identifier, payload}]}`,
/// `{type: "app", appId}` or `{type: "any"}`. A payload is the endpoint's text as signed,
/// such as `{"value":"<address>"}`.
pub fn destination(destination: &RequestDestination) -> Value {
    match destination {
        RequestDestination::Endpoints(endpoints) => json!({
            "type": "endpoints",
            "endpoints": endpoints
                .iter()
                .map(|(identifier, payload)| json!({
                    "identifier": identifier.as_str(),
                    "payload": payload.as_str(),
                }))
                .collect::<Vec<_>>(),
        }),
        RequestDestination::RequiredApp(app) => json!({ "type": "app", "appId": b64(app) }),
        RequestDestination::Any => json!({ "type": "any" }),
    }
}

/// A receipt the payer holds (FFI `Receipt`).
pub fn receipt(receipt: &ReceivedReceipt) -> Value {
    json!({
        "record": hex(&receipt.record),
        "payee": pubky_id(&receipt.payee),
        "requestHash": receipt.request_hash.as_ref().map(|hash| hex(hash)),
        "attemptId": receipt.attempt_id.to_text(),
        "messageId": receipt.msg_id,
        "acceptedAtMs": receipt.accepted_at_ms,
        "paymentReference": receipt.payment_reference,
    })
}

/// The ID of a request: `{payee, requestHash}` (FFI `RequestId`).
pub fn request_id(payee: &[u8; 32], request_hash: &[u8; 32]) -> Value {
    json!({ "payee": pubky_id(payee), "requestHash": hex(request_hash) })
}

/// A one-time request the payer holds (FFI `OneTimeRequest`), with its destination.
pub fn request(view: &RequestView) -> Value {
    json!({
        "id": request_id(&view.request.payee, &view.request.request_hash),
        "record": hex(&view.record),
        "amount": amount(&view.amount),
        "paymentReference": view.payment_reference,
        "label": view.label,
        "proposalExpiresAtMs": view.proposal_expires_at_ms,
        "status": case_name(&view.status),
        "held": view.held.is_some(),
        "manualOnly": view.manual_only,
        "divergent": view.divergent,
        "receipt": view.receipt.as_ref().map(receipt),
        "destination": destination(&view.destination),
    })
}

/// A request the payee sent, or a payment it received without one (FFI `PayeeRequest`).
pub fn payee_activity(activity: &PayeeActivity) -> Value {
    json!({
        "record": hex(&activity.record),
        "payer": pubky_id(&activity.payer),
        "requestHash": activity.request_hash.as_ref().map(|hash| hex(hash)),
        "amount": activity.amount.as_ref().map(amount),
        "label": activity.label,
        "attemptId": activity.attempt_id.map(|id| id.to_text()),
        "status": case_name(&activity.status),
        "divergent": activity.divergent,
        "recurring": activity.recurring,
    })
}

/// A request just sent (FFI `SentRequest`): `{msgId, delivery, request}`, `delivery` the case
/// of where the message stands (`accepted`, `queued`, `kept`, `waitingForAdmission`, ...) and
/// `request` its `{payee, requestHash}`.
pub fn sent_request(sent: &SentRequest) -> Value {
    json!({
        "msgId": sent.msg_id,
        "delivery": case_name(&sent.delivery),
        "request": sent.request.as_ref().map(|r| request_id(&r.payee, &r.request_hash)),
    })
}

/// A payment's outcome (FFI `PaymentOutcome`): `{outcome, attemptId}`, with `proofMessageId`
/// for `paid` and `reason` for `failed`. `outcome` is the case in lowerCamelCase: `paid`,
/// `pending`, `failed`, `uncertain`, `paidUnreported`, `paidOutcomeUnwritten`,
/// `recordHeld`, `recordDivergent` or `resolved`.
pub fn pay_outcome(outcome: &PayOutcome) -> Value {
    let mut view = json!({
        "outcome": case_name(outcome),
        "attemptId": outcome.attempt_id().to_text(),
    });
    match outcome {
        PayOutcome::Paid { proof_msg_id, .. } => {
            view["proofMessageId"] = json!(proof_msg_id);
        }
        PayOutcome::Failed { reason, .. } => {
            view["reason"] = json!(reason);
        }
        _ => {}
    }
    view
}

/// Whether `outcome` still waits on the payment method or the network, so a resume later
/// may finish it with no one acting.
pub fn outcome_waits(outcome: &PayOutcome) -> bool {
    matches!(
        outcome,
        PayOutcome::Pending { .. } | PayOutcome::PaidOutcomeUnwritten { .. }
    )
}

/// A payment in the payer's journal (FFI `PaymentEntry`): `{attemptId, payee, requestHash,
/// amount, endpoint, stage, paidByMethod, recordHeld, paid, receipt}`. `stage` is the case in
/// lowerCamelCase (`paying`, `railPaid`, `finished`, ...).
pub fn payment(view: &PaymentView) -> Value {
    json!({
        "attemptId": view.attempt_id.to_text(),
        "payee": pubky_id(&view.payee),
        "requestHash": view.request_hash.as_ref().map(|hash| hex(hash)),
        "amount": amount(&view.amount),
        "endpoint": view.endpoint_id.as_str(),
        "stage": case_name(&view.stage),
        "paidByMethod": view.paid_by_method,
        "recordHeld": view.record_held,
        "paid": view.paid.as_ref().map(amount),
        "receipt": view.receipt.as_ref().map(receipt),
    })
}

/// What the engine knows of one contact's admission: `{identity, admitted, kept, reply}`.
/// `admitted` is whether the identity may start a conversation with this one; `kept` and
/// `reply` are the two reasons the Dispatch's allow list holds (Dispatch spec 7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Admission {
    /// It may start a conversation.
    pub admitted: bool,
    /// The identity keeps it explicitly.
    pub kept: bool,
    /// It holds a reply grant, as `allowConnect` gives.
    pub reply: bool,
}

impl Admission {
    /// `{identity, admitted, kept, reply}` for `identity`.
    pub fn to_json(self, identity: &[u8; 32]) -> Value {
        json!({
            "identity": pubky_id(identity),
            "admitted": self.admitted,
            "kept": self.kept,
            "reply": self.reply,
        })
    }
}
