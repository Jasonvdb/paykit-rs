//! The options of `start`, read from the JavaScript object the host passes.
//!
//! The plain values are read as JSON: `JSON.stringify` of the object drops its functions, and
//! the rest is parsed strictly, so a misspelt option is refused rather than ignored. The
//! callbacks are read from the object itself.
//!
//! The identity secret is the one secret that crosses into the engine. It is decoded into a
//! buffer that is wiped on drop, and the JSON text that carried it is wiped once parsed. The
//! host's own copy, a JavaScript string, is outside the engine's reach.

use js_sys::{Function, Reflect, JSON};
use paykit_sdk::PaymentEndpointIdentifier;
use serde::Deserialize;
use wasm_bindgen::{JsCast, JsValue};
use zeroize::{Zeroize, Zeroizing};

use crate::error::EngineError;

/// The longest store prefix: the store names add `-signer`, and a store name is at most 64
/// characters (`OpfsStore::open`).
const MAX_STORE_PREFIX_LEN: usize = 64 - "-signer".len();

/// The store prefix when the host gives none.
const DEFAULT_STORE_PREFIX: &str = "paykit";

/// The client ID of the app's authentication request when the host gives none.
const DEFAULT_CLIENT_ID: &str = "paykit-wasm";

/// The most fraction digits an amount carries (Paykit wire 1.2).
const MAX_FRACTION_DIGITS: u32 = 18;

/// The options of `start`.
pub struct StartOptions {
    /// The identity's 32-byte secret key, wiped on drop.
    pub secret: Zeroizing<[u8; 32]>,
    /// The Dispatch origin, the one requests are signed for: `https://<host>[:<port>]`.
    pub dispatch_origin: String,
    /// The plain HTTP loopback listener that requests for the origin go to instead, for a
    /// Dispatch on this machine; `None` to reach the origin itself.
    pub dispatch_listener: Option<String>,
    /// The prefix of the two store names: `<prefix>-app` and `<prefix>-signer`.
    pub store_prefix: String,
    /// The client ID the app's authentication request names.
    pub client_id: String,
    /// The Payment Endpoint Identifiers the host's method pays, each with its fraction digits.
    pub endpoints: Vec<(PaymentEndpointIdentifier, u32)>,
    /// The host's Homeserver callbacks.
    pub homeserver: HomeserverCallbacks,
    /// The host's payment method callbacks.
    pub rail: RailCallbacks,
}

/// The host's Homeserver: `get(identityZ32, path)`, `put(path, bytes)` and `delete(path)`.
#[derive(Clone)]
pub struct HomeserverCallbacks {
    /// `get(identityZ32, path) -> Promise<Uint8Array | null>`.
    pub get: Function,
    /// `put(path, bytes) -> Promise<void>`, on the engine's own identity.
    pub put: Function,
    /// `delete(path) -> Promise<void>`, on the engine's own identity.
    pub delete: Function,
}

/// The host's payment method: `pay`, `payment` and `verify`, each optional.
#[derive(Clone, Default)]
pub struct RailCallbacks {
    /// `pay(payment) -> Promise<RailOutcome>`.
    pub pay: Option<Function>,
    /// `payment(attemptId) -> Promise<RailOutcome | null>`.
    pub payment: Option<Function>,
    /// `verify(check) -> Promise<number | null>`.
    pub verify: Option<Function>,
}

/// The plain values of the options, as `JSON.stringify` writes them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlainOptions {
    identity_secret_hex: String,
    dispatch_origin: String,
    #[serde(default)]
    dispatch_listener: Option<String>,
    #[serde(default)]
    store_prefix: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    /// The callbacks' object: `{}` once its functions are dropped. Required here, so a host
    /// that forgot it hears so; its callbacks are read from the object itself.
    #[serde(rename = "homeserver")]
    _homeserver: serde::de::IgnoredAny,
    rail: PlainRail,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlainRail {
    endpoints: Vec<PlainEndpoint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlainEndpoint {
    identifier: String,
    fraction_digits: u32,
}

impl Drop for PlainOptions {
    fn drop(&mut self) {
        self.identity_secret_hex.zeroize();
    }
}

impl StartOptions {
    /// Reads the options from the object `value`.
    ///
    /// # Errors
    ///
    /// [`EngineError`] `bad_options` for a missing or malformed option.
    pub fn read(value: &JsValue) -> Result<Self, EngineError> {
        if !value.is_object() {
            return Err(EngineError::bad_options("the options are not an object"));
        }
        let mut json: String = JSON::stringify(value)
            .ok()
            .and_then(|text| text.as_string())
            .ok_or_else(|| EngineError::bad_options("the options do not convert to JSON"))?;
        let parsed = serde_json::from_str::<PlainOptions>(&json);
        json.zeroize();
        // serde's error may quote the offending value; only where it is is kept.
        let plain = parsed.map_err(|error| {
            EngineError::bad_options(format!(
                "the options do not read (line {}, column {})",
                error.line(),
                error.column()
            ))
        })?;

        let secret = decode_secret(&plain.identity_secret_hex)?;
        let store_prefix = plain
            .store_prefix
            .clone()
            .unwrap_or_else(|| DEFAULT_STORE_PREFIX.to_owned());
        check_store_prefix(&store_prefix)?;
        let client_id = plain
            .client_id
            .clone()
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned());
        if client_id.is_empty() {
            return Err(EngineError::bad_options("clientId is empty"));
        }
        let endpoints = endpoints(&plain.rail.endpoints)?;

        let homeserver_object = property(value, "homeserver")?;
        let homeserver = HomeserverCallbacks {
            get: function(&homeserver_object, "homeserver", "get")?,
            put: function(&homeserver_object, "homeserver", "put")?,
            delete: function(&homeserver_object, "homeserver", "delete")?,
        };
        let rail_object = property(value, "rail")?;
        let rail = RailCallbacks {
            pay: optional_function(&rail_object, "rail", "pay")?,
            payment: optional_function(&rail_object, "rail", "payment")?,
            verify: optional_function(&rail_object, "rail", "verify")?,
        };

        Ok(Self {
            secret,
            dispatch_origin: plain.dispatch_origin.clone(),
            dispatch_listener: plain.dispatch_listener.clone(),
            store_prefix,
            client_id,
            endpoints,
            homeserver,
            rail,
        })
    }

    /// The name of the app's store.
    pub fn app_store_name(&self) -> String {
        format!("{}-app", self.store_prefix)
    }

    /// The name of the signer's store.
    pub fn signer_store_name(&self) -> String {
        format!("{}-signer", self.store_prefix)
    }
}

