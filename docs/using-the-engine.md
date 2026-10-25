# Using the engine

`crates/host` is a Win32 machine: it maps a PE32 image at its fixed base, gives it
guest memory, interprets its x86, and hands every import call to a shim. It does
not know which program it is running. A port supplies that, and the rest of this
page is what a port supplies and what to expect from it.

The engine assumes the image is not `/DYNAMICBASE` and has no relocation
directory: it is mapped once at `pe::IMAGE_BASE` and never moves.

## What a port is

Four things, and nothing else about the game may leak into `crates/host`:

1. **A generated import table.** `tools/arity.ts` (or an equivalent) turns the
   executable's IAT and call sites into a `[Slot; N]`, wrapped in a
   `dispatch::Table`.
2. **A `Target`.** The executable's name, its screen, its table, the shim tables
   in resolution order, and any file that stands in for a device the browser
   cannot have.
3. **Shim overrides** — usually none. Generic Win32 behaviour lives here; a port
   lists a function again only when the generic answer is wrong for its game.
4. **A host ABI**, if the port runs in a browser. The kit ships a generic one:
   `crates/host-wasm` builds `host_wasm.wasm`, reads the image's own import
   directory at load time, and exposes `wh_*` calls a page can drive. A port
   writes its own ABI only to add what its page needs beyond that — a build
   directory contract, saves, a metrics panel.

A working example of all four is the Doraemon port in
`../doraemon-monopoly-localization/crates/rehost`; the generic ABI that needs no
table at all is `crates/host-wasm`, with the loader in
`../doraemon-monopoly-localization/tools/web-client` as its page.

## Step by step

### 1. Measure the import surface

```sh
bun tools/shim-surface.ts /path/to/Game.exe            # human table
bun tools/shim-surface.ts /path/to/Game.exe --json     # what the generator eats
bun tools/arity.ts /path/to/Game.exe --rust crates/port/src/dispatch.gen.rs
```

The table is *per executable*. If a folder holds several playable builds (an
original plus localized split builds), measure each one and compare: identical
import lists can share one table, and a different list needs its own, keyed by
the import-surface hash the build records.

### 2. Describe the target

```rust
use host::dispatch::Table;
use host::shim::Binding;
use host::target::Target;

pub static TABLE: Table = Table { slots: &SLOTS };   // from step 1
pub static OVERRIDES: &[Binding] = &[];             // usually empty at first
pub static SHIMS: &[&[Binding]] = &[
    host::shim::kernel32::SHIMS,
    host::shim::files::SHIMS,
    host::shim::user32::SHIMS,
    host::shim::gdi32::SHIMS,
    host::shim::advapi32::SHIMS,
    host::shim::dsound::SHIMS,
    host::shim::ddraw::SHIMS,
    host::shim::dinput::SHIMS,
    host::shim::winmm::SHIMS,
    OVERRIDES,                                      // last, so it wins
];

pub static TARGET: Target = Target {
    name: "mygame",
    exe_name: "Game.exe",
    screen: (640, 480, 8),
    table: TABLE,
    shims: SHIMS,
    provided_files: &[("data1.cab", 200_000_000)],
};
```

- `exe_name` is what `GetModuleFileNameA` and `GetCommandLineA` answer, so it
  must be the build actually loaded, not the folder's canonical name.
- `screen` seeds the window and display before the guest calls
  `SetDisplayMode`, and answers rectangle queries that have no window yet.
- `provided_files` is a name and a length with no bytes: the guest measures the
  file to decide a disc is present and never reads it. Do not supply a fake
  registry key or a synthetic disc elsewhere — if the guest still checks for one,
  the image is not a ported image (see step 0 below).

### 3. Boot it

```rust
let session = Session::load_for(std::fs::read(path)?, &TARGET)?;
let outcome = session.run(50_000_000);
```

`Session::load` is the same call with an empty target, for tests that do not need
a table.

### 4. Extend or override

- **Extend**: add a module with `pub const SHIMS: &[Binding]` and append it to
  `SHIMS`.
- **Override**: register the same `(dll, name)` again in a later table. Later
  wins, so one function can be fixed without forking a module.
- **A behaviour that is only a fact about the game** (its name, its screen, its
  disc markers) belongs in `Target`, not in a shim. That is the test for
  whether something is generic.

### 5. Give it a browser ABI

Either link `host-wasm` — it exports the whole contract, including a table built
from the image — or write the port's own exports over `host::session::Session`.
Sizes to expect: the module is roughly 460 KB before compression, and the
interpreter runs at tens of millions of instructions per second natively, a few
hundred thousand per frame in a browser.

```sh
tools/build-web.sh                          # engine -> dist/ (a payload, no page)
bun ../doraemon-monopoly-localization/tools/web-client/browser-check.ts <install-dir>
```

The payload is what a loader loads: `host_wasm.wasm`, `game/` with its
`files.json`, and `artifact.json`. It ships no page, because how the machine is
presented — canvas, clock, keys, audio — is the loader's decision, not the
kit's; the reference one lives in the sibling repo. [loading-a-payload.md](loading-a-payload.md)
is the loader's side of that: the boot calls in order, and how to launch the
executable you mean rather than the one a tree lists first.

## What to expect

- **An unimplemented import stops the run, loudly.** The session returns
  `Outcome::HostCall`, which names `DLL!Function` and its slot. It never returns
  a plausible zero and carries on. `trace --skip` is the probe for what comes
  next: it continues past the call with `eax = 0` and shows where the guest goes,
  which is a hypothesis, not a shim.
- **Arity is checked, not guessed.** A shim declares its own stdcall argument
  bytes; where the call-site table has an opinion and disagrees, that is an
  error. Silence from the table is normal — the stack is restored from the value
  saved before the `call`, not from the arity.
- **A wrong table is a wrong run.** Slots are matched by IAT address. Loading an
  image whose table was generated from a different build will call the wrong
  shim; a port should refuse to boot when the image's import-surface hash does
  not match the table it carries.
- **`GetModuleFileNameA` is the loaded name.** A port that hard-codes one file
  name will lie to a split build the moment two builds share a folder.
- **Nothing in the engine knows a product name.** If a shim needs one, the shim
  is a port override, not generic behaviour.
- **Tests that need a real executable take it from `SHIMKIT_GUEST`.** Without it
  they skip; the crate's own tests never name a game or a path.

## Checklist for a new target

- [ ] import table generated from *each* playable executable, and the surface
      hash of each recorded in the build
- [ ] `Target` names the loaded executable, the screen, the table, the shim order
- [ ] disc/registry stand-ins in `provided_files`, not in shims
- [ ] trace reaches the guest's own first diagnostic with the real install
- [ ] a supplied file reaches the guest through `ReadFile`
- [ ] a shim's declared arity cross-checks against the table
- [ ] the port boots an image with a mismatched table only after refusing to
- [ ] `SHIMKIT_GUEST=/path/to/Game.exe cargo test` covers the table-dependent
      tests
