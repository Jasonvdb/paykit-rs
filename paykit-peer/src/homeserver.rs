//! The fake Homeserver of the local demo, over plain HTTP on loopback:
//! `GET|PUT|DELETE {base}/{identityZ32}{path}`, `200` with the bytes or `404` on a read, `204` on
//! a write. No authentication: a test stand-in for a Pubky Homeserver, never a real one.

use std::sync::Arc;
use std::time::Duration;

use pubky_dispatch_client::app::{HomeserverReader, HomeserverWriter};
use pubky_dispatch_client::transport::BoxFuture;
use pubky_dispatch_client::TransportError;
use pubky_dispatch_core::enc::pubky_id;
use reqwest::StatusCode;

/// The largest file the peer reads: the client's own bound on a Homeserver file is lower.
const MAX_FILE_BYTES: u64 = 16 << 20;

#[derive(Clone)]
pub struct FakeHomeserver {
    http: reqwest::Client,
    base: String,
}

impl FakeHomeserver {
    /// `base` must be `http://127.0.0.1:<port>` or `http://localhost:<port>`.
    pub fn new(base: &str) -> anyhow::Result<Self> {
        crate::transport::check_loopback_url("--homeserver", base)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    fn url(&self, identity: &[u8; 32], path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("{}/{}/{path}", self.base, pubky_id(identity))
    }

    pub async fn get(
        &self,
        identity: [u8; 32],
        path: &str,
    ) -> Result<Option<Vec<u8>>, TransportError> {
        let answer = self
            .http
            .get(self.url(&identity, path))
            .send()
            .await
            .map_err(failed)?;
        match answer.status() {
            StatusCode::OK => {
                if answer
                    .content_length()
                    .is_some_and(|len| len > MAX_FILE_BYTES)
                {
                    return Err(TransportError::TooLarge(MAX_FILE_BYTES));
                }
                Ok(Some(answer.bytes().await.map_err(failed)?.to_vec()))
            }
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(None),
            other => Err(TransportError::Other(format!(
                "the Homeserver answered {other}"
            ))),
        }
    }

    pub async fn put(
        &self,
        identity: [u8; 32],
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<(), TransportError> {
        let answer = self
            .http
            .put(self.url(&identity, path))
            .body(bytes)
            .send()
            .await
            .map_err(failed)?;
        written(answer.status())
    }

    pub async fn delete(&self, identity: [u8; 32], path: &str) -> Result<(), TransportError> {
        let answer = self
            .http
            .delete(self.url(&identity, path))
            .send()
            .await
            .map_err(failed)?;
        written(answer.status())
    }
}

fn written(status: StatusCode) -> Result<(), TransportError> {
    if status.is_success() {
        Ok(())
    } else {
        Err(TransportError::Other(format!(
            "the Homeserver answered {status}"
        )))
    }
}

fn failed(error: reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else if error.is_connect() {
        TransportError::Connect("the fake Homeserver can't be reached".into())
    } else {
        TransportError::Other("the fake Homeserver request failed".into())
    }
}

impl HomeserverReader for FakeHomeserver {
    fn get(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move { FakeHomeserver::get(self, identity, &path).await })
    }
}

impl HomeserverWriter for FakeHomeserver {
    fn put(
        &self,
        identity: [u8; 32],
        path: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move { FakeHomeserver::put(self, identity, &path, bytes).await })
    }

    fn delete(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<(), TransportError>> {
        Box::pin(async move { FakeHomeserver::delete(self, identity, &path).await })
    }
}

/// One identity's own files, as its signer reads and writes them.
pub struct OwnHomeserver {
    pub identity: [u8; 32],
    pub files: Arc<FakeHomeserver>,
}

impl pubky_dispatch_signer::Homeserver for OwnHomeserver {
    fn get(&self, path: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        let path = path.to_owned();
        Box::pin(async move { self.files.get(self.identity, &path).await })
    }

    fn put(&self, path: &str, bytes: Vec<u8>) -> BoxFuture<'_, Result<(), TransportError>> {
        let path = path.to_owned();
        Box::pin(async move { self.files.put(self.identity, &path, bytes).await })
    }

    fn delete(&self, path: &str) -> BoxFuture<'_, Result<(), TransportError>> {
        let path = path.to_owned();
        Box::pin(async move { self.files.delete(self.identity, &path).await })
    }
}
