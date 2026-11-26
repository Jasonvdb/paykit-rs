//! Local development only: a Dispatch transport for a Pubky Dispatch service running on this
//! machine, reached over plain HTTP on loopback.
//!
//! Every Dispatch request is signed for the service's https origin, and the service checks
//! the signatures against that origin; it never reads the Host header or the scheme. This
//! transport keeps the origin the requests are signed for and only changes where they are
//! sent: a request for the configured origin goes to the plain HTTP listener on loopback
//! instead, and a request for any other origin is refused before anything is sent.
//!
//! It is built on the client's transport for loopback test runs, which checks no origin or
//! address and allows plain HTTP. That is why this module accepts only a `localhost` origin
//! and a loopback listener: it can never reach another machine, and it can never send to an
//! origin read from someone else's `identity.json`.
//!
//! The listener must be on this machine. It works in an iOS simulator or on a desktop, which
//! share the machine's loopback, and not on a physical phone.

use crate::WalletError;
use pubky_dispatch_client::transport::{
    BoxFuture, FirstSend, HttpConfig, HttpTransport, Request, Response, Transport, TransportError,
};
use pubky_dispatch_core::records::is_origin;
use std::sync::Arc;

/// Sends the requests signed for one local https origin to a plain HTTP listener on
/// loopback.
struct LocalDispatch {
    /// The client's transport for loopback test runs: no origin or address checks, plain
    /// HTTP allowed. Only ever given the checked loopback listener.
    inner: HttpTransport,
    /// The origin the requests are signed for, as the service is configured with it.
    origin: String,
    /// Where those requests are sent instead.
    listener: String,
}

impl LocalDispatch {
    fn redirect(&self, request: Request) -> Result<Request, TransportError> {
        if request.origin != self.origin {
            return Err(TransportError::Refused(
                "the origin is not the local development Dispatch".into(),
            ));
        }
        Ok(Request {
            origin: self.listener.clone(),
            ..request
        })
    }
}

impl Transport for LocalDispatch {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, TransportError>> {
        match self.redirect(request) {
            Ok(request) => self.inner.send(request),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }

    fn send_before<'a>(
        &'a self,
        request: Request,
        first: &'a FirstSend,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        match self.redirect(request) {
            Ok(request) => self.inner.send_before(request, first),
            Err(error) => Box::pin(std::future::ready(Err(error))),
        }
    }
}

/// The transport for a local development Dispatch whose signed origin is `origin`
/// (`https://localhost[:<port>]`) and whose plain HTTP listener is `listener`
/// (`http://127.0.0.1:<port>` or `http://localhost:<port>`).
///
/// # Errors
///
/// [`WalletError::InvalidInput`] if either value has another form, or the HTTP client can't
/// be built.
pub(crate) fn transport(origin: &str, listener: &str) -> Result<Arc<dyn Transport>, WalletError> {
    check_origin(origin)?;
    check_listener(listener)?;
    let inner = HttpTransport::without_address_checks_for_loopback_tests(HttpConfig::default())
        .map_err(|_| WalletError::invalid("local dispatch transport"))?;
    Ok(Arc::new(LocalDispatch {
        inner,
        origin: origin.to_owned(),
        listener: listener.to_owned(),
    }))
}

/// A Dispatch origin (Dispatch wire 3.3) whose host is `localhost`.
fn check_origin(origin: &str) -> Result<(), WalletError> {
    let host = origin
        .strip_prefix("https://")
        .map(|rest| rest.split_once(':').map_or(rest, |(host, _)| host));
    if is_origin(origin) && host == Some("localhost") {
        Ok(())
    } else {
        Err(WalletError::invalid("dispatch origin"))
    }
}

/// Only a plain listener on loopback: `http://127.0.0.1:<port>` or
/// `http://localhost:<port>`, the port from 1 to 65535 without leading zeros, and nothing
/// after it.
fn check_listener(listener: &str) -> Result<(), WalletError> {
    let refused = || WalletError::invalid("dispatch listener");
    let rest = listener.strip_prefix("http://").ok_or_else(refused)?;
    let (host, port) = rest.split_once(':').ok_or_else(refused)?;
    let port_ok = !port.starts_with('0')
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|n| n != 0);
    if (host == "127.0.0.1" || host == "localhost") && port_ok {
        Ok(())
    } else {
        Err(refused())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_dispatch_client::transport::TimeoutClass;

    const ORIGIN: &str = "https://localhost:18580";
    const LISTENER: &str = "http://127.0.0.1:18580";

    fn invalid(result: Result<Arc<dyn Transport>, WalletError>, name: &str) -> bool {
        matches!(result, Err(WalletError::InvalidInput { field }) if field == name)
    }

    #[test]
    fn test_local_dispatch_accepts_a_localhost_origin_and_a_loopback_listener() {
        for (origin, listener) in [
            (ORIGIN, LISTENER),
            ("https://localhost", "http://localhost:8080"),
            ("https://localhost:443", "http://127.0.0.1:65535"),
        ] {
            assert!(transport(origin, listener).is_ok(), "{origin} {listener}");
        }
    }

    #[test]
    fn test_local_dispatch_refuses_an_origin_that_is_not_localhost() {
        for origin in [
            "http://localhost:18580",
            "https://localhost:18580/",
            "https://localhost:018580",
            "https://127.0.0.1:18580",
            "https://localhost.example:18580",
            "https://dispatch.example",
            "https://user@localhost:18580",
            "",
        ] {
            assert!(
                invalid(transport(origin, LISTENER), "dispatch origin"),
                "{origin}"
            );
        }
    }

    #[test]
    fn test_local_dispatch_refuses_a_listener_off_loopback() {
        for listener in [
            "https://127.0.0.1:18580",
            "http://127.0.0.1",
            "http://127.0.0.1:",
            "http://127.0.0.1:0",
            "http://127.0.0.1:018580",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:18580/",
            "http://127.0.0.1:18580/path",
            "http://10.0.0.1:18580",
            "http://192.168.1.2:18580",
            "http://[::1]:18580",
            "http://dispatch.example:18580",
            "http://localhost.example:18580",
            "http://user@127.0.0.1:18580",
        ] {
            assert!(
                invalid(transport(ORIGIN, listener), "dispatch listener"),
                "{listener}"
            );
        }
    }

    #[tokio::test]
    async fn test_local_dispatch_refuses_another_origin_before_sending() {
        let local = transport(ORIGIN, LISTENER).unwrap();
        let request = Request {
            origin: "https://dispatch.example".into(),
            method: "GET".into(),
            path_and_query: "/v1/time".into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: TimeoutClass::Ordinary,
        };
        assert!(matches!(
            local.send(request).await,
            Err(TransportError::Refused(_))
        ));
    }
}
