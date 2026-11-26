//! The arguments of the request and payment calls, read from the JavaScript values the host
//! passes. Each is read as JSON, strictly: a misspelt field is refused rather than ignored.
//! A malformed argument is `invalid_input`, and its text never quotes the value, which may be
//! a payment reference or an address the host's user typed.

use std::collections::BTreeMap;

use js_sys::JSON;
use paykit_sdk::{
    Asset, Decimal, PayOptions, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference, RequestDestination, RequestDraft, RequestRef,
};
use pubky_dispatch_core::enc::unb64_array;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use wasm_bindgen::JsValue;

use crate::error::EngineError;
use crate::views::identity_key;

/// The latest Unix time in milliseconds a request may name (Dispatch's `MAX_TIME_MS`).
const MAX_TIME_MS: u64 = pubky_dispatch_core::time::MAX_TIME_MS;

/// `value` read as `T` through JSON; `what` names the argument in the error.
fn read<T: DeserializeOwned>(value: &JsValue, what: &str) -> Result<T, EngineError> {
    let refused = || EngineError::invalid_input(format!("{what} does not read"));
    if value.is_undefined() {
        return serde_json::from_str("null").map_err(|_| refused());
    }
    let text = JSON::stringify(value)
        .ok()
        .and_then(|text| text.as_string())
        .ok_or_else(refused)?;
    serde_json::from_str(&text).map_err(|error| {
        EngineError::invalid_input(format!(
            "{what} does not read (line {}, column {})",
            error.line(),
            error.column()
        ))
    })
}

/// A 32-byte value in 64 lowercase or uppercase hex digits.
fn hex32(text: &str, what: &str) -> Result<[u8; 32], EngineError> {
    let refused = || EngineError::invalid_input(format!("{what} is not 64 hex digits"));
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return Err(refused());
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        let high = char::from(pair[0]).to_digit(16).ok_or_else(refused)?;
        let low = char::from(pair[1]).to_digit(16).ok_or_else(refused)?;
        *byte = u8::try_from((high << 4) | low).map_err(|_| refused())?;
    }
    Ok(out)
}

/// A record key, as `requests()` and `payeeActivity()` give it: 64 hex digits.
pub fn record_key(text: &str) -> Result<[u8; 32], EngineError> {
    hex32(text, "the record")
}

/// A request's ID: `{payee, requestHash}`, as `requests()` gives it.
pub fn request_ref(value: &JsValue) -> Result<RequestRef, EngineError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Plain {
        payee: String,
        request_hash: String,
    }
    let plain: Plain = read(value, "the request ID")?;
    Ok(RequestRef {
        payee: identity_key(&plain.payee)?,
        request_hash: hex32(&plain.request_hash, "requestHash")?,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlainAmount {
    value: String,
    asset: String,
}

fn amount(plain: &PlainAmount) -> Result<PaymentAmount, EngineError> {
    let refused = || EngineError::invalid_input("the amount is not a decimal and an asset");
    Ok(PaymentAmount {
        asset: Asset::new(&plain.asset).map_err(|_| refused())?,
        value: Decimal::new(&plain.value).map_err(|_| refused())?,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlainEndpoint {
    identifier: String,
    payload: String,
}

/// Where a request may be paid, as the host writes it.
#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum PlainDestination {
    /// To these endpoints, such as an address the host made for this request.
    Endpoints { endpoints: Vec<PlainEndpoint> },
    /// To any endpoint of this app of the payee.
    App { app_id: String },
    /// To any of the payee's current Private Payment Lists or public endpoints.
    Any,
}

fn destination(plain: PlainDestination) -> Result<RequestDestination, EngineError> {
    Ok(match plain {
        PlainDestination::Endpoints { endpoints } => {
            if endpoints.is_empty() {
                return Err(EngineError::invalid_input("destination.endpoints is empty"));
            }
            let mut values = BTreeMap::new();
            for endpoint in endpoints {
                let identifier =
                    PaymentEndpointIdentifier::new(&endpoint.identifier).map_err(|_| {
                        EngineError::invalid_input(
                            "destination.endpoints holds an invalid Payment Endpoint Identifier",
                        )
                    })?;
                let payload = PaymentEndpointPayload::new(&endpoint.payload).map_err(|_| {
                    EngineError::invalid_input("destination.endpoints holds an invalid payload")
                })?;
                if values.insert(identifier, payload).is_some() {
                    return Err(EngineError::invalid_input(
                        "destination.endpoints names an identifier twice",
                    ));
                }
            }
            RequestDestination::Endpoints(values)
        }
        PlainDestination::App { app_id } => RequestDestination::RequiredApp(
            unb64_array::<16>(&app_id)
                .map_err(|_| EngineError::invalid_input("destination.appId is not an App ID"))?,
        ),
        PlainDestination::Any => RequestDestination::Any,
    })
}

fn time(value: Option<u64>, what: &str) -> Result<Option<u64>, EngineError> {
    match value {
        Some(ms) if ms > MAX_TIME_MS => Err(EngineError::invalid_input(format!(
            "{what} is past the latest time"
        ))),
        other => Ok(other),
    }
}

/// The argument of `sendRequest`: the payer and the request (`RequestInput` in the
/// declarations).
pub struct RequestInput {
    /// The payer.
    pub to: [u8; 32],
    /// The request.
    pub draft: RequestDraft,
}

/// Reads `sendRequest`'s argument: `{to, amount: {value, asset}, paymentReference,
/// destination, proposalExpiresAtMs?, paymentDeadlineMs?}`.
pub fn request_input(value: &JsValue) -> Result<RequestInput, EngineError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Plain {
        to: String,
        amount: PlainAmount,
        payment_reference: String,
        destination: PlainDestination,
        #[serde(default)]
        proposal_expires_at_ms: Option<u64>,
        #[serde(default)]
        payment_deadline_ms: Option<u64>,
    }
    let plain: Plain = read(value, "the request")?;
    let reference = PaymentReference::new(&plain.payment_reference).map_err(|_| {
        EngineError::invalid_input(
            "paymentReference is empty, too long or holds a control character",
        )
    })?;
    let mut draft = RequestDraft::new(
        amount(&plain.amount)?,
        reference,
        destination(plain.destination)?,
    );
    if let Some(at) = time(plain.proposal_expires_at_ms, "proposalExpiresAtMs")? {
        draft = draft.with_proposal_expires_at_ms(at);
    }
    if let Some(at) = time(plain.payment_deadline_ms, "paymentDeadlineMs")? {
        draft = draft.with_payment_deadline_ms(at);
    }
    Ok(RequestInput {
        to: identity_key(&plain.to)?,
        draft,
    })
}

/// Reads `payRequest`'s options: `undefined`, or `{endpoint?}`, the Payment Endpoint
/// Identifier to pay; the first usable one otherwise.
pub fn pay_options(value: &JsValue) -> Result<PayOptions, EngineError> {
    #[derive(Default, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Plain {
        #[serde(default)]
        endpoint: Option<String>,
    }
    let plain: Option<Plain> = read(value, "the payment options")?;
    let mut options = PayOptions::default();
    if let Some(endpoint) = plain.unwrap_or_default().endpoint {
        options.endpoint = Some(PaymentEndpointIdentifier::new(&endpoint).map_err(|_| {
            EngineError::invalid_input("endpoint is not a Payment Endpoint Identifier")
        })?);
    }
    Ok(options)
}
