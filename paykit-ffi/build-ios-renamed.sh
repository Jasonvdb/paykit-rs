#!/bin/bash
# Demo build: Paykit v0.2 for the iOS simulator on Apple Silicon, renamed so one app can link it
# beside a Paykit v0.1 package (Swift module `Paykit`, FFI module `paykitFFI`, `libpaykit.a`).
#
#   Swift module   PaykitV2
#   FFI module     PaykitV2FFI
#   Rust library   paykit_v2 (also the UniFFI symbol prefix: uniffi_paykit_v2_*, ffi_paykit_v2_*)
#
# The default output is a dynamic framework (PaykitV2FFI.framework in an XCFramework) that
# exports only its UniFFI symbols, so the C libraries and Rust runtime inside it can't collide
# with the copies inside v0.1's static library (same-named ring, blake3, compiler-rt and
# outline-atomics symbols). `--static` packages the static library instead, for comparison.
#
# UniFFI names its scaffolding symbols after the crate name, which is the `[lib] name` in
# paykit-ffi/Cargo.toml. This script renames that library to `paykit_v2` for the length of the
# build and puts the manifest back on exit.
#
# Usage: CARGO_TARGET_DIR=... paykit-ffi/build-ios-renamed.sh [--static] PACKAGE_DIRECTORY
# PAYKIT_V2_FEATURES sets the Cargo features (default: test-wallet,demo-signer; set it empty for
# none). `demo-signer` exports the in-process signer the demo host approves its own app with.
set -eu
set -o pipefail

fail() { echo "$*" >&2; exit 1; }
require_file() { [[ -s $1 ]] || fail "Missing or empty file: $1"; }

