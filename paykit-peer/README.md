# paykit-peer

A scripted Paykit v0.2 wallet for local tests. One process plays one wallet identity:

- its signer runs in-process (`pubky-dispatch-signer`), seeded from a random 32-byte identity
  secret;
- it talks to a local Pubky Dispatch over plain HTTP on loopback: requests are signed for the
  Dispatch's https origin (`https://localhost:18580`) and sent to its plain listener
  (`http://127.0.0.1:18580`);
- it reads and writes Homeserver files on the local fake Homeserver (`http://127.0.0.1:18590`,
  `GET|PUT|DELETE /{identityZ32}/{path}`);
- it pays and is paid on-chain on a local bitcoind in regtest (`http://127.0.0.1:18443`),
  from a bitcoind wallet of its own.

**Test tool only.** The identity secret and the app's keys are kept in plain text in the state
directory, and every payment is made without a user. It refuses any Dispatch, Homeserver or
bitcoind address that isn't `127.0.0.1` or `localhost`. It is native only: on wasm32 it is an
empty `main`, and all its dependencies sit in a non-wasm32 target table.

## Build

```sh
cargo build --locked -p paykit-peer
```

## Payment method

| Item | Value |
|---|---|
| Receive identifier | `btc-regtest-p2wpkh`, a new `getnewaddress "" bech32` address of the peer's wallet |
| Identifiers it pays | `btc-regtest-p2wpkh`, `btc-regtest-p2tr` |
| Payload | `{"value":"<address>"}` |
| Amount | asset `btc`, decimal BTC with 8 fraction digits |
| Proof | `{"txid":"<64 lowercase hex>"}` |
| Settlement ID | the 32 txid bytes |
| Verify | find the transaction in bitcoind, sum its outputs to the payload's address, accept at or above the amount; unconfirmed is accepted |
| Fee | `sendtoaddress` at a fixed 2 sat/vB, on top of the amount |

## Command line

```text
paykit-peer [--state <dir>] [--dispatch-url http://127.0.0.1:18580] [--origin https://localhost:18580]
            [--homeserver http://127.0.0.1:18590] [--bitcoind-url http://127.0.0.1:18443]
            [--rpc-user admin1] [--rpc-password 123] [--wallet <name>] [--funder-wallet default]
            <command>
```

| Command | What it does |
|---|---|
| `init [--fund <sats>]` | Create the identity, enrol it at the Dispatch, approve a Paykit app for the pay and request scopes, join it; with `--fund`, fund the peer's wallet |
| `id` | Print the Pubky ID (z-base-32, no prefix) alone on one line, from the state directory |
| `fund <sats>` | Fund the peer's bitcoind wallet up to at least `<sats>` from `--funder-wallet` (mining 101 blocks to the funder when it is short), then mine one block |
| `allow <id>` | Allow identity `<id>` to start a conversation (admission, `allow_connect`) |
| `request <id> <sats> [--any] [--reference <text>]` | Request `<sats>` from `<id>`: to a new P2WPKH address, or with `--any` to any of this identity's Private Payment Lists (`RequestDestination::Any`) |
| `publish-list <id>` | Set this app's Private Payment List to a new P2WPKH address and publish it to `<id>` |
| `run [--auto-pay] [--auto-confirm] [--fund <sats>] [--poll-ms 1000]` | Receive in a loop, print events, and take commands on stdin |

`<id>` is a Pubky ID, with or without a `pubky` or `pk:` prefix.

Every command except `run` needs `--state`. Each opens the state directory, does its work and
exits. A directory is locked while a process uses it: while a peer is running, send it commands
on its stdin instead.

`run` reads one command per line on stdin: `allow <id>`, `request <id> <sats> [--any]`,
`publish-list <id>`, `fund <sats>`, `id`, and `quit`. It stops at `quit` or when stdin closes.
Each round it reads the inbox, sends what is queued, then:

- with `--auto-pay`, pays every open one-time request it holds (one try per request; a
  refusal before any attempt, such as no destination known yet, is tried again next round);
- with `--auto-confirm`, verifies every proof it receives on-chain and, when it verifies,
  confirms the payment, which sends the receipt.

Without `--state` the peer lives in memory: `run` then makes a new identity each start.

The bitcoind wallet defaults to `paykit-peer-<first 12 characters of the Pubky ID>`, created at
`init` and loaded at every start. A node restart unloads it; the peer loads it again.

### Examples

