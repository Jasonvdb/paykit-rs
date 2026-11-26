#!/usr/bin/env node
// Smoke scenarios for the engine, on the harness (`harness.mjs`): engines in dedicated workers
// of headless Chrome, with the Origin Private File System on disk, against a live Dispatch, a
// Homeserver with the fake Homeserver's API and, for `peer`, a native peer (`paykit-peer`).
//
//   node paykit-wasm/www/smoke.mjs --pkg <engine module folder> [--scenario start|peer|flow|all]
//
// Environment: see `settings` in harness.mjs (PAYKIT_WASM_DISPATCH_ORIGIN,
// PAYKIT_WASM_DISPATCH_LISTENER, PAYKIT_WASM_HOMESERVER, PAYKIT_WASM_PROFILE_ROOT,
// PAYKIT_WASM_BROWSER, PAYKIT_WASM_PEER, PAYKIT_WASM_RUN_ROOT, PAYKIT_WASM_BITCOIND, ...).
//
// Scenarios (default all):
//   start  1. worker A starts the engine: it enrols and joins ("new") with the secret's
//             identity; a second start in the same worker is refused; it stops.
//          2. the Homeserver holds identity.json, naming the engine's app, and the signer's
//             backup.
//          3. a fresh worker B with the same secret and stores loads ("loaded") the same
//             identity and app; identity.json is unchanged.
//          4. a fresh worker C with another secret on the same stores is refused
//             (identity_mismatch).
//   peer   1. a native peer N starts; the browser engine W starts.
//          2. W allows N (admissionChanged, admits, contacts) and N allows W.
//          3. N requests sats from W: W's feed loop gets it as messageReceived and
//             requestReceived events, with N's amount, reference and endpoint.
//          4. W's worker is terminated without stop, as a crash; a new worker starts the same
//             identity on the same stores: it loads, its requests() still hold the request,
//             and it tells no requestReceived for it again.
//          5. N requests again: the restarted feed loop gets the new request.
//          6. signerTick runs; stop ends the feed loop (feedStopped).
//          7. resetIdentity on a running engine destroys its stores and deletes the signer's
//             files, so another identity can start on the same store prefix.
//   flow   Payments on bitcoind regtest between the browser engine W, which pays and verifies
//          through a test payment method in Node (`RegtestRail`), and a native peer N that
//          pays and confirms by itself (`run --auto-pay --auto-confirm`). Both allow each other.
//          a. N requests 10,000 sats from W; W pays, its method taking 20 s, as a payment out
//             of a shared pool does; N verifies on-chain and confirms; W gets the receipt.
//             Paying the request again is refused (refused, with a reason).
//          b. W requests 12,000 sats from N to a fresh P2TR address of its method; N pays; W
//             gets the proof, verifies it on-chain and confirms; N gets the receipt.
//             Malformed and unknown records are refused with typed errors.
//          c. N requests 15,000 sats; W's method sends the transaction but holds its answer,
//             and W's worker is terminated meanwhile. A new worker loads, resumes the payment
//             from the method's record, sends the proof, and gets the receipt. One payment.
//          d. With a 3 s pay limit, N requests 11,000 sats and W's method takes 9 s: payRequest
//             resolves pending, the method is not asked again while it runs, and when it
//             answers the engine finishes the payment by itself; W gets the receipt.
//          e. W rejects a request of N's (N sees it rejected), and cancels a request it sent to
//             an identity that does not admit it yet.
//          Each payment's transaction is checked in bitcoind: it pays the requested address
//          exactly the requested amount.
// Each run enrols new random identities on the Dispatch, which keeps them. No secret is
// printed. Every wait is on an event or a call's answer, never on a sleep.

import { createHash } from "node:crypto";
import { parseArgs } from "node:util";

import {
  BACKUP_PATH,
  IDENTITY_PATH,
  check,
  checkServices,
  homeserverFile,
  launch,
  log,
  newIdentity,
  paidTo,
  satsToBtc,
  zbase32,
} from "./harness.mjs";

