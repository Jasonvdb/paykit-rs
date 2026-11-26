# paykit-wasm API reference

The JavaScript API of the Paykit v0.2 engine for a browser wallet: the Paykit SDK and the
identity's Pubky Dispatch signer, compiled to WebAssembly. This is what a web host integrates
against. `paykit_wasm.d.ts` declares the same types for TypeScript.

Package files:

| File | What |
| --- | --- |
| `paykit_wasm.js` | the ES module: `init` (default export) and the calls below |
| `paykit_wasm_bg.wasm` | the module, about 8.2 MB |
| `paykit_wasm.d.ts`, `paykit_wasm_bg.wasm.d.ts` | TypeScript declarations |

Contents:

1. [Threading rules](#1-threading-rules)
2. [Minimal usage](#2-minimal-usage)
3. [Value formats](#3-value-formats)
4. [Starting and stopping](#4-starting-and-stopping)
5. [Calls](#5-calls)
6. [Records](#6-records)
7. [Events](#7-events)
8. [Errors](#8-errors)
9. [Homeserver callbacks](#9-homeserver-callbacks)
10. [Rail callbacks](#10-rail-callbacks)
11. [Restart rules](#11-restart-rules)
12. [Recipes](#12-recipes)

> **Demo shortcut.** The host passes the identity's 32-byte secret key to `start`, and the
> signer runs in the same worker as the app. Pubky's direction keeps identity keys out of apps.
> The engine accepts the key because the host, a wallet that derives it from its own seed,
> holds it already. The engine never stores the secret: the host passes it to every `start`.

## 1. Threading rules

- **Dedicated worker only.** Load the module in a dedicated module worker
  (`new Worker(url, {type: "module"})`). Its stores live in the Origin Private File System
  (OPFS) and use synchronous access handles, which only a dedicated worker can open. It does
  not run on the page's main thread, in a shared worker or in a service worker.
- **One engine per worker.** A worker holds at most one started engine. A second `start` in
  the same worker is `already_started`.
- **One tab.** Each store is held by one worker at a time (OPFS exclusive locks). A second
  tab, or a second worker of the same tab, that starts on the same `storePrefix` waits up to
  10 s for the files and then fails with `store`. Run the engine in one tab, and tell the user
  when another tab has it.
- **Calls take turns.** Calls are `async`. A call made while another runs, the feed loop's
  receive round included, waits and runs after it. `payRequest` holds its turn while the
  host's `pay` runs, up to `rail.payLimitMs`.
- **Callbacks run in the worker.** The Homeserver and rail callbacks are called in the worker.
  A callback that needs the page (such as a wallet SDK that lives there) forwards the call to
  the page with `postMessage` and resolves when the page answers.
- **Panics.** A panic is printed to the console and aborts the module. The worker can make no
  further calls: terminate it, start a new worker and call `start` again (see
  [Restart rules](#11-restart-rules)).
- **Clock.** The engine reads the worker's `Date.now()`. Event times (`atMs`) are that clock.
- Do not call `instantiated()`: it runs once by itself when the module is instantiated.

## 2. Minimal usage

`engine-worker.js`, the dedicated worker: the engine, its callbacks, and a message protocol to
the page.

```js
import init, * as engine from "./paykit-wasm-pkg/paykit_wasm.js";

const ready = init({
  module_or_path: new URL("./paykit-wasm-pkg/paykit_wasm_bg.wasm", import.meta.url),
});

// Calls the page and waits for its answer (for callbacks the page serves).
let next = 0;
const waiting = new Map();
function askPage(op, args) {
  const id = ++next;
  self.postMessage({ kind: "callback", id, op, args });
  return new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
}

const HOMESERVER = "http://127.0.0.1:18590"; // the fake Homeserver: GET/PUT/DELETE /{id}{path}

function homeserver(own) {
  return {
    async get(identityZ32, path) {
      const answer = await fetch(`${HOMESERVER}/${identityZ32}${path}`);
      if (answer.status === 404) return null;
      if (!answer.ok) throw new Error(`get answered ${answer.status}`);
      return new Uint8Array(await answer.arrayBuffer());
    },
    async put(path, bytes) {
      const answer = await fetch(`${HOMESERVER}/${own}${path}`, { method: "PUT", body: bytes });
      if (!answer.ok) throw new Error(`put answered ${answer.status}`);
    },
    async delete(path) {
      const answer = await fetch(`${HOMESERVER}/${own}${path}`, { method: "DELETE" });
      if (!answer.ok && answer.status !== 404) throw new Error(`delete answered ${answer.status}`);
    },
  };
}

self.onmessage = async ({ data }) => {
  if (data.kind === "answer") {
    // The page's answer to a callback.
    const call = waiting.get(data.id);
    waiting.delete(data.id);
    data.ok ? call.resolve(data.value) : call.reject(new Error(data.error));
    return;
  }
  const { id, op, args } = data; // a call from the page
  try {
    await ready;
    let value;
    if (op === "start") {
      const { identitySecretHex, identity } = args[0];
      value = await engine.start({
        identitySecretHex,
        dispatchOrigin: "https://localhost:18580",
        dispatchListener: "http://127.0.0.1:18580",
        storePrefix: "wallet",
        homeserver: homeserver(identity),
        rail: {
          endpoints: [
            { identifier: "btc-regtest-p2tr", fractionDigits: 8 },
            { identifier: "btc-regtest-p2wpkh", fractionDigits: 8 },
          ],
          pay: (payment) => askPage("pay", [payment]),
          payment: (attemptId) => askPage("payment", [attemptId]),
          verify: (check) => askPage("verify", [check]),
        },
        onEvent: (event) => self.postMessage({ kind: "event", event }),
      });
    } else {
      value = await engine[op](...args);
    }
    self.postMessage({ kind: "result", id, ok: true, value: value ?? null });
  } catch (error) {
    self.postMessage({
      kind: "result", id, ok: false,
      error: error.message, code: error.code ?? null, reason: error.reason ?? null,
    });
  }
};
```

The page: start the engine, follow events, call it.

```js
// onEngineEvent(event) and railOnPage {pay, payment, verify} are the host's own.
const worker = new Worker(new URL("./engine-worker.js", import.meta.url), { type: "module" });
let next = 0;
const pending = new Map();
function call(op, ...args) {
  const id = ++next;
  worker.postMessage({ id, op, args });
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}
worker.onmessage = async ({ data }) => {
  if (data.kind === "result") {
    const { resolve, reject } = pending.get(data.id);
    pending.delete(data.id);
    if (data.ok) return resolve(data.value);
    return reject(Object.assign(new Error(data.error), { code: data.code, reason: data.reason }));
  }
  if (data.kind === "event") return onEngineEvent(data.event);
  if (data.kind === "callback") {
    // rail.pay / rail.payment / rail.verify, served by the page's wallet.
    try {
      const value = await railOnPage[data.op](...data.args);
      worker.postMessage({ kind: "answer", id: data.id, ok: true, value });
    } catch (error) {
      worker.postMessage({ kind: "answer", id: data.id, ok: false, error: String(error) });
    }
  }
};

const { identity, appId, enrolment } = await call("start", { identitySecretHex, identity: myZ32 });
// Read the state once, then follow the events.
const requests = await call("requests");
const activity = await call("payeeActivity");
const lists = await call("receivedLists");
const shared = await call("sharedList");

await call("allowConnect", contactZ32);
await call("sendRequest", {
  to: contactZ32,
  amount: { value: "0.00012000", asset: "btc" },
  paymentReference: "Dinner",
  destination: { type: "endpoints", endpoints: [
    { identifier: "btc-regtest-p2tr", payload: JSON.stringify({ value: receiveAddress }) },
  ] },
});
```

## 3. Value formats

Every argument and result is a plain JSON-shaped value.

| Value | Format |
| --- | --- |
| Pubky ID (`identity`, `payee`, `payer`, `to`, ...) | 52 z-base-32 characters, no prefix. Arguments also accept a `pubky` or `pk:` prefix. |
| App ID (`appId`, `listApp`) | 16 bytes in base64url without padding (22 characters) |
| Request hash, record | 32 bytes in 64 lowercase hex digits |
| Attempt ID | text, as the engine gives it |
| Message ID (`msgId`, `messageId`, `proofMessageId`) | a UUID, as the engine gives it |
| Amount | `{value, asset}`: `value` a decimal string in whole units of `asset`, such as `{value: "0.00012000", asset: "btc"}` |
| Payment Endpoint | `{identifier, payload}`: `identifier` a Payment Endpoint Identifier (1 to 64 of `A-Z a-z 0-9 - _ .`, such as `btc-regtest-p2tr`); `payload` the text the payee publishes, such as `{"value":"bcrt1p..."}` |
| Time | Unix milliseconds, a number |
| Status, stage, outcome, reason | the paykit-ffi enum case in lowerCamelCase, such as `open`, `beingPaidHere`, `proofReceived` |

Status and reason strings are open sets: show an unknown one as a generic state rather than
failing.

## 4. Starting and stopping

### `start(options: StartOptions): Promise<StartResult>`

Opens the stores, runs the in-process signer, and then:

- **First start** (the stores hold no engine): the signer enrols the identity at the Dispatch,
  which publishes `/pub/pubky-dispatch/v1/identity.json` and the signer's backup
  `/priv/pubky-dispatch/v1/signer-backup.json` through the host's Homeserver. The signer
  approves a new app for the pay and request scopes, and the app joins. `enrolment: "new"`.
- **Later starts**: the signer and the app reload from their stores. Nothing is enrolled or
  approved again. `enrolment: "loaded"`.

Then it starts the feed loop, unless `autoReceive` is `false`, and resumes payments left open.

`StartOptions`:

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `identitySecretHex` | string | yes | the identity's 32-byte secret, 64 hex digits (the first 32 bytes of the BIP39 seed) |
| `dispatchOrigin` | string | yes | the Dispatch origin requests are signed for, such as `https://localhost:18580` |
| `dispatchListener` | string | no | a plain HTTP loopback listener that requests for a `https://localhost` origin go to, such as `http://127.0.0.1:18580`. Omit it to reach the origin itself. |
| `storePrefix` | string | no | stores `<prefix>-app` and `<prefix>-signer`: 1 to 57 of `A-Z a-z 0-9 - _`. Default `paykit`. |
| `clientId` | string | no | the client ID of the app's authentication request. Default `paykit-wasm`. |
| `autoReceive` | boolean | no | run the engine's own feed loop until `stop`. Default `true`. With `false`, the host calls `receive`, `resumePayments` and `signerTick` itself. |
| `onEvent` | `(event: EngineEvent) => void` | no | called with each event, in order. A callback that throws is ignored. |
| `homeserver` | `EngineHomeserver` | yes | see [Homeserver callbacks](#9-homeserver-callbacks) |
| `rail` | `EngineRail` | yes | see [Rail callbacks](#10-rail-callbacks) |

Unknown fields are refused (`bad_options`). The secret is wiped from the parsed copy.

`StartResult`: `{identity: string, appId: string, enrolment: "new" | "loaded"}`.

Errors: `bad_options`, `already_started`, `busy`, `store`, `identity_mismatch` (the stores
belong to another identity; nothing is sent), `signer`, `enrolment`, `paykit`.

### `stop(): Promise<void>`

Ends the feed loop (`feedStopped`), waits for the call in progress, closes the signer and the
app, and releases the stores. Errors: `not_started`, `busy`, `signer` (stopped all the same).

### `resetIdentity(options?: ResetOptions): Promise<void>`

For tests and for re-enrolling after a Dispatch database reset. It stops the engine if one
runs, destroys its two stores, and deletes the signer's `identity.json` and backup from the
identity's Homeserver. With no engine running, it destroys the stores of
`options.storePrefix` (default `paykit`) and deletes the signer's files through
`options.homeserver` when given. The identity's account at the Dispatch is left as it is.
Errors: `bad_options`, `busy`, `store`, `homeserver` (the stores are destroyed first; call it
again to finish).

### Other calls

| Call | Result |
| --- | --- |
| `identity(): string` | the started engine's Pubky ID (sync). `not_started`, `busy`. |
| `appId(): string` | the started engine's App ID (sync). `not_started`, `busy`. |
| `version(): string` | the crate's version (sync), callable before `start` |

## 5. Calls

All calls below need a started engine: otherwise they reject with `not_started` (or `busy`
while it starts or stops). `paykit` is a client or store failure.

### Contacts and admission

| Call | Result | Errors |
| --- | --- | --- |
| `allowConnect(identity)` | `Promise<void>`. Lets that identity start a conversation with this one: a reply grant at this identity's Dispatch, which also admits a pending connect request from it. A payer allows each payee it expects requests from; a contact pair allows each other. Emits `admissionChanged`. | `invalid_input`, `paykit` |
| `admits(identity)` | `Promise<boolean>`: whether that identity may start a conversation, from this app's copy of the allow and deny lists | `invalid_input`, `paykit` |
| `contacts()` | `Promise<Contacts>`: the allow list and deny list, read from the Dispatch. Emits `admissionChanged` for each change. | `paykit` |

### Payer: requests received

| Call | Result | Errors |
| --- | --- | --- |
| `requests()` | `Promise<OneTimeRequest[]>`: the one-time requests held as payer | `paykit` (also while the app's first listing is not done) |
| `payRequest(id: RequestId, options?: PayRequestOptions)` | `Promise<PaymentOutcome>`: the SDK writes the attempt, the host's `rail.pay` pays it, and the proof goes to the payee | see below |
| `rejectRequest(id: RequestId, reason?: string)` | `Promise<string>`: the rejection's message ID. The payee sees `reason`. | `invalid_input`, `refused` (`notFound`, `alreadyPaid`, `paymentInProgress`, ...), `conflict`, `paykit` |
| `payments()` | `Promise<PaymentEntry[]>`: the payer's journal, each with the payee's receipt once it came | `paykit` |
| `requestReceipts(id: RequestId)` | `Promise<Receipt[]>`: the receipts of one request | `invalid_input`, `paykit` |
| `resumePayments()` | `Promise<PaymentOutcome[]>`: finishes payments left open. The feed loop does it by itself after `start`, when a late `pay` answers, and at each turn while an outcome waits. Emits `paymentsResumed` when there are outcomes. | `rail`, `paykit` |

`PayRequestOptions` (all optional):

| Field | Meaning |
| --- | --- |
| `endpoint` | the Payment Endpoint Identifier to pay; otherwise the first usable one |
| `listApp` | for a request with destination `any` or `app`: the App ID of the payee app whose Private Payment List to pay (a `ReceivedList.appId`); otherwise any current one |
| `mode` | for a request with destination `any` or `app`: `"private"` (default) pays an endpoint of the payee's Private Payment Lists; `"public"` pays the payee's public endpoints. Pass `"public"` only when the user chose it. |

`payRequest` resolves with an outcome whenever money may have moved; it never rejects then.
`outcome`:

| `outcome` | Meaning | Host action |
| --- | --- | --- |
| `paid` | the method paid; the proof is sent or queued (`proofMessageId`) | show paid; the receipt follows (`receiptReceived`) |
| `pending` | the method has not answered within `rail.payLimitMs` | show "paying"; the engine finishes it when `pay` answers (`paymentsResumed`) |
| `failed` | definitely not paid (`reason`) | show the failure; the user may try again |
| `uncertain` | may be paid | ask the user; do not pay again blindly |
| `paidUnreported`, `paidOutcomeUnwritten`, `recordHeld`, `recordDivergent`, `resolved` | rarer states of a paid or held attempt | show as in progress; the engine resumes them |

`payRequest` rejects, with nothing paid, as `refused` with a `reason` such as `alreadyPaid`,
`paymentInProgress`, `beingHandledByOtherWallet`, `rejected`, `cancelled`, `deadlinePassed`,
`noDestination`, `unsupportedEndpoint`, `attemptFailed`, `identityUnavailable`, or, for
destination `any`, `waitingForList` (no usable list from the payee yet: the engine has asked
the payee for one; pay again after `listReceived`). Also `invalid_input`, `conflict`,
`paykit`.

### Payee: requests sent and payments received

| Call | Result | Errors |
| --- | --- | --- |
| `sendRequest(request: RequestInput)` | `Promise<SentRequest>`: signs, records and sends a Payment Request. The payee activity shows it from now on (`activityUpdated`, status `requested`). | `invalid_input`; `refused` (`scopeMissing`, `heldForUser`, `notNow`, ...); `paykit` (read `payeeActivity()` before sending again: it may have been kept) |
| `payeeActivity()` | `Promise<PayeeRequest[]>`: requests sent and payments received | `paykit` |
| `verifyPayment(record)` | `Promise<number \| null>`: checks the proof in that activity record with `rail.verify`; the verified payment time, or `null` when the method does not confirm it | `invalid_input`; `refused` (`notFound` for a record without a proof, `paymentTimeOutside`, `maybeCredited`, ...); `rail` (the method could not check now); `paykit` |
| `confirmPayment(record)` | `Promise<string \| null>`: verifies as `verifyPayment`, then sends the payer a receipt; the receipt's message ID (the same on a repeat), or `null` when not confirmed. Status becomes `receipted`. | as `verifyPayment`, and `refused` `divergent` |
| `cancelSentRequest(record, reason?)` | `Promise<string>`: the cancellation's message ID. A proof that crosses it still counts. | `invalid_input`, `refused`, `conflict`, `paykit` |

`RequestInput`:

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `to` | string | yes | the payer's Pubky ID. The payer must admit this identity (its `allowConnect`), or delivery waits (`waitingForAdmission`). |
| `amount` | `{value, asset}` | yes | such as `{value: "0.00012000", asset: "btc"}` |
| `paymentReference` | string | yes | 1 to 256 characters, no control characters. The payer sees it, and the receipt copies it: pass the memo. |
| `destination` | `RequestDestination` | yes | `{type: "endpoints", endpoints: [{identifier, payload}]}` (endpoints the host supplies, such as a fresh address), `{type: "any"}` (an endpoint of a Private Payment List this identity shared with the payer, or its public endpoints) or `{type: "app", appId}` |
| `proposalExpiresAtMs` | number | no | the last time the payer may act on it |
| `paymentDeadlineMs` | number | no | the payment's own deadline |

`SentRequest`: `{msgId, delivery, request: {payee, requestHash} | null}`. `delivery` is
`accepted` (in the payer's inbox), `queued` (in the outbox, tried again each round), `kept`
(signed and kept while the Dispatch did not answer; sent again, the same bytes),
`waitingForAdmission` (the payer does not admit this identity yet) or
`certificateNotCurrent`.

### Private Payment Lists

| Call | Result | Errors |
| --- | --- | --- |
| `setListEndpoints(endpoints: ListEndpoint[])` | `Promise<void>`: sets the reusable list this identity shares. Each contact it is shared with is sent the changed list, now or at a later round; an empty array withdraws the details from all of them. Emits `sharedListUpdated`. | `invalid_input` (an invalid identifier or payload, or an identifier twice); `refused` (`scopeMissing`, `notNow`); `paykit` |
| `publishListTo(identity)` | `Promise<void>`: shares the list with that contact: a signed Private Payment List goes to its inbox, now or at a later round. The contact must admit this identity. Emits `sharedListUpdated`. | `invalid_input`; `refused`: `usage` (before `setListEndpoints`, or for this identity itself), `heldForUser` (the deny list holds the contact), `notNow` (an earlier publication to it is unresolved; a later round sends it), `listDeliveryUnknown`; `paykit` |
| `withdrawListFrom(identity)` | `Promise<void>`: stops sharing with that contact; an empty list supersedes what it holds. Emits `sharedListUpdated`. | `invalid_input`; `refused` (`notNow`, `listDeliveryUnknown`); `paykit` |
| `sharedList()` | `Promise<SharedList>`: the list shared and its recipients | `paykit` |
| `receivedLists()` | `Promise<ReceivedList[]>`: the lists contacts shared with this identity, the latest from each of their apps | `paykit` |

`listDeliveryUnknown` means an earlier list to that contact was given up with an unknown
outcome. This API has no call to send it again; with the demo's local services it should not
happen.

### Feed and signer

| Call | Result | Errors |
| --- | --- | --- |
| `receive()` | `Promise<ReceiveResult>`: one receive round now, with its events. For `autoReceive: false`, or a read at once. | `paykit` |
| `signerTick()` | `Promise<{moreDue: boolean}>`: the signer's due work (certificate renewals, the key bundles the app asked for, deletions owed). The feed loop runs it once a minute, and at once when a read waits for scope keys. | `signer` |

## 6. Records

### `RequestId`

`{payee: string, requestHash: string}`: as `requests()` gives it in `OneTimeRequest.id`.

### `OneTimeRequest` (payer)

```ts
{
  id: { payee: string; requestHash: string };
  record: string;                 // hex
  amount: { value: string; asset: string };
  paymentReference: string;       // the payee's memo
  label: string | null;
  proposalExpiresAtMs: number | null;
  status: OneTimeRequestStatus;
  held: boolean;                  // held for the user
  manualOnly: boolean;
  divergent: boolean;
  receipt: Receipt | null;
  destination:
    | { type: "endpoints"; endpoints: { identifier: string; payload: string }[] }
    | { type: "app"; appId: string }
    | { type: "any" };
}
```

`status`: `open`, `beingPaidHere`, `beingHandledByOtherWallet`, `mayBePaid`, `paid`,
`rejected`, `cancelled`, `expired`, `held`, `unrecorded`, `blocked`, `divergent`.

### `Receipt` (payer)

```ts
{
  record: string;                 // hex
  payee: string;
  requestHash: string | null;
  attemptId: string;
  messageId: string;
  acceptedAtMs: number | null;
  paymentReference: string | null;
}
```

### `PaymentOutcome`

`{outcome: string, attemptId: string, proofMessageId?: string | null, reason?: string}`; see
[payRequest](#payer-requests-received).

### `PaymentEntry` (payer's journal)

```ts
{
  attemptId: string;
  payee: string;
  requestHash: string | null;
  amount: { value: string; asset: string };
  endpoint: string;               // the Payment Endpoint Identifier paid
  stage: "recorded" | "attempted" | "paying" | "railPaid" | "railFailed"
       | "acceptanceOwed" | "finished" | "abandoned";
  paidByMethod: boolean;
  recordHeld: boolean;
  paid: { value: string; asset: string } | null;
  receipt: Receipt | null;
}
```

### `PayeeRequest` (payee activity)

```ts
{
  record: string;                 // hex: the argument of verifyPayment, confirmPayment, cancelSentRequest
  payer: string;
  requestHash: string | null;     // null for a payment received without a request
  amount: { value: string; asset: string } | null;
  label: string | null;
  attemptId: string | null;
  status: "requested" | "accepted" | "rejected" | "cancelled" | "proofReceived" | "receipted";
  divergent: boolean;
  recurring: boolean;
}
```

### `Admission`, `Contacts`

```ts
Admission = { identity: string; admitted: boolean; kept: boolean; reply: boolean }
Contacts  = { allowed: Admission[]; denied: string[] }
```

`admitted`: the identity may start a conversation with this one. `kept` and `reply` are the two
reasons the Dispatch's allow list holds it; `allowConnect` gives `reply`.

### `ListEndpoint`, `ReceivedList`, `SharedList`

```ts
ListEndpoint = { identifier: string; payload: string }

ReceivedList = {
  payee: string;                  // the contact who shared it
  appId: string;                  // the contact's app: payRequest's listApp
  version: { generation: number; counter: number }; // greater for each newer list
  endpoints: ListEndpoint[];      // empty once the contact withdrew it
  acceptedAtMs: number | null;    // when this identity's inbox accepted it
  usableUntilMs: number;          // the earlier of its expiry and 30 days after acceptance
  usable: boolean;                // has endpoints and has not expired, as of the read
}

SharedList = {
  endpoints: ListEndpoint[] | null; // null before setListEndpoints
  recipients: string[];             // chosen with publishListTo and not withdrawn since
}
```

A payment checks a list again before it pays: a list shown as usable may still be refused, for
example when the contact removed the app that published it.

### `ReceiveResult`

```ts
{
  generation: number;
  sequence: number;
  more: boolean;
  stopped: boolean;
  scopeKeysAwaited: boolean;
  recordsUnlisted: string | null;
  accountGone: boolean;
  applied: string[];              // message IDs whose effects the round applied
}
```

## 7. Events

`onEvent(event)` gets `{type, atMs, ...fields}`, in order. After each receive round, and after
each call that changes state, the engine compares the requests, the payee activity and the
lists with what it saw before, and emits an event for each change. The comparison starts from
the state at `start`: read `requests()`, `payeeActivity()`, `receivedLists()` and
`sharedList()` once after `start`, then follow the events. Nothing held at start is told
again, and no event carries a key or a secret.

| `type` | Fields | When |
| --- | --- | --- |
| `received` | `ReceiveResult` fields | a receive round finished |
| `messageReceived` | `kind`, `msgId`, `sender`, `seq` | the Dispatch delivered a message, such as `paykit.payment_request` |
| `admissionChanged` | `identity`, `admitted`, `kept`, `reply` | a contact's admission changed |
| `requestReceived` | `request: OneTimeRequest` | a new request is held as payer |
| `requestUpdated` | `request`, `previousStatus` | a request's status changed |
| `receiptReceived` | `request`, `receipt: Receipt` | the payee's receipt for a request arrived |
| `proofReceived` | `activity: PayeeRequest` | a proof arrived for the payee to verify |
| `activityUpdated` | `activity`, `previousStatus` (`null` when new) | a sent request or received payment changed |
| `paymentsResumed` | `outcomes: PaymentOutcome[]` | payments left open were resumed (after a start, or a `pay` that answered after the limit) |
| `listReceived` | `list: ReceivedList`, `previousVersion` (`null` when new) | a contact shared, changed or withdrew its Private Payment List |
| `sharedListUpdated` | `list: SharedList` | the list this identity shares, or its recipients, changed |
| `error` | `during`, `code`, `message` | a step of the feed loop failed. It is told once while it keeps failing the same way, and the loop goes on. |
| `feedStopped` | `reason` | the feed loop ended (`stopped`) |

`error.during` is one of `receive`, `acknowledge`, `signer`, `flush`, `requests`,
`payeeActivity`, `receivedLists` or `resumePayments`. Show it in a log or a quiet banner, not
as a failure of the user's action.

## 8. Errors

A failed call rejects (or throws, for the sync calls) a JavaScript `Error`:

- `message`: `<code>: <text>`. The text never quotes an address, a payload or a reference.
- `code`: the code below.
- `reason`: for `refused` only, the refusal's name in lowerCamelCase.

| `code` | Meaning |
| --- | --- |
| `bad_options` | `start` or `resetIdentity` options are missing or malformed |
| `already_started` | `start` in a worker whose engine runs |
| `not_started` | no engine runs in this worker |
| `busy` | the engine is starting or stopping |
| `store` | an OPFS store failed or is held elsewhere (another tab) |
| `identity_mismatch` | the stores belong to another identity |
| `signer` | the in-process signer failed |
| `enrolment` | enrolment or the app's approval failed |
| `paykit` | a client, Dispatch or store failure |
| `invalid_input` | a malformed argument, or a value the SDK rejects as invalid |
| `homeserver` | a Homeserver read or write failed |
| `refused` | a Paykit rule refused the call; nothing was paid or sent. See `reason`. |
| `rail` | the host's payment method failed |
| `conflict` | the state changed meanwhile: read again and decide again |

Refusal reasons seen in practice include `alreadyPaid`, `notFound`, `rejected`, `cancelled`,
`paymentInProgress`, `waitingForList`, `usage`, `noDestination`, `notNow`, `heldForUser` and
`scopeMissing`. Show an unknown reason generically.

## 9. Homeserver callbacks

The same shape as paykit-ffi's `WalletHomeserver`:

```ts
interface EngineHomeserver {
  get(identityZ32: string, path: string): Promise<Uint8Array | null>;
  put(path: string, bytes: Uint8Array): Promise<void>;
  delete(path: string): Promise<void>;
}
```

- `get` reads any identity's file. Resolve `null` (or `undefined`) when the file is absent;
  reject on any other failure.
- `put` and `delete` write the engine's own identity's Homeserver, through the host's
  authenticated session for it. Resolve only once the write is durable. The signer writes
  `/pub/pubky-dispatch/v1/identity.json` and `/priv/pubky-dispatch/v1/signer-backup.json`, so
  the session needs `/pub/` and `/priv/` write access. The engine never asks to write another
  identity's files. A `delete` of an absent file should resolve.
- Paths are absolute (`/pub/...`, `/priv/...`).
- Each call has 30 s before it counts as timed out.

Backends:

| Backend | `get` | `put` / `delete` |
| --- | --- | --- |
| fake (`http://127.0.0.1:18590`) | `GET /{identityZ32}{path}`: `200` + bytes, or `404` | `PUT` / `DELETE /{ownZ32}{path}`: `204`. No auth. |
| staging (`@synonymdev/pubky` 0.18) | the public storage read of `pubky://{identityZ32}{path}` | the signed-in session's write and delete of `path` |

## 10. Rail callbacks

```ts
interface EngineRail {
  endpoints: { identifier: string; fractionDigits: number }[];
  payLimitMs?: number;                 // 1,000 to 3,600,000; default 60,000
  pay?(payment: RailPayment): Promise<RailOutcome>;
  payment?(attemptId: string): Promise<RailOutcome | null>;
  verify?(check: RailProofCheck): Promise<number | null>;
}

RailPayment    = { attemptId: string; endpointId: string; payload: string;
                   amount: { value: string; asset: string }; paymentReference: string | null }
RailOutcome    = { status: "paid"; proof: Record<string, unknown>; paidAtMs?: number }
               | { status: "failed" }
               | { status: "pending" }
RailProofCheck = { endpointId: string; payload: string | null;
                   amount: { value: string; asset: string }; attemptId: string;
                   proof: Record<string, unknown> }
```

The engine answers the synchronous parts of the method itself, from `endpoints`:

- it pays an endpoint of a declared identifier whose payload is a JSON object with a non-empty
  string `value`;
- the transfer is the amount asked, the method's fee on top;
- the settlement ID of a proof is the 32 bytes of its `txid` (64 lowercase hex digits).

Each callback is optional. Without `pay`, a payment is refused; without `payment` or `verify`,
that call fails as unavailable.

**`pay(payment)`** pays at least `amount` to `JSON.parse(payload).value`, fees on top.

- `attemptId` is the idempotency key. Keep a record per attempt ID. A repeat for an attempt
  already paid must return that payment, not pay again. The engine never calls `pay` twice at
  once for one attempt.
- Resolve `{status: "paid", proof: {txid}, paidAtMs}` once paid, or `{status: "failed"}` only
  when definitely nothing was sent. A `pay` that throws or rejects leaves the attempt open:
  the engine asks `payment` later.
- It may take as long as the method needs, such as a payment out of a shared pool that
  waits for the pool's next round. The engine waits `payLimitMs`; past it, `payRequest` resolves `pending`,
  the call runs on to its end (at most 4 x `payLimitMs`), and the engine finishes the payment
  when it answers.

**`payment(attemptId)`** answers from the host's record of the attempt: `{status: "pending"}`
while its payment runs, `{status: "paid", proof, paidAtMs}`, `{status: "failed"}`, or `null`
only when `pay` was never called with that ID. Keep the record outside the worker (on the page,
in `localStorage` or IndexedDB): after a worker restart, it is how the engine finishes an
interrupted payment. While the engine's own `pay` for an attempt runs, the engine answers
`pending` itself and does not call `payment`.

**`verify(check)`** returns the verified payment time in Unix ms (first seen), or `null`.

- Find the transaction of `proof.txid` (esplora `GET /api/tx/{txid}` or bitcoind RPC), sum
  all its outputs to the address, and accept when the sum is at least `amount`. Zero
  confirmations are accepted. Scan every output: a batched payment pays many outputs.
- `payload` is the endpoint's payload as the payee signed it, such as `{"value":"bcrt1p..."}`.
  It is **`null` for a request with destination `any` or `app`**: the payee signed no address
  for it. Check the payment against this identity's own endpoints for `endpointId`, such as
  the addresses it put in its Private Payment List (for a wallet that shares one stable
  address, that address).
- `payment` and `verify` each have 30 s.

The demo's regtest contract:

| Item | Value |
| --- | --- |
| Identifiers | `btc-regtest-p2tr` (`bcrt1p...`), `btc-regtest-p2wpkh` (`bcrt1q...`), `fractionDigits: 8` |
| Payload | `{"value":"<address>"}` |
| Amount | asset `btc`, decimal BTC with 8 fraction digits |
| Proof | `{"txid":"<64 lowercase hex>"}` |

## 11. Restart rules

- **What persists.** The app's and the signer's state live in two OPFS stores,
  `<storePrefix>-app` and `<storePrefix>-signer`, sealed under a key the origin holds. They are
  bound to the page's origin. The secret does not persist: pass it to every `start`.
- **Normal restart.** Call `stop()`, terminate the worker, and later `start` in a new worker
  with the same secret and `storePrefix`: it loads (`enrolment: "loaded"`).
- **Crash or reload.** A worker that ends without `stop` (a page reload, a terminated worker, a
  panic) leaves the stores as their last commit left them, and the browser releases their
  files once the worker is gone. The next `start` waits up to 10 s for that release, then loads.
  Nothing is enrolled or approved again, and the Dispatch hands over again what the last read
  did not finish.
- **Payments left open** are resumed after every start (`paymentsResumed`): an attempt whose
  `pay` was cut off is finished from `rail.payment`, which must answer from the host's own
  record of the attempt.
- **Events after a restart.** The comparison restarts from the state at the new `start`: read
  the state once (`requests()`, `payeeActivity()`, `receivedLists()`, `sharedList()`), then
  follow the events. Nothing already held is told again.
- **Same stores, another secret** fails with `identity_mismatch` before anything is sent.
- **A Dispatch database reset** invalidates the enrolment. Call `resetIdentity` (with the
  Homeserver, so `identity.json` and the signer's backup are deleted), then `start` again,
  which enrols the identity anew. Or use a new mnemonic.
- **One tab**: see [Threading rules](#1-threading-rules).

## 12. Recipes

**Show the identity.** `start` resolves `identity`; show it as `pubky<identity>` if the UI
uses the prefix.

**Add a contact.** Both sides call `allowConnect(otherId)`. `admissionChanged` confirms it, and
`contacts()` lists it.

**Request money (payee).** `sendRequest({to, amount, paymentReference, destination: {type:
"endpoints", endpoints: [{identifier: "btc-regtest-p2tr", payload: JSON.stringify({value:
receiveAddress})}]}})`. Then wait for `proofReceived` for that `requestHash`, call
`confirmPayment(activity.record)` (it verifies with `rail.verify` and sends the receipt), and
`activityUpdated` shows `receipted`. If `confirmPayment` resolves `null`, the payment is not
visible yet: try again after a short wait or the next block.

**Pay a request (payer).** On `requestReceived`, show `request.amount`,
`request.paymentReference` and `request.id.payee`. On approval, `payRequest(request.id)`.
`paid` means the proof is sent; `receiptReceived` follows when the payee confirms. `pending`
means the method is still paying: `paymentsResumed` and `requestUpdated` follow.

**Share payment details privately.** `setListEndpoints([{identifier: "btc-regtest-p2tr",
payload: JSON.stringify({value: receiveAddress})}])`, then `publishListTo(contactId)`. The
contact's engine emits `listReceived`.

**Request to any destination.** `sendRequest({..., destination: {type: "any"}})` after the
list was shared with the payer. The payer pays an address of that list. Verify with the
`payload: null` rule in [Rail callbacks](#10-rail-callbacks).

**Pay a request with destination `any`.** `request.destination.type === "any"`. Show the
contact's shared details from `receivedLists()` (the entry whose `payee` is
`request.id.payee`). `payRequest(request.id)` pays one of them; with several apps, pass
`{listApp: list.appId}`. `refused` `waitingForList` means no list arrived yet: the engine has
asked the payee for one, so pay again after `listReceived`.
