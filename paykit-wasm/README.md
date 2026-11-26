# paykit-wasm

A Paykit engine for a browser wallet: the Paykit SDK and the identity's Pubky Dispatch signer,
compiled to WebAssembly, with a JavaScript API. It runs in a **dedicated worker**, one engine per
worker, and keeps its state in the Origin Private File System.

On every target but `wasm32-unknown-unknown` the crate is empty.

> **Demo shortcut.** The host passes the identity's secret key to `start`, and the signer runs in
> the same worker as the app. Pubky's direction keeps identity keys out of apps. The engine accepts
> the key because the host, a wallet that derives it from its own seed, holds it already.

## Build

```sh
export CARGO_TARGET_DIR=<target dir>
export PATH=<folder with wasm-bindgen 0.2.130>:$PATH
paykit-wasm/build.sh --out-dir <pkg folder>      # profile browser-release by default
```

The script builds `-p paykit-wasm` for `wasm32-unknown-unknown` with `--locked`, then runs
`wasm-bindgen --target web`. The output folder holds `paykit_wasm.js`, `paykit_wasm_bg.wasm`,
`paykit_wasm.d.ts` and `paykit_wasm_bg.wasm.d.ts`. Build with `-p paykit-wasm`, never
`--workspace`, for wasm32.

The `browser-release` module is about 8.2 MB.

## Use

In a dedicated module worker:

```js
import init, { start, identity, requests, stop } from "./pkg/paykit_wasm.js";
await init({ module_or_path: new URL("./pkg/paykit_wasm_bg.wasm", import.meta.url) });

const { identity, appId, enrolment } = await start({
  identitySecretHex,                         // 64 hex digits
  dispatchOrigin: "https://localhost:18580", // the origin requests are signed for
  dispatchListener: "http://127.0.0.1:18580",// optional: a plain HTTP loopback listener
  storePrefix: "paykit",                     // optional: stores <prefix>-app, <prefix>-signer
  clientId: "paykit-wasm",                   // optional
  homeserver: { get, put, delete },          // see below
  rail: { endpoints: [{ identifier, fractionDigits }], pay, payment, verify,
          payLimitMs: 60000 },               // optional: how long a payment waits for pay
  onEvent: (event) => postMessage({ kind: "event", event }), // optional
  autoReceive: true,                         // optional: the engine's own feed loop
});
```

`paykit_wasm.d.ts` declares every type, and `API.md` is the full reference for a host: every
call, record and event, the callback contracts, threading and restart rules, and a usage
example. In short:

| Call | Result |
| --- | --- |
| `start(options: StartOptions)` | `Promise<{identity, appId, enrolment: "new" \| "loaded"}>`; starts the feed loop unless `autoReceive: false` |
| `identity()`, `appId()` | the Pubky ID (z-base-32 without a prefix), the App ID (base64url) |
| `allowConnect(identity)` | `Promise<void>`: lets that identity start a conversation (a payer allows each payee it expects requests from) |
| `admits(identity)` | `Promise<boolean>`: whether it may, from this app's copy of the lists |
| `contacts()` | `Promise<{allowed: Admission[], denied: string[]}>`, read from the Dispatch |
| `requests()` | `Promise<OneTimeRequest[]>`: the requests held as payer, each with its `destination` |
| `payeeActivity()` | `Promise<PayeeRequest[]>`: requests sent and payments received as payee |
| `payments()` | `Promise<PaymentEntry[]>`: the payer's payments, each with its `stage` and, once it came, the payee's `receipt` |
| `requestReceipts(id)` | `Promise<Receipt[]>`: the payee's receipts of one request |
| `sendRequest(request: RequestInput)` | `Promise<SentRequest>`: payee: sign, record and send a Payment Request `{to, amount: {value, asset}, paymentReference, destination, proposalExpiresAtMs?, paymentDeadlineMs?}` |
| `verifyPayment(record)` | `Promise<number \| null>`: payee: check the proof in an activity record with `rail.verify`; the paid time, or `null` |
| `confirmPayment(record)` | `Promise<string \| null>`: payee: verify, then send the payer a receipt; its message ID (the same on a repeat), or `null` when not confirmed |
| `cancelSentRequest(record, reason?)` | `Promise<string>`: payee: cancel a request sent |
| `payRequest(id, options?)` | `Promise<PaymentOutcome>`: payer: pay a request through `rail.pay` and send the proof; `{outcome: "paid" \| "pending" \| ..., attemptId, proofMessageId?}` |
| `rejectRequest(id, reason?)` | `Promise<string>`: payer: reject a request |
| `resumePayments()` | `Promise<PaymentOutcome[]>`: finish the payments left open (the feed loop does it by itself) |
| `setListEndpoints(endpoints)` | `Promise<void>`: set the reusable Private Payment List this identity shares, `[{identifier, payload}]` |
| `publishListTo(identity)` | `Promise<void>`: share it with a contact |
| `withdrawListFrom(identity)` | `Promise<void>`: stop sharing it with a contact |
| `sharedList()` | `Promise<{endpoints, recipients}>`: the list shared and with whom |
| `receivedLists()` | `Promise<ReceivedList[]>`: the lists contacts shared with this identity |
| `receive()` | `Promise<ReceiveResult>`: one receive round now, with its events |
| `signerTick()` | `Promise<{moreDue}>`: the signer's due work (renewals, key bundles) |
| `stop()` | `Promise<void>`: ends the feed loop, closes the signer and the app, releases the stores |
| `resetIdentity(options?)` | `Promise<void>`, for tests: stops the engine, destroys its stores, deletes the signer's files on the Homeserver |
| `version()` | the crate's version |

