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

The `browser-release` module is about 7.5 MB.

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
  rail: { endpoints: [{ identifier, fractionDigits }], pay, payment, verify },
  onEvent: (event) => postMessage({ kind: "event", event }), // optional
  autoReceive: true,                         // optional: the engine's own feed loop
});
```

`paykit_wasm.d.ts` declares every type. In short:

| Call | Result |
| --- | --- |
| `start(options: StartOptions)` | `Promise<{identity, appId, enrolment: "new" \| "loaded"}>`; starts the feed loop unless `autoReceive: false` |
| `identity()`, `appId()` | the Pubky ID (z-base-32 without a prefix), the App ID (base64url) |
| `allowConnect(identity)` | `Promise<void>`: lets that identity start a conversation (a payer allows each payee it expects requests from) |
| `admits(identity)` | `Promise<boolean>`: whether it may, from this app's copy of the lists |
| `contacts()` | `Promise<{allowed: Admission[], denied: string[]}>`, read from the Dispatch |
| `requests()` | `Promise<OneTimeRequest[]>`: the requests held as payer, each with its `destination` |
| `payeeActivity()` | `Promise<PayeeRequest[]>`: requests sent and payments received as payee |
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

Errors reject with an `Error` whose message is `<code>: <text>`. The codes are `bad_options`,
`already_started`, `not_started`, `busy` (the engine is starting or stopping), `store`,
`identity_mismatch`, `signer`, `enrolment`, `paykit`, `invalid_input` and `homeserver`.

### Events

`start`'s optional `onEvent(event)` is called with plain objects `{type, atMs, ...}`, in order.
The feed loop runs a receive round whenever the Dispatch's feed has something (a long poll of up
to 25 s between rounds, paced so it never spins), and the signer's due work once a minute. After
each round the engine compares the requests and the payee activity with what it saw before:

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
| `error` | `during`, `code`, `message` | a step of the loop failed (told once while it keeps failing); the loop goes on |
| `feedStopped` | `reason` | the feed loop ended |

The comparison starts from the state at `start`: read `requests()` and `payeeActivity()` once
after `start`, then follow the events. A request already held at start is not told again.

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

- `pay(payment) -> Promise<RailOutcome>`, limited to 60 s, after which the attempt stays open.
- `payment(attemptId) -> Promise<RailOutcome | null>`.
- `verify(check) -> Promise<number | null>`: the verified payment time in Unix ms.

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
  sleeps. Later scenarios import it.
- `smoke.mjs`: the scenarios `start` (enrol, reload, refuse another identity on the same
  stores) and `peer` (allow both ways with a native peer, a request from the peer received as
  typed events, a worker terminated without `stop` and restarted on the same stores, the feed
  going on after it, `signerTick`, `stop`, `resetIdentity`).

Playwright comes from `paykit-browser/rig`:

```sh
(cd paykit-browser/rig && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm ci)
node paykit-wasm/www/smoke.mjs --pkg <pkg folder> [--scenario start|peer|all]
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

Each run enrols new random identities on the Dispatch, which keeps them. The `peer` scenario
needs bitcoind for the peer's receive addresses; the harness unloads the peer's bitcoind wallet
at the end.
