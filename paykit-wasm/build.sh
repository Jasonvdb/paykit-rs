#!/usr/bin/env bash
# Build the Paykit engine module for a browser: paykit_wasm.js, paykit_wasm_bg.wasm and their
# TypeScript declarations.
#
#   paykit-wasm/build.sh [--out-dir <dir>] [dev|release|browser-release]
#
# The profile defaults to browser-release (size-optimized, see the workspace Cargo.toml). The
# script builds the paykit-wasm cdylib for wasm32-unknown-unknown with --locked, then runs
# `wasm-bindgen --target web --out-name paykit_wasm` on it into the output folder, which it
# empties first. The default folder is <target dir>/paykit-wasm-pkg/<profile>. Needs
# wasm-bindgen on PATH (or in WASM_BINDGEN) at exactly the wasm-bindgen version of Cargo.lock,
# and jq. The last line it prints is the output folder.
#
# Load the module in a dedicated worker:
#
#   import init, { start } from "./paykit_wasm.js";
#   await init({ module_or_path: new URL("./paykit_wasm_bg.wasm", import.meta.url) });

set -euo pipefail

usage() {
  echo "usage: $0 [--out-dir <dir>] [dev|release|browser-release]" >&2
  exit 2
}

die() {
  echo "error: $*" >&2
  exit 1
}

out=""
profile=browser-release
while [[ $# -gt 0 ]]; do
  case "$1" in
    --out-dir)
      [[ $# -ge 2 && -n "$2" ]] || usage
      out=$2
      shift 2
      ;;
    dev | release | browser-release)
      profile=$1
      shift
      ;;
    *) usage ;;
  esac
done

crate=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$crate/.." && pwd)

command -v jq >/dev/null 2>&1 || die "jq is not on PATH"
if [[ -z "$out" ]]; then
  target_dir=$(cargo metadata --locked --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" |
    jq -r .target_directory) || die "could not read the target directory"
  [[ -n "$target_dir" && "$target_dir" != null ]] || die "cargo reported no target directory"
  out="$target_dir/paykit-wasm-pkg/$profile"
fi

bindgen=${WASM_BINDGEN:-wasm-bindgen}
"$root/paykit-browser/rig/wasm-bindgen-version.sh" --check "$bindgen" >/dev/null

echo "== building paykit-wasm for wasm32-unknown-unknown, profile $profile" >&2
messages=$(cargo build --locked --manifest-path "$root/Cargo.toml" -p paykit-wasm \
  --target wasm32-unknown-unknown --profile "$profile" --message-format json-render-diagnostics) ||
  die "could not build paykit-wasm"
wasm=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "paykit_wasm")
  | .filenames[] | select(endswith(".wasm"))' <<<"$messages" | tail -n 1)
[[ -n "$wasm" && -f "$wasm" ]] || die "cargo reported no paykit_wasm .wasm file"

rm -rf "$out"
mkdir -p "$out"
"$bindgen" --target web --out-name paykit_wasm --out-dir "$out" "$wasm" ||
  die "wasm-bindgen failed"

for file in paykit_wasm.js paykit_wasm_bg.wasm paykit_wasm.d.ts; do
  [[ -f "$out/$file" ]] || die "wasm-bindgen wrote no $file"
done
echo "== engine module in $out:" >&2
for file in "$out"/*; do
  [[ -f "$file" ]] || continue
  echo "   $(basename "$file") $(wc -c <"$file" | tr -d ' ') bytes" >&2
done
echo "$out"
