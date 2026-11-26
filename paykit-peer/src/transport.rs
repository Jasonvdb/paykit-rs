//! The Dispatch transport: requests signed for the Dispatch's https origin, sent over plain
//! HTTP to its listener on loopback (the `LoopbackListener` of
//! `paykit-browser/rig/dispatch_enroll.rs`).
//!
//! Test tool only. The origin is never sent, so the signatures still cover the https origin,
//! which the service checks them against. A request for any other origin is refused before
//! anything is sent, and only a loopback listener is accepted.

use pubky_dispatch_client::transport::{
    BoxFuture, FirstSend, HttpConfig, HttpTransport, Request, Response, Transport,
};
use pubky_dispatch_client::TransportError;

pub struct LoopbackListener {
    /// The client's transport for loopback test runs: no origin or address checks, plain HTTP.
    inner: HttpTransport,
    /// The origin the requests are signed for.
    origin: String,
    /// Where they go instead.
    listener: String,
}

impl LoopbackListener {
    /// `listener` must be `http://127.0.0.1:<port>` or `http://localhost:<port>`.
    pub fn new(origin: &str, listener: &str) -> anyhow::Result<Self> {
        check_loopback_url("--dispatch-url", listener)?;
        if !pubky_dispatch_core::records::is_origin(origin) {
            anyhow::bail!("--origin {origin:?} is not an https origin without a path");
        }
        let inner = HttpTransport::without_address_checks_for_loopback_tests(HttpConfig::default())
            .map_err(|error| anyhow::anyhow!("could not build the HTTP transport: {error}"))?;
        Ok(Self {
            inner,
            origin: origin.to_owned(),
            listener: listener.trim_end_matches('/').to_owned(),
        })
    }

    fn redirect(&self, request: Request) -> Result<Request, TransportError> {
        if request.origin != self.origin {
            return Err(TransportError::Refused(format!(
                "{} is not the local Dispatch origin",
                request.origin
            )));
        }
        Ok(Request {
            origin: self.listener.clone(),
            ..request
        })
    }
}

impl Transport for LoopbackListener {
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

/// Only `http://127.0.0.1:<port>` or `http://localhost:<port>`, with an optional trailing `/`:
/// the peer talks to nothing off this machine.
pub fn check_loopback_url(flag: &str, url: &str) -> anyhow::Result<()> {
    let refused = || {
        anyhow::anyhow!("{flag} {url:?} is not http://127.0.0.1:<port> or http://localhost:<port>")
    };
    let rest = url.strip_prefix("http://").ok_or_else(refused)?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (host, port) = rest.split_once(':').ok_or_else(refused)?;
    let port_ok = !port.is_empty()
        && !port.starts_with('0')
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok();
    if (host == "127.0.0.1" || host == "localhost") && port_ok {
        Ok(())
    } else {
        Err(refused())
    }
}

#[cfg(test)]
mod tests {
    use super::check_loopback_url;

    #[test]
    fn test_loopback_url_accepts_loopback_listeners() {
        for url in [
            "http://127.0.0.1:18580",
            "http://127.0.0.1:18580/",
            "http://localhost:18590",
        ] {
            assert!(check_loopback_url("--x", url).is_ok(), "{url}");
        }
    }

    #[test]
    fn test_loopback_url_refuses_anything_else() {
        for url in [
            "https://127.0.0.1:18580",
            "http://10.0.0.1:18580",
            "http://example.com:80",
            "http://127.0.0.1",
            "http://127.0.0.1:0",
            "http://127.0.0.1:018580",
            "http://127.0.0.1:70000",
            "http://127.0.0.1:18580/path",
            "http://127.0.0.1.example.com:18580",
        ] {
            assert!(check_loopback_url("--x", url).is_err(), "{url}");
        }
    }
}
