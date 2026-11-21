# The web bundle contract

A payload is the machine plus a game, and no page (`docs/loading-a-payload.md`).
A **web bundle** is what a game host needs on top of that: the payload plus one
manifest that tells the host what it is, what to serve, and which files the game
writes that are worth keeping. The host uploads, hosts and saves a bundle
without ever knowing what a shim is; the kit stays pageless, and the host's
player is just the loader this contract anticipated.

Two bundle kinds exist, so two very different ports can share one host
pipeline:

| kind | what it is | who writes the manifest |
| --- | --- | --- |
| `shimkit` | a binary rehost payload: this kit's interpreter runs the game's own exe | `shimkit-bake`, through `tools/build-web.sh` |
| `wasmshell` | a source port's own static web shell -- its wasm, its page, its asset streaming | the port's bundle script (e.g. reVC's `hosting/make-bundle.sh`) |

## The manifest

`bundle.json`, at the bundle root:

```json
{
  "format": 1,
  "kind": "shimkit",
  "slug": "doraemon-monopoly",
  "title": "Doraemon Monopoly",
  "entry": "Doraemon.exe",
  "saves": ["*.SAV", "SAVE.DAT"],
  "game": { "files": 55, "bytes": 146593385 },
  "module": { "name": "host_wasm.wasm", "bytes": 461250, "build": "9bb2…" },
  "release": { "version": "0.1.0", "tag": "shimkit-v0.1.0" }
}
```

| field | meaning |
| --- | --- |
| `format` | manifest format; 1 is the first |
| `kind` | `shimkit` or `wasmshell`; decides which player a host uses |
| `slug`, `title` | the URL name and the display name; bake derives both from the install folder when the publisher does not name them (`SHIMKIT_SLUG`, `SHIMKIT_TITLE`) |
| `entry` | the executable a loader boots; a manifest with `variants` has none, because each variant's own `artifact.json` names its `default` |
| `saves` | glob patterns naming the files the game writes that are worth keeping (`SHIMKIT_SAVES`, comma-separated; default `*`, keep everything a guest writes) |
| `game` | how many files the install holds and their total bytes |
| `module` | the wasm module's size and FNV-1a content stamp, when one ships in the bundle |
| `release` | which kit release the module came from, when it ships with its `shimkit.json` |

`module` and `release` are provenance, not requirements: a manifest stays
truthful and simply omits what its bundle does not carry.

### Variants

One game, several builds -- one per language, say -- is one bundle whose root
manifest lists them:

```json
{
  "format": 1,
  "kind": "shimkit",
  "slug": "doraemon-monopoly",
  "title": "Doraemon Monopoly",
  "saves": ["*"],
  "variants": [
    { "name": "english", "dir": "english" },
    { "name": "vietnamese", "dir": "vietnamese" }
  ]
}
```

Each variant directory is a complete payload with its own `artifact.json` and
`game/`. A host treats a manifest without `variants` as one variant rooted at
the manifest itself, so a player needs no special case for the common shape.
`tools/pack-bundle.sh` assembles this from payloads `build-web.sh` produced.

## Distribution

A bundle ships as one zip whose root is the bundle directory itself --
`bundle.json` first, then the payload's files -- because a host uploads one
archive through a chunked session and unpacks it on its side. `build-web.sh`
with `SHIMKIT_ZIP=1` writes `<out>.zip` beside the output directory.

## What a host serves

The bundle's files, statically, under one prefix, so relative fetches work:
`artifact.json`, `game/files.json`, every file `files.json` names. A `shimkit`
bundle needs nothing else; a `wasmshell` bundle brings its own shell page and
its own asset streaming, and whatever headers that shell needs (Range on big
files, a `Content-Encoding` it declares) are between the host's static serving
and the shell, not part of this contract.

## What a host saves

`saves` names the files worth persisting. The kit's side of the deal: the module
surfaces every file the guest writes, through `wh_saved_*` exports, and bumps
`wh_saved_version` when any of them changes -- it does not know what a save is,
and does not need to. The host's side: keep the guest's writes somewhere
(browser-local at least, an account's cloud storage when the host has users),
replay them into the next session through `wh_provide_saved`, and filter them
through the manifest's `saves` globs. Guests and accounts, offline behaviour
and conflict rules are the host's policies; the contract only fixes the file
names and the two calls.

## Which module ships

A hosted payload should run a **release** module, not whatever a developer's
tree builds today. `build-web.sh` takes `SHIMKIT_MODULE=<path>`: it ships that
module as it is -- a release is already optimised and already verified by the
workflow that staged it -- and copies the release's `shimkit.json` beside it,
which is how `release` reaches the manifest. Building without it still works
for development; the manifest's `module.build` stamp makes the difference
visible either way.