const { values: flags } = parseArgs({
  options: { pkg: { type: "string" }, scenario: { type: "string", default: "all" } },
});
if (!flags.pkg || !["start", "peer", "flow", "all"].includes(flags.scenario)) {
  console.error("usage: smoke.mjs --pkg <engine module folder> [--scenario start|peer|flow|all]");
  process.exit(2);
}

/** The P1 scenario: enrol, reload, refuse another identity. */
async function startScenario(harness) {
  const { secret, identity } = newIdentity();
  const options = harness.startOptions(secret);
  log(`start: identity ${identity}, stores ${options.storePrefix}-app and -signer`);

  // 1. First start: enrol and join.
  const a = await harness.openEngine("a");
  log(`engine version ${await a.must("version")}`);
  const first = await a.must("start", options);
  log(`first start: ${JSON.stringify(first)}`);
  check(first.enrolment === "new", "the first start enrols (enrolment new)");
  check(first.identity === identity, "the identity is the Pubky ID of the secret");
  check(/^[A-Za-z0-9_-]{22}$/.test(first.appId), "the App ID is 16 bytes in base64url");
  check((await a.must("identity")) === identity, "identity() gives the same Pubky ID");
  check((await a.must("appId")) === first.appId, "appId() gives the same App ID");
  const again = await a.call("start", options);
  check(!again.ok && again.error.startsWith("already_started:"), "a second start in the same worker is refused");
  await a.must("stop");
  await a.waitFor((event) => event.type === "feedStopped", { what: "feedStopped" });
  const stopped = await a.call("identity");
  check(!stopped.ok && stopped.error.startsWith("not_started:"), "identity() after stop says not_started");
  await a.terminate();

  // 2. What the signer published.
  const doc = await homeserverFile(identity, IDENTITY_PATH);
  check(doc !== null, "identity.json exists on the Homeserver");
  // A signed document: its body is base64url JSON, and it names apps by z-base-32 App ID.
  const body = JSON.parse(Buffer.from(JSON.parse(doc.toString("utf8")).body, "base64url").toString("utf8"));
  check(body.identity === identity, "identity.json is the identity's");
  check(body.inbox?.url === options.dispatchOrigin, "identity.json names the Dispatch origin");
  const appZ32 = zbase32(Buffer.from(first.appId, "base64url"));
  check(
    Array.isArray(body.apps) && body.apps.length === 1 && body.apps[0].app_id === appZ32,
    "identity.json lists the engine's app, and only it",
  );
  check((await homeserverFile(identity, BACKUP_PATH)) !== null, "the signer's backup exists on the Homeserver");
  const docHash = createHash("sha256").update(doc).digest("hex");

  // 3. Second start in a fresh worker: load.
  const b = await harness.openEngine("b");
  const second = await b.must("start", options);
  check(second.enrolment === "loaded", "the second start loads (enrolment loaded)");
  check(second.identity === identity && second.appId === first.appId, "the second start has the same identity and app");
  await b.must("stop");
  await b.terminate();
  const docAfter = await homeserverFile(identity, IDENTITY_PATH);
  check(
    docAfter !== null && createHash("sha256").update(docAfter).digest("hex") === docHash,
    "identity.json is unchanged by the second start",
  );

  // 4. Another identity on the same stores.
  const c = await harness.openEngine("c");
  const other = newIdentity();
  const refused = await c.call("start", {
    ...options,
    identitySecretHex: other.secret.toString("hex"),
    ownIdentity: other.identity,
  });
  check(!refused.ok && refused.error.startsWith("identity_mismatch:"), "another identity's start on the same stores is refused");
  await c.terminate();
  log(`start: PASS (identity ${identity}, app ${first.appId})`);
}

/** The request a peer's `request_sent` names, as the engine should show it. */
function expectRequest(sent, sats) {
  return {
    requestHash: sent.request_hash,
    amount: { value: (sats / 1e8).toFixed(8), asset: "btc" },
    reference: sent.reference,
    endpoint: { identifier: sent.endpoint_id, address: sent.address },
  };
}

