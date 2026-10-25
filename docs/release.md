# Releases

A release is one built module and the facts needed to use it safely. It is
published by CI from a tag (`.github/workflows/release-shimkit.yml`), so the
bytes a consumer downloads are the bytes CI built from the tagged commit. There
is no local publish path on purpose: a tag is a promise, and a promise made by a
workstation is not one.

## What a tag means

`shimkit-v<version>`, where `<version>` is the `[workspace.package] version` in
`Cargo.toml`. The workflow refuses a tag that does not match, so a version bump
and the tag are one commit apart at most.

**A tag is a module.** Re-tagging is not supported: two different bytes must not
claim one version, or a payload cannot say what it was built with. A change to
the engine (a new shim, a fixed one, a new export) means a new version and a
new tag.

## Assets

```
shimkit-v0.1.0/
  host_wasm.wasm          the engine and its browser ABI
  shimkit.json            what it is and what it promises
  artifact.schema.json    the payload format this module expects
  SHA256SUMS              `sha256sum -c SHA256SUMS` over the three above
```

Flat files, no archive: the module stays byte-identical to the build output, and
a consumer that is not this project can fetch and verify it with `curl` and
`sha256sum -c` alone. `SHA256SUMS` is one file rather than a sidecar per asset
because a checksum for a checksum is noise.

## `shimkit.json`

```json
{
  "format": 1,
  "version": "0.1.0",
  "tag": "shimkit-v0.1.0",
  "abi": 1,
  "artifact": 2,
  "module": { "name": "host_wasm.wasm", "bytes": 461250, "sha256": "…" },
  "imports": 0,
  "exports": ["wh_alloc", "wh_load", "wh_run", "…"],
  "built": "2026-10-25"
}
```

| Field | Meaning | Read by |
| --- | --- | --- |
| `format` | manifest schema; 1 today | consumers, so a field can be added without breaking them |
| `version`, `tag` | the kit version and the tag it came from | a consumer comparing what it pinned |
| `abi` | the `wh_*` call surface; bumped only when an export is renamed, removed, or changes meaning | the consumer, before it runs the module |
| `artifact` | the `artifact.json` shape this module can be paired with | a payload writer |
| `module` | exact size and digest of the asset | everyone who downloads it |
| `imports` | how many wasm imports the module has; 0 for the engine | consumers that require a self-contained module |
| `exports` | the module's actual export names, read from the built bytes | a loader feature-detecting an added surface |
| `built` | build date | diagnostics |

`abi` and `artifact` are independent: the first describes how to *call* the
module, the second what a payload *says about itself*. Additive exports do not
bump `abi`; adding an optional artifact field does not bump `artifact`.

## What the workflow checks before publishing

1. The tag equals `shimkit-v<workspace version>`.
2. The module exports the calls a loader cannot do without (`wh_alloc`,
   `wh_load`, `wh_run`, `wh_provide_file`, `wh_message_ptr`).
3. Its wire size is within budget: **256 KiB brotli**. The module is downloaded
   once per release and reused for every game and every payload, and the current
   build is ~119 KB on the wire, so the budget is roughly twice today's size,
   around 450 more bindings. Past it, split the module into variants rather than
   shipping one that grows without a decision.
4. `SHA256SUMS` covers every asset attached.

The manifest is generated from the built file: size, digest and export list are
read out of the bytes, so a manifest cannot describe a module that was not built.

## Consuming a release

```sh
tag=shimkit-v0.1.0
base=https://github.com/<owner>/win32-shim-kit/releases/download/$tag
for asset in host_wasm.wasm shimkit.json artifact.schema.json SHA256SUMS; do
  curl -fsSLO "$base/$asset"
done
sha256sum -c SHA256SUMS
```

Then put `host_wasm.wasm` beside a payload's `game/`, and write an
`artifact.json` that names it (`artifact.schema.json` in the same release is that
format). A consumer should refuse a module whose `abi` it does not implement and
should report the `version` it used, so a payload can be traced back to the
engine that ran it.

A **composed** module (a port that adds or overrides shims) publishes its own
release in this same shape, with its own manifest. It keeps the payload format
and the ABI family; it declares what it added. Consumers verify by digest and
`abi`, never by comparing bytes to this release.

## Not in a release

No page, no loader, no `serve.ts`, no bake binary, no crates, no game data and no
patch payloads. A release is one engine build plus the facts about it.
