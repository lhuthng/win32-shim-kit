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
tools/build-web.sh <install-dir>/<exe>  # the same, booting that executable
```

The executable form is the usual one: a path to the game's own `.exe` names both
the folder the payload is built from and the entry point it boots, so the loader
never has to guess between two executables in one install.

`dist/` holds `host_wasm.wasm`, `game/` (the install plus a `files.json`), and
`artifact.json` with the executable name, size, entry address and import count.
It contains no page: any page that can fetch those bytes and call the module's
`wh_*` exports can run it. Without an install directory the build produces the
module alone, which is the shape to use when the game data must stay off the
server.

[docs/loading-a-payload.md](docs/loading-a-payload.md) is the contract: what a
payload holds, the order of the boot calls, and how a page chooses which
executable to boot.

## The demo

`demo/` is a built, static site: a Svelte page plus one folder per baked payload.
It is what GitHub Pages serves, and everything in it is generated — the site from
`demo-site/`, the payloads from `tools/build-web.sh`:

```bash
tools/build-web.sh <install-dir>/<exe> demo/payloads/<slug>   # a published payload
tools/build-web.sh <install-dir>/<exe> local/payloads/<slug>  # this machine only
cd demo-site && bun install && bun run build                  # the page -> demo/
bun tools/demo-check.ts demo 120000000 <slug>                 # just that payload
```

A payload whose game files are not the repository's to ship is baked into
`local/payloads/<slug>/` and described by `local/payloads/index.json`: gitignored,
never built into `demo/`, and served by `demo-site/serve.ts` to this machine only.
The page plays it by name, `?game=<slug>`, and the merged list still mentions it
locally -- but nothing of it is committed, so a published build never sees it.

`demo/payloads/index.json` is the list the published page reads. `local/payloads/`
is merged in by the local server and the two checkers. Add an entry after a bake,
and list a second demo the same way; the player boots whichever one is picked.

```bash
bun tools/demo-check.ts demo            # serves demo/ the way Pages does and boots every payload
bun tools/demo-player-check.ts demo     # drives demo-site's own loader against the payload
```

The first demo is **Elimination III** (2000): a DirectDraw 640×480×8 arena
shooter that also uses DirectSound through ordinal 1 and DirectInput 3.0. It
runs from its own executable, unmodified, against the shims in this repository.

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
- **Audio is partial.** Sound effects play, mixed from the voice's own memory so
  a guest streaming into a buffer is heard. Music is not: a CD track (`open
  cdaudio`) and a MIDI sequence have no device here, and an SDL game's device is
  opened at the format it asks for and never mixed, so an SDL build runs
  silently by design.
- **SDL 1.2 is answered for the calls a game makes, not as a port of SDL.** The
  video surface, the event queue, the clock and the audio configuration are
  there; joysticks report none, and `SDL_WaitEvent` returns immediately instead
  of blocking the frame.
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
5. A 16-bit register write keeps the upper half. `Registers::set_word`
   preserves bits 16-31 of the 32-bit register, as x86 does; only a 32-bit
   write replaces the whole thing. Hercules' scaled sprite blitter
   (`0x40d2c7`) packs a fixed-point source coordinate into the upper half of
   `eax` and counts rows with `dec ax`, so zero-extending that word made it
   sample index 0 and punched transparent holes through the menu text and
   Muses. Fixed 2026-11-04; guarded by
   `a_word_move_keeps_the_upper_half_of_a_fixed_point_accumulator` in
   `crates/host/src/cpu/exec.rs`. Do not "correct" this back.

## Performance (measured 2026-11-04, Hercules)

The shipped wasm interprets about 38-50 M guest instructions per second (V8 and
JavaScriptCore agree), and Hercules needs 1.7-2.2 M instructions per gameplay
frame. That is the ceiling: 20-30 guest fps with the CPU entirely busy. The
page's old 12 ms slice out of a 16.7 ms `requestAnimationFrame` used only ~72%
of it; `sliceMs: 16` in a payload's `config.json` measured **22.5 fps / 43.5 M
instr/s** against **20.0 / 36.9 M** at 12 ms. 20 ms bought almost nothing,
because the callback starts delaying rAF itself. `present()` also reuses one
`ImageData` now and asks for the palette revision once per frame instead of 240
times.

Those slice figures were taken before the loop checked its clock at the right
place; see [Frame pacing](#frame-pacing-2026-11-04) for the corrected
measurement and for why the frame *rate* was never the number that mattered.

PC sampling (`tmp/hercules-probe/bench.mjs <payload> <limit> 250000 profile`,
with `BENCH_WASM=` to point at a module other than the deployed one) says where
the rest goes:

- intro FMV: ~90% in the video decoder at `0x402362..0x4023b3`;
- the menu, which recomposes the whole 320x240 screen with sprites every frame:
  ~62% in the three row loops below, and almost nothing else;
- a level that draws a few sprites: the same three loops are only ~20%, and the
  rest is spread thin across the engine -- no other loop is worth replacing.

Measure the scene you care about. A window picked by instruction count alone is
a trap: guest progression is driven by guest *time*, so with the default probe
clock the intro is still on screen after 1.5 G instructions, and a "gameplay"
window there is really the loading logo.

### Native sprite rows (2026-11-04)

The three row loops now run in the host through `target::Hle`, armed only for
`HERCULES.EXE` (`crates/host/src/hle.rs`, `HERCULES_HLE`). The replacement walks
a whole row per dispatch and sets `eip` to the first instruction after the loop,
so the guest sees the same registers, flags, and memory when it resumes.

Verified byte for byte, not by eye. `tmp/hercules-probe/ab-hle.mjs` boots the
same module twice through `wh_set_hle(0|1)`, drives both with a guest clock that
is a pure function of the instruction counter, and `ab-hle-compare.py` compares
every presented frame plus present-aligned checkpoints of the composed screen:

```
476/476 frames identical (442 distinct), menu identical, menu2 identical
```

```
AB_RATE=64 AB_FROM=1100000000 AB_KEYS="" \
AB_SHOTS="menu@1200000000,menu2@1300000000" \
  node tmp/hercules-probe/ab-hle.mjs local/payloads/hercules tmp/hercules-probe/ab-off 0
