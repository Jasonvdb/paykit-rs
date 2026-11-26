//! Receiving: one bounded receive round, the feed loop that runs it whenever the Dispatch has
//! something, and what the engine tells the host of each round (`events.rs`).
//!
//! **A round** takes a turn on the engine, then reads the feed with `Paykit::receive` until the
//! read says there is no more for now, at most [`READS_PER_ROUND`] times. Each read's effects
//! are committed with its cursor move by the SDK. Then the round acknowledges, for Paykit, the
//! duty events its reads showed, as a host of paykit-ffi does after `receive`; runs the
//! signer's due work at once when the app waits for scope keys; and compares the requests and
//! the payee activity with what it saw before, emitting an event for each change.
//!
//! **The feed loop** runs from `start` until `stop`, as a local task of the worker. Between
//! rounds it does not hold the engine: it waits on the feed with a `FeedWait` handle
//! (Dispatch wire 7.4.4, a long poll of up to 25 s), so the host's calls run meanwhile. It
//! paces itself with the client's `WaitPace`, so no state makes it spin, and a failed round is
//! told to the host as an `error` event and tried again after the pace's pause. Once a minute
//! it runs the signer's due work, which renews the app's certificate and answers its key
//! bundle requests.
//!
//! **Payments left open** are resumed by the loop (`Paykit::resume_payments`): once after
//! `start`, so a payment a restart interrupted is finished, its proof sent; at once when a
//! host `pay` call answers after the SDK stopped waiting for it (`flights.rs`); and at each
//! turn of the loop while an outcome says the payment still waits on its method.

use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::Duration;

use js_sys::Date;
use paykit_sdk::{PayeeStatus, RequestView};
use pubky_dispatch_client::app::{AppEvent, WaitPace, Woken, MAX_WAIT_SECONDS};
use pubky_dispatch_core::enc::pubky_id;
use serde_json::{json, Value};

use crate::engine::Engine;
use crate::error::EngineError;
use crate::turn::Turn;
use crate::views::{self, Admission};

/// The most feed reads in one round. A read that says `more` is followed by the next at once;
/// past this the round ends, and the loop reads again without waiting.
const READS_PER_ROUND: usize = 8;

/// How often the feed loop runs the signer's due work, in milliseconds.
pub const SIGNER_TICK_MS: f64 = 60_000.0;

/// The protocol whose duty events the engine acknowledges: Paykit's.
const PAYKIT_PROTOCOL: &str = "paykit";

/// A request's key: its payee and its request hash.
type RequestKey = ([u8; 32], [u8; 32]);

/// What the engine last saw of a request: its status, and its receipt's message ID.
type SeenRequest = (String, Option<String>);

/// What the engine last saw, to tell the host what changed.
#[derive(Default)]
pub struct Observed {
    /// Each one-time request the payer holds, by payee and request hash: its status, and the
    /// message ID of its receipt.
    requests: HashMap<RequestKey, SeenRequest>,
    /// Each payee record's status.
    activity: HashMap<[u8; 32], String>,
    /// Each contact's admission.
    admissions: HashMap<[u8; 32], Admission>,
    /// The last error told for each step, so a step that keeps failing the same way is told
    /// once.
    errors: HashMap<&'static str, String>,
}

/// Why a round ended early.
pub enum RoundError {
    /// The engine stopped.
    Stopped,
    /// A read failed: the loop tells the host, and tries again after a pause.
    Failed(EngineError),
}