```sh
peer() { paykit-peer --state "$DIR/$1" "${@:2}"; }

peer a init --fund 500000
peer b init --fund 500000
A=$(peer a id); B=$(peer b id)

# One-shot commands, while no peer runs on the directory.
peer a allow "$B"
peer b allow "$A"
peer a request "$B" 10000

# Run B so it pays, and A so it confirms; commands go on stdin.
mkfifo b.in
peer b run --auto-pay --auto-confirm < b.in > b.out &
exec 3> b.in
echo "request $A 20000" >&3
echo "publish-list $A" >&3
echo quit >&3
```

## Events

`stdout` carries only events, one JSON object per line. Each has `event` and `at_ms` (Unix ms).
Logs go to `stderr`. No event carries a secret. In `run`, each event is printed once per process
for each state it reports, so a restarted peer prints the current state of earlier requests
again.

| Event | Fields | When |
|---|---|---|
| `ready` | `identity`, `app_id` (hex), `wallet`, `origin`, `state` (`dir`/`memory`), `enrolled` (`new`/`loaded`) | the peer is open |
| `running` | `auto_pay`, `auto_confirm`, `poll_ms` | `run` started |
| `funded` | `wallet`, `txid` (`null` when it held enough), `balance_sats` | after `fund` / `--fund` |
| `id` | `identity` | `id` on stdin |
| `allowed` | `id` | after `allow` |
| `request_sent` | `to`, `msg_id`, `request_hash`, `sats`, `reference`, `destination` (`endpoints`/`any`), `endpoint_id`, `address` (`null` for `any`), `delivery` | after `request` |
| `list_published` | `to`, `endpoint_id`, `address` | after `publish-list` |
| `request_received` | `from` (the payee), `request_hash`, `sats`, `amount`, `asset`, `reference`, `status`, `recurring`, `manual_only` | payer: a request is held |
| `request_status` | `request_hash`, `status` (`Open`, `Paid`, ...) | payer: each status a request reaches |
| `paid` | `payee`, `request_hash`, `attempt_id`, `proof_msg_id`, `txid`, `address`, `endpoint_id`, `sats` | payer: the rail paid and the proof was sent |
| `pay_outcome` | `payee`, `request_hash`, `outcome`, `attempt_id` | payer: an outcome other than paid (pending, uncertain, ...) |
| `pay_failed` | `payee`, `request_hash`, `error` | payer: the payment was refused; tried again next round |
| `receipt_received` | `payee`, `request_hash`, `attempt_id`, `msg_id`, `txid`, `sats`, `stage` | payer: the payee's receipt is held |
| `payee_status` | `payer`, `record`, `request_hash`, `status` (`Requested`, `ProofReceived`, `Receipted`, ...) | payee: each status a sale reaches |
| `proof_received` | `payer`, `record`, `request_hash`, `attempt_id`, `sats` | payee: a proof arrived |
| `verified` | `payer`, `record`, `request_hash`, `attempt_id`, `txid`, `sats_received`, `paid_at_ms` | payee: the proof verified on-chain |
| `verify_failed` | `record`, `attempt_id`, `txid`, `sats_received` | payee: the proof did not verify (not retried) |
| `confirmed` | `payer`, `record`, `request_hash`, `attempt_id`, `txid`, `receipt_msg_id` | payee: the payment is credited and the receipt sent |
| `app_event` | `kind` | an event of the Dispatch app's receive |
| `command_ok` / `command_failed` | `command`, `line`, `error` | a stdin command finished |
| `warning` | `during`, `error`, ... | something to retry |
| `stopping` | `why` | `run` ends |
| `fatal` | `error` | the peer stops with exit code 1 |

## State directory

```text
<dir>/lock         held by the running process
<dir>/peer.json    identity secret (hex, PLAIN TEXT), Pubky ID, App ID, origin, bitcoind wallet
<dir>/signer.json  the signer's store
<dir>/app.json     the Paykit app's store
<dir>/rail.json    the rail's history: attempt ID -> txid
```

Files are mode 0600 in a 0700 directory. A Dispatch database reset invalidates the identity:
start again with a new directory.

## Smoke test

`paykit-peer/smoke.sh` runs two peers with fresh identities against the local services. They
allow each other; A requests 10,000 sats from B, B pays on-chain, A verifies and confirms, B gets
the receipt; then the reverse; then A publishes a Private Payment List to B and requests with
`--any`, and B pays the list's address. Every step is asserted from the event lines, every txid
is checked in bitcoind, and a mined block must confirm each one. It prints
`SMOKE PASSED: ...` and exits 0. See the script's header for its environment variables.