/** Checks that `request` (a OneTimeRequest) is the one `expected` describes, from `payee`. */
function checkRequest(request, expected, payee, where) {
  check(request.id.payee === payee, `${where}: the request is from the peer`);
  check(request.id.requestHash === expected.requestHash, `${where}: the request hash is the peer's`);
  check(
    request.amount.value === expected.amount.value && request.amount.asset === expected.amount.asset,
    `${where}: the amount is ${expected.amount.value} ${expected.amount.asset}`,
  );
  check(request.paymentReference === expected.reference, `${where}: the payment reference is the peer's`);
  const endpoints = request.destination?.type === "endpoints" ? request.destination.endpoints : [];
  check(
    endpoints.length === 1 &&
      endpoints[0].identifier === expected.endpoint.identifier &&
      JSON.parse(endpoints[0].payload).value === expected.endpoint.address,
    `${where}: the endpoint is ${expected.endpoint.identifier} paying ${expected.endpoint.address}`,
  );
  check(request.status === "open", `${where}: the request is open`);
}

/** The P2 gate: admission both ways, a request from a native peer, a worker restart. */
async function peerScenario(harness) {
  // 1. The native peer N and the browser engine W.
  const n = await harness.startPeer({ name: "p2-peer" });
  const { secret, identity: wId } = newIdentity();
  const options = harness.startOptions(secret);
  const w = await harness.openEngine("w");
  const started = await w.must("start", options);
  check(started.identity === wId, "W starts with the secret's identity");
  log(`peer: W ${wId} (${started.enrolment}), N ${n.identity}`);

  // 2. Admission both ways.
  await w.must("allowConnect", n.identity);
  const admitted = await w.waitFor(
    (event) => event.type === "admissionChanged" && event.identity === n.identity,
    { what: "admissionChanged for N" },
  );
  check(admitted.admitted && admitted.reply, "W's admissionChanged says N is admitted with a reply grant");
  check((await w.must("admits", n.identity)) === true, "W admits N");
  const contacts = await w.must("contacts");
  check(
    contacts.allowed.some((entry) => entry.identity === n.identity && entry.admitted && entry.reply),
    "W's contacts list N as allowed",
  );
  check(!(await w.must("admits", newIdentity().identity)), "W admits no stranger");
  const malformed = await w.call("allowConnect", "not-a-pubky-id");
  check(!malformed.ok && malformed.error.startsWith("invalid_input:"), "allowConnect refuses a malformed Pubky ID");
  await n.command(`allow ${wId}`);
  await n.waitFor((event) => event.event === "allowed" && event.id === wId, { what: "N allowed W" });
  log("ok: N allows W");

  // 3. N requests from W.
  const sats = 12345;
  const from = n.log.events.length;
  await n.command(`request ${wId} ${sats}`);
  const sent = await n.waitFor((event) => event.event === "request_sent" && event.to === wId, {
    what: "request_sent",
    from,
  });
  log(`N sent request ${sent.request_hash} (msg ${sent.msg_id}) to ${sent.address}`);
  const expected = expectRequest(sent, sats);
  const message = await w.waitFor(
    (event) => event.type === "messageReceived" && event.msgId === sent.msg_id,
    { what: "messageReceived for N's request" },
  );
  check(
    message.sender === n.identity && message.kind === "paykit.payment_request",
    "W's messageReceived is N's paykit.payment_request",
  );
  const received = await w.waitFor(
    (event) => event.type === "requestReceived" && event.request.id.requestHash === sent.request_hash,
    { what: "requestReceived for N's request" },
  );
  checkRequest(received.request, expected, n.identity, "the event");
  const listed = (await w.must("requests")).find((r) => r.id.requestHash === sent.request_hash);
  check(listed !== undefined, "W's requests() holds the request");
  checkRequest(listed, expected, n.identity, "requests()");

  // 4. A crash: the worker ends without stop; a new worker loads the same stores.
  await w.terminate();
  log("W's worker terminated without stop");
  const w2 = await harness.openEngine("w2");
  const restarted = await w2.must("start", options);
  check(restarted.enrolment === "loaded", "the restarted engine loads (enrolment loaded)");
  check(restarted.identity === wId && restarted.appId === started.appId, "the restarted engine has the same identity and app");
  const kept = (await w2.must("requests")).find((r) => r.id.requestHash === sent.request_hash);
  check(kept !== undefined, "after the restart, requests() still holds the request");
  checkRequest(kept, expected, n.identity, "after the restart");
  check(await w2.must("admits", n.identity), "after the restart, W still admits N");

  // 5. The restarted feed loop goes on: a new request from N arrives as an event.
  const sats2 = 23456;
  const from2 = n.log.events.length;
  await n.command(`request ${wId} ${sats2}`);
  const sent2 = await n.waitFor((event) => event.event === "request_sent" && event.to === wId, {
    what: "the second request_sent",
    from: from2,
  });
  const received2 = await w2.waitFor(
    (event) => event.type === "requestReceived" && event.request.id.requestHash === sent2.request_hash,
    { what: "requestReceived for N's second request" },
  );
  checkRequest(received2.request, expectRequest(sent2, sats2), n.identity, "the restarted feed's event");
  check(
    w2.log.filter((event) => event.type === "requestReceived" && event.request.id.requestHash === sent.request_hash)
      .length === 0,
    "the restarted engine does not tell the first request again (it was in its state at start)",
  );
  const errors = [...w.events, ...w2.events].filter((event) => event.type === "error");
  check(errors.length === 0, `no error events (${errors.map((e) => `${e.during}: ${e.message}`).join("; ")})`);

  // 6. The signer's due work, then a stop.
  const tick = await w2.must("signerTick");
  check(typeof tick.moreDue === "boolean", `signerTick answers (moreDue ${tick.moreDue})`);
  await w2.must("stop");
  const ended = await w2.waitFor((event) => event.type === "feedStopped", { what: "feedStopped" });
  check(ended.reason === "stopped", "stop ends the feed loop");
  await w2.terminate();

  // 7. resetIdentity on a running engine.
  const w3 = await harness.openEngine("w3");
  check((await w3.must("start", options)).enrolment === "loaded", "a third worker loads the identity");
  await w3.must("resetIdentity");
  check((await homeserverFile(wId, IDENTITY_PATH)) === null, "resetIdentity deleted identity.json");
  check((await homeserverFile(wId, BACKUP_PATH)) === null, "resetIdentity deleted the signer's backup");
  const notStarted = await w3.call("identity");
  check(!notStarted.ok && notStarted.error.startsWith("not_started:"), "the engine is stopped after resetIdentity");
  const fresh = newIdentity();
  const reused = await w3.must("start", {
    ...options,
    identitySecretHex: fresh.secret.toString("hex"),
    ownIdentity: fresh.identity,
  });
  check(
    reused.enrolment === "new" && reused.identity === fresh.identity,
    "another identity starts on the reset store prefix (its stores were destroyed)",
  );
  await w3.must("stop");
  await w3.terminate();
  log(`peer: PASS (W ${wId}, N ${n.identity}, requests ${sent.request_hash.slice(0, 12)}..., ${sent2.request_hash.slice(0, 12)}...)`);
}