/// Runs one receive round on `engine` and returns its summary (`ReceiveResult` in the
/// declarations), having emitted the round's events.
pub async fn round(engine: &Rc<Engine>) -> Result<Value, RoundError> {
    let mut turn = Turn::take(engine).await.map_err(|_| RoundError::Stopped)?;
    let mut applied = Vec::new();
    let mut last_event = None;
    let mut scope_keys_awaited = false;
    let mut summary = json!({});
    for _ in 0..READS_PER_ROUND {
        let read = Box::pin(turn.paykit.receive(None))
            .await
            .map_err(|error| RoundError::Failed(EngineError::sdk("receive", &error)))?;
        for event in &read.report.events {
            if let Some(id) = event.id() {
                last_event = Some(id);
            }
            if let AppEvent::MessageReceived {
                seq,
                kind,
                msg_id,
                sender,
                ..
            } = event
            {
                engine.events.emit(
                    "messageReceived",
                    json!({
                        "kind": kind,
                        "msgId": msg_id,
                        "sender": pubky_id(sender),
                        "seq": seq,
                    }),
                );
            }
        }
        applied.extend(read.applied.iter().cloned());
        scope_keys_awaited |= read.scope_keys_awaited;
        let more = read.report.more && !read.report.stopped;
        summary = json!({
            "generation": read.report.cursor.generation,
            "sequence": read.report.cursor.seq,
            "more": read.report.more,
            "stopped": read.report.stopped,
            "scopeKeysAwaited": read.scope_keys_awaited,
            "recordsUnlisted": read.records_unlisted,
            "accountGone": read.account_gone,
        });
        if !more {
            break;
        }
    }
    if let Some(id) = last_event {
        let acked = Box::pin(turn.paykit.app_mut().ack_events_for(PAYKIT_PROTOCOL, id)).await;
        report(
            engine,
            "acknowledge",
            acked.map_err(|error| EngineError::paykit("acknowledge the round's events", &error)),
        );
    }
    if scope_keys_awaited {
        report(
            engine,
            "signer",
            run_signer(engine, &mut turn).await.map(|_| ()),
        );
    }
    // A read does not send: what waits in the outbox, such as a proof or a receipt whose
    // first try failed, is tried again now.
    deliver(engine, &mut turn).await;
    drop(turn);
    if let Value::Object(fields) = &mut summary {
        fields.insert("applied".into(), json!(applied));
    }
    engine.events.emit("received", summary.clone());
    Ok(summary)
}

/// Runs the signer's due work (`Signer::run_due`) and resets the loop's clock for it.
/// Returns whether more is due than one run does.
pub async fn run_signer(engine: &Engine, turn: &mut Turn) -> Result<bool, EngineError> {
    engine.signer_due_at.set(Date::now() + SIGNER_TICK_MS);
    let ran = Box::pin(turn.signer.run_due())
        .await
        .map_err(|error| EngineError::signer("run the signer's due work", &error))?;
    Ok(ran.more_due())
}

/// Tells the host of `step`'s error, once while it keeps failing the same way; forgets it
/// once the step succeeds.
fn report(engine: &Engine, step: &'static str, result: Result<(), EngineError>) {
    let mut observed = engine.observed.borrow_mut();
    match result {
        Ok(()) => {
            observed.errors.remove(step);
        }
        Err(error) => {
            let text = error.to_string();
            if observed.errors.get(step) != Some(&text) {
                observed.errors.insert(step, text);
                drop(observed);
                engine.events.emit(
                    "error",
                    json!({ "during": step, "code": error.code(), "message": error.text() }),
                );
            }
        }
    }
}

/// The one-time requests the payer holds, as the FFI's `requests` lists them.
pub async fn requests(turn: &Turn) -> Result<Vec<RequestView>, EngineError> {
    Ok(Box::pin(turn.paykit.requests())
        .await
        .map_err(|error| EngineError::sdk("read the requests", &error))?
        .into_iter()
        .filter(|view| !view.recurring)
        .collect())
}

/// Takes the state the host sees at start as seen: later rounds tell only what changes from
/// it. A read that fails leaves its part empty, so its items are told as new later.
pub async fn baseline(engine: &Rc<Engine>) {
    let Ok(mut turn) = Turn::take(engine).await else {
        return;
    };
    if let Ok(views) = requests(&turn).await {
        let mut observed = engine.observed.borrow_mut();
        for view in &views {
            observed.requests.insert(
                (view.request.payee, view.request.request_hash),
                (
                    views::case_name(&view.status),
                    view.receipt.as_ref().map(|r| r.msg_id.clone()),
                ),
            );
        }
    }
    if let Ok(activity) = Box::pin(turn.paykit.payee_activity()).await {
        let mut observed = engine.observed.borrow_mut();
        for sale in &activity {
            observed
                .activity
                .insert(sale.record, views::case_name(&sale.status));
        }
    }
    if let Ok(list) = Box::pin(turn.paykit.app_mut().allow_list()).await {
        let mut observed = engine.observed.borrow_mut();
        for peer in &list.peers {
            observed.admissions.insert(
                peer.pubky,
                Admission {
                    admitted: true,
                    kept: peer.kept,
                    reply: peer.reply.is_some(),
                },
            );
        }
    }
}

