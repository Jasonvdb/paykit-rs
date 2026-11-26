#!/usr/bin/env node
// Smoke test of the engine's start: a dedicated worker in headless Chrome, with the
// Origin Private File System on disk, against a live Dispatch and a Homeserver with the fake
// Homeserver's API.
//
//   node paykit-wasm/www/smoke.mjs --pkg <engine module folder>
//
// Environment (defaults in brackets):
//   PAYKIT_WASM_DISPATCH_ORIGIN    the Dispatch origin requests are signed for [https://localhost:18580]
//   PAYKIT_WASM_DISPATCH_LISTENER  its plain HTTP loopback listener [http://127.0.0.1:18580]
//   PAYKIT_WASM_HOMESERVER         the fake Homeserver [http://127.0.0.1:18590]
//   PAYKIT_WASM_PROFILE_ROOT       where the browser profile's folder is made [the OS temp folder]
//   PAYKIT_WASM_BROWSER            chrome (Playwright's chromium, channel "chrome") or chromium [chrome]
//
// Each run enrols a new random identity on the Dispatch: the service keeps it. Steps:
//   1. worker A starts the engine: it enrols and joins ("new"), and its identity is the one
//      the secret gives; a second start in the same worker is refused; it stops.
//   2. the Homeserver holds the identity's identity.json, which names the engine's app, and
//      the signer's backup.
//   3. a fresh worker B starts with the same secret and stores: it loads ("loaded") the same
//      identity and app, and identity.json is unchanged, so nothing was enrolled again.
//   4. a fresh worker C starts with another secret on the same stores: refused with
//      identity_mismatch, before anything is sent.
// Playwright comes from paykit-browser's rig (`npm ci` in paykit-browser/rig).
// No secret is printed.

import { createHash, createPrivateKey, createPublicKey, randomBytes } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import http from "node:http";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, extname, join, normalize, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(join(here, "../../paykit-browser/rig/package.json"));
const { chromium } = require("playwright");

const { values: flags } = parseArgs({ options: { pkg: { type: "string" } } });
if (!flags.pkg) {
  console.error("usage: smoke.mjs --pkg <engine module folder>");
  process.exit(2);
}
const pkg = resolve(flags.pkg);
const origin = process.env.PAYKIT_WASM_DISPATCH_ORIGIN ?? "https://localhost:18580";
const listener = process.env.PAYKIT_WASM_DISPATCH_LISTENER ?? "http://127.0.0.1:18580";
const homeserver = process.env.PAYKIT_WASM_HOMESERVER ?? "http://127.0.0.1:18590";
const profileRoot = process.env.PAYKIT_WASM_PROFILE_ROOT ?? tmpdir();
const browserName = process.env.PAYKIT_WASM_BROWSER ?? "chrome";

const IDENTITY_PATH = "/pub/pubky-dispatch/v1/identity.json";
const BACKUP_PATH = "/priv/pubky-dispatch/v1/signer-backup.json";
const Z32 = "ybndrfg8ejkmcpqxot1uwisza345h769";

function log(line) {
  console.log(`[smoke] ${line}`);
}

function check(condition, what) {
  if (!condition) throw new Error(`check failed: ${what}`);
  log(`ok: ${what}`);
}

// z-base-32, most significant bits first, as the Dispatch spells Pubky IDs.
function zbase32(bytes) {
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

// The Ed25519 public key of a 32-byte secret key.
function publicKeyOf(secret) {
  const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), secret]);
  const key = createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
  return createPublicKey(key).export({ format: "der", type: "spki" }).subarray(-32);
}

async function homeserverFile(identity, path) {
  const answer = await fetch(`${homeserver}/${identity}${path}`);
  if (answer.status === 404) return null;
  if (!answer.ok) throw new Error(`the Homeserver answered ${answer.status} for ${path}`);
  return Buffer.from(await answer.arrayBuffer());
}

