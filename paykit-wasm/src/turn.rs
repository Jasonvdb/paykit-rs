//! Taking turns on the engine, and the engine's stop signal.
//!
//! The app and the signer are one value, [`Running`], that one call at a time takes out of the
//! engine for its length: a [`Turn`]. A call that finds it taken waits for it, in the worker's
//! event loop, instead of failing: the feed loop, the host's calls and `stop` queue behind
//! each other. No borrow of the engine is held across an await. The worker runs one thread,
//! so plain `Cell`s and `RefCell`s suffice.
//!
//! [`StopSignal`] tells the feed loop to end its wait at once when the engine stops.

use std::cell::{Cell, RefCell};
use std::future::{poll_fn, Future};
use std::ops::{Deref, DerefMut};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Poll, Waker};

use paykit_browser::timer::JsTimer;
use paykit_sdk::Timer as _;

use crate::engine::{Engine, Running};
use crate::error::EngineError;

/// Where the engine's [`Running`] is, and who waits for it.
#[derive(Default)]
pub struct TurnState {
    /// The app and the signer, `None` while a turn has them or once the engine has stopped.
    running: RefCell<Option<Running>>,
    /// The calls waiting for a turn.
    waiters: RefCell<Vec<Waker>>,
    /// The engine has stopped: no turn is given any more.
    closed: Cell<bool>,
}

impl TurnState {
    /// A state holding `running`.
    pub fn holding(running: Running) -> Self {
        Self {
            running: RefCell::new(Some(running)),
            ..Self::default()
        }
    }

    /// Wakes every waiter: the free turn goes to whichever polls first, and the others wait
    /// again. Waking all of them keeps a waiter whose call was dropped from holding up the rest.
    fn wake_all(&self) {
        for waker in self.waiters.borrow_mut().drain(..) {
            waker.wake();
        }
    }
}

/// One call's turn on the engine: it holds the app and the signer, and gives them back when
/// dropped.
pub struct Turn {
    engine: Rc<Engine>,
    running: Option<Running>,
}

impl Turn {
    /// Waits for a turn on `engine`.
    ///
    /// # Errors
    ///
    /// [`EngineError`] `not_started` once the engine has stopped.
    pub fn take(engine: &Rc<Engine>) -> impl Future<Output = Result<Turn, EngineError>> {
        let engine = Rc::clone(engine);
        poll_fn(move |cx| {
            let state = engine.turns();
            if state.closed.get() {
                return Poll::Ready(Err(EngineError::not_started()));
            }
            if let Some(running) = state.running.borrow_mut().take() {
                return Poll::Ready(Ok(Turn {
                    engine: Rc::clone(&engine),
                    running: Some(running),
                }));
            }
            state.waiters.borrow_mut().push(cx.waker().clone());
            Poll::Pending
        })
    }

    /// Ends the engine: the app and the signer leave it for good, every waiting call fails
    /// with `not_started`, and no turn is given again.
    pub fn close(mut self) -> Running {
        let state = self.engine.turns();
        state.closed.set(true);
        state.wake_all();
        self.running
            .take()
            .expect("a turn holds the app and the signer until it is closed or dropped")
    }
}

impl Deref for Turn {
    type Target = Running;

    fn deref(&self) -> &Running {
        self.running
            .as_ref()
            .expect("a turn holds the app and the signer until it is closed or dropped")
    }
}

impl DerefMut for Turn {
    fn deref_mut(&mut self) -> &mut Running {
        self.running
            .as_mut()
            .expect("a turn holds the app and the signer until it is closed or dropped")
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let state = self.engine.turns();
            *state.running.borrow_mut() = Some(running);
            state.wake_all();
        }
    }
}

/// A flag the feed loop waits on beside its feed read, raised once when the engine stops.
#[derive(Default)]
pub struct StopSignal {
    raised: Cell<bool>,
    wakers: RefCell<Vec<Waker>>,
}

impl StopSignal {
    /// Raises the signal: every wait on it ends.
    pub fn raise(&self) {
        self.raised.set(true);
        for waker in self.wakers.borrow_mut().drain(..) {
            waker.wake();
        }
    }

    /// Whether the signal is raised.
    pub fn is_raised(&self) -> bool {
        self.raised.get()
    }

    /// Runs `future` until it completes, or until the signal is raised: `None` then, and the
    /// future is dropped, which cancels it.
    pub async fn unless_raised<F: Future>(&self, future: F) -> Option<F::Output> {
        let mut future = pin!(future);
        poll_fn(|cx| {
            if self.raised.get() {
                return Poll::Ready(None);
            }
            if let Poll::Ready(output) = future.as_mut().poll(cx) {
                return Poll::Ready(Some(output));
            }
            self.wakers.borrow_mut().push(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    /// Sleeps `ms` milliseconds on the browser's timers: `false` when the signal was raised
    /// first.
    pub async fn sleep(&self, ms: u64) -> bool {
        self.unless_raised(JsTimer.sleep(ms)).await.is_some()
    }
}