/** Checks in bitcoind that `txid` pays `address` exactly `sats`. */
async function checkChain(txid, address, sats, what) {
  check(/^[0-9a-f]{64}$/.test(txid ?? ""), `${what}: the proof names a txid (${txid})`);
  const { sats: paid, tx } = await paidTo(txid, address);
  check(tx.txid === txid, `${what}: bitcoind holds the transaction ${txid.slice(0, 16)}...`);
  check(paid === BigInt(sats), `${what}: it pays ${address.slice(0, 14)}... exactly ${sats} sats (${paid})`);
}

/** Has N request `sats` from W and waits for W to hold it: `{sent, request}`. */
async function peerRequests(n, w, wId, sats, reference) {
  const from = n.log.events.length;
  await n.command(`request ${wId} ${sats} --reference ${reference}`);
  const sent = await n.waitFor((event) => event.event === "request_sent" && event.to === wId, {
    what: `request_sent for ${sats} sats`,
    from,
  });
  const received = await w.waitFor(
    (event) => event.type === "requestReceived" && event.request.id.requestHash === sent.request_hash,
    { what: `requestReceived for N's ${sats} sats` },
  );
  check(
    received.request.amount.value === satsToBtc(sats) && received.request.paymentReference === reference,
    `W holds N's request for ${satsToBtc(sats)} btc, reference ${reference}`,
  );
  return { sent, request: received.request };
}

