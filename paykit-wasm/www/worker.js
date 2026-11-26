// Harness worker: one engine in a dedicated module worker, driven by messages from the page.
//
//   page -> worker: {id, op, args}     op is an export of the engine module; args its arguments
//   worker -> page: {kind: "result", id, ok: true, value} | {kind: "result", id, ok: false, error}
//                   {kind: "event", event}                 each engine event, in order
//
// For "start" and "resetIdentity", args[0] may carry `homeserverUrl`, the base URL of a
// Homeserver with the fake Homeserver's API (GET/PUT/DELETE /{identityZ32}{path}), and
// `ownIdentity`, the engine's Pubky ID. The worker then builds the Homeserver callbacks on
// fetch against it, for that identity, and drops both keys from the options. "start" also
// gets an `onEvent` callback that forwards each event to the page, and, for `railUrl`, the
// path of a test payment method on the page's server (`RegtestRail` in harness.mjs), the
// rail callbacks `pay`, `payment` and `verify` as fetch calls to it.

import init, * as engine from "./pkg/paykit_wasm.js";

const ready = init({ module_or_path: new URL("./pkg/paykit_wasm_bg.wasm", import.meta.url) });

function fetchHomeserver(base, own) {
  const url = (identityZ32, path) => `${base}/${identityZ32}${path}`;
  return {
    async get(identityZ32, path) {
      const answer = await fetch(url(identityZ32, path));
      if (answer.status === 404) return null;
      if (!answer.ok) throw new Error(`get answered ${answer.status}`);
      return new Uint8Array(await answer.arrayBuffer());
    },
    async put(path, bytes) {
      const answer = await fetch(url(own, path), { method: "PUT", body: bytes });
      if (!answer.ok) throw new Error(`put answered ${answer.status}`);
    },
    async delete(path) {
      const answer = await fetch(url(own, path), { method: "DELETE" });
      if (!answer.ok && answer.status !== 404) throw new Error(`delete answered ${answer.status}`);
    },
  };
}

function fetchRail(path) {
  const call = async (op, argument) => {
    const answer = await fetch(new URL(`${path}/${op}`, self.location), {
      method: "POST",
      body: JSON.stringify(argument),
    });
    if (!answer.ok) throw new Error(`rail ${op} answered ${answer.status}: ${await answer.text()}`);
    return await answer.json();
  };
  return {
    pay: (payment) => call("pay", payment),
    payment: (attemptId) => call("payment", attemptId),
    verify: (check) => call("verify", check),
  };
}

// The options with the harness's keys replaced by the callbacks they describe.
function withHomeserver(options) {
  if (!options || options.homeserverUrl === undefined) return options;
  const { homeserverUrl, ownIdentity, railUrl, ...rest } = options;
  // The engine's identity is known only once it starts, and the signer writes during the
  // start: the caller passes it, as a host that derives it from the same key would.
  const withCallbacks = { ...rest, homeserver: fetchHomeserver(homeserverUrl, ownIdentity) };
  if (railUrl !== undefined) withCallbacks.rail = { ...rest.rail, ...fetchRail(railUrl) };
  return withCallbacks;
}

async function run(op, args) {
  await ready;
  const call = engine[op];
  if (typeof call !== "function" || op === "default" || op.startsWith("init")) {
    throw new Error(`unknown op ${op}`);
  }
  if (op === "start") {
    const options = withHomeserver(args[0]);
    options.onEvent = (event) => self.postMessage({ kind: "event", event });
    return await call(options);
  }
  if (op === "resetIdentity") return await call(withHomeserver(args[0]));
  return await call(...args);
}

self.onmessage = async (message) => {
  const { id, op, args } = message.data;
  try {
    const value = await run(op, args ?? []);
    self.postMessage({ kind: "result", id, ok: true, value: value ?? null });
  } catch (error) {
    self.postMessage({
      kind: "result",
      id,
      ok: false,
      error: String(error?.message ?? error),
      code: error?.code ?? null,
      reason: error?.reason ?? null,
    });
  }
};
