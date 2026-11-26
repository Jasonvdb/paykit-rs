// A reusable headless-Chrome harness for the engine: a static server on 127.0.0.1 for the
// page, the worker and the engine module; a persistent browser profile, so the Origin Private
// File System is on disk; engines in dedicated workers, with Homeserver callbacks on fetch
// against a Homeserver with the fake Homeserver's API; every engine event streamed to Node;
// native peers (`paykit-peer`) driven over their stdin and stdout; and test payment methods
// (`RegtestRail`) that pay and verify on bitcoind regtest from Node, which the workers reach
// over the page server.
//
// Waits are on events, never on sleeps: `engine.waitFor(match)` and `peer.waitFor(match)`
// resolve with the first event, seen or still to come, that `match` accepts, and fail after a
// timeout with the events seen so far in the message.
//
//   import { launch } from "./harness.mjs";
//   const harness = await launch({ pkg });
//   const w = await harness.openEngine("w");
//   const started = await w.must("start", harness.startOptions(secret));
//   const peer = await harness.startPeer({ stateDir });
//   ...
//   await harness.close();
//
// Playwright comes from paykit-browser's rig (`npm ci` there, with
// PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 when the installed Chrome is used). No secret is printed.

import { spawn } from "node:child_process";
import { createPrivateKey, createPublicKey, randomBytes } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import http from "node:http";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, extname, join, normalize, resolve, sep } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(here, "../../paykit-browser/rig/package.json"));

/** The settings every scenario shares, from the environment (defaults in brackets). */
export const settings = {
  /** The Dispatch origin requests are signed for [https://localhost:18580]. */
  origin: process.env.PAYKIT_WASM_DISPATCH_ORIGIN ?? "https://localhost:18580",
  /** Its plain HTTP loopback listener [http://127.0.0.1:18580]. */
  listener: process.env.PAYKIT_WASM_DISPATCH_LISTENER ?? "http://127.0.0.1:18580",
  /** The fake Homeserver [http://127.0.0.1:18590]. */
  homeserver: process.env.PAYKIT_WASM_HOMESERVER ?? "http://127.0.0.1:18590",
  /** Where the browser profile's folder is made [the OS temp folder]. */
  profileRoot: process.env.PAYKIT_WASM_PROFILE_ROOT ?? tmpdir(),
  /** chrome (the installed Chrome, Playwright channel "chrome") or chromium [chrome]. */
  browser: process.env.PAYKIT_WASM_BROWSER ?? "chrome",
  /** The native peer binary [paykit-peer on PATH]. */
  peer: process.env.PAYKIT_WASM_PEER ?? "paykit-peer",
  /** Where peers keep their state folders [the OS temp folder]. */
  runRoot: process.env.PAYKIT_WASM_RUN_ROOT ?? tmpdir(),
  /** bitcoind's RPC, for the peers [http://127.0.0.1:18443, admin1, 123]. */
  bitcoind: process.env.PAYKIT_WASM_BITCOIND ?? "http://127.0.0.1:18443",
  rpcUser: process.env.PAYKIT_WASM_RPC_USER ?? "admin1",
  rpcPassword: process.env.PAYKIT_WASM_RPC_PASSWORD ?? "123",
};

/** The payment method both sides declare (the demo's regtest contract). */
export const REGTEST_ENDPOINTS = [
  { identifier: "btc-regtest-p2tr", fractionDigits: 8 },
  { identifier: "btc-regtest-p2wpkh", fractionDigits: 8 },
];

/** The signer's files on the identity's Homeserver. */
export const IDENTITY_PATH = "/pub/pubky-dispatch/v1/identity.json";
export const BACKUP_PATH = "/priv/pubky-dispatch/v1/signer-backup.json";

const Z32 = "ybndrfg8ejkmcpqxot1uwisza345h769";
const DEFAULT_WAIT_MS = 60_000;

export function log(line) {
  console.log(`[harness] ${line}`);
}