/// The 32 bytes of 64 hex digits.
fn decode_secret(hex: &str) -> Result<Zeroizing<[u8; 32]>, EngineError> {
    let refused = || EngineError::bad_options("identitySecretHex is not 64 hex digits");
    let digits = hex.as_bytes();
    if digits.len() != 64 {
        return Err(refused());
    }
    let mut secret = Zeroizing::new([0u8; 32]);
    for (byte, pair) in secret.iter_mut().zip(digits.chunks_exact(2)) {
        let high = hex_value(pair[0]).ok_or_else(refused)?;
        let low = hex_value(pair[1]).ok_or_else(refused)?;
        *byte = (high << 4) | low;
    }
    Ok(secret)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// A store prefix is 1 to [`MAX_STORE_PREFIX_LEN`] ASCII letters, digits, `-` or `_`, so both
/// store names are valid. It names no identity: the browser keeps store names in the clear.
fn check_store_prefix(prefix: &str) -> Result<(), EngineError> {
    let valid = !prefix.is_empty()
        && prefix.len() <= MAX_STORE_PREFIX_LEN
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(EngineError::bad_options(format!(
            "storePrefix is not 1 to {MAX_STORE_PREFIX_LEN} ASCII letters, digits, - or _"
        )))
    }
}

fn endpoints(
    plain: &[PlainEndpoint],
) -> Result<Vec<(PaymentEndpointIdentifier, u32)>, EngineError> {
    if plain.is_empty() {
        return Err(EngineError::bad_options("rail.endpoints is empty"));
    }
    let mut endpoints: Vec<(PaymentEndpointIdentifier, u32)> = Vec::with_capacity(plain.len());
    for endpoint in plain {
        let identifier = PaymentEndpointIdentifier::new(&endpoint.identifier).map_err(|_| {
            EngineError::bad_options("rail.endpoints holds an invalid Payment Endpoint Identifier")
        })?;
        if endpoints.iter().any(|(known, _)| *known == identifier) {
            return Err(EngineError::bad_options(
                "rail.endpoints names an identifier twice",
            ));
        }
        if endpoint.fraction_digits > MAX_FRACTION_DIGITS {
            return Err(EngineError::bad_options(format!(
                "rail.endpoints: fractionDigits is above {MAX_FRACTION_DIGITS}"
            )));
        }
        endpoints.push((identifier, endpoint.fraction_digits));
    }
    Ok(endpoints)
}

fn property(object: &JsValue, name: &str) -> Result<JsValue, EngineError> {
    let value = Reflect::get(object, &JsValue::from_str(name))
        .map_err(|_| EngineError::bad_options(format!("{name} cannot be read")))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(EngineError::bad_options(format!("{name} is not an object")))
    }
}

fn function(object: &JsValue, owner: &str, name: &str) -> Result<Function, EngineError> {
    optional_function(object, owner, name)?
        .ok_or_else(|| EngineError::bad_options(format!("{owner}.{name} is missing")))
}

fn optional_function(
    object: &JsValue,
    owner: &str,
    name: &str,
) -> Result<Option<Function>, EngineError> {
    let value = Reflect::get(object, &JsValue::from_str(name))
        .map_err(|_| EngineError::bad_options(format!("{owner}.{name} cannot be read")))?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    value
        .dyn_into::<Function>()
        .map(Some)
        .map_err(|_| EngineError::bad_options(format!("{owner}.{name} is not a function")))
}
