//! The host's `pay` calls in progress, and the answers that came after the engine stopped
//! waiting for them (Paykit spec 7.2: the attempt ID is the idempotency key of one handoff).
//! The same design as paykit-ffi's `flights.rs`, on the worker's event loop.
//!
//! A payment can take long: an on-chain payment that leaves a shared pool, for one, waits for
//! the pool's next round before it has a transaction ID. The SDK waits for the host's `pay` at
//! most its limit ([`crate::rail`], a minute by default). The host's call runs as a task of its
//! own, so it always runs to its end, whatever the SDK does meanwhile:
//!
//! - While it runs, the attempt is never handed to the host again and the host's `payment`
//!   lookup is not asked: both answer "pending", the method's own answer for a payment in
//!   flight.
//! - When it answers after the limit, a paid or failed outcome is kept under its attempt ID,
//!   and the feed loop is woken to resume the payments, which takes the kept outcome without
//!   asking the host: the proof goes out then.
//! - A `pay` that never answers stops counting as in flight after [`STALE_AFTER`] times its
//!   limit: the host's lookup then decides, as it does after a restart.
//!
//! Everything here is in memory, for the life of the engine. After a restart the host's
//! `payment` lookup answers instead, which is why the host keeps its own record of each
//! attempt.

use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Poll, Waker};

use js_sys::Date;
use paykit_browser::timer::JsTimer;
use paykit_sdk::Timer as _;
use serde_json::Value;

use crate::host::CallError;

/// A running `pay` counts as in flight for this many times its limit.
pub const STALE_AFTER: u64 = 4;

/// The host's answer to one `pay` call: its value as JSON, or why it gave none.
pub type Answer = Result<Value, CallError>;

/// One attempt's `pay` call in progress.
struct Running {
    /// Which call this is, so a stale call's late answer does not end a newer one.
    call: u64,
    /// When it stops counting as in flight, on the worker's wall clock in Unix ms.
    stale_at_ms: f64,
    /// The SDK still waits for the answer.
    waiting: bool,
    /// The answer, once it came while the SDK waited.
    answer: Option<Answer>,
    /// The SDK's waiting call.
    waker: Option<Waker>,
}

#[derive(Default)]
struct State {
    next_call: u64,
    running: HashMap<String, Running>,
    /// Paid or failed outcomes that came after the SDK stopped waiting, by attempt ID.
    late: HashMap<String, Value>,
    /// A `pay` answered after the SDK stopped waiting, since the feed loop last looked.
    arrived: bool,
    /// The feed loop, while it waits for an arrival.
    arrival_wakers: Vec<Waker>,
}

/// The engine's `pay` calls in progress and their late answers. Cloning shares them.
#[derive(Clone, Default)]
pub struct Flights(Arc<Mutex<State>>);

/// The result of waiting for one handoff.
pub enum Waited {
    /// The host answered within the limit.
    Answered(Answer),
    /// The limit passed: the payment may have been sent, and its answer will be kept.
    StillRunning,
}

impl Flights {
    fn state(&self) -> MutexGuard<'_, State> {
        // One thread: a poisoned lock can only come from a panic, which aborts the instance.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records a new `pay` call for `attempt`, which counts as in flight for
    /// [`STALE_AFTER`] times `limit_ms`, and returns its call number; `None` while one is in
    /// flight already.
    pub fn start(&self, attempt: &str, limit_ms: u64) -> Option<u64> {
        let now = Date::now();
        let mut state = self.state();
        if state
            .running
            .get(attempt)
            .is_some_and(|running| now < running.stale_at_ms)
        {
            return None;
        }
        state.next_call += 1;
        let call = state.next_call;
        let lasting = limit_ms.saturating_mul(STALE_AFTER) as f64;
        state.running.insert(
            attempt.to_owned(),
            Running {
                call,
                stale_at_ms: now + lasting,
                waiting: true,
                answer: None,
                waker: None,
            },
        );
        state.late.remove(attempt);
        Some(call)
    }

    /// The host's `answer` to call `call` for `attempt`: handed to the SDK while it waits;
    /// otherwise a paid or failed outcome is kept, and the feed loop is woken either way, so it
    /// resumes the payments.
    pub fn finish(&self, attempt: &str, call: u64, answer: Answer) {
        let mut state = self.state();
        let Some(running) = state.running.get_mut(attempt) else {
            return;
        };
        if running.call != call {
            return;
        }
        if running.waiting {
            running.answer = Some(answer);
            if let Some(waker) = running.waker.take() {
                waker.wake();
            }
            return;
        }
        state.running.remove(attempt);
        if let Ok(value) = answer {
            if is_definitive(&value) {
                state.late.insert(attempt.to_owned(), value);
            }
        }
        state.arrived = true;
        for waker in state.arrival_wakers.drain(..) {
            waker.wake();
        }
    }

