# win32-shim-kit

Run an old Windows game in a browser by answering the calls it makes into
Windows DLLs.

A game built for Windows does not care how the machine is provided. It calls
`KERNEL32.dll`, or whichever other DLL it imports, and expects an answer. This
kit provides that answer: an engine that runs the game's own machine code as
wasm, and a set of shim functions, organised by DLL, that answer the calls as
they arrive.

Adding a function to a DLL's module is the normal unit of work here.

## How it is put together

| Part | What it is |
|---|---|
| `crates/host` | the engine: x86 interpreter, PE32 loader, guest memory, the call path into shims |
| `crates/host/src/shim/` | one module per DLL, each holding that DLL's functions |
| `crates/host-wasm` | the browser module and its `wh_*` exports |
| `crates/shimgen` | turns a JSON map of functions into Rust, and validates the map |
| `crates/bake` | turns a game install into the files a page loads |
| `schema/` | the JSON formats for a function map and for a payload |
| `docs/` | one page per topic: adding functions, loading a payload, releases |

The engine knows no game. It builds the import surface from the executable it is
handed, so what runs is decided by the shims that exist.

## Add a function

A map is one JSON file per DLL, with one entry per function:

```json
{
  "kind": "shim",
  "dll": "KERNEL32.dll",
  "target": "<port-name>",
  "functions": [
    {
      "name": "GetVersion",
      "args": [],
      "returns": "u32",
      "arg_bytes": 0,
      "stub": { "kind": "custom", "body": "Ok(Return(WINDOWS_VERSION))" },
      "description": "Report a fixed Windows version",
      "evidence": "trace: first call at instruction 184412"
    }
  ]
}
```

Every function is one of four stub kinds:

| kind | meaning |
|---|---|
| `return` | answer a constant |
| `noop` | succeed without doing anything |
| `custom` | a Rust body, for the calls that need real work |
| `unimplemented` | refuse loudly instead of guessing |

Generate and validate:

```bash
cargo run -q -p shimgen -- validate examples/*.json
cargo run -q -p shimgen -- generate examples/kernel32.json --out target/kernel32.rs
```

The generated code targets `host::shim::{Action, Binding, Call, Host}`, is
dropped into that DLL's module, and is listed in the crate's `SHIMS`. A `custom`
body is copied verbatim, so the generator scaffolds but does not check meaning.

## Run a game in a browser

```bash
tools/build-web.sh <install-dir>     # -> dist/: module, game, artifact.json
```

`dist/` holds `host_wasm.wasm`, `game/` (the install plus a `files.json`), and
`artifact.json` with the executable name, size, entry address and import count.
It contains no page: any page that can fetch those bytes and call the module's
`wh_*` exports can run it. Without an install directory the build produces the
module alone, which is the shape to use when the game data must stay off the
server.

[docs/loading-a-payload.md](docs/loading-a-payload.md) is the contract: what a
payload holds, the order of the boot calls, and how a page chooses which
executable to boot.

## Limits

- **Coverage is per function, not per DLL.** Nothing here claims a DLL is
  supported. A DLL is answered only for the functions that have an entry, and
  the next program that runs will ask for ones that do not exist yet.
- **The list grows by running the target.** A call with no shim stops the run
  and names itself: `wh_run` returns 2, the message carries `DLL!Function`, and
  `wh_slot` gives that import's index. Porting is that loop, run, read the name,
  add the function, repeat. The maps in `examples/` show the format; they are not
  a statement of coverage.
- **A shim is data, and wrong data is silent until it is not.** Each entry
  declares its own argument bytes. A wrong count corrupts the guest's stack
  later, which is why `evidence` is required and not decoration.
- **An interpreter, not a recompiler.** Instructions are decoded and executed
  one at a time, and a run is paced by a budget per call. A heavy frame costs
  more calls, not a higher frame rate.
- **32-bit PE images, one at a time.** No 64-bit image, and no second process.
- **The environment a Windows box had is gone on purpose.** No CD drive and no
  registry, so a guest that checks for either stops before it draws. Saves are
  dropped when the session ends.
- **One screen, and dialogs are reported, not drawn.** A message box the guest
  would have shown arrives as a notice the page can print.
- **Audio is partial.** Sound effects play. A streamed music buffer arrives as a
  `DATA` event and is logged, not mixed.
- **Known interfaces, not arbitrary COM.** A method with no shim reports that the
  object exists but the method does not.
- **Inventory maps are records, not enforcement.** They say what a port did about
  a call; nothing checks the claim at build time.

## Rules

1. A shim needs a reason. `description` is required, and "it was crashing" is
   not one.
2. A shim needs evidence. `evidence` records the trace line, call count or call
   site that proved the requirement.
3. Silence is a bug. `unimplemented` stops the run with a name instead of
   returning zero and corrupting state later.
4. No generated file is edited by hand.

## Status

- The engine compiles for native and for `wasm32-unknown-unknown`. Its tests run
  without a game image; `SHIMKIT_GUEST=/path/to/Game.exe` adds the cases that
  need one.
- `host_wasm.wasm` runs on its own: the page loads a PE32 image, the module
  derives that image's import table at load time, and every import the shims
  cover is answered.
- A release is published from a `shimkit-v*` tag: the module, a manifest with
  its digest, the payload schema, and checksums.
- No crate is published. Consumers use the repository as a path dependency
  beside their own crate.

## Docs

- [docs/using-the-engine.md](docs/using-the-engine.md): how a port uses the
  engine.
- [docs/loading-a-payload.md](docs/loading-a-payload.md): the payload contract.
- [docs/discovery.md](docs/discovery.md): how to find what a target calls.
- [docs/release.md](docs/release.md): what a release contains.
- [docs/two-kinds-of-port.md](docs/two-kinds-of-port.md): closed-source and
  source ports.
