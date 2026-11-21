#!/usr/bin/env bash
# Builds the browser payload: the wasm module, and — when an install directory is
# given — the game itself, with `artifact.json` saying what the build is.
#
#   tools/build-web.sh                         module only -> dist/
#   tools/build-web.sh <install-dir> [out]     ... plus <out>/game/, loadable
#   tools/build-web.sh <install-dir>/<exe> [out]  the same, booting that exe
#
# The second form takes the executable itself as the entry point, which is what
# a payload usually knows: `Madagascar/Game/Game.exe` says both where the files
# are and which of them boots. `SHIMKIT_EXE` still overrides the choice when a
# folder holds more than one.
#
# Output:
#   <out>/host_wasm.wasm   the kit's engine and browser ABI
#   <out>/game/            the baked install and its files.json (optional)
#   <out>/artifact.json    executable, entry point, file and module sizes
#   <out>/bundle.json      the web-bundle manifest a host reads
#                         (docs/web-bundle-contract.md)
#
# Environment:
#   SHIMKIT_EXE, SHIMKIT_SCREEN, SHIMKIT_WINDOWS   passed to the bake, as before
#   SHIMKIT_SLUG, SHIMKIT_TITLE, SHIMKIT_SAVES     passed to the bake; name the
#                                                  bundle for a web host
#   SHIMKIT_MODULE=<path>                          ship this module instead of
#                                                  building one -- a release's
#                                                  `host_wasm.wasm`, which a
#                                                  hosted payload should always
#                                                  be. Its sibling
#                                                  `shimkit.json` (if any) is
#                                                  copied too, so the manifests
#                                                  can name the release.
#   SHIMKIT_ZIP=1                                  also zip the payload contents
#                                                  into `<out>.zip` beside the
#                                                  output directory, ready for a
#                                                  chunked upload
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
if [ $# -gt 0 ] && [ -f "$1" ]; then
    # An entry point names the install directory and the executable at once.
    SHIMKIT_EXE="$(basename "$1")"
    export SHIMKIT_EXE
    install="$(cd "$(dirname "$1")" && pwd)"
    shift
elif [ $# -gt 0 ] && [ -d "$1" ]; then
    install="$(cd "$1" && pwd)"
    shift
fi
out="${1:-$here/dist}"

cargo build --quiet --manifest-path "$here/Cargo.toml" -p bake --bin shimkit-bake

mkdir -p "$out"
given_module="${SHIMKIT_MODULE:-}"
if [ -n "$given_module" ]; then
    # A named module ships as it is: a release is already optimised and already
    # verified by the workflow that staged it, so re-running wasm-opt over it
    # would only churn its bytes and break the stamp its shimkit.json carries.
    if [ ! -f "$given_module" ]; then
        printf '%s\n' "SHIMKIT_MODULE=$given_module is not a file" >&2
        exit 1
    fi
    cp "$given_module" "$out/host_wasm.wasm"
    release_manifest="$(dirname "$given_module")/shimkit.json"
    if [ -f "$release_manifest" ]; then
        cp "$release_manifest" "$out/shimkit.json"
    fi
    printf '%s\n' "shipping module: $given_module"
else
    cargo build --quiet --manifest-path "$here/Cargo.toml" -p host-wasm --lib \
        --target wasm32-unknown-unknown --release
    module="$here/target/wasm32-unknown-unknown/release/host_wasm.wasm"
    if [ ! -f "$module" ]; then
        printf '%s\n' "the build produced no $module" >&2
        exit 1
    fi
    # `wasm-opt` is worth running: it takes a sixth off the module (2.00 MB -> 1.75 MB
    # on the 2026-11 Hercules build) and measured 6% faster in the interpreter
    # (48.1 -> 51.0 MIPS, deterministic clock, 400M instructions). It is optional --
    # a machine without binaryen still gets a correct module -- so a missing tool is
    # a note, not a failure. `WASM_OPT=` overrides the path.
    wasm_opt="${WASM_OPT:-$(command -v wasm-opt || true)}"
    if [ -n "$wasm_opt" ] && [ -x "$wasm_opt" ]; then
        "$wasm_opt" -O3 "$module" -o "$out/host_wasm.wasm"
        printf '%s\n' "wasm-opt -O3: $(du -h "$module" | cut -f1) -> $(du -h "$out/host_wasm.wasm" | cut -f1)"
    else
        cp "$module" "$out/host_wasm.wasm"
        printf '%s\n' 'wasm-opt not found: shipping the unoptimised module (brew install binaryen)'
    fi
fi
# A payload carries no page. Clear anything a loader was copied into here by an
# older build, so the folder is the same shape whether or not it was reused.
rm -f "$out/index.html" "$out/main.js" "$out/style.css"

if [ -n "$install" ]; then
    "$here/target/debug/shimkit-bake" "$install" "$out"
    # The settings that belong to the game rather than to the page: the frame
    # rate it was written for, and whether a click should hand it the pointer.
    # Written once and then left alone -- a rebuilt payload must not throw away
    # the numbers someone worked out for it.
    if [ ! -f "$out/config.json" ]; then
        cat > "$out/config.json" <<'JSON'
{
  "fps": null,
  "mouse": "lock"
}
JSON
        printf '%s\n' "wrote $out/config.json (fps: null runs as fast as the host allows)"
    fi
else
    rm -rf "$out/game" "$out/artifact.json"
    printf '%s\n' 'no install directory given: the payload has no game'
fi

# A hosted bundle is uploaded as one zip whose root is the payload itself, so
# the archive is made from the contents, not the folder. Skipped on purpose when
# the payload already carries its zip: re-running with SHIMKIT_ZIP over an
# out dir that holds its own archive would zip the zip into itself.
if [ "${SHIMKIT_ZIP:-0}" = "1" ]; then
    # Absolute, because the subshell runs inside `$out` and a relative path
    # would land the archive there -- inside the very bundle it archives.
    zip="$(cd "$(dirname "$out")" && pwd)/$(basename "$out").zip"
    rm -f "$zip"
    (cd "$out" && zip -qr "$zip" .)
    printf '%s\n' "zipped payload: $zip ($(du -h "$zip" | cut -f1))"
fi

printf '%s\n' "built $out"
du -sh "$out"
