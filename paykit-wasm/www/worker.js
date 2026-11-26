// Smoke test worker: one engine in a dedicated module worker, driven by messages from the page.
//
//   page -> worker: {id, op: "start" | "identity" | "stop" | "version", args}
//   worker -> page: {id, ok: true, value} | {id, ok: false, error}
//
// For "start", args are the plain start options plus `homeserverUrl`, the base URL of a
// Homeserver with the fake Homeserver's API (GET/PUT/DELETE /{identityZ32}{path}). The worker
// builds the Homeserver callbacks on fetch against it, for the engine's own identity.

import init, { identity, start, stop, version } from "./pkg/paykit_wasm.js";

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
      const answer = await fetch(url(own(), path), { method: "PUT", body: bytes });
      if (!answer.ok) throw new Error(`put answered ${answer.status}`);
    },
    async delete(path) {
      const answer = await fetch(url(own(), path), { method: "DELETE" });
      if (!answer.ok) throw new Error(`delete answered ${answer.status}`);
    },
  };
}

async function run(op, args) {
  await ready;
  switch (op) {
    case "version":
      return version();
    case "start": {
      const { homeserverUrl, ownIdentity, ...options } = args;
      // The engine's identity is known only once it starts, and the signer writes during the
      // start: the caller passes it, as a host that derives it from the same key would.
      options.homeserver = fetchHomeserver(homeserverUrl, () => ownIdentity);
      return await start(options);
    }
    case "identity":
      return identity();
    case "stop":
      return await stop();
    default:
      throw new Error(`unknown op ${op}`);
  }
}

self.onmessage = async (event) => {
  const { id, op, args } = event.data;
  try {
    const value = await run(op, args ?? {});
    self.postMessage({ id, ok: true, value: value ?? null });
  } catch (error) {
    self.postMessage({ id, ok: false, error: String(error?.message ?? error) });
  }
};