The names follow paykit-ffi's wallet calls in camelCase, and the records follow the FFI's
(`OneTimeRequest`, `PayeeRequest`, `Receipt`) with keys as text: Pubky IDs in z-base-32, hashes
and record keys in lowercase hex, statuses as the FFI case in lowerCamelCase (`open`, `paid`,
`proofReceived`, ...). Calls wait their turn: a call made while another runs, the feed loop's
included, runs after it.

- **First start** (no engine record in the app's store): the signer is set up and enrols the
  identity at the Dispatch, which publishes `/pub/pubky-dispatch/v1/identity.json` and the
  signer's backup `/priv/pubky-dispatch/v1/signer-backup.json` through the host's Homeserver.
  The signer approves a new app for the pay and request scopes, and the app joins.
  `enrolment` is `"new"`.
- **Later starts**: the signer and the app reload from their stores. `enrolment` is `"loaded"`.
- A start with another identity's secret on the same stores fails with `identity_mismatch`
  before anything is sent.

- **Restart.** A worker that ends without `stop` (terminated, or aborted by a panic) leaves the
  stores as their last commit left them. A new worker's `start` with the same secret and
  `storePrefix` loads, waiting up to 10 s for the browser to release the old worker's files.
- `resetIdentity` leaves the identity's account at the Dispatch. Use it before enrolling the
  same identity again after a Dispatch database reset.

A request's ID is `{payee, requestHash}` as `requests()` gives it; a payee's `record` is the
`record` of a `payeeActivity()` entry. A request's `destination` is
`{type: "endpoints", endpoints: [{identifier, payload}]}` (endpoints the host made for it, such
as a fresh address: `payload` `{"value":"<address>"}`), `{type: "any"}` (any of the payee's
current Private Payment Lists or public endpoints) or `{type: "app", appId}`.

Errors reject with an `Error` whose message is `<code>: <text>` and whose `code` property is the
code: `bad_options`, `already_started`, `not_started`, `busy` (the engine is starting or
stopping), `store`, `identity_mismatch`, `signer`, `enrolment`, `paykit`, `invalid_input`,
`homeserver`, `refused`, `rail` and `conflict`. `refused` means a rule of Paykit refused the
call, with nothing paid or sent; its `reason` property names the refusal (`alreadyPaid`,
`paymentInProgress`, `rejected`, `cancelled`, `notFound`, `noDestination`, ...). `rail` means
the host's method failed; `conflict` means the state changed meanwhile: read again and decide
again. A payment that may have moved money is never an error: `payRequest` resolves `pending`
or `uncertain`.

### Payments

The payer's side, as a wallet runs it:

1. A request arrives: `requestReceived`, status `open`.
2. `payRequest(request.id)`: the SDK writes the attempt, then calls `rail.pay` with the attempt
   ID as the idempotency key. When `pay` answers paid, the proof goes to the payee and
   `payRequest` resolves `paid` (`requestUpdated`, status `paid`).
3. The payee confirms: `receiptReceived` with the receipt; `requests()` and `payments()` show it.

The payee's side:

1. `sendRequest(...)`, to an address the host made for it: `activityUpdated`, status
   `requested`.
2. The payer pays: `proofReceived` (status `proofReceived`).
3. `confirmPayment(activity.record)` checks the proof with `rail.verify` and sends the receipt
   (status `receipted`). `verifyPayment` alone checks without crediting.

**Slow payments.** `pay` may take as long as the method needs, such as a payment out of a shared
pool that waits for the pool's next round. `payRequest` waits up to `rail.payLimitMs` (default a
minute). Past it, `pay` goes on by itself and `payRequest` resolves `pending`: the request shows
`beingPaidHere`. While `pay` runs, the engine never hands the attempt to the host again and does
not ask `rail.payment`. When `pay` answers, the engine finishes the payment by itself
(`paymentsResumed`), sends the proof, and the receipt follows.