mode=dynamic
if [[ ${1:-} == --static ]]; then mode=static; shift; fi
[[ $# == 1 ]] || fail "Usage: $0 [--static] PACKAGE_DIRECTORY"
[[ $(uname -s) == Darwin && $(uname -m) == arm64 ]] || fail "Needs macOS on Apple Silicon with Xcode"
[[ -n ${CARGO_TARGET_DIR:-} ]] || fail "Set CARGO_TARGET_DIR"

ffi=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$ffi/.."
mkdir -p "$1"
package=$(cd "$1" && pwd)
features=${PAYKIT_V2_FEATURES-test-wallet,demo-signer}
config="$ffi/uniffi-ios-renamed.toml"
lib=paykit_v2
module=PaykitV2
ffi_module=PaykitV2FFI
target=aarch64-apple-ios-sim
export IPHONEOS_DEPLOYMENT_TARGET=15.0
require_file "$config"

work=$(mktemp -d "$package/.build.XXXXXX")
manifest="$ffi/Cargo.toml"
cp "$manifest" "$work/Cargo.toml.orig"
finish() {
    cp "$work/Cargo.toml.orig" "$manifest"
    rm -rf "$work"
}
trap finish EXIT
trap 'exit 1' HUP INT TERM

# Rename the library for this build only.
python3 -I - "$manifest" "$lib" <<'PY'
from pathlib import Path
import sys
path, lib = Path(sys.argv[1]), sys.argv[2]
text = path.read_text()
before = '[lib]\nname = "paykit"\n'
if text.count(before) != 1:
    sys.exit(f"Expected exactly one {before!r} in {path}")
path.write_text(text.replace(before, f'[lib]\nname = "{lib}"\n'))
PY

# Generate the bindings from the host library, with the same features as the iOS library so
# the UniFFI checksums match.
host_features=bindgen${features:+,$features}
cargo build --locked --release -p paykit-ffi --features "$host_features" --lib --bin paykit-bindgen
host="$CARGO_TARGET_DIR/release"
require_file "$host/lib$lib.dylib"
generated="$work/generated"
mkdir -p "$generated/kotlin" "$generated/swift"
for language in kotlin swift; do
    "$host/paykit-bindgen" generate --no-format --metadata-no-deps --library "$host/lib$lib.dylib" \
        --config "$config" --language "$language" --out-dir "$generated/$language"
done
kotlin=$(find "$generated/kotlin" -name "$lib.kt" -type f)
require_file "$kotlin"
swift="$generated/swift"
for file in "$module.swift" "$ffi_module.h" "$ffi_module.modulemap"; do
    require_file "$swift/$file"
done
python3 "$ffi/prepare-bindings.py" "$kotlin" "$swift/$module.swift" "$swift/WalletRedaction.swift"

cargo build --locked --release --target "$target" -p paykit-ffi ${features:+--features "$features"} --lib
built="$CARGO_TARGET_DIR/$target/release"
require_file "$built/lib$lib.a"

framework="$ffi_module.xcframework"
rm -rf "$package/$framework" "$package/Sources"
if [[ $mode == dynamic ]]; then
    bundle="$work/$ffi_module.framework"
    mkdir -p "$bundle/Headers" "$bundle/Modules"
    # Link the dynamic library from the static one, exporting only UniFFI's symbols. Cargo's
    # own cdylib also exports every Rust `#[no_mangle]` function of every dependency, such
    # as blake3's `blake3_compress_in_place_portable`, which v0.1 defines too; rustc's export
    # list can't be narrowed from the command line (ld refuses -unexported_symbol with it).
    printf '%s\n' "_uniffi_${lib}_*" "_ffi_${lib}_*" "_UNIFFI_META_*" > "$work/exports.txt"
    xcrun clang -dynamiclib -target "arm64-apple-ios$IPHONEOS_DEPLOYMENT_TARGET-simulator" \
        -isysroot "$(xcrun --sdk iphonesimulator --show-sdk-path)" \
        -Wl,-force_load,"$built/lib$lib.a" -Wl,-exported_symbols_list,"$work/exports.txt" \
        -Wl,-dead_strip -install_name "@rpath/$ffi_module.framework/$ffi_module" \
        -liconv -o "$bundle/$ffi_module"
    strip -x "$bundle/$ffi_module"
    # Only UniFFI's own symbols may leave the library.
    others=$(nm -gU -j "$bundle/$ffi_module" | grep -v -E "^_(uniffi_${lib}_|ffi_${lib}_|UNIFFI_META_)" || true)
    [[ -z $others ]] || fail "The dynamic library exports more than its UniFFI symbols: $others"
    cp "$swift/$ffi_module.h" "$bundle/Headers/"
    # The generator's module map, as a framework module.
    sed 's/^module /framework module /' "$swift/$ffi_module.modulemap" > "$bundle/Modules/module.modulemap"
    grep -q "^framework module $ffi_module " "$bundle/Modules/module.modulemap" ||
        fail "Unexpected generated module map"
    python3 -I - "$bundle/Info.plist" "$ffi_module" <<'PY'
import plistlib, sys
path, name = sys.argv[1:]
with open(path, "wb") as file:
    plistlib.dump({
        "CFBundleDevelopmentRegion": "en",
        "CFBundleExecutable": name,
        "CFBundleIdentifier": "com.synonym.paykit.v2.ffi",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": name,
        "CFBundlePackageType": "FMWK",
        "CFBundleShortVersionString": "0.2.0",
        "CFBundleVersion": "1",
        "CFBundleSupportedPlatforms": ["iPhoneSimulator"],
        "DTPlatformName": "iphonesimulator",
        "MinimumOSVersion": "15.0",
    }, file)
PY
    codesign --force --sign - --timestamp=none "$bundle"
    xcodebuild -create-xcframework -framework "$bundle" -output "$package/$framework"
else
    require_file "$built/lib$lib.a"
    # The headers sit in a folder of their own, so Xcode's shared include folder doesn't mix
    # this module map with another static XCFramework's.
    headers="$work/headers/$ffi_module"
    mkdir -p "$headers"
    cp "$swift/$ffi_module.h" "$headers/"
    cp "$swift/$ffi_module.modulemap" "$headers/module.modulemap"
    xcodebuild -create-xcframework -library "$built/lib$lib.a" -headers "$work/headers" \
        -output "$package/$framework"
fi

mkdir -p "$package/Sources/$module"
cp "$swift/$module.swift" "$swift/WalletRedaction.swift" "$package/Sources/$module/"
cat > "$package/Package.swift" <<SWIFT
// swift-tools-version: 5.9
// Generated by paykit-ffi/build-ios-renamed.sh: Paykit v0.2, renamed, iOS simulator (arm64) only.
import PackageDescription

let package = Package(
    name: "$module",
    platforms: [.iOS(.v15)],
    products: [.library(name: "$module", targets: ["$module"])],
    targets: [
        .binaryTarget(name: "$ffi_module", path: "$framework"),
        .target(name: "$module", dependencies: ["$ffi_module"], path: "Sources/$module")
    ],
    swiftLanguageVersions: [.v5]
)
SWIFT
echo "Built the $mode $module package at $package (features: ${features:-none})"