/** Throws unless `condition`; logs `what` when it holds. */
export function check(condition, what) {
  if (!condition) throw new Error(`check failed: ${what}`);
  log(`ok: ${what}`);
}

/** z-base-32, most significant bits first, as the Dispatch spells Pubky IDs. */
export function zbase32(bytes) {
  let out = "";
  let buffer = 0;
  let bits = 0;
  for (const byte of bytes) {
    buffer = (buffer << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      out += Z32[(buffer >>> (bits - 5)) & 31];
      bits -= 5;
    }
    buffer &= (1 << bits) - 1;
  }
  if (bits > 0) out += Z32[(buffer << (5 - bits)) & 31];
  return out;
}

/** The Ed25519 public key of a 32-byte secret key. */
export function publicKeyOf(secret) {
  const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), secret]);
  const key = createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
  return createPublicKey(key).export({ format: "der", type: "spki" }).subarray(-32);
}

/** A new random identity: `{secret, identity}`, the Pubky ID in z-base-32. */
export function newIdentity() {
  const secret = randomBytes(32);
  return { secret, identity: zbase32(publicKeyOf(secret)) };
}

/** A file on the Homeserver, or `null` when absent. */
export async function homeserverFile(identity, path) {
  const answer = await fetch(`${settings.homeserver}/${identity}${path}`);
  if (answer.status === 404) return null;
  if (!answer.ok) throw new Error(`the Homeserver answered ${answer.status} for ${path}`);
  return Buffer.from(await answer.arrayBuffer());
}

/** Fails unless the Dispatch listener and the Homeserver answer. */
export async function checkServices() {
  for (const [what, url] of [
    ["the Dispatch listener", `${settings.listener}/`],
    ["the Homeserver", `${settings.homeserver}/__health`],
  ]) {
    try {
      await fetch(url);
    } catch {
      throw new Error(`${what} does not answer at ${url}: start it first`);
    }
  }
}

/** A list of events that can be waited on. */
class EventLog {
  constructor(label) {
    this.label = label;
    this.events = [];
    this.waiters = new Set();
  }

  push(event) {
    this.events.push(event);
    for (const waiter of [...this.waiters]) {
      if (waiter.match(event)) {
        this.waiters.delete(waiter);
        clearTimeout(waiter.timer);
        waiter.resolve(event);
      }
    }
  }

  /** The first event from index `from` on that `match` accepts, seen or to come. */
  waitFor(match, { what = "an event", timeoutMs = DEFAULT_WAIT_MS, from = 0 } = {}) {
    const seen = this.events.slice(from).find(match);
    if (seen) return Promise.resolve(seen);
    return new Promise((resolve, reject) => {
      const waiter = { match, resolve };
      waiter.timer = setTimeout(() => {
        this.waiters.delete(waiter);
        const recent = this.events
          .slice(-12)
          .map((event) => JSON.stringify(event).slice(0, 300))
          .join("\n  ");
        reject(new Error(`${this.label}: no ${what} within ${timeoutMs} ms; last events:\n  ${recent}`));
      }, timeoutMs);
      this.waiters.add(waiter);
    });
  }

  /** The events `match` accepts. */
  filter(match) {
    return this.events.filter(match);
  }
}

/** One engine in a dedicated worker of the page. */
class Engine {
  constructor(harness, name) {
    this.harness = harness;
    this.name = name;
    this.log = new EventLog(`engine ${name}`);
  }

  /** Calls the engine's export `op`: `{ok: true, value}` or `{ok: false, error, code,
   * reason}`, `code` and `reason` the engine error's properties. */
  call(op, ...args) {
    return this.harness.page.evaluate(
      ([name, op, args]) =>
        window.callWorker(name, op, args).then(
          (value) => ({ ok: true, value }),
          (error) => ({ ok: false, error: error.message, code: error.code ?? null, reason: error.reason ?? null }),
        ),
      [this.name, op, args],
    );
  }