**Restarts.** A start resumes the payments an earlier worker left open (`paymentsResumed`): an
attempt whose `pay` was cut off by a terminated worker is finished from `rail.payment`, which
must then answer from the host's own record of the attempt (`pending` while its payment runs,
then `paid` with the proof). The feed loop resumes again at each of its turns while an outcome
still waits on the method.

### Private Payment Lists

A Private Payment List is reusable payment details one identity shares with chosen contacts,
signed and sent to each contact's inbox. A request with destination `{type: "any"}` is paid to
one of them, so the payee sends no address with the request.

- `setListEndpoints([{identifier: "btc-regtest-p2tr", payload: "{\"value\":\"<address>\"}"}])`
  sets the list. Each contact it is shared with is sent the changed list; an empty list
  withdraws the details from all of them.
- `publishListTo(contact)` shares it (the contact must admit this identity);
  `withdrawListFrom(contact)` stops. `sharedList()` reads `{endpoints, recipients}`, and
  `sharedListUpdated` tells each change.
- A list a contact shared arrives as `listReceived` and is read with `receivedLists()`:
  `{payee, appId, version, endpoints, acceptedAtMs, usableUntilMs, usable}`, the latest from
  each of the contact's apps. A withdrawn list has no endpoints.
- Paying a request with destination `any`: `payRequest(id)` pays an endpoint of the payee's
  lists (`options.listApp` picks the app, `options.endpoint` the identifier). With no list yet
  it is `refused` `waitingForList`, and the engine asks the payee for one.
- Being paid on one: the proof's `rail.verify` check has `payload: null`, as the payee signed
  no address for the request. The method checks the payment against its own endpoints for
  `endpointId`, such as the addresses it put in its list.

### Events

`start`'s optional `onEvent(event)` is called with plain objects `{type, atMs, ...}`, in order.
The feed loop runs a receive round whenever the Dispatch's feed has something (a long poll of up
to 25 s between rounds, paced so it never spins), and the signer's due work once a minute. After
each round, and after each call that changes them, the engine compares the requests and the
payee activity with what it saw before:

| `type` | Fields | When |
| --- | --- | --- |
| `received` | `generation`, `sequence`, `applied`, `more`, `stopped`, `scopeKeysAwaited`, `recordsUnlisted`, `accountGone` | a receive round finished |
| `messageReceived` | `kind`, `msgId`, `sender`, `seq` | the Dispatch delivered a message |
| `admissionChanged` | `identity`, `admitted`, `kept`, `reply` | a contact's admission changed (`allowConnect`, `contacts`) |
| `requestReceived` | `request` | a new request is held as payer |
| `requestUpdated` | `request`, `previousStatus` | a request's status changed |
| `receiptReceived` | `request`, `receipt` | the payee's receipt for a request arrived |
| `proofReceived` | `activity` | a proof arrived for the payee to verify |
| `activityUpdated` | `activity`, `previousStatus` | a sent request or received payment changed |
| `paymentsResumed` | `outcomes` (`PaymentOutcome[]`) | the engine resumed payments left open (after a start, or a `pay` that answered after the limit) |
| `listReceived` | `list` (`ReceivedList`), `previousVersion` | a contact shared, changed or withdrew its Private Payment List |
| `sharedListUpdated` | `list` (`SharedList`) | the list this identity shares, or its recipients, changed |
| `error` | `during`, `code`, `message` | a step of the loop failed (told once while it keeps failing); the loop goes on |
| `feedStopped` | `reason` | the feed loop ended |

The comparison starts from the state at `start`: read `requests()`, `payeeActivity()`,
`receivedLists()` and `sharedList()` once after `start`, then follow the events. Anything
already held at start is not told again.

### Homeserver callbacks

The same shape as paykit-ffi's `WalletHomeserver`:

- `get(identityZ32, path) -> Promise<Uint8Array | null>`: any identity's file; `null` when absent.
- `put(path, bytes) -> Promise<void>` and `delete(path) -> Promise<void>`: the engine's own
  identity, resolving once the write is durable. The session needs `/pub/` and `/priv/` access.

Paths are absolute (`/pub/...`). Each callback has 30 s before the call counts as timed out.

### Rail callbacks

The engine answers the synchronous parts of the payment method itself, from `rail.endpoints`. It
pays an endpoint of a declared identifier whose payload is a JSON object with a non-empty string
`value`. The transfer is the amount asked, and the settlement ID is the 32 bytes of the proof's
`txid` (64 lowercase hex digits). The rest calls the host:

