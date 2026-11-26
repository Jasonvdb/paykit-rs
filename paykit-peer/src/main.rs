//! `paykit-peer`: a scripted Paykit v0.2 wallet for local tests.
//!
//! **Local test tool only.** It plays one wallet identity end to end, with:
//!
//! - the identity's signer in-process, seeded from a 32-byte identity secret it generates and
//!   keeps in its state directory, in plain text (a test shortcut, never for real keys);
//! - a local Pubky Dispatch, reached over plain HTTP on loopback: requests are signed for the
//!   Dispatch's https origin and sent to its plain listener instead ([`transport`]);
//! - a fake Homeserver over plain HTTP on loopback ([`homeserver`]);
//! - an on-chain rail on a local bitcoind in regtest, through its JSON-RPC ([`rail`]).
//!
//! It refuses any Dispatch, Homeserver or bitcoind address that isn't loopback. See
//! `paykit-peer --help` and the README for the commands and the events it prints.

#![forbid(unsafe_code)]

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
mod bitcoind;
#[cfg(not(target_arch = "wasm32"))]
mod cli;
#[cfg(not(target_arch = "wasm32"))]
mod events;
#[cfg(not(target_arch = "wasm32"))]
mod homeserver;
#[cfg(not(target_arch = "wasm32"))]
mod peer;
#[cfg(not(target_arch = "wasm32"))]
mod rail;
#[cfg(not(target_arch = "wasm32"))]
mod state;
#[cfg(not(target_arch = "wasm32"))]
mod store;
#[cfg(not(target_arch = "wasm32"))]
mod transport;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    cli::main()
}
