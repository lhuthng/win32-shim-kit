# Loading a payload

A payload is the machine plus a game, and no page. `tools/build-web.sh` writes it;
a loader fetches it and decides what runs and how it is shown. This page is that
contract: what a payload holds, the calls that boot an executable, and how to
launch the one you mean rather than the one that happens to be first.

The reference loader is a page in
`../doraemon-monopoly-localization/tools/web-client`; nothing here is specific to
it, and any runtime that can fetch bytes and call exports is equivalent.

## What a payload holds

```
<out>/host_wasm.wasm        the engine and its browser ABI (461,250 B in the reference build)
<out>/game/                 the install tree, baked
<out>/game/files.json       [{ "name": ..., "bytes": ... }], the list a loader fetches first
<out>/artifact.json         what the build is: executable, entry, sizes, import count
```

`artifact.json` from the reference Doraemon build:

```json
{
  "format": 2,
  "kit": { "version": "0.1.0", "abi": 1 },
  "screen": { "width": 640, "height": 480, "depth": 8 },
  "default": "Doraemon.exe",
  "executables": [
    { "name": "Doraemon.exe", "bytes": 876544, "entry": 4908548,
      "entry_hex": "0x004ae604", "image_bytes": 897024, "imports": 162 }
  ],
  "game": { "root": "game", "files": 55, "bytes": 146593385 },
  "module": { "name": "host_wasm.wasm", "bytes": 461250 }
}
```

Format 1 named one `executable`; format 2 lists every build and names a `default`.
`schema/artifact.schema.json` is the same format, and a release ships a copy, so a
loader can validate a payload without this repository.
**"Portable" describes the image, not the folder**: the baked executable is a
normal PE32 whose CD check and Setup-registry check have been lifted, so it runs
with nothing installed. It is not wrapped in a launcher, and no file starts
executing until a loader calls `wh_load`.

## The boot sequence

Five steps, in this order. Steps 2 and 3 are read *at* `wh_load`, not at
`wh_run`, so setting them afterwards has no effect on the image already mapped.

| # | Call | Notes |
| --- | --- | --- |
| 1 | `new WebAssembly.instantiate(bytes, {})` | zero imports; the module resolves the guest's imports itself |
| 2 | `wh_set_screen(w, h, depth)` | seeds the display mode the guest has before it calls `SetDisplayMode`; defaults to 640×480×8 |
| 3 | `wh_set_exe_name(ptr, len)` | what `GetModuleFileNameA` and `GetCommandLineA` answer; unset means `program.exe` |
| 4 | `wh_load(ptr, len)` | parses and maps the image, and builds its import table from its own import directory |
| 5 | `wh_provide_file(name, bytes)` × N | every other file the guest opens; `wh_provide_sized` stands in for one it only measures |

Two details that bite:

- **`wh_alloc` hands back one reused scratch buffer.** It is not a heap you can
  hold pointers into, so a name and its bytes go in a single allocation
  (`wh_alloc(name_len + data_len)`), and anything you need later must be copied
  out before the next call.
- **Loading drops supplied files.** `wh_load` and `wh_reset` create a new session,
  so re-provide the tree after either. `wh_reset` remaps the bytes from the last
  `wh_load` with the target it already built, so the name and screen survive it.

A loader, with presentation left to `present()`:

```js
const { instance } = await WebAssembly.instantiate(await wasmBytes, {});
const host = instance.exports;

const write = (bytes) => {                       // copy in, return the pointer
  const ptr = host.wh_alloc(bytes.length);
  new Uint8Array(host.memory.buffer, ptr, bytes.length).set(bytes);
  return ptr;
};
const message = () => {
  const ptr = host.wh_message_ptr(), len = host.wh_message_len();
  return ptr && len ? new TextDecoder().decode(new Uint8Array(host.memory.buffer, ptr, len)) : "";
};

// What this payload is: the build to boot and the mode to seed before the guest
// asks for one. `?exe=` overrides it; `default` is what the writer recorded.
const artifact = await (await fetch("./artifact.json")).json();
const roots = artifact.executables.map((entry) => entry.name);
const asked = new URLSearchParams(globalThis.location?.search ?? "").get("exe");
const boot = roots.includes(asked) ? asked : (artifact.default ?? roots[0]);
const exeBytes = gameFiles.find(([file]) => file === boot)[1];
host.wh_set_screen(artifact.screen.width, artifact.screen.height, artifact.screen.depth);
const name = new TextEncoder().encode(boot);
host.wh_set_exe_name(write(name), name.length);
if (host.wh_load(write(exeBytes), exeBytes.length) !== 0) throw new Error(message());

for (const [file, bytes] of gameFiles) {          // game/files.json, minus the exe
  const encoded = new TextEncoder().encode(file);
  const ptr = host.wh_alloc(encoded.length + bytes.length);
  const memory = new Uint8Array(host.memory.buffer);
  memory.set(encoded, ptr);
  memory.set(bytes, ptr + encoded.length);
  host.wh_provide_file(ptr, encoded.length, ptr + encoded.length, bytes.length);
}

let flips = host.wh_flips();                      // 0 = nothing presented yet
function frame(now) {
  host.wh_set_time(now);                          // the guest's clock, in ms
  const outcome = host.wh_run(250_000);           // the reference page's per-frame budget
  if (outcome === 2 || outcome === 3) {           // a shim gap or a fault: it names itself
    throw new Error(message());
  }
  if (host.wh_flips() !== flips) {                // present only when the guest presented
    flips = host.wh_flips();
    present();                                    // see below
  }
  if (outcome !== 1) requestAnimationFrame(frame); // 1 = halted (ExitProcess)
}
requestAnimationFrame(frame);
```