// The test page, the worker and the engine module, on http://localhost:<port>.
function serve() {
  const files = { "/": join(here, "index.html"), "/worker.js": join(here, "worker.js") };
  const types = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm" };
  const server = http.createServer((req, res) => {
    const path = new URL(req.url, "http://localhost").pathname;
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
  return new Promise((done) => server.listen(0, "127.0.0.1", () => done(server)));
}

async function main() {
  for (const [what, url] of [
    ["the Dispatch listener", `${listener}/`],
    ["the Homeserver", `${homeserver}/__health`],
  ]) {
    try {
      await fetch(url);
    } catch {
      throw new Error(`${what} does not answer at ${url}: start it first`);
    }
  }
  const wasmBytes = statSync(join(pkg, "paykit_wasm_bg.wasm")).size;
  log(`engine module ${join(pkg, "paykit_wasm_bg.wasm")}: ${wasmBytes} bytes`);

  const secret = randomBytes(32);
  const identity = zbase32(publicKeyOf(secret));
  const storePrefix = `smoke-${randomBytes(6).toString("hex")}`;
  const options = {
    identitySecretHex: secret.toString("hex"),
    dispatchOrigin: origin,
    dispatchListener: listener,
    storePrefix,
    clientId: "paykit-wasm-smoke",
    rail: {
      endpoints: [
        { identifier: "btc-regtest-p2tr", fractionDigits: 8 },
        { identifier: "btc-regtest-p2wpkh", fractionDigits: 8 },
      ],
    },
    homeserverUrl: homeserver,
    ownIdentity: identity,
  };
  log(`identity ${identity}, stores ${storePrefix}-app and ${storePrefix}-signer`);

  const server = await serve();
  const pageUrl = `http://localhost:${server.address().port}/`;
  const profile = mkdtempSync(join(profileRoot, "paykit-wasm-profile-"));
  const context = await chromium.launchPersistentContext(profile, {
    headless: true,
    ...(browserName === "chrome" ? { channel: "chrome" } : {}),
  });
  try {
    log(`browser ${browserName} ${context.browser()?.version() ?? ""}, page ${pageUrl}`);
    const page = context.pages()[0] ?? (await context.newPage());
    page.on("console", (message) => log(`console.${message.type()}: ${message.text()}`));
    page.on("pageerror", (error) => log(`page error: ${error.message}`));
    await page.goto(pageUrl);
    await page.waitForFunction(() => typeof window.callWorker === "function");
    const call = (name, op, args) =>
      page.evaluate(
        ([name, op, args]) =>
          window.callWorker(name, op, args).then(
            (value) => ({ ok: true, value }),
            (error) => ({ ok: false, error: error.message }),
          ),
        [name, op, args],
      );
    const must = async (name, op, args) => {
      const started = Date.now();
      const result = await call(name, op, args);
      if (!result.ok) throw new Error(`${name} ${op}: ${result.error}`);
      log(`${name} ${op} in ${Date.now() - started} ms`);
      return result.value;
    };
    const isWorker = await page.evaluate(() => typeof Worker === "function");
    check(isWorker, "the page can start dedicated workers");

    // 1. First start: enrol and join.
    await page.evaluate(() => window.openWorker("a"));
    log(`engine version ${await must("a", "version")}`);
    const first = await must("a", "start", options);
    log(`first start: ${JSON.stringify(first)}`);
    check(first.enrolment === "new", "the first start enrols (enrolment new)");
    check(first.identity === identity, "the identity is the Pubky ID of the secret");
    check(/^[A-Za-z0-9_-]{22}$/.test(first.appId), "the App ID is 16 bytes in base64url");
    check((await must("a", "identity")) === identity, "identity() gives the same Pubky ID");
    const again = await call("a", "start", options);
    check(!again.ok && again.error.startsWith("already_started:"), "a second start in the same worker is refused");
    await must("a", "stop");
    const stopped = await call("a", "identity");
    check(!stopped.ok && stopped.error.startsWith("not_started:"), "identity() after stop says not_started");
    await page.evaluate(() => window.closeWorker("a"));

    // 2. What the signer published.
    const doc = await homeserverFile(identity, IDENTITY_PATH);
    check(doc !== null, "identity.json exists on the Homeserver");
    // A signed document: its body is base64url JSON, and it names apps by z-base-32 App ID.
    const body = JSON.parse(Buffer.from(JSON.parse(doc.toString("utf8")).body, "base64url").toString("utf8"));
    check(body.identity === identity, "identity.json is the identity's");
    check(body.inbox?.url === origin, "identity.json names the Dispatch origin");
    const appZ32 = zbase32(Buffer.from(first.appId, "base64url"));
    check(
      Array.isArray(body.apps) && body.apps.length === 1 && body.apps[0].app_id === appZ32,
      "identity.json lists the engine's app, and only it",
    );
    const backup = await homeserverFile(identity, BACKUP_PATH);
    check(backup !== null, "the signer's backup exists on the Homeserver");
    const docHash = createHash("sha256").update(doc).digest("hex");

    // 3. Second start in a fresh worker: load.
    await page.evaluate(() => window.openWorker("b"));
    const second = await must("b", "start", options);
    log(`second start: ${JSON.stringify(second)}`);
    check(second.enrolment === "loaded", "the second start loads (enrolment loaded)");
    check(second.identity === identity, "the second start has the same identity");
    check(second.appId === first.appId, "the second start has the same app");
    await must("b", "stop");
    await page.evaluate(() => window.closeWorker("b"));
    const docAfter = await homeserverFile(identity, IDENTITY_PATH);
    check(
      docAfter !== null && createHash("sha256").update(docAfter).digest("hex") === docHash,
      "identity.json is unchanged by the second start",
    );

    // 4. Another identity on the same stores.
    await page.evaluate(() => window.openWorker("c"));
    const other = randomBytes(32);
    const refused = await call("c", "start", {
      ...options,
      identitySecretHex: other.toString("hex"),
      ownIdentity: zbase32(publicKeyOf(other)),
    });
    check(!refused.ok && refused.error.startsWith("identity_mismatch:"), "another identity's start on the same stores is refused");
    await page.evaluate(() => window.closeWorker("c"));

    log(`PASS: identity ${identity}, app ${first.appId}, wasm ${wasmBytes} bytes`);
  } finally {
    await context.close();
    server.close();
    rmSync(profile, { recursive: true, force: true });
  }
}

main().catch((error) => {
  log(`FAIL: ${error.message}`);
  process.exit(1);
});
