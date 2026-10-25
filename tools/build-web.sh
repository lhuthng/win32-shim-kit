#!/usr/bin/env bash
# Builds the browser payload: the wasm module, and — when an install directory is
# given — the game itself, with `artifact.json` saying what the build is.
#
#   tools/build-web.sh                       module only -> dist/
#   tools/build-web.sh <install-dir> [out]   ... plus <out>/game/, loadable
#
# Output:
#   <out>/host_wasm.wasm   the kit's engine and browser ABI
#   <out>/game/            the baked install and its files.json (optional)
#   <out>/artifact.json    executable, entry point, file and module sizes
#
# There is no page here on purpose. This is what a loader loads: a page, a shell
# or another runtime fetches `host_wasm.wasm`, `artifact.json` and
# `game/files.json`, then drives the module over its `wh_*` exports. The
# reference loader lives outside the kit, in
# `../doraemon-monopoly-localization/tools/web-client`.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
install=""
# An empty first argument used to fall through to "no install", which wipes
# `<out>/game` and `artifact.json`. Refuse it instead of deleting a payload.
if [ $# -gt 0 ] && [ -z "$1" ]; then
    printf '%s\n' 'the install directory argument is empty' >&2
    exit 2
fi
if [ $# -gt 0 ] && [ -d "$1" ]; then
    install="$(cd "$1" && pwd)"
    shift
fi
out="${1:-$here/dist}"

cargo build --quiet --manifest-path "$here/Cargo.toml" -p host-wasm --lib \
    --target wasm32-unknown-unknown --release
cargo build --quiet --manifest-path "$here/Cargo.toml" -p bake --bin shimkit-bake

module="$here/target/wasm32-unknown-unknown/release/host_wasm.wasm"
if [ ! -f "$module" ]; then
    printf '%s\n' "the build produced no $module" >&2
    exit 1
fi

mkdir -p "$out"
cp "$module" "$out/host_wasm.wasm"
# A payload carries no page. Clear anything a loader was copied into here by an
# older build, so the folder is the same shape whether or not it was reused.
rm -f "$out/index.html" "$out/main.js" "$out/style.css"

if [ -n "$install" ]; then
    "$here/target/debug/shimkit-bake" "$install" "$out"
else
    rm -rf "$out/game" "$out/artifact.json"
    printf '%s\n' 'no install directory given: the payload has no game'
fi

printf '%s\n' "built $out"
du -sh "$out"