# ... same with 1 for ab-on, then:
/usr/bin/python3 tmp/hercules-probe/ab-hle-compare.py \
    tmp/hercules-probe/ab-off tmp/hercules-probe/ab-on
```

Measured on the menu, `1.1G..1.35G` instructions (250 M, 124 frames):

| | MIPS | guest fps |
|---|---|---|
| interpreter | 56.1 | 27.8 |
| native rows | 110.0 | 54.5 |

**1.96x**, with the native rows replacing 62.5% of instructions. A window that
spans the menu and the transition into a level measures 58.3 -> 74.7 MIPS
(**1.28x**, 16.9 -> 21.7 fps, 25% native), and a whole run including the intro
FMV is 55.3 -> 69.6 MIPS (1.26x), because the video decoder is not touched.

The comparison is exact only while both sides see the same input at the same
instruction. A press placed at an instruction count lands a few thousand
instructions later on the armed side, because one `wh_run` call can retire a
whole sprite, and a transition frame can then be drawn at a different phase.
Presses are held for several guest frames to keep that rare; the clean proof is
the key-free run above.

Two things that mattered more than the row code:

- The dispatch is asked "is this a native entry?" once per guest instruction. A
  `HashMap` there cost 33% of the whole run with three entries armed -- more
  than the rows saved. It is a masked slice scan now (`HleTable` in
  `session.rs`).
- The `test` in each loop feeds only its own jump; the `inc`/`dec` that closes
  the same iteration overwrites every flag but the carry. Computing parity and
  storing five flags per pixel bought nothing. Only the last iteration pays for
  a real `inc`, and the scaled row's `and`/`add`/`inc edi` need no flags at all.

Three traps in the harness, each of which silently produced a green result:

- The presented surface is empty: Hercules composes into its own DIB at
  `0x01515488` and presents with `StretchDIBits`, so hashing
  `wh_framebuffer_offset()` compares blank against blank. Hash the DIB, and
  index it as `wh_memory_ptr() - IMAGE_BASE + VA`.
- A checkpoint taken at an arbitrary instruction count catches the guest
  mid-draw, and the two sides reach that count at different points inside the
  frame. Shoot on the next present instead.
- A clock derived only from instructions deadlocks as soon as every thread parks
  on a deadline: no instruction retires, so the clock never advances. `AB_RATE`
  speeds the clock up (the game needs to reach the menu inside the budget), and
  a stalled call buys a millisecond.

The interpreter is still the right default everywhere else. Keep it that way:
every replacement has to prove itself against the interpreter on real frames,
and these are the exact loops where the partial-register sprite bug lived.

### Frame pacing (2026-11-04)

Throughput was not the whole story: the game got faster and still looked rough.
The frame loop runs `wh_run` in chunks until the guest presents or a wall-clock
slice is spent, and **the slice was only consulted after each chunk**. A chunk is
a budget in *instructions retired*, and a run of native sprite rows retires many
times its budget in one call, so one `wh_run(250_000)` measured **7.5 ms at the
median and 18 ms at p99**. A "16 ms" slice therefore meant "16 to 24 ms", and
more than half the callbacks ran past the 16.7 ms display slot. A callback that
crosses a vsync does not shorten the next one -- it loses it.

`tmp/hercules-probe/bench-pace.mjs` reproduces the page's loop against a vsync
grid and reports the interval between presents, which is what the eye sees:

| menu, 4 s | before | after |
|---|---|---|
| rAF callbacks per second | 38 | **60** |
| callback p50 / p99 | 19.0 / 22.3 ms | **14.7 / 16.4 ms** |
| callbacks over 16.7 ms | 57% | **0%** |
| frames landing on exactly 2 vsyncs | 31% | **88%** |
| frames landing on 1 or 3+ vsyncs | 70% | **13%** |
| display interval p10 / p50 / p90 | 16.7 / 29.3 / 55.8 ms | **29.4 / 33.1 / 36.7 ms** |
| guest fps / MIPS | 29.6 / 62.5 | 30.3 / 69.1 |

The frame rate barely moves -- it was already ~30 -- but the interval stops
swinging between one and three vsyncs, and that swing is the stutter. The intro
FMV, which is CPU-bound and has no slack at all, went from 5.3 to 7.7 fps
(39 -> 60 callbacks a second, 55% of them over the slot before).

Two things to keep:

- Check the clock *before* the chunk that would overshoot, against the cost of
  the previous chunk, and keep the chunk small (64,000). The estimate adapts to
  a slower machine by itself. Clamp the estimate to half the slice, or one hitch
  makes the loop refuse its own first chunk and it never recovers.
- Stopping at the present is correct. Holding through the whole slice instead,
  to stop the guest's wait being quantized to a vsync, measured *worse*: 29.3
  fps, callbacks 16.1 ms p50, interval 17.1-49.4 ms.

`sliceMs: 15` is the shipped setting. The ceiling has to stay clear of a whole
vsync; what changed is that it is now honoured to within a chunk rather than to
within a chunk's *maximum*, which was nearly half a slot.

## Audio (2026-11-04, Hercules)

A looping sound effect that would not stop. Two things were wrong, and one of
them was in the measurement rather than in the engine.

**`GetStatus` answered a value no device produces.** For a buffer that is
playing and looping, a real device answers `DSBSTATUS_PLAYING |
DSBSTATUS_LOOPING` (`5`); this host answered `DSBSTATUS_LOOPING` alone (`4`), on
the theory that a guest comparing with equality would be confused by a value it
had not been told about. Hercules is the counter-example: its per-frame sound
update at `0x0040f820` reads the status and tests one bit of it.

```text
40f877: call [ecx + 0x24]           ; GetStatus(&status)
40f87a: test byte [esp + 0x10], 1   ; DSBSTATUS_PLAYING
40f87e: jne  0x40f888               ; playing: leave the voice alone
40f880: mov  [esi + 0xb99848], -1   ; finished: forget the voice
```

So `4` read as *not playing*. The guest cleared its handle to the voice, and
that handle is what its own stop (`0x40fa10`, which bails on `handle == -1`) and
play (`0x40fbf0`, same test) paths consult before doing anything. The theory in
the old comment was also unsupported: Cave Story never asks for a status at all
-- `GetStatus` appears nowhere in its sound code.

**Sound outlived the guest.** Nothing stopped the page's mixer when the guest
halted. A real machine goes silent with the process, because the buffers belong
to it; here the mixer kept rendering whatever it held, so the last sounds of a
session repeated forever and a looping one was the one still audible. Hercules
leaves a session by `ExitProcess`, so quitting the demo left the menu ambience
running. The frame loop now clears the mixer on a non-zero `wh_run`, and the
diagnostic reports `looping=` so the condition is visible without listening.

**The harness was lying, and that is the part worth keeping.** Everything above
was first measured with `tmp/hercules-probe/audio-trace.mjs` at its default
`AUDIO_RATE=64`, which runs the guest's clock 64x faster than instructions. The
page hands the guest *real* elapsed time and this host retires ~55M instructions
a second, so the page's regime is `AUDIO_RATE` of about 2. At 64 the same
session looked broken -- two 1.57 s ambience loops sounding for 92x and 71x their
own length, never stopped -- and at 2 they sound for 7x and 6x and are stopped
normally, with every voice released at the exit:

| Hercules, same scenario | `AUDIO_RATE=64` | `AUDIO_RATE=2` |
|---|---|---|
| stops / releases at the exit | 135 / 135 | **599 / 599** |
| voices left on the mixer | 46, 2 looping | **0** |
| longest 1.57 s loop | 144 s (92x) | **11.4 s (7x)** |

The guest's sound system is timer-driven, and 27x the clock is not a faster
game, it is a different one. A timing conclusion from that harness is only
worth what it measures at `AUDIO_RATE=2`.

Two tools came out of it, both worth keeping: `wh_audio_trace` switches the
DirectSound call trail on at runtime (`std::env` does not exist in the wasm
module, so `SHIMKIT_LOG_AUDIO` could never serve a page), which is the only
record of *which object* a guest aimed `Stop` and `Release` at; and the probe
watches the guest's own voice table at `0xb99908`, which is the only record of
a guest forgetting a sound that is still playing. Neither showed a fault at
`AUDIO_RATE=2`.

### The page could not have been playing anything

The report after those fixes was that the loop was still there *and* that the
audio was distorted. Looking at the page rather than the guest turned up
something worse than either: `drainAudio` threw on the first audio event of
every session.

`wh_audio_events_ptr` answers with a pointer into the host's heap, and it was
missing from the player's `POINTER_EXPORTS` list. A wasm `i32` result reaches
JavaScript signed, and the guest arena here is a fixed 2 GiB reservation
(`MEMORY_TOP = 0x0110_0000 + 0x7f20_0000`), so every buffer the host allocates
above it -- the event queue, the render stream, the scratch buffer -- lands
above 2 GiB and arrives negative. A negative offset is what a typed array
rejects:

```text
RangeError: Start offset -2139997824 is outside the bounds of the buffer
    at Object.drainAudio