  /** Calls `op` and returns its value; throws on an error. */
  async must(op, ...args) {
    const started = Date.now();
    const result = await this.call(op, ...args);
    if (!result.ok) throw new Error(`${this.name} ${op}: ${result.error}`);
    log(`${this.name} ${op} in ${Date.now() - started} ms`);
    return result.value;
  }

  /** The first engine event `match` accepts (see `EventLog.waitFor`). */
  waitFor(match, options) {
    return this.log.waitFor(match, options);
  }

  /** Every event so far. */
  get events() {
    return this.log.events;
  }

  /** Terminates the worker without `stop`, as a crash would. */
  async terminate() {
    await this.harness.page.evaluate((name) => window.closeWorker(name), this.name);
  }
}

/** A native peer: `paykit-peer --state <dir> run`, its stdout events in a log. */
class Peer {
  constructor(child, stateDir) {
    this.child = child;
    this.stateDir = stateDir;
    this.log = new EventLog(`peer ${stateDir}`);
    this.exited = new Promise((resolve) => child.on("exit", (code) => resolve(code)));
    createInterface({ input: child.stdout }).on("line", (line) => {
      try {
        this.log.push(JSON.parse(line));
      } catch {
        log(`peer stdout: ${line}`);
      }
    });
    this.stderr = [];
    createInterface({ input: child.stderr }).on("line", (line) => this.stderr.push(line));
  }

  /** Sends one command line on the peer's stdin. */
  send(line) {
    this.child.stdin.write(`${line}\n`);
  }

  /** Sends `line` and waits for its `command_ok`; throws on `command_failed`. */
  async command(line, { timeoutMs = DEFAULT_WAIT_MS } = {}) {
    const from = this.log.events.length;
    this.send(line);
    const done = await this.log.waitFor(
      (event) =>
        (event.event === "command_ok" || event.event === "command_failed") && event.line === line,
      { what: `the end of "${line}"`, timeoutMs, from },
    );
    if (done.event === "command_failed") throw new Error(`peer "${line}": ${done.error}`);
    return from;
  }

  /** The first peer event `match` accepts. */
  waitFor(match, options) {
    return this.log.waitFor(match, options);
  }

  /** Quits the peer and waits for it to exit; kills it after 10 s. */
  async quit() {
    if (this.child.exitCode !== null) return this.child.exitCode;
    this.send("quit");
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill("SIGKILL"), 10_000);
    const code = await this.exited;
    clearTimeout(timer);
    return code;
  }
}

/** An amount in whole BTC as a decimal string, in sats, without floating point. */
export function btcToSats(value) {
  const match = /^(\d+)(?:\.(\d{1,8}))?$/.exec(value);
  if (!match) throw new Error(`not a BTC amount with at most 8 fraction digits: ${value}`);
  return BigInt(match[1]) * 100_000_000n + BigInt((match[2] ?? "").padEnd(8, "0"));
}

/** Sats as a decimal string in whole BTC, 8 fraction digits. */
export function satsToBtc(sats) {
  const value = BigInt(sats);
  return `${value / 100_000_000n}.${(value % 100_000_000n).toString().padStart(8, "0")}`;
}

/** The sats a transaction pays to `address`, over all its outputs: `{sats, tx}`. bitcoind has a
 * transaction index on regtest, so any transaction is found, in the mempool or in a block. */