The budget is instructions, not time: the reference page spends 250,000 per
frame, and a headless check can pass millions at once because nothing is waiting
on a frame.

`wh_run` returns the outcome code: `0` budget reached, `1` halted, `2` a host call
with no answer, `3` fault, `-1` no session. Codes 2 and 3 never trap: the text in
`wh_message_*` names `DLL!Function` and its slot (`wh_slot`), which is what
distinguishes a missing shim from a game bug. `wh_notice_next` drains the host's
own notices: a stand-in file it could not satisfy, a device the browser lacks.

To present: `wh_memory_ptr() + wh_framebuffer_offset()`, indexed bytes,
`wh_framebuffer_width()` × `wh_framebuffer_height()` with `wh_framebuffer_pitch()`
as the row stride, mapped through `wh_palette(i)` (`0x00RRGGBB`, which a canvas
wants re-ordered) and `wh_palette_version()` to know when it changed. Presented
frames also come with audio: `wh_audio_event_count` / `wh_audio_events_ptr` before
`wh_audio_events_clear`, and PCM for a voice through `wh_audio_pcm_ptr` /
`wh_audio_pcm_len`.

## Which executable runs

The machine has no notion of a default program. `wh_load` takes bytes and
`wh_set_exe_name` takes a name; both are the loader's choice, and the name is not
cosmetic: it becomes the `Target`'s `exe_name`, so `GetModuleFileNameA` answers
with it and any exe-keyed behaviour sees it.

`artifact.json` records the builds and names a `default`, and a loader boots
`?exe=`, then `default`, then the first name in `executables`. That order is the
writer's decision, not the alphabet's: `bake` sorts the tree and names the first
root `.exe` unless `SHIMKIT_EXE` says otherwise, so a payload that ships two
builds can say which one a player gets.

Rules worth keeping:

- **One payload, one image.** Bake from a tree whose root holds the executable you
  intend to play. A second root `.exe` (a `SETUP.EXE`, a `PLAY.EXE` beside a
  launcher) makes "first alphabetically" silently decisive in both `bake` and a
  loader that guesses the same way. Nothing in the payload records the intent.
- **Root means discoverable, not loadable.** The convention is about finding an
  image in a tree. `wh_load` accepts bytes from anywhere: an executable in a
  subfolder, a file the user picked, a second language's build.
- **Name it if it can be ambiguous.** When a payload may hold more than one
  executable, take the name from your own selector (a `?exe=` parameter, a list in
  your own metadata) and check it is in the file list before loading, rather than
  inheriting the sorted-first rule from whichever tool baked the tree.
- **Switching images in one module works**, serially: `wh_set_screen` (if the
  screen differs) → `wh_set_exe_name` → `wh_load` → re-provide files → run. There
  is one session at a time; a second `wh_load` replaces the first, and the old
  target is abandoned (one leak per load, by design).
- **A portable image is the port's job, not the loader's.** A guest that still
  scans for a CD or reads `HKLM\SOFTWARE\Gameone\...` stops before it draws, and
  no loader flag fixes that: the image has to have those checks lifted, which is
  what a port's converter does when it converts an install.

## There is no registry of targets here

Nothing in the payload says which game it is beyond what the executable implies:
the import table is derived from the image at load time, and the shims answer by
`DLL!Function`. A loader therefore needs no per-game code to boot a build, and
should not assume any, since a build may be any PE32 the shims cover.

## Checking a payload

```sh
tools/build-web.sh                         # module only -> dist/, no game
tools/build-web.sh <install-dir> [out]     # ... plus game/ and artifact.json
bun ../doraemon-monopoly-localization/tools/web-client/browser-check.ts [install-dir] [payload] [--exe Name.exe]
```

`browser-check.ts` speaks the ABI above with no page and no browser: it fetches the
module over HTTP, loads the executable by name, hands over `game/files.json`, and
requires a presented frame with lit pixels. `page-check.ts` in the same folder
drives the reference page instead. Both fail a command rather than a tab, which is
the reason they exist next to the page.

## See also

- `using-the-engine.md`: what a port supplies (table, `Target`, shims) and the
  native `Session` API the ABI wraps.
- `two-kinds-of-port.md`: a binary rehost needs shims; a source port mostly does
  not.
- `discovery.md`: how to find which Win32 calls a target actually makes.