/// Tries the outbox now (`Paykit::flush`), then tells the host what changed: the end of every
/// round, and of every call that queues a message. A failed try is told as an `error` event
/// with `during: "flush"`; the messages stay queued for the next round.
pub async fn deliver(engine: &Engine, turn: &mut Turn) {
    let flushed = Box::pin(turn.paykit.flush())
        .await
        .map_err(|error| EngineError::sdk("send the queued messages", &error));
    report(engine, "flush", flushed);
    observe(engine, turn).await;
}

/// Resumes the payments left open (`Paykit::resume_payments`) and returns their outcomes
/// (`PayOutcome` in the declarations): a payment a restart interrupted, or one whose host `pay`
/// answered late, is finished and its proof sent. Emits `paymentsResumed` when there are
/// outcomes, then the changes they made. While an outcome still waits on the method, the feed
/// loop resumes again at its next turn.
pub async fn resume(engine: &Rc<Engine>) -> Result<Vec<Value>, EngineError> {
    let mut turn = Turn::take(engine).await?;
    engine.flights.take_arrived();
    let resumed = Box::pin(turn.paykit.resume_payments()).await;
    let result = match resumed {
        Ok(outcomes) => {
            engine
                .payments_open
                .set(outcomes.iter().any(views::outcome_waits));
            let list: Vec<Value> = outcomes.iter().map(views::pay_outcome).collect();
            if !list.is_empty() {
                engine
                    .events
                    .emit("paymentsResumed", json!({ "outcomes": list.clone() }));
            }
            Ok(list)
        }
        Err(error) => {
            engine.payments_open.set(true);
            Err(EngineError::sdk("resume the payments", &error))
        }
    };
    observe(engine, &turn).await;
    result
}

/// Compares the requests and the payee activity with what was seen before, and emits an event
/// for each change.
pub async fn observe(engine: &Engine, turn: &Turn) {
    match requests(turn).await {
        Ok(views) => {
            report(engine, "requests", Ok(()));
            for view in &views {
                observe_request(engine, view);
            }
        }
        Err(error) => report(engine, "requests", Err(error)),
    }
    match Box::pin(turn.paykit.payee_activity()).await {
        Ok(activity) => {
            report(engine, "payeeActivity", Ok(()));
            for sale in &activity {
                let status = views::case_name(&sale.status);
                let previous = engine
                    .observed
                    .borrow_mut()
                    .activity
                    .insert(sale.record, status.clone());
                if previous.as_ref() == Some(&status) {
                    continue;
                }
                let json = views::payee_activity(sale);
                if sale.status == PayeeStatus::ProofReceived {
                    engine
                        .events
                        .emit("proofReceived", json!({ "activity": json.clone() }));
                }
                engine.events.emit(
                    "activityUpdated",
                    json!({ "activity": json, "previousStatus": previous }),
                );
            }
        }
        Err(error) => report(
            engine,
            "payeeActivity",
            Err(EngineError::sdk("read the payee activity", &error)),
        ),
    }
}

