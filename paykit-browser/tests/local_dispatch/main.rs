//! `FetchTransport::for_local_development` against a live Dispatch on this machine, in a
//! browser. Local development only.
//!
//! `rig/test.sh --local-dispatch` runs this target, and only this mode runs it. It does not
//! start a service: a Pubky Dispatch whose origin is `https://localhost:<port>` must already
//! answer, in plain HTTP with CORS for loopback pages, on a listener on loopback. The run
//! passes the origin in `PAYKIT_BROWSER_LOCAL_DISPATCH_ORIGIN` and the listener in
//! `PAYKIT_BROWSER_LOCAL_DISPATCH_LISTENER`, at compile time, and serves the test page on
//! `http://localhost:<port>`.
//!
//! Each test signs its requests for the https origin with a fresh identity key, and the
//! transport sends them to the plain listener. The service checks the signatures against its
//! own origin, so an answer that verifies shows the rewrite kept the signed bytes, and that
//! the listener answered the CORS preflight and exposed the signed response headers. The
//! tests create no account: an account observation writes nothing.
//!
//! The transport exists only on `wasm32-unknown-unknown`, so natively the target is empty.
//! wasm-bindgen-test runs it in a browser page or in a dedicated worker; the runner's
//! environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::fetch::{FetchConfig, FetchTransport};
use pubky_dispatch_client::transport::{Request, TimeoutClass, Transport, TransportError};
use pubky_dispatch_client::{Credentials, DispatchClient};
use pubky_dispatch_core::http::header;
use pubky_dispatch_core::keys::IdentityKey;
use pubky_dispatch_core::records::AccountAssociation;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

/// The Dispatch's origin, the label the requests are signed for, as `rig/test.sh
/// --local-dispatch` set it at compile time.
///
/// # Panics
///
/// When the target was built without it.
fn origin() -> &'static str {
    option_env!("PAYKIT_BROWSER_LOCAL_DISPATCH_ORIGIN").unwrap_or_else(|| {
        panic!(
            "PAYKIT_BROWSER_LOCAL_DISPATCH_ORIGIN is not set: run this target with \
             `paykit-browser/rig/test.sh --local-dispatch`"
        )
    })
}

/// The plain HTTP listener the requests go to, as `rig/test.sh --local-dispatch` set it at
/// compile time.
///
/// # Panics
///
/// When the target was built without it.
fn listener() -> &'static str {
    option_env!("PAYKIT_BROWSER_LOCAL_DISPATCH_LISTENER").unwrap_or_else(|| {
        panic!(
            "PAYKIT_BROWSER_LOCAL_DISPATCH_LISTENER is not set: run this target with \
             `paykit-browser/rig/test.sh --local-dispatch`"
        )
    })
}

fn transport() -> FetchTransport {
    FetchTransport::for_local_development(FetchConfig::default(), origin(), listener())
        .unwrap_or_else(|error| panic!("the local development transport: {error:?}"))
}

/// The origin of the page or worker the test runs in, from the global `location`.
fn page_origin() -> String {
    let location = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("location"))
        .expect("the global scope has a location");
    js_sys::Reflect::get(&location, &JsValue::from_str("origin"))
        .ok()
        .and_then(|origin| origin.as_string())
        .expect("the location has an origin")
}

/// The test page is served on `http://localhost:<port>`, the shape of a local web wallet's
/// origin, and the listener is another origin, so every call below is cross-origin.
#[wasm_bindgen_test]
fn test_local_dispatch_page_is_on_localhost_over_http() {
    let page = page_origin();
    console_log!(
        "[local-dispatch] page origin {page}, listener {}",
        listener()
    );
    assert!(
        page.starts_with("http://localhost:"),
        "the page's origin is {page}"
    );
    assert_ne!(page, listener());
}

/// A signed request for the https origin goes to the plain listener and gets an answer the
/// client verifies: a fresh identity's account observation (Dispatch account binding,
/// Section 2). The request carries the Dispatch's request headers, so the browser sends a CORS
/// preflight first; the answer's signature covers the identity, the origin the request was
/// signed for and the request's own signature, so it verifies only if the service saw the
/// signed bytes for the https origin. The observation names that origin, no account and no
/// creation.
#[wasm_bindgen_test]
async fn test_local_dispatch_signed_request_gets_a_verified_answer() {
    let identity = IdentityKey::generate().expect("a fresh identity key");
    let client = DispatchClient::new(
        origin(),
        identity.public(),
        Credentials::Signer {
            identity: identity.public(),
            key: Arc::new(identity.clone()),
        },
        None,
        Arc::new(transport()),
        Arc::new(JsClock),
    )
    .expect("the client is made");

    let observation = client
        .observe_account()
        .await
        .unwrap_or_else(|error| panic!("the account observation: {error:?}"));
    let body = observation.body();
    assert_eq!(body.identity, identity.public());
    assert_eq!(body.origin, origin());
    assert_eq!(body.association, AccountAssociation::Absent);
    assert_eq!(body.created, None);
    console_log!(
        "[local-dispatch] signed account observation of {} verified, observed at {}",
        identity.id(),
        body.observed_at
    );
}

/// A plain public read through the transport, with no Dispatch header, reaches the service:
/// an unknown identity's `identity.json` is a `404` with the service's signature header,
/// which the listener exposes to the page.
#[wasm_bindgen_test]
async fn test_local_dispatch_public_read_reaches_the_service() {
    let identity = IdentityKey::generate().expect("a fresh identity key");
    let request = Request {
        method: "GET".into(),
        origin: origin().into(),
        path_and_query: format!("/v1/{}/identity", identity.id()),
        headers: Vec::new(),
        body: Vec::new(),
        timeout: TimeoutClass::Ordinary,
    };
    let response = transport()
        .send(request)
        .await
        .unwrap_or_else(|error| panic!("the identity read: {error:?}"));
    assert_eq!(response.status, 404, "an unknown identity's identity.json");
    assert!(
        response.header(header::SERVICE_SIGNATURE).is_some(),
        "the answer's signature header is not exposed"
    );
}

/// A request for any other origin is refused before anything is sent, even one on the same
/// host and port.
#[wasm_bindgen_test]
async fn test_local_dispatch_refuses_other_origins() {
    let others = ["https://localhost", listener(), "https://dispatch.invalid"];
    for other in others.into_iter().filter(|other| *other != origin()) {
        let request = Request {
            method: "GET".into(),
            origin: other.into(),
            path_and_query: "/v1/x/identity".into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: TimeoutClass::Ordinary,
        };
        let error = transport().send(request).await.unwrap_err();
        assert!(
            matches!(error, TransportError::Refused(_)),
            "{other}: {error:?}"
        );
    }
}