/** Waits until N confirms the payment of `requestHash` and W holds its receipt; checks both
 * sides agree and the chain holds the payment. */
async function receiptReaches(n, w, { sent, request }, sats, what, { attemptId, txid } = {}) {
  const confirmed = await n.waitFor(
    (event) => event.event === "confirmed" && event.request_hash === sent.request_hash,
    { what: `N's confirmed for ${what}`, timeoutMs: 90_000 },
  );
  const verified = n.log.events.find(
    (event) => event.event === "verified" && event.request_hash === sent.request_hash,
  );
  check(
    verified !== undefined && verified.sats_received === sats,
    `${what}: N verified ${sats} sats on-chain (${verified?.sats_received})`,
  );
  if (txid) check(confirmed.txid === txid, `${what}: N confirmed the transaction W's method sent`);
  const receiptEvent = await w.waitFor(
    (event) => event.type === "receiptReceived" && event.request.id.requestHash === sent.request_hash,
    { what: `W's receiptReceived for ${what}`, timeoutMs: 90_000 },
  );
  check(
    receiptEvent.receipt.messageId === confirmed.receipt_msg_id,
    `${what}: W's receipt is the one N sent (${confirmed.receipt_msg_id})`,
  );
  if (attemptId) check(receiptEvent.receipt.attemptId === attemptId, `${what}: the receipt is for W's attempt`);
  const listed = (await w.must("requests")).find((r) => r.id.requestHash === sent.request_hash);
  check(listed?.status === "paid" && listed.receipt?.messageId === confirmed.receipt_msg_id, `${what}: requests() shows it paid, with the receipt`);
  const receipts = await w.must("requestReceipts", request.id);
  check(receipts.length === 1 && receipts[0].messageId === confirmed.receipt_msg_id, `${what}: requestReceipts gives the receipt`);
  const payment = (await w.must("payments")).find((p) => p.requestHash === sent.request_hash);
  check(payment?.paidByMethod && payment.receipt?.messageId === confirmed.receipt_msg_id, `${what}: payments() holds the payment with its receipt (stage ${payment?.stage})`);
  await checkChain(confirmed.txid, sent.address, sats, what);
  return { confirmed, receipt: receiptEvent.receipt };
}

