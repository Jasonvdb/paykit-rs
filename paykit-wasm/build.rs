//! Gives the engine module a 2 MiB stack instead of the linker's 1 MiB default, as the browser
//! rig's harness bundle does (`paykit-browser/rig/harness/build.rs`): the SDK's and the signer's
//! futures nest deep, and a debug build of the network flow needs up to 1216 KiB. The setting is
//! for wasm32 only.

/// The stack size of the wasm32 engine module, in bytes.
const ENGINE_STACK_BYTES: u32 = 2 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-cdylib=-zstack-size={ENGINE_STACK_BYTES}");
    }
}
