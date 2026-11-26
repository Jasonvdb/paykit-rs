//! Calls into the host's JavaScript callbacks.
//!
//! A callback may return a promise or a plain value; either is awaited, for at most a limit,
//! on the browser's timers. The SDK's and the signer's host traits need `Send` futures, and a
//! JavaScript promise is not `Send`: [`sendable`] runs the call as a local task of the worker
//! and hands its result back through a `Send` future (`paykit_browser::send::run_local`).

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::task::Poll;

use js_sys::{Array, Function, Promise, Reflect};
use paykit_browser::send::run_local;
use paykit_browser::timer::JsTimer;
use paykit_sdk::Timer as _;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

/// Why a host callback gave no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The callback threw, or its promise rejected. The text is the error's `message`, when it
    /// has one; it is the host's own text.
    Failed(String),
    /// The callback did not settle within the limit.
    TimedOut,
    /// The worker dropped the call before it settled.
    Dropped,
}

/// Calls `function` with `args` and awaits its value for at most `limit_ms`.
pub async fn call(function: &Function, args: &Array, limit_ms: u64) -> Result<JsValue, CallError> {
    let returned = function
        .apply(&JsValue::NULL, args)
        .map_err(|thrown| CallError::Failed(message_of(&thrown)))?;
    let mut settled = pin!(JsFuture::from(Promise::resolve(&returned)));
    let mut timeout = JsTimer.sleep(limit_ms);
    poll_fn(|cx| {
        if let Poll::Ready(result) = settled.as_mut().poll(cx) {
            return Poll::Ready(
                result.map_err(|rejected| CallError::Failed(message_of(&rejected))),
            );
        }
        if timeout.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(CallError::TimedOut));
        }
        Poll::Pending
    })
    .await
}

/// Runs `future`, which may hold JavaScript values, as a local task of this worker, and
/// returns a `Send` future of its output. The task starts now.
pub fn sendable<T, F>(future: F) -> impl Future<Output = Result<T, CallError>> + Send + 'static
where
    F: Future<Output = Result<T, CallError>> + 'static,
    T: Send + 'static,
{
    let task = run_local(future);
    async move { task.await.map_err(|_| CallError::Dropped)? }
}

/// The `message` of a thrown value, or a fixed text when it has none.
fn message_of(thrown: &JsValue) -> String {
    Reflect::get(thrown, &JsValue::from_str("message"))
        .ok()
        .and_then(|message| message.as_string())
        .or_else(|| thrown.as_string())
        .unwrap_or_else(|| "the host's callback failed".to_owned())
}