- `pay(payment) -> Promise<RailOutcome>`: `payment` is `{attemptId, endpointId, payload,
  amount: {value, asset}, paymentReference}`. It may take long (see Payments); the engine waits
  `rail.payLimitMs` (default 60 s, 1 s to 1 h), and the call runs on to its end after that.
  The engine never calls it twice at once for one attempt. A repeat with an attempt ID already
  paid must return that payment, not pay again.
- `payment(attemptId) -> Promise<RailOutcome | null>`: the host's record of an attempt:
  `pending` while its payment runs, `null` only when `pay` was never called with it.
- `verify(check) -> Promise<number | null>`: the verified payment time in Unix ms. `check` is
  `{endpointId, payload, amount, attemptId, proof}`; `payload` is `null` for a request with
  destination `any` or `app` (see Private Payment Lists).

`RailOutcome` is `{status: "paid", proof, paidAtMs?}`, `{status: "failed"}` or
`{status: "pending"}`. Each callback is optional. Without `pay` a payment is refused, and without
`payment` or `verify` that call fails as unavailable.

## Smoke harness

`www/` holds a reusable harness and its scenarios:

- `harness.mjs`: a static server on 127.0.0.1 for the page, the worker and the engine module; a
  persistent headless Chrome profile (the installed Chrome, through Playwright), so the Origin
  Private File System is on disk; engines in dedicated workers (`worker.js`) whose Homeserver
  callbacks are `fetch` calls to a Homeserver with the fake Homeserver's API
  (`GET`/`PUT`/`DELETE /{identityZ32}{path}`); every engine event streamed to Node; native peers
  (`paykit-peer ... run`) driven over stdin and stdout. `waitFor(match)` on an engine or a peer
  resolves on the first matching event, seen or to come, so scenarios wait on events, never on
  sleeps. `RegtestRail` is a test payment method in Node on bitcoind regtest, with a bitcoind
  wallet of its own: `pay` sends with `sendtoaddress`, `verify` reads the transaction index, and
  `behaviour` makes a payment slow (`delayMs` before sending, `holdMs` after). Workers reach it
  over the page server (start option `railUrl`, made by `harness.openRail(name)`). Later
  scenarios import it.
- `smoke.mjs`: the scenarios `start` (enrol, reload, refuse another identity on the same
  stores) and `peer` (allow both ways with a native peer, a request from the peer received as
  typed events, a worker terminated without `stop` and restarted on the same stores, the feed
  going on after it, `signerTick`, `stop`, `resetIdentity`), and `flow` (on regtest with a
  native peer that pays and confirms by itself: the browser engine pays a request with a 20 s
  method and gets the receipt; it requests to a fresh address, verifies the peer's payment
  on-chain and confirms it; a worker terminated mid-payment restarts, resumes the payment and
  gets the receipt; a payment past a 3 s pay limit resolves `pending` and is finished when the
  method answers; a rejection and a cancellation; every transaction checked in bitcoind), and
  `list` (Private Payment Lists both ways with a native peer, requests with destination `any`
  both ways paid to the list addresses, a restart that keeps both lists, a withdrawal).
  `RegtestRail.listAddress()` makes an address for the engine's list, which `verify` checks
  when a proof has no payload.

Playwright comes from `paykit-browser/rig`:

```sh
(cd paykit-browser/rig && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm ci)
node paykit-wasm/www/smoke.mjs --pkg <pkg folder> [--scenario start|peer|flow|list|all]
```

The script reads these environment variables:

- `PAYKIT_WASM_DISPATCH_ORIGIN` (default `https://localhost:18580`)
- `PAYKIT_WASM_DISPATCH_LISTENER` (default `http://127.0.0.1:18580`)
- `PAYKIT_WASM_HOMESERVER` (default `http://127.0.0.1:18590`)
- `PAYKIT_WASM_PROFILE_ROOT` (default: the OS temporary folder)
- `PAYKIT_WASM_BROWSER` (`chrome` or `chromium`; default `chrome`)
- `PAYKIT_WASM_PEER` (the `paykit-peer` binary; default `paykit-peer` on `PATH`)
- `PAYKIT_WASM_RUN_ROOT` (where peers keep their state folders; default: the OS temporary folder)
- `PAYKIT_WASM_BITCOIND`, `PAYKIT_WASM_RPC_USER`, `PAYKIT_WASM_RPC_PASSWORD` (the peers'
  bitcoind; default `http://127.0.0.1:18443`, `admin1`, `123`)

Each run enrols new random identities on the Dispatch, which keeps them. The `peer`, `flow` and
`list` scenarios need bitcoind; `flow` and `list` fund its wallets from bitcoind's `default` wallet and mines a
block. The harness unloads the peers' and the rails' bitcoind wallets at the end.