    /// Waits for the answer to call `call` for `attempt` for at most `limit_ms`.
    pub async fn handoff(&self, attempt: &str, call: u64, limit_ms: u64) -> Waited {
        let mut guard = Waiting {
            flights: self,
            attempt,
            call,
            done: false,
        };
        let mut limit = pin!(JsTimer.sleep(limit_ms));
        let waited = poll_fn(|cx| {
            if let Some(answer) = self.take_answer(attempt, call, Some(cx.waker())) {
                return Poll::Ready(Waited::Answered(answer));
            }
            if limit.as_mut().poll(cx).is_ready() {
                return Poll::Ready(match self.stop_waiting(attempt, call) {
                    Some(answer) => Waited::Answered(answer),
                    None => Waited::StillRunning,
                });
            }
            Poll::Pending
        })
        .await;
        guard.done = true;
        waited
    }

    /// The answer of call `call`, which ends its record; `None` while there is none yet, and
    /// `waker` is woken when it comes.
    fn take_answer(&self, attempt: &str, call: u64, waker: Option<&Waker>) -> Option<Answer> {
        let mut state = self.state();
        let running = state.running.get_mut(attempt)?;
        if running.call != call {
            return None;
        }
        match running.answer.take() {
            Some(answer) => {
                state.running.remove(attempt);
                Some(answer)
            }
            None => {
                running.waker = waker.cloned();
                None
            }
        }
    }

    /// The SDK stops waiting for call `call`: its answer is kept when it comes. Returns the
    /// answer if it came just now.
    fn stop_waiting(&self, attempt: &str, call: u64) -> Option<Answer> {
        if let Some(answer) = self.take_answer(attempt, call, None) {
            return Some(answer);
        }
        let mut state = self.state();
        if let Some(running) = state.running.get_mut(attempt) {
            if running.call == call {
                running.waiting = false;
                running.waker = None;
            }
        }
        None
    }

    /// Whether `attempt`'s `pay` is running and still counts as in flight.
    pub fn in_flight(&self, attempt: &str) -> bool {
        let now = Date::now();
        self.state()
            .running
            .get(attempt)
            .is_some_and(|running| now < running.stale_at_ms)
    }

    /// The paid or failed outcome a late `pay` answered for `attempt`.
    pub fn late(&self, attempt: &str) -> Option<Value> {
        self.state().late.get(attempt).cloned()
    }

    /// Whether a `pay` answered late since the last call, which this resets.
    pub fn take_arrived(&self) -> bool {
        std::mem::take(&mut self.state().arrived)
    }

    /// Resolves once a `pay` answered late and [`Flights::take_arrived`] has not yet seen it.
    pub fn arrival(&self) -> impl Future<Output = ()> + '_ {
        poll_fn(|cx| {
            let mut state = self.state();
            if state.arrived {
                return Poll::Ready(());
            }
            state.arrival_wakers.push(cx.waker().clone());
            Poll::Pending
        })
    }
}

/// Whether `value` is an outcome the SDK can record for good: paid or failed.
fn is_definitive(value: &Value) -> bool {
    matches!(
        value.get("status").and_then(Value::as_str),
        Some("paid" | "failed")
    )
}

/// The SDK's wait for one handoff. Dropped before it ends, as when its call is cancelled, the
/// handoff counts as no longer waited for, so a later answer is kept.
struct Waiting<'a> {
    flights: &'a Flights,
    attempt: &'a str,
    call: u64,
    done: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if !self.done {
            if let Some(answer) = self.flights.stop_waiting(self.attempt, self.call) {
                // The answer came while the wait was being dropped: keep it as late.
                self.flights.finish_unwaited(self.attempt, answer);
            }
        }
    }
}

impl Flights {
    /// Keeps `answer` of a call whose wait ended without taking it, as a late answer.
    fn finish_unwaited(&self, attempt: &str, answer: Answer) {
        let mut state = self.state();
        if let Ok(value) = answer {
            if is_definitive(&value) {
                state.late.insert(attempt.to_owned(), value);
            }
        }
        state.arrived = true;
        for waker in state.arrival_wakers.drain(..) {
            waker.wake();
        }
    }
}