export async function paidTo(txid, address) {
  const tx = await bitcoindCall("getrawtransaction", [txid, true]);
  let sats = 0n;
  for (const output of tx.vout) {
    if (output.scriptPubKey?.address === address) sats += btcToSats(output.value.toFixed(8));
  }
  return { sats, tx };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * A test payment method on bitcoind regtest: a bitcoind wallet of its own pays with
 * `sendtoaddress` and verifies with the transaction index, under the demo's regtest contract
 * (payload `{"value":"<address>"}`, proof `{"txid"}`, amount in BTC). The worker's rail
 * callbacks reach it over the page server at `/rail/<name>/<op>` (`url`).
 *
 * `behaviour` shapes a payment, to play a slow method such as a payment out of a shared pool:
 * `delayMs` before the transaction is sent, `holdMs` after it is sent before `pay` answers.
 * Every call is kept in `calls`; each attempt's record in `attempts`.
 */
export class RegtestRail {
  constructor(name, wallet) {
    this.name = name;
    this.wallet = wallet;
    this.url = `/rail/${name}`;
    this.behaviour = { delayMs: 0, holdMs: 0 };
    /** attemptId -> {state: "paying" | "paid" | "failed", txid, address, sats, paidAtMs} */
    this.attempts = new Map();
    /** [{op, attemptId, atMs}] */
    this.calls = [];
  }

  /** A new address of the rail's wallet: `bech32m` (P2TR) or `bech32` (P2WPKH). */
  async newAddress(type = "bech32m") {
    return bitcoindCall("getnewaddress", ["", type], this.wallet);
  }

  async pay({ attemptId, endpointId, payload, amount }) {
    this.calls.push({ op: "pay", attemptId, atMs: Date.now() });
    const known = this.attempts.get(attemptId);
    if (known) return this.outcome(known);
    const record = { state: "paying", endpointId };
    this.attempts.set(attemptId, record);
    const { delayMs, holdMs } = this.behaviour;
    if (delayMs) await sleep(delayMs);
    try {
      if (amount.asset !== "btc") throw new Error(`asset ${amount.asset}`);
      record.address = JSON.parse(payload).value;
      record.sats = btcToSats(amount.value);
      record.txid = await bitcoindCall(
        "sendtoaddress",
        { address: record.address, amount: satsToBtc(record.sats), fee_rate: 2 },
        this.wallet,
      );
      record.paidAtMs = Date.now();
      record.state = "paid";
      log(`rail ${this.name}: paid ${record.sats} sats to ${record.address.slice(0, 14)}... in ${record.txid}`);
    } catch (error) {
      // Nothing was sent: bitcoind refused before broadcasting.
      record.state = "failed";
      log(`rail ${this.name}: pay failed, nothing sent: ${error.message}`);
    }
    if (holdMs) await sleep(holdMs);
    return this.outcome(record);
  }

  payment(attemptId) {
    this.calls.push({ op: "payment", attemptId, atMs: Date.now() });
    const record = this.attempts.get(attemptId);
    return record ? this.outcome(record) : null;
  }

  async verify({ payload, amount, proof }) {
    this.calls.push({ op: "verify", attemptId: null, atMs: Date.now() });
    const address = JSON.parse(payload ?? "{}").value;
    if (!address || amount.asset !== "btc" || !/^[0-9a-f]{64}$/.test(proof?.txid ?? "")) return null;
    let found;
    try {
      found = await paidTo(proof.txid, address);
    } catch {
      return null;
    }
    if (found.sats < btcToSats(amount.value)) return null;
    // First seen: the wallet's receive time when the address is the rail's own, else now.
    const own = await bitcoindCall("gettransaction", [proof.txid, true], this.wallet).catch(() => null);
    return own?.timereceived ? own.timereceived * 1000 : Date.now();
  }

  outcome(record) {
    if (record.state === "paid") {
      return { status: "paid", proof: { txid: record.txid }, paidAtMs: record.paidAtMs };
    }
    return { status: record.state === "failed" ? "failed" : "pending" };
  }

  /** The `pay` calls made for `attemptId`. */
  payCalls(attemptId) {
    return this.calls.filter((call) => call.op === "pay" && call.attemptId === attemptId);
  }
}

/** The harness: the server, the browser, the page, its engines and the peers. */
class Harness {
  constructor({ server, context, page, profile }) {
    this.server = server;
    this.context = context;
    this.page = page;
    this.profile = profile;
    this.engines = new Map();
    this.peers = [];
    this.rails = server.rails;
  }

  /** The page's URL. */
  get url() {
    return `http://localhost:${this.server.address().port}/`;
  }

  /** Opens a new worker `name` for an engine. */
  async openEngine(name) {
    const engine = new Engine(this, name);
    this.engines.set(name, engine);
    await this.page.evaluate((name) => window.openWorker(name), name);
    return engine;
  }

  /** `start` options for the identity of `secret`, on the fake Homeserver. */
  startOptions(secret, overrides = {}) {
    return {
      identitySecretHex: secret.toString("hex"),
      dispatchOrigin: settings.origin,
      dispatchListener: settings.listener,
      storePrefix: `h-${randomBytes(6).toString("hex")}`,
      clientId: "paykit-wasm-harness",
      rail: { endpoints: REGTEST_ENDPOINTS },
      homeserverUrl: settings.homeserver,
      ownIdentity: zbase32(publicKeyOf(secret)),
      ...overrides,
    };
  }

  /** A new test payment method `name` (`RegtestRail`), with a bitcoind wallet of its own
   * funded with `fundSats` from the `default` wallet and one block mined. Pass its `url` as
   * the `railUrl` start option. */
  async openRail(name, { fundSats = 200_000 } = {}) {
    const wallet = `paykit-wasm-${name}-${randomBytes(4).toString("hex")}`;
    await bitcoindCall("createwallet", [wallet]);
    const rail = new RegtestRail(name, wallet);
    this.rails.set(name, rail);
    if (fundSats > 0) {
      const address = await rail.newAddress("bech32");
      await bitcoindCall("sendtoaddress", { address, amount: satsToBtc(fundSats), fee_rate: 2 }, "default");
      const miner = await bitcoindCall("getnewaddress", [], "default");
      await bitcoindCall("generatetoaddress", [1, miner]);
    }
    log(`rail ${name}: wallet ${wallet}, funded ${fundSats} sats`);
    return rail;
  }

  /** Starts a native peer on a new state folder under `settings.runRoot`, and waits for
   * its `ready` event. `args` go after `run`. */
  async startPeer({ name = "peer", args = [] } = {}) {
    mkdirSync(settings.runRoot, { recursive: true });
    const stateDir = mkdtempSync(join(settings.runRoot, `${name}-`));
    const child = spawn(
      settings.peer,
      [
        "--state", stateDir,
        "--dispatch-url", settings.listener,
        "--origin", settings.origin,
        "--homeserver", settings.homeserver,
        "--bitcoind-url", settings.bitcoind,
        "--rpc-user", settings.rpcUser,
        "--rpc-password", settings.rpcPassword,
        "run", ...args,
      ],
      { stdio: ["pipe", "pipe", "pipe"] },
    );
    const peer = new Peer(child, stateDir);
    this.peers.push(peer);
    const ready = await Promise.race([
      peer.waitFor((event) => event.event === "ready", { what: "ready" }),
      peer.exited.then((code) => {
        throw new Error(`the peer exited with ${code}: ${peer.stderr.slice(-5).join(" | ")}`);
      }),
    ]);
    peer.identity = ready.identity;
    peer.wallet = ready.wallet;
    log(`peer ${name}: ${ready.identity} (${ready.enrolled}), wallet ${ready.wallet}`);
    return peer;
  }

  /** Quits the peers, unloads their bitcoind wallets, closes the browser and the server, and
   * deletes the browser profile. */
  async close() {
    for (const peer of this.peers) {
      await peer.quit().catch(() => {});
      if (peer.wallet) await bitcoindCall("unloadwallet", [peer.wallet]).catch(() => {});
    }
    for (const rail of this.rails.values()) {
      await bitcoindCall("unloadwallet", [rail.wallet]).catch(() => {});
    }
    await this.context.close().catch(() => {});
    this.server.close();
    rmSync(this.profile, { recursive: true, force: true });
  }
}

/** One bitcoind JSON-RPC call; `params` an array or an object of named parameters. With
 * `wallet`, the call goes to that wallet. */
export async function bitcoindCall(method, params, wallet) {
  const auth = Buffer.from(`${settings.rpcUser}:${settings.rpcPassword}`).toString("base64");
  const url = wallet ? `${settings.bitcoind}/wallet/${encodeURIComponent(wallet)}` : settings.bitcoind;
  const answer = await fetch(url, {
    method: "POST",
    headers: { authorization: `Basic ${auth}`, "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "1.0", id: "harness", method, params }),
  });
  const body = await answer.json();
  if (body.error) throw new Error(`bitcoind ${method}: ${body.error.message}`);
  return body.result;
}

/** Answers a worker's rail call `POST /rail/<name>/<op>` with the rail's JSON answer. */
async function railCall(rails, path, req, res) {
  const [, , name, op] = path.split("/");
  const rail = rails.get(name);
  let body = "";
  for await (const chunk of req) body += chunk;
  try {
    if (!rail || !["pay", "payment", "verify"].includes(op)) throw new Error(`no rail call ${path}`);
    const value = await rail[op](JSON.parse(body));
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(value ?? null));
  } catch (error) {
    res.writeHead(500, { "content-type": "text/plain" });
    res.end(error.message);
  }
}

/** Serves the page, the worker and the engine module in `pkg` on 127.0.0.1, and the rail
 * calls of `server.rails`. */
function serve(pkg) {
  const files = { "/": join(here, "index.html"), "/worker.js": join(here, "worker.js") };
  const types = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm" };
  const rails = new Map();
  const server = http.createServer((req, res) => {
    const path = new URL(req.url, "http://localhost").pathname;
    if (req.method === "POST" && path.startsWith("/rail/")) {
      railCall(rails, path, req, res);
      return;
    }
    let file = files[path];
    if (!file && path.startsWith("/pkg/")) {
      const candidate = normalize(join(pkg, path.slice("/pkg/".length)));
      if (candidate.startsWith(pkg + sep)) file = candidate;
    }
    try {
      if (!file || !statSync(file).isFile()) throw new Error("absent");
      res.writeHead(200, { "content-type": types[extname(file)] ?? "application/octet-stream" });
      res.end(readFileSync(file));
    } catch {
      res.writeHead(404);
      res.end();
    }
  });
  server.rails = rails;
  return new Promise((done) => server.listen(0, "127.0.0.1", () => done(server)));
}

/** Starts the server and the browser, and opens the page. `pkg` is the engine module folder. */
export async function launch({ pkg }) {
  const { chromium } = require("playwright");
  const folder = resolve(pkg);
  const wasm = join(folder, "paykit_wasm_bg.wasm");
  log(`engine module ${wasm}: ${statSync(wasm).size} bytes`);
  const server = await serve(folder);
  mkdirSync(settings.profileRoot, { recursive: true });
  const profile = mkdtempSync(join(settings.profileRoot, "paykit-wasm-profile-"));
  const context = await chromium.launchPersistentContext(profile, {
    headless: true,
    ...(settings.browser === "chrome" ? { channel: "chrome" } : {}),
  });
  const page = context.pages()[0] ?? (await context.newPage());
  const harness = new Harness({ server, context, page, profile });
  page.on("console", (message) => {
    // The page asks for a favicon that the server does not have.
    if (!message.text().includes("favicon")) log(`console.${message.type()}: ${message.text()}`);
  });
  page.on("pageerror", (error) => log(`page error: ${error.message}`));
  await page.exposeBinding("__engineEvent", (_source, name, event) => {
    harness.engines.get(name)?.log.push(event);
  });
  await page.goto(harness.url);
  await page.waitForFunction(() => typeof window.callWorker === "function");
  log(`browser ${settings.browser} ${context.browser()?.version() ?? ""}, page ${harness.url}`);
  return harness;
}