/** The P3 gate: requests, payments, proofs, confirmations and receipts both ways, on regtest. */
async function flowScenario(harness) {
  const n = await harness.startPeer({
    name: "p3-peer",
    args: ["--auto-pay", "--auto-confirm", "--fund", "300000"],
  });
  await n.waitFor((event) => event.event === "funded", { what: "N funded" });
  const rail = await harness.openRail("w", { fundSats: 300_000 });
  const { secret, identity: wId } = newIdentity();
  const options = harness.startOptions(secret, { railUrl: rail.url });
  const w = await harness.openEngine("w");
  const started = await w.must("start", options);
  log(`flow: W ${wId} (${started.enrolment}), N ${n.identity}`);
  await w.must("allowConnect", n.identity);
  await n.command(`allow ${wId}`);

  // a. N requests from W; W pays, the method taking 20 s; N confirms; W gets the receipt.
  const a = await peerRequests(n, w, wId, 10_000, "invoice-a");
  rail.behaviour = { delayMs: 20_000, holdMs: 0 };
  const payStarted = Date.now();
  const paidA = await w.must("payRequest", a.request.id);
  const payTook = Date.now() - payStarted;
  check(paidA.outcome === "paid", `a: payRequest waits for the slow method and resolves paid (${payTook} ms)`);
  check(payTook >= 20_000, "a: the payment took the method's 20 s");
  const recordA = rail.attempts.get(paidA.attemptId);
  check(recordA?.state === "paid", "a: the method paid the attempt payRequest names");
  check(recordA.address === a.sent.address && recordA.endpointId === a.sent.endpoint_id, "a: W paid N's endpoint");
  const updatedA = await w.waitFor(
    (event) => event.type === "requestUpdated" && event.request.id.requestHash === a.sent.request_hash && event.request.status === "paid",
    { what: "requestUpdated to paid" },
  );
  check(updatedA.previousStatus === "open", "a: requestUpdated tells open -> paid");
  await receiptReaches(n, w, a, 10_000, "a", { attemptId: paidA.attemptId, txid: recordA.txid });
  rail.behaviour = { delayMs: 0, holdMs: 0 };
  const again = await w.call("payRequest", a.request.id);
  check(!again.ok && again.code === "refused" && typeof again.reason === "string", `a: paying it again is refused (reason ${again.reason})`);
  check(rail.payCalls(paidA.attemptId).length === 1, "a: the method was asked to pay once");

  // b. W requests from N to a fresh address; N pays; W verifies and confirms; N gets the receipt.
  const address = await rail.newAddress("bech32m");
  const sentB = await w.must("sendRequest", {
    to: n.identity,
    amount: { value: satsToBtc(12_000), asset: "btc" },
    paymentReference: "invoice-b",
    destination: {
      type: "endpoints",
      endpoints: [{ identifier: "btc-regtest-p2tr", payload: JSON.stringify({ value: address }) }],
    },
  });
  check(/^[0-9a-f]{64}$/.test(sentB.request?.requestHash ?? "") && sentB.request.payee === wId, `b: sendRequest gives the request (delivery ${sentB.delivery})`);
  const hashB = sentB.request.requestHash;
  const requested = await w.waitFor(
    (event) => event.type === "activityUpdated" && event.activity.requestHash === hashB,
    { what: "activityUpdated for W's request" },
  );
  check(requested.activity.status === "requested" && requested.activity.payer === n.identity, "b: W's activity shows the request sent to N");
  const paidB = await n.waitFor((event) => event.event === "paid" && event.request_hash === hashB, {
    what: "N paid W's request",
    timeoutMs: 90_000,
  });
  check(paidB.address === address && paidB.sats === 12_000, "b: N paid W's fresh address 12,000 sats");
  const proof = await w.waitFor(
    (event) => event.type === "proofReceived" && event.activity.requestHash === hashB,
    { what: "W's proofReceived", timeoutMs: 90_000 },
  );
  const record = proof.activity.record;
  check(proof.activity.attemptId === paidB.attempt_id, "b: the proof is N's attempt");
  const verifiedAt = await w.must("verifyPayment", record);
  check(typeof verifiedAt === "number" && Math.abs(verifiedAt - Date.now()) < 600_000, `b: verifyPayment confirms it on-chain (paid at ${verifiedAt})`);
  const receiptB = await w.must("confirmPayment", record);
  check(typeof receiptB === "string" && receiptB.length > 0, `b: confirmPayment sends a receipt (${receiptB})`);
  check((await w.must("confirmPayment", record)) === receiptB, "b: confirming again gives the same receipt");
  await w.waitFor(
    (event) => event.type === "activityUpdated" && event.activity.record === record && event.activity.status === "receipted",
    { what: "activityUpdated to receipted" },
  );
  log("ok: b: W's activity shows the payment receipted");
  const gotB = await n.waitFor(
    (event) => event.event === "receipt_received" && event.request_hash === hashB,
    { what: "N's receipt_received", timeoutMs: 90_000 },
  );
  check(gotB.msg_id === receiptB && gotB.txid === paidB.txid, "b: N holds W's receipt for its transaction");
  await checkChain(paidB.txid, address, 12_000, "b");
  const malformed = await w.call("confirmPayment", "not-a-record");
  check(!malformed.ok && malformed.code === "invalid_input", "b: a malformed record is invalid_input");
  const unknown = await w.call("verifyPayment", "00".repeat(32));
  check(!unknown.ok && unknown.code === "refused" && unknown.reason === "notFound", `b: an unknown record is refused notFound (${unknown.code} ${unknown.reason})`);
  const badRequest = await w.call("sendRequest", { to: n.identity, amount: { value: "1", asset: "btc" }, destination: { type: "any" } });
  check(!badRequest.ok && badRequest.code === "invalid_input", "b: a request without a paymentReference is invalid_input");

  // c. A restart between pay and receipt: the method sends but holds its answer, the worker dies.
  const c = await peerRequests(n, w, wId, 15_000, "invoice-c");
  rail.behaviour = { delayMs: 0, holdMs: 30_000 };
  const sentBefore = rail.attempts.size;
  const inFlight = w.call("payRequest", c.request.id);
  const deadline = Date.now() + 30_000;
  let attemptC;
  while (!attemptC && Date.now() < deadline) {
    attemptC = [...rail.attempts.entries()].slice(sentBefore).find(([, r]) => r.state === "paid");
    if (!attemptC) await new Promise((resolve) => setTimeout(resolve, 100));
  }
  check(attemptC !== undefined, "c: the method sent the transaction and holds its answer");
  const [attemptIdC, recordC] = attemptC;
  await w.terminate();
  const lost = await inFlight;
  check(!lost.ok, `c: W's worker terminated mid-payment (${lost.error})`);
  check(
    !n.log.events.some((event) => event.event === "proof_received" && event.request_hash === c.sent.request_hash),
    "c: N has no proof yet",
  );
  // The restarted engine uses a 3 s pay limit, for d.
  const w2 = await harness.openEngine("w2");
  const restarted = await w2.must("start", { ...options, rail: { ...options.rail, payLimitMs: 3_000 } });
  check(restarted.enrolment === "loaded", "c: the new worker loads the engine");
  const resumed = await w2.waitFor(
    (event) => event.type === "paymentsResumed" && event.outcomes.some((o) => o.attemptId === attemptIdC),
    { what: "paymentsResumed for the interrupted attempt" },
  );
  check(
    resumed.outcomes.find((o) => o.attemptId === attemptIdC).outcome === "paid",
    "c: the restarted engine resumes the attempt as paid, from the method's record",
  );
  await receiptReaches(n, w2, c, 15_000, "c", { attemptId: attemptIdC, txid: recordC.txid });
  check(rail.payCalls(attemptIdC).length === 1, "c: the method was asked to pay once: no second payment");

  // d. Past the pay limit: payRequest resolves pending, and the engine finishes it when pay answers.
  const d = await peerRequests(n, w2, wId, 11_000, "invoice-d");
  rail.behaviour = { delayMs: 9_000, holdMs: 0 };
  const startedD = Date.now();
  const pendingD = await w2.must("payRequest", d.request.id);
  check(pendingD.outcome === "pending", `d: payRequest resolves pending after the pay limit (${Date.now() - startedD} ms)`);
  const resumedD = await w2.waitFor(
    (event) => event.type === "paymentsResumed" && event.outcomes.some((o) => o.attemptId === pendingD.attemptId && o.outcome === "paid"),
    { what: "paymentsResumed paid for the slow attempt", timeoutMs: 60_000 },
  );
  check(resumedD.atMs - startedD >= 9_000, `d: the engine finished the payment when the method answered (${resumedD.atMs - startedD} ms)`);
  check(
    !rail.calls.some((call) => call.op === "payment" && call.attemptId === pendingD.attemptId && call.atMs < startedD + 9_000),
    "d: the method's lookup was not asked while its pay ran",
  );
  const recordD = rail.attempts.get(pendingD.attemptId);
  await receiptReaches(n, w2, d, 11_000, "d", { attemptId: pendingD.attemptId, txid: recordD.txid });
  check(rail.payCalls(pendingD.attemptId).length === 1, "d: the method was asked to pay once");

  // e. A rejection, and a cancellation of a request sent.
  const e = await peerRequests(n, w2, wId, 5_000, "invoice-e");
  const rejection = await w2.must("rejectRequest", e.request.id, "Not this one");
  check(typeof rejection === "string" && rejection.length > 0, "e: rejectRequest gives the rejection's message ID");
  await w2.waitFor(
    (event) => event.type === "requestUpdated" && event.request.id.requestHash === e.sent.request_hash && event.request.status === "rejected",
    { what: "requestUpdated to rejected" },
  );
  log("ok: e: W's request shows rejected");
  await n.waitFor(
    (event) => event.event === "payee_status" && event.request_hash === e.sent.request_hash && event.status === "Rejected",
    { what: "N sees the rejection", timeoutMs: 90_000 },
  );
  log("ok: e: N sees its request rejected");
  const payRejected = await w2.call("payRequest", e.request.id);
  check(!payRejected.ok && payRejected.code === "refused", `e: paying a rejected request is refused (reason ${payRejected.reason})`);
  const stranger = newIdentity().identity;
  const sentE = await w2.must("sendRequest", {
    to: stranger,
    amount: { value: satsToBtc(1_000), asset: "btc" },
    paymentReference: "invoice-e2",
    destination: { type: "endpoints", endpoints: [{ identifier: "btc-regtest-p2tr", payload: JSON.stringify({ value: await rail.newAddress() }) }] },
  });
  log(`e: a request to an identity that is not enrolled: delivery ${sentE.delivery}`);
  const activityE = await w2.waitFor(
    (event) => event.type === "activityUpdated" && event.activity.requestHash === sentE.request?.requestHash,
    { what: "activityUpdated for the request to a stranger" },
  );
  const cancellation = await w2.must("cancelSentRequest", activityE.activity.record, "Sent by mistake");
  check(typeof cancellation === "string" && cancellation.length > 0, "e: cancelSentRequest gives the cancellation's message ID");
  const cancelled = (await w2.must("payeeActivity")).find((entry) => entry.record === activityE.activity.record);
  check(cancelled?.status === "cancelled", `e: the payee activity shows it cancelled (${cancelled?.status})`);

  const errors = [...w.events, ...w2.events].filter((event) => event.type === "error");
  check(errors.length === 0, `no error events (${errors.map((e) => `${e.during}: ${e.code}: ${e.message}`).join("; ")})`);
  await w2.must("stop");
  await w2.terminate();
  log(`flow: PASS (W ${wId}, N ${n.identity}; txids a ${recordA.txid.slice(0, 12)}..., b ${paidB.txid.slice(0, 12)}..., c ${recordC.txid.slice(0, 12)}..., d ${recordD.txid.slice(0, 12)}...)`);
}

async function main() {
  await checkServices();
  const harness = await launch({ pkg: flags.pkg });
  try {
    if (flags.scenario === "start" || flags.scenario === "all") await startScenario(harness);
    if (flags.scenario === "peer" || flags.scenario === "all") await peerScenario(harness);
    if (flags.scenario === "flow" || flags.scenario === "all") await flowScenario(harness);
    log(`PASS: ${flags.scenario}`);
  } finally {
    await harness.close();
  }
}

main().catch((error) => {
  log(`FAIL: ${error.message}`);
  process.exit(1);
});
