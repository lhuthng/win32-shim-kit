# Two kinds of port

A game reaches the browser in one of two ways, and the work is different enough
that mixing them up wastes months. This kit supports both but generates
different things for them.

## 1. Binary rehost

**Example: Doraemon Monopoly (`Doraemon.exe`, 1998) → `doraemon-monopoly-localization`.**

There is no source. The wasm module is an x86 interpreter holding the original
`.exe`; the game keeps executing its own 1998 machine code. Every import the
`.exe` makes — `KERNEL32!HeapAlloc`, `DDRAW!DirectDrawCreate`, a COM vtable
method on `IDirectDrawPalette` — is a point where the interpreter stops and asks
the host what to do.

So the host must answer **everything the game calls**, in the game's own terms:

- arguments are read off the guest stack with the declared byte count,
- results are written into guest memory at guest addresses,
- handles are plain numbers that the guest stores and passes back.

`kind: "shim"` maps describe exactly that. Most entries are small: answer 1, put
a pointer in the structure, report an error code. A few carry real behaviour —
heap allocation with reuse, palette entries, rectangle blits, the message pump.

A shim map is not documentation here; it *is* the port's compatibility surface.

## 2. Source port

**Example: GTA: Vice City → reVC compiled to wasm (`WinGames/reVC`, `wasm-revc-probe`, `hosting`).**

reVC is an open-source reimplementation. It is compiled to wasm directly, so the
game's own code runs natively and **Win32 is gone at the skeleton boundary**.
`src/skel/` (SDL2, GLFW, Android, win) is the only layer that ever mentioned
`HWND`, `MSG`, or `DirectSound`; a wasm build replaces that layer instead of
emulating it. The actual work is elsewhere:

- the platform layer: canvas presentation, input translation (`src/skel/events.cpp`),
  audio through OpenAL for wasm (`src/audio/sampman_oal.cpp`, `oal/channel.cpp`),
- data: the game's assets do not fit a browser download, so
  `make-blocks.py`, `slim-audio.py` and `slim-sfx.py` recompress them into
  blocks under `hosting/public/`.

For this kind of port a shim is usually the **wrong** answer: writing a Win32
emulation layer to keep a Win32 call alive is more work than replacing the
twenty lines that made the call.

`kind: "inventory"` maps describe exactly that: what the code used to ask Win32
for, and what answers it now. The value is regression protection — when a new
backend or a mobile UI is added, the map says what must not come back.

## Choosing

| Question | Binary rehost | Source port |
|---|---|---|
| Do you have the source? | no | yes |
| Does the game's own machine code run? | yes, interpreted | no, recompiled |
| Where does Win32 live? | everywhere in the guest | only in one skeleton layer |
| What is the main work? | shims + interpreter correctness | platform layer + asset pipeline |
| What does this kit produce? | Rust host module (`kind: shim`) | Markdown inventory (`kind: inventory`) |
| What is the failure mode? | a shim that answers plausibly but wrongly | a reintroduced platform dependency |

## Why one format

Both kinds need the same three facts per call: **which** function, **why** the
game depends on it, and **what the port does about it now**. Those are the schema
fields. Only the last one differs in form, which is why `stub` and `status` are
separate fields rather than one merged field.

The Doraemon port is the reference implementation of the shim half; the GTA VC
port is the reference for the inventory half. Neither needs the other's half.