fn observe_request(engine: &Engine, view: &RequestView) {
    let key = (view.request.payee, view.request.request_hash);
    let status = views::case_name(&view.status);
    let receipt = view.receipt.as_ref().map(|r| r.msg_id.clone());
    let previous = engine
        .observed
        .borrow_mut()
        .requests
        .insert(key, (status.clone(), receipt.clone()));
    let json = views::request(view);
    let had_receipt = matches!(previous, Some((_, Some(_))));
    match previous {
        None => engine
            .events
            .emit("requestReceived", json!({ "request": json.clone() })),
        Some((previous_status, _)) if previous_status != status => engine.events.emit(
            "requestUpdated",
            json!({ "request": json.clone(), "previousStatus": previous_status }),
        ),
        Some(_) => {}
    }
    if let (Some(held), false) = (&view.receipt, had_receipt) {
        engine.events.emit(
            "receiptReceived",
            json!({ "request": json, "receipt": views::receipt(held) }),
        );
    }
}

/// What the engine last saw of `identity`'s admission: none when it saw nothing.
pub fn admission_of(engine: &Engine, identity: &[u8; 32]) -> Admission {
    engine
        .observed
        .borrow()
        .admissions
        .get(identity)
        .copied()
        .unwrap_or_default()
}

/// Records `identity`'s admission as `now`, and emits `admissionChanged` if it changed.
pub fn observe_admission(engine: &Engine, identity: &[u8; 32], now: Admission) {
    let previous = engine
        .observed
        .borrow_mut()
        .admissions
        .insert(*identity, now)
        .unwrap_or_default();
    if previous != now {
        engine
            .events
            .emit("admissionChanged", now.to_json(identity));
    }
}

/// Runs the feed loop on `engine` until it stops: see the module docs.
pub fn spawn(engine: Rc<Engine>) {
    wasm_bindgen_futures::spawn_local(async move {
        let reason = run(&engine).await;
        engine
            .events
            .emit("feedStopped", json!({ "reason": reason }));
    });
}

async fn run(engine: &Rc<Engine>) -> &'static str {
    let mut pace = WaitPace::new();
    let mut read = true;
    loop {
        if engine.stop.is_raised() {
            return "stopped";
        }
        if read {
            match round(engine).await {
                Ok(_) => report(engine, "receive", Ok(())),
                Err(RoundError::Stopped) => return "stopped",
                Err(RoundError::Failed(error)) => {
                    report(engine, "receive", Err(error));
                    let pause = pace.after_failed_read();
                    if !engine.stop.sleep(millis(pause)).await {
                        return "stopped";
                    }
                    continue;
                }
            }
        }
        if engine.payments_open.get() || engine.flights.take_arrived() {
            if engine.stop.is_raised() {
                return "stopped";
            }
            let resumed = resume(engine).await.map(|_| ());
            report(engine, "resumePayments", resumed);
        }
        if Date::now() >= engine.signer_due_at.get() {
            let Ok(mut turn) = Turn::take(engine).await else {
                return "stopped";
            };
            let ran = run_signer(engine, &mut turn).await.map(|_| ());
            drop(turn);
            report(engine, "signer", ran);
        }
        let wait = match Turn::take(engine).await {
            Ok(turn) => turn.paykit.app().feed_wait(MAX_WAIT_SECONDS),
            Err(_) => return "stopped",
        };
        let asked = wait.seconds();
        let started = Date::now();
        let woken = engine
            .stop
            .unless_raised(first_of(wait.wait(), engine.flights.arrival()))
            .await;
        let woken = match woken {
            None => return "stopped",
            // A `pay` answered late: resume the payments now, then wait again.
            Some(None) => {
                read = false;
                continue;
            }
            Some(Some(woken)) => woken,
        };
        let took = Duration::from_millis((Date::now() - started).max(0.0) as u64);
        if let Some(pause) = pace.after_wait(woken, asked, took) {
            if !engine.stop.sleep(millis(pause)).await {
                return "stopped";
            }
        }
        read = woken != Woken::Quiet;
    }
}

/// Runs `main` until it completes, or until `interrupt` does first: `None` then, and `main` is
/// dropped, which cancels it.
async fn first_of<F: Future>(main: F, interrupt: impl Future<Output = ()>) -> Option<F::Output> {
    let mut main = pin!(main);
    let mut interrupt = pin!(interrupt);
    poll_fn(|cx| {
        if let Poll::Ready(output) = main.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        if interrupt.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
