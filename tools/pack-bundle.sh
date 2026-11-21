#!/usr/bin/env bash
# Packs several baked payloads into one bundle with language (or other) variants.
#
#   tools/pack-bundle.sh <out-bundle-dir> <name>=<payload-dir> [<name>=<payload-dir>...]
#
# Each payload dir is a build-web.sh output (game/, artifact.json, ...). The
# packer copies every payload into `<out>/<name>/`, then writes the root
# `bundle.json` a host reads: the same web-bundle contract as a single payload,
# with `variants` naming the directories and no `entry`, because each variant's
# own artifact.json knows which executable it boots.
#
#   SHIMKIT_SLUG, SHIMKIT_TITLE, SHIMKIT_SAVES   same meaning as in bake; the
#                                                defaults derive from the
#                                                bundle directory's name
#   SHIMKIT_ZIP=1                                also write `<out>.zip`
#
# A single-payload bundle needs no packer: bake's own bundle.json is the
# contract, and a host treats a manifest without `variants` as one variant
# rooted at the manifest itself.
set -euo pipefail

if [ $# -lt 2 ]; then
    printf '%s\n' "usage: pack-bundle.sh <out-bundle-dir> <name>=<payload-dir> [...]" >&2
    exit 2
fi
out="$1"
shift

slug="${SHIMKIT_SLUG:-$(basename "$out")}"
title="${SHIMKIT_TITLE:-}"
saves="${SHIMKIT_SAVES:-}"

mkdir -p "$out"
variants=""
for pairing in "$@"; do
    name="${pairing%%=*}"
    payload="${pairing#*=}"
    if [ -z "$name" ] || [ "$name" = "$pairing" ] || [ ! -d "$payload" ]; then
        printf '%s\n' "expected <name>=<payload-dir> with an existing directory, got: $pairing" >&2
        exit 2
    fi
    if [ ! -f "$payload/artifact.json" ]; then
        printf '%s\n' "$payload has no artifact.json; bake it first" >&2
        exit 1
    fi
    rm -rf "$out/$name"
    cp -R "$payload" "$out/$name"
    if [ -n "$variants" ]; then
        variants="$variants,"
    fi
    variants="$variants
    { \"name\": \"$name\", \"dir\": \"$name\" }"
done

# The JSON is assembled by hand because the manifest has no nested quoting and
# the script stays as dependency-free as the rest of tools/.
if [ -z "$title" ]; then
    title=""
    while IFS= read -r -d '-' word || [ -n "$word" ]; do
        word="$(printf '%s' "$word" | tr -d '\n')"
        [ -z "$word" ] && continue
        first="$(printf '%s' "$word" | cut -c1 | tr '[:lower:]' '[:upper:]')"
        rest="$(printf '%s' "$word" | cut -c2-)"
        title="$title $first$rest"
    done <<< "$slug-"
    title="${title# }"
fi
if [ -z "$saves" ]; then
    saves_json='["*"]'
else
    saves_json=""
    IFS=',' read -r -a globs <<< "$saves"
    for glob in "${globs[@]}"; do
        glob="${glob#"${glob%%[![:space:]]*}"}"   # left trim
        glob="${glob%"${glob##*[![:space:]]}"}"   # right trim
        [ -z "$glob" ] && continue
        if [ -n "$saves_json" ]; then
            saves_json="$saves_json, "
        fi
        saves_json="$saves_json\"$glob\""
    done
    saves_json="[$saves_json]"
fi

cat > "$out/bundle.json" <<JSON
{
  "format": 1,
  "kind": "shimkit",
  "slug": "$slug",
  "title": "$title",
  "saves": $saves_json,
  "variants": [$variants
  ]
}
JSON

if [ "${SHIMKIT_ZIP:-0}" = "1" ]; then
    # Absolute, because the subshell runs inside `$out` and a relative path
    # would land the archive inside the very bundle it archives.
    zip="$(cd "$(dirname "$out")" && pwd)/$(basename "$out").zip"
    rm -f "$zip"
    (cd "$out" && zip -qr "$zip" .)
    printf '%s\n' "zipped bundle: $zip ($(du -h "$zip" | cut -f1))"
fi

printf '%s\n' "packed $(( $# )) variants into $out"