```

`frame` re-arms itself at the top of the callback, so the exception did not stop
the loop: the game kept running, kept rendering, and was silent, with the event
queue growing and nothing anywhere saying why. `tmp/hercules-probe/mix-lab.mjs`
reproduces it with `MIX_WRAP=page`, which wraps the exports exactly as the
player did. The fix is the missing name in the list; the belt to that
braces is that `drainAudio` now catches and reports a throw once instead of
letting it escape.

### The mixer, measured instead of guessed

The mixer moved out of `player.js` into `demo-site/src/lib/mixer.js` so it can
be driven without a browser, and two rigs drive it:

- `tmp/hercules-probe/mix-lab.mjs` boots a real payload, paces the guest one
  60 Hz frame at a time, feeds either mixer the real event stream and writes the
  mix to a WAV. `MIX_VARIANT=legacy` runs the previous mixer, extracted verbatim
  from `HEAD` by `tmp/extract-legacy-mixer.py`.
- `tmp/hercules-probe/mix-synth.mjs` plays one looping sine into both mixers with
  a play cursor quantised to whole milliseconds, which is the only way to make a
  seam a number.

What that established, including where the guesses were wrong:

| | previous mixer | now |
|---|---|---|
| block rate | the loudest voice's, re-chosen every block | the context's, so Web Audio never resamples a block |
| interpolation | linear (about -8 dB at 11 kHz) | Catmull-Rom |
| playhead | re-seeded from the guest's cursor every block | the mixer's own, corrected by rate with a 2 ms dead band |
| `step` | `frequency / rate`, unclamped | clamped to 1/64 .. 64 |
| loop wrap | one subtraction | a modulo |
| output stage | `DynamicsCompressor`, -1 dB / 20:1 / 250 ms | a soft clip above 0.9 |
| stop latency | up to 140 ms (90 ms queued + 50 ms block) | about 60 ms |

Two of the suspicions did not survive the measurement. **Block seams are not the
distortion**: with a seamless synthetic loop, both mixers step across a boundary
by exactly one sample, and on 30 s of real Hercules content the same proportion
of boundaries (about 19%) stand out from their neighbourhood in both. And **the
limiter was not the distortion either** -- the real session peaks at 0.452, so
it never reached its -1 dB threshold. It is replaced anyway, because a -1 dB,
20:1 compressor with a 250 ms release is engaged for most of a busy mix and
breathes with every drum hit, which is not what DirectSound did: it clipped.

What is *not* claimed here is that the looping sound is fixed. The scripted
session never gets past the intro -- one looping ambience, stopped before the
end, peak 0.452, no NaN, no clipping -- and no browser is installed on this
machine to drive the real page. What the page can now say for itself is in the
diagnostic line once a second:

```text
audio: ctx=running voices=3 looping=1 oldest=12.4s#0x01980510 peak=0.45 clipped=0 clock=... oc=0
```

`oldest` is the age of the oldest looping voice in seconds of its own audio. A
looping voice has no end of its own, so only a `Stop` or a `Release` can take it
off the mixer: an age of minutes on a one-second buffer names the fault and the
object it is on without anyone having to listen for it.

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
