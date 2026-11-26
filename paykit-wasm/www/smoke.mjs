#!/usr/bin/env node
// Smoke scenarios for the engine, on the harness (`harness.mjs`): engines in dedicated workers
// of headless Chrome, with the Origin Private File System on disk, against a live Dispatch, a
// Homeserver with the fake Homeserver's API and, for `peer`, a native peer (`paykit-peer`).
//
//   node paykit-wasm/www/smoke.mjs --pkg <engine module folder> [--scenario start|peer|all]
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
  zbase32,
} from "./harness.mjs";

const { values: flags } = parseArgs({
  options: { pkg: { type: "string" }, scenario: { type: "string", default: "all" } },
});
if (!flags.pkg || !["start", "peer", "all"].includes(flags.scenario)) {
  console.error("usage: smoke.mjs --pkg <engine module folder> [--scenario start|peer|all]");
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

async function main() {
  await checkServices();
  const harness = await launch({ pkg: flags.pkg });
  try {
    if (flags.scenario === "start" || flags.scenario === "all") await startScenario(harness);
    if (flags.scenario === "peer" || flags.scenario === "all") await peerScenario(harness);
    log(`PASS: ${flags.scenario}`);
  } finally {
    await harness.close();
  }
}

main().catch((error) => {
  log(`FAIL: ${error.message}`);
  process.exit(1);
});
