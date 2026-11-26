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

The `browser-release` module is about 5.2 MB.

## Use

In a dedicated module worker:

```js
import init, { start, identity, stop } from "./pkg/paykit_wasm.js";
await init({ module_or_path: new URL("./pkg/paykit_wasm_bg.wasm", import.meta.url) });

const { identity, appId, enrolment } = await start({
  identitySecretHex,                         // 64 hex digits
  dispatchOrigin: "https://localhost:18580", // the origin requests are signed for
  dispatchListener: "http://127.0.0.1:18580",// optional: a plain HTTP loopback listener
  storePrefix: "paykit",                     // optional: stores <prefix>-app, <prefix>-signer
  clientId: "paykit-wasm",                   // optional
  homeserver: { get, put, delete },          // see below
  rail: { endpoints: [{ identifier, fractionDigits }], pay, payment, verify },
});
```

`paykit_wasm.d.ts` declares every type. In short:

| Call | Result |
| --- | --- |
| `start(options: StartOptions)` | `Promise<{identity, appId, enrolment: "new" \| "loaded"}>` |
| `identity()` | the Pubky ID, z-base-32 without a prefix |
| `stop()` | `Promise<void>`: closes the signer and the app and releases the stores |
| `version()` | the crate's version |

- **First start** (no engine record in the app's store): the signer is set up and enrols the
  identity at the Dispatch, which publishes `/pub/pubky-dispatch/v1/identity.json` and the
  signer's backup `/priv/pubky-dispatch/v1/signer-backup.json` through the host's Homeserver.
  The signer approves a new app for the pay and request scopes, and the app joins.
  `enrolment` is `"new"`.
- **Later starts**: the signer and the app reload from their stores. `enrolment` is `"loaded"`.
- A start with another identity's secret on the same stores fails with `identity_mismatch`
  before anything is sent.

Errors reject with an `Error` whose message is `<code>: <text>`. The codes are `bad_options`,
`already_started`, `not_started`, `busy`, `store`, `identity_mismatch`, `signer`, `enrolment`
and `paykit`.

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

## Smoke test

`www/smoke.mjs` starts the engine in headless Chrome, in dedicated workers, with the Origin Private
File System on disk. It runs against a live Dispatch and a Homeserver with the fake Homeserver's
API (`GET`/`PUT`/`DELETE /{identityZ32}{path}`). Playwright comes from `paykit-browser/rig`
(`npm ci` there).

```sh
node paykit-wasm/www/smoke.mjs --pkg <pkg folder>
```

The script reads these environment variables:

- `PAYKIT_WASM_DISPATCH_ORIGIN` (default `https://localhost:18580`)
- `PAYKIT_WASM_DISPATCH_LISTENER` (default `http://127.0.0.1:18580`)
- `PAYKIT_WASM_HOMESERVER` (default `http://127.0.0.1:18590`)
- `PAYKIT_WASM_PROFILE_ROOT` (default: the OS temporary folder)
- `PAYKIT_WASM_BROWSER` (`chrome` or `chromium`; default `chrome`)

Each run enrols a new random identity on the Dispatch.
