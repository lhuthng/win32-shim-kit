// Loads a baked payload and drives the kit's browser ABI.
//
// The contract is `docs/loading-a-payload.md` in the kit: a payload is
// `host_wasm.wasm`, `artifact.json` and a `game/` folder with `files.json`.
// Nothing here is specific to one game; the executable and the screen come from
// the artifact, and the shims answer whatever DLL!Function the image imports.
//
// One detail shapes the code: `wh_alloc` is a single reused scratch buffer, so a
// name and its bytes go into one allocation and anything needed later is copied
// out before the next call.

import { createRenderer } from "./webgl2.js";
import { createMixer } from "./mixer.js";


/**
 * Exports that answer with a pointer into the module's memory.
 *
 * A pointer is an unsigned 32-bit offset, and `i32` arrives signed, so each of
 * these is widened before the page uses the answer as one.
 */
const POINTER_EXPORTS = [
  "wh_alloc",
  "wh_memory_ptr",
  "wh_message_ptr",
  "wh_saved_ptr",
  "wh_render_ptr",
  "wh_audio_samples",
  "wh_audio_pcm_ptr",
  // The event queue is a `Vec` in the host's heap, and the guest arena here is
  // 2047 MiB, so it always lands above 2 GiB: read signed it is negative, and a
  // negative offset is what a typed array rejects. Leaving it off this list made
  // `drainAudio` throw on the first event of the session -- the exception
  // escaped `frame`, which re-arms itself at the top, so the game went on
  // rendering in silence with nothing said about why.
  "wh_audio_events_ptr",
  "wh_framebuffer_offset",
];

/**
 * The page's audio context, one for the whole page.
 *
 * It lives here rather than inside `boot` because a browser only lets a user
 * gesture start a context, and `boot` runs after an `await` -- by then the
 * click that started the game no longer counts and the context it creates stays
 * suspended. A game whose first screen has music then plays that music into a
 * context that never runs: the menu is silent and the sound only arrives at the
 * first click on the frame, which is what resumes it.
 */
let audio = null;

/**
 * Starts the page's audio from a click.
 *
 * Called by the page's own controls, which are gestures the browser accepts,
 * and safe to call before `boot`: the context is created on the first call and
 * every session mixes into it.
 */
export function resumeAudio() {
  audioContext();
}

/** The context to mix into, created on first use. */
function audioContext() {
  if (!audio) {
    const Constructor = window.AudioContext ?? window.webkitAudioContext;
    if (!Constructor) return null;
    audio = new Constructor();
  }
  if (audio.state === "suspended") audio.resume().catch(() => {});
  return audio;
}

/** PC set 1 scancodes, which are also the DIK_* names the host's input keeps. */
const SCANCODES = {
  Escape: 0x01,
  Digit1: 0x02, Digit2: 0x03, Digit3: 0x04, Digit4: 0x05, Digit5: 0x06,
  Digit6: 0x07, Digit7: 0x08, Digit8: 0x09, Digit9: 0x0a, Digit0: 0x0b,
  Minus: 0x0c, Equal: 0x0d, Backspace: 0x0e, Tab: 0x0f,
  KeyQ: 0x10, KeyW: 0x11, KeyE: 0x12, KeyR: 0x13, KeyT: 0x14,
  KeyY: 0x15, KeyU: 0x16, KeyI: 0x17, KeyO: 0x18, KeyP: 0x19,
  BracketLeft: 0x1a, BracketRight: 0x1b,
  Enter: 0x1c, ControlLeft: 0x1d, KeyA: 0x1e, KeyS: 0x1f, KeyD: 0x20,
  KeyF: 0x21, KeyG: 0x22, KeyH: 0x23, KeyJ: 0x24, KeyK: 0x25, KeyL: 0x26,
  Semicolon: 0x27, Quote: 0x28, Backquote: 0x29, ShiftLeft: 0x2a,
  Backslash: 0x2b, KeyZ: 0x2c, KeyX: 0x2d, KeyC: 0x2e, KeyV: 0x2f,
  KeyB: 0x30, KeyN: 0x31, KeyM: 0x32,
  Comma: 0x33, Period: 0x34, Slash: 0x35, ShiftRight: 0x36,
  NumpadMultiply: 0x37, AltLeft: 0x38, Space: 0x39, CapsLock: 0x3a,
  F1: 0x3b, F2: 0x3c, F3: 0x3d, F4: 0x3e, F5: 0x3f,
  F6: 0x40, F7: 0x41, F8: 0x42, F9: 0x43, F10: 0x44,
  NumLock: 0x45, ScrollLock: 0x46,
  Numpad7: 0x47, Numpad8: 0x48, Numpad9: 0x49, NumpadSubtract: 0x4a,
  Numpad4: 0x4b, Numpad5: 0x4c, Numpad6: 0x4d, NumpadAdd: 0x4e,
  Numpad1: 0x4f, Numpad2: 0x50, Numpad3: 0x51, Numpad0: 0x52,
  NumpadDecimal: 0x53, ControlRight: 0x1d, AltRight: 0x38,
  ArrowUp: 0x48, ArrowDown: 0x50, ArrowLeft: 0x4b, ArrowRight: 0x4d,
  Insert: 0x52, Delete: 0x53, Home: 0x47, End: 0x4f,
  PageUp: 0x49, PageDown: 0x51,
};

/** Keys the page must not scroll or type with while the game has them. */
const CAPTURED = new Set([
  "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Space", "Tab",
  "Backspace", "Enter", "Slash", "Quote",
]);

function scancode(code) {
  return SCANCODES[code] ?? 0;
}

function decodeMessage(host) {
  const pointer = (host.wh_message_ptr() >>> 0);
  const length = host.wh_message_len();
  if (!pointer || !length) return "";
  return new TextDecoder().decode(new Uint8Array(host.memory.buffer, pointer, length));
}

function decodeNotices(host) {
  const notices = [];
  while (host.wh_notice_next() === 1) notices.push(decodeMessage(host));
  return notices;
}

// ---- saving ---------------------------------------------------------------

/** Where a game's saved files live in this browser: one key per payload. */
const SAVE_PREFIX = "win32-shim-kit:saves:";

function toBase64(bytes) {
  let text = "";
  // In chunks, because a spread of a whole file as an argument list throws at a
  // few thousand bytes and a save can be larger than that.
  for (let at = 0; at < bytes.length; at += 0x8000) {
    text += String.fromCharCode.apply(null, bytes.subarray(at, at + 0x8000));
  }
  return btoa(text);
}

function fromBase64(text) {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

/**
 * The files this browser kept for a game.
 *
 * Storage is best effort. A browser in private mode, or one that has run out of
 * room, throws on the way in and on the way out, and a game that cannot save is
 * still a game: the answer is an empty folder and the run goes on.
 */
function storedSaves(key) {
  try {
    const raw = localStorage.getItem(SAVE_PREFIX + key);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Object.entries(parsed?.files ?? {}).map(([name, encoded]) => ({
      name,
      bytes: fromBase64(encoded),
    }));
  } catch {
    return [];
  }
}

function storeSaves(key, files) {
  try {
    const encoded = {};
    for (const file of files) encoded[file.name] = toBase64(file.bytes);
    localStorage.setItem(SAVE_PREFIX + key, JSON.stringify({ version: 1, files: encoded }));
  } catch {
    // No storage, or no room: the game plays, it just forgets.
  }
}

/**
 * The last segment of a path, lowercased.
 *
 * A payload names its saves the way a folder lists them, but the guest builds
 * paths the way Windows does: `Doukutsu.exe\Profile.dat` here, and whatever
 * directory a real install sits in elsewhere. The folder a guest puts in front
 * of a name is its own business, so a listing compares leaves.
 */
export function leafName(path) {
  return path.toLowerCase().split(/[\\/]/).pop();
}

/**
 * Whether a file is one the payload calls a save. `*` takes everything, and a
 * payload that names nothing keeps everything the guest writes -- which is the
 * case where the guest's own folder is the save.
 */
export function keptBy(patterns, name) {
  if (!patterns || patterns.length === 0) return true;
  const leaf = leafName(name);
  return patterns.some((pattern) => pattern === "*" || leafName(pattern) === leaf);
}

/**
 * Boots one payload.
 *
 * `payloadUrl` is the folder holding `host_wasm.wasm`, `artifact.json` and
 * `game/`. The returned handle is the only thing a page needs: run the frame
 * loop itself (it is started here), feed it keys and pointer, and `stop()` when
 * the demo goes away.
 */
export async function boot({
  canvas,
  payloadUrl,
  /** Key the game's saved files are kept under, one per game. */
  saveKey = payloadUrl,
  /**
   * The files this game saves, from the payload's own listing. A game that
   * keeps its progress in `Profile.dat` should not carry a rewritten settings
   * file over to the next build, so only these come back and only these are
   * stored. `null` keeps whatever the guest writes.
   */
  saves = null,
  /**
   * Frames per second to hold the guest to, for an embedder that has its own
   * idea of the rate. A payload's `config.json` says what the game itself was
   * written for, and that is read below; this only fills in when it is silent.
   */
  fps = null,
  onStatus = () => {},
  onDiagnostic = () => {},
}) {
  const base = payloadUrl.endsWith("/") ? payloadUrl : `${payloadUrl}/`;
  onStatus("fetching payload");

  const [artifact, fileList, settings] = await Promise.all([
    // The manifest is the cache key for everything else, so a stale copy would
    // silently replay an old module: never serve it from cache.
    fetch(`${base}artifact.json`, { cache: "no-store" }).then((response) => {
      if (!response.ok) throw new Error(`artifact.json: HTTP ${response.status}`);
      return response.json();
    }),
    fetch(`${base}game/files.json`, { cache: "no-store" }).then((response) => {
      if (!response.ok) throw new Error(`game/files.json: HTTP ${response.status}`);
      return response.json();
    }),
    // Optional, and missing on every payload baked before it existed: the
    // settings that belong to this game rather than to the page.
    fetch(`${base}config.json`)
      .then((response) => (response.ok ? response.json() : {}))
      .catch(() => ({})),
  ]);

  // The module's URL never changes, so a browser keeps whatever build it first
  // fetched -- a page reload replays the old shims without saying so, which
  // reads as a fix that did not take. The artifact's build stamp is a hash of
  // the module, so it is the cache key: the same build is fetched once, and a
  // rebuilt payload cannot be missed -- not even one whose size did not move.
  const build = artifact.module?.build ?? artifact.module?.bytes ?? 0;
  // Revalidated on every load, not merely keyed by the stamp. A payload whose
  // module was replaced without rebaking its artifact keeps the same stamp, and
  // the browser then replays whatever build it cached first while every rebuild
  // looks like a fix that did not take.
  const wasmResponse = await fetch(`${base}host_wasm.wasm?v=${build}`, {
    cache: "no-cache",
  }).then((response) => {
    if (!response.ok) throw new Error(`host_wasm.wasm: HTTP ${response.status}`);
    return response.arrayBuffer();
  });
  if (
    typeof artifact.module?.bytes === "number" &&
    artifact.module.bytes !== wasmResponse.byteLength
  ) {
    // The artifact names a size the served module does not have, so the payload
    // was rebuilt and its artifact was not. Saying so beats running a module
    // nobody meant to run and calling it the new one.
    console.warn(
      `host_wasm.wasm is ${wasmResponse.byteLength} bytes but artifact.json says ${artifact.module.bytes}: the baked artifact is stale`,
    );
  }

  const builds = Array.isArray(artifact.executables)
    ? artifact.executables
    : artifact.executable
      ? [artifact.executable]
      : [];
  const executableName =
    artifact.default ??
    builds[0]?.name ??
    fileList.find((file) => /\.exe$/i.test(file.name))?.name;
  if (!executableName) throw new Error("the payload names no executable");

  onStatus("fetching the game");
  const executableEntry = fileList.find(
    (file) => file.name.toLowerCase() === executableName.toLowerCase(),
  );
  if (!executableEntry) throw new Error(`files.json does not list ${executableName}`);
  const executable = new Uint8Array(
    await (await fetch(`${base}game/${encodeURIComponent(executableEntry.name)}`)).arrayBuffer(),
  );

  const { instance } = await WebAssembly.instantiate(wasmResponse, {});
  // A wasm `i32` result reaches JavaScript signed, so a pointer at 2 GiB or
  // above arrives negative -- and a negative offset is what a typed array
  // rejects with "offset is out of bounds". The guest arena is free to reach
  // that, and the buffers the host allocates above it do too, so every export
  // that answers with a pointer is read as unsigned once, here, rather than at
  // each of the places a pointer is used.
  const host = { ...instance.exports };
  for (const name of POINTER_EXPORTS) {
    const raw = host[name];
    if (typeof raw === "function") host[name] = (...args) => raw(...args) >>> 0;
  }
  // TEMP DIAGNOSTIC: the live host exports on the window, so a frozen session
  // can be probed from the console. Remove when done.
  if (typeof window !== "undefined") window.__wh = host;

  const screen = artifact.screen ?? { width: 640, height: 480, depth: 8 };
  host.wh_set_screen(screen.width, screen.height, screen.depth);

  // A payload says which Windows it wants to be: a launcher that refuses
  // anything newer than Windows 98 needs this before it reads the platform. A
  // module older than the export answers with its own default instead, which
  // is the same platform the bake recorded unless someone overrode it.
  const windows = artifact.windows;
  if (windows && typeof host.wh_set_windows === "function") {
    host.wh_set_windows(windows.major, windows.minor, windows.build, windows.platform);
  }

  const name = new TextEncoder().encode(executableName);
  const namePointer = host.wh_alloc(name.length) >>> 0;
  new Uint8Array(host.memory.buffer, namePointer, name.length).set(name);
  host.wh_set_exe_name(namePointer, name.length);

  const imagePointer = host.wh_alloc(executable.length) >>> 0;
  new Uint8Array(host.memory.buffer, imagePointer, executable.length).set(executable);
  if (host.wh_load(imagePointer, executable.length) !== 0) {
    throw new Error(decodeMessage(host) || "the executable did not load");
  }

  onStatus("providing files");
  let provided = 0;
  for (const file of fileList) {
    if (file.name.toLowerCase() === executableName.toLowerCase()) continue;
    const response = await fetch(`${base}game/${encodeURIComponent(file.name)}`);
    if (!response.ok) continue; // a file the payload lists but does not ship
    const contents = new Uint8Array(await response.arrayBuffer());
    const encoded = new TextEncoder().encode(file.name);
    // Name and bytes in one allocation: `wh_alloc` reuses one buffer.
    const pointer = host.wh_alloc(encoded.length + contents.length) >>> 0;
    const memory = new Uint8Array(host.memory.buffer);
    memory.set(encoded, pointer);
    memory.set(contents, pointer + encoded.length);
    if (host.wh_provide_file(pointer, encoded.length, pointer + encoded.length, contents.length) === 0) {
      provided += 1;
    }
  }
  // The guest's own files from the last session, restored before it runs: a game
  // reads its save at boot and must find it where it left it.
  for (const file of storedSaves(saveKey)) {
    if (!keptBy(saves, file.name)) continue;
    const name = new TextEncoder().encode(file.name);
    const pointer = host.wh_alloc(name.length + file.bytes.length) >>> 0;
    const memory = new Uint8Array(host.memory.buffer);
    memory.set(name, pointer);
    memory.set(file.bytes, pointer + name.length);
    host.wh_provide_saved(pointer, name.length, pointer + name.length, file.bytes.length);
  }
  for (const notice of decodeNotices(host)) onDiagnostic(notice);

  // ---- presentation -------------------------------------------------------

  const context = canvas.getContext("2d", { alpha: false });
  const palette = new Uint32Array(256);

  // A Direct3D 8 payload does not blit a framebuffer: the host records what the
  // device was asked to do and this page replays it through WebGL2. The replay
  // goes into a canvas of its own and is copied into the frame canvas, so a 2D
  // game keeps the `putImageData` path it has always used and neither kind of
  // payload has to be declared up front -- the device's existence says which it
  // is.
  let scene = null;
  function direct3d() {
    const width = host.wh_render_width ? host.wh_render_width() : 0;
    if (!width) return null;
    if (scene === null) {
      try {
        scene = createRenderer({ host, onDiagnostic });
      } catch (error) {
        onDiagnostic(`WebGL2: ${error.message ?? error}`);
        // A refusal is remembered as `false`, so a browser without WebGL2 is not
        // asked again every frame.
        scene = false;
      }
    }
    return scene || null;
  }
  let paletteVersion = -1;
  let frameSize = { width: 0, height: 0 };
  // One ImageData, reused for every presented frame of the same size. Allocating
  // a fresh 300 KB buffer and a fresh Uint32Array view on each present was pure
  // garbage: putImageData reads the buffer we hand it, and every pixel is
  // rewritten below anyway.
  let image = null;
  let imageOut = null;
  let imageSize = { width: 0, height: 0 };

  function refreshPalette() {
    const version = host.wh_palette_version();
    if (version === paletteVersion) return;
    paletteVersion = version;
    for (let index = 0; index < 256; index += 1) {
      const colour = host.wh_palette(index);
      // `wh_palette` is 0x00RRGGBB; the canvas wants 0xAABBGGRR.
      palette[index] =
        0xff000000 |
        ((colour & 0xff) << 16) |
        (((colour >> 8) & 0xff) << 8) |
        ((colour >> 16) & 0xff);
    }
  }

  function present() {
    const renderer = direct3d();
    if (renderer) {
      // `render` drains the stream, so it runs exactly once per presented frame:
      // an unreplayed record is a command that never reached the screen.
      if (!renderer.render()) return;
      const { width, height } = renderer.size();
      if (canvas.width !== width || canvas.height !== height) {
        canvas.width = width;
        canvas.height = height;
      }
      frameSize = { width, height };
      context.drawImage(renderer.canvas, 0, 0, width, height);
      return;
    }

    const width = host.wh_framebuffer_width();
    const height = host.wh_framebuffer_height();
    const pitch = host.wh_framebuffer_pitch();
    // `wh_framebuffer_offset` is relative to the guest's image base, and
    // `wh_memory_ptr` is where that base sits inside the module's linear memory.
    const offset = (host.wh_memory_ptr() >>> 0) + host.wh_framebuffer_offset();
    if (!width || !height || !pitch || !host.wh_framebuffer_offset()) return;

    const depth = host.wh_framebuffer_depth ? host.wh_framebuffer_depth() : 8;
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
    frameSize = { width, height };

    const memory = new Uint8Array(host.memory.buffer);
    if (!image || imageSize.width !== width || imageSize.height !== height) {
      image = context.createImageData(width, height);
      imageOut = new Uint32Array(image.data.buffer);
      imageSize = { width, height };
    }
    const out = imageOut;
    // The palette changes rarely; asking the module for its revision 240 times
    // per frame was 240 wasm calls to learn the same answer. The depth branch
    // likewise belongs outside the row loop -- it cannot change mid-frame.
    if (depth === 8) refreshPalette();

    if (depth === 8) {
      for (let y = 0; y < height; y += 1) {
        const row = offset + y * pitch;
        const line = y * width;
        for (let x = 0; x < width; x += 1) out[line + x] = palette[memory[row + x]];
      }
    } else if (depth === 16) {
      for (let y = 0; y < height; y += 1) {
        const row = offset + y * pitch;
        const line = y * width;
        // Five and six bit channels widen by repeating their top bits, which is
        // how a display expands them: shifting alone caps white at 248,252,248,
        // so every bright pixel comes out a shade dull and a shade green.
        for (let x = 0; x < width; x += 1) {
          const at = row + x * 2;
          const value = memory[at] | (memory[at + 1] << 8);
          const red = (value >> 11) & 0x1f;
          const green = (value >> 5) & 0x3f;
          const blue = value & 0x1f;
          out[line + x] =
            0xff000000 |
            (((blue << 3) | (blue >> 2)) << 16) |
            (((green << 2) | (green >> 4)) << 8) |
            ((red << 3) | (red >> 2));
        }
      }
    } else {
      for (let y = 0; y < height; y += 1) {
        const row = offset + y * pitch;
        const line = y * width;
        for (let x = 0; x < width; x += 1) {
          const at = row + x * 4;
          // Little-endian BGRX, which is SDL's 32-bit XRGB surface.
          out[line + x] =
            0xff000000 |
            (memory[at] << 16) |
            (memory[at + 1] << 8) |
            memory[at + 2];
        }
      }
    }
    context.putImageData(image, 0, 0);
  }

  // ---- audio --------------------------------------------------------------

  /**
   * The mixer, and the queue that carries its blocks to the sound card.
   *
   * The mixer is `mixer.js`: it owns the voice table and renders samples out of
   * guest memory. It is a module rather than part of this closure so that
   * `tmp/hercules-probe/mix-lab.mjs` can drive the real thing without a browser
   * and write the result to a WAV. This half is only the transport -- a
   * context, a queue of rendered blocks, and the pause switch.
   */
  const mixer = createMixer(host);
  /** Seconds of audio rendered into one queued block. */
  const CHUNK = 0.025;
  /**
   * How far ahead of the audio clock the queue is kept filled.
   *
   * This is also the delay between the guest stopping a sound and the last of
   * that sound being heard, because a block already handed to the sound card
   * cannot be recalled. It was 90 ms of 50 ms blocks, so a stop took up to
   * 140 ms to become audible -- long enough to be heard as a sound that "will
   * not go off when it should".
   */
  const AHEAD = 0.06;

  /** Peak and clipped-sample count of the last block, for the diagnostic. */
  let audioPeak = 0;
  let audioClipped = 0;
  /** The first exception the audio path threw, so it is reported once. */
  let audioFault = null;

  /**
   * The bus every voice mixes into.
   *
   * A plain gain, because the mix is soft-clipped inside `mixInto` where the
   * peak is known. See `softClip` in `mixer.js` for why the
   * `DynamicsCompressorNode` that used to stand here was itself the distortion.
   */
  let master = null;
  function output(context) {
    if (master) return master;
    master = context.createGain();
    master.connect(context.destination);
    return master;
  }

  /** Where the queue ends, on the audio clock. */
  let queued = 0;

  /**
   * Renders one block of the mix and queues it.
   *
   * The block is rendered at the context's own rate so that Web Audio never
   * resamples it. That is not an optimisation: a block at any other rate is
   * resampled by the browser *per block*, with the resampler's phase restarting
   * at every boundary, and the rate used to be picked per block from whichever
   * voice was loudest -- so it could change under the resampler as well.
   */
  function mixBlock(context) {
    const rate = context.sampleRate || 48_000;
    const frames = Math.max(1, Math.round(CHUNK * rate));
    const buffer = context.createBuffer(2, frames, rate);
    const left = buffer.getChannelData(0);
    const right = buffer.getChannelData(1);
    const mixed = mixer.mixInto(left, right, rate);
    audioPeak = mixed.peak;
    audioClipped = mixed.clipped;
    const source = context.createBufferSource();
    source.buffer = buffer;
    source.connect(output(context));
    source.start(queued);
    queued += frames / rate;
  }

  /**
   * Keeps a block or two queued ahead of the audio clock.
   *
   * Called once a frame, so the guest's samples are in memory before they are
   * mixed, and a block is short enough that a stop or a retune is heard within
   * a frame or two.
   */
  function pumpAudio() {
    const context = audioContext();
    if (!context || context.state === "suspended") return;
    // Nothing queued, or the clock ran past the queue -- a pause, a stall, the
    // first block. Start again from now rather than trying to catch up.
    if (queued < context.currentTime + 0.005) queued = context.currentTime + 0.015;
    for (let guard = 0; guard < 8 && queued < context.currentTime + AHEAD; guard += 1) {
      mixBlock(context);
    }
  }

  function drainAudio() {
    try {
      // Every frame, not only on a present: the guest writes its samples after
      // `Play`, and a sound must not wait for the next flip to be heard.
      mixer.applyEvents();
      pumpAudio();
    } catch (error) {
      // A throw in here used to escape `frame` silently. `frame` re-arms itself
      // at the top, so the game kept rendering, the sound was gone, and nothing
      // anywhere said why -- which is exactly how the missing pointer widening
      // above survived three rounds of looking at this. Say it once.
      if (!audioFault) {
        audioFault = error;
        onDiagnostic(`audio: ${error?.message ?? error}`);
      }
    }
  }

  /**
   * The mixer and the guest's clock follow the tab and the pause button.
   *
   * A hidden tab gets no animation frames and a paused one runs none, so the
   * guest stops and nothing calls `drainAudio` -- but `AudioContext` is the
   * browser's, not the page's, and on macOS Brave and Chrome leave it running
   * while the page is in the background. A streaming game refills its music a
   * half buffer at a time, so a looping `AudioBufferSource` then repeats
   * whatever it holds with nobody refilling it, which is heard as one short
   * fragment stuck on a loop. Worse, the context can be suspended on the way out
   * and nothing here ever asked it to resume, so the frozen note is still there
   * on the way back.
   *
   * Suspending it ourselves makes both directions explicit: silence while the
   * guest is not running, and the same position -- loops, playheads, gain -- when
   * it is. The audio clock stops with it, which is what keeps a refill after the
   * pause seeking to the right place rather than to where the wall clock says.
   *
   * The guest's clock stops too. A pause is not time passing for the game: it is
   * still the same frame when it resumes, and a `GetTickCount` that jumped by the
   * pause would make every one of its waits return at once and skip ahead.
   */
  function notRunning() {
    // `document` is absent in the headless harness that boots this loader.
    return paused || (typeof document !== "undefined" && document.hidden);
  }

  function followPause() {
    if (notRunning()) {
      if (frozenAt) return;
      frozenAt = performance.now();
      if (audio) audio.suspend().catch(() => {});
      return;
    }
    if (!frozenAt) return;
    const away = performance.now() - frozenAt;
    frozenAt = 0;
    // The page's clock keeps running; the guest's does not. Everything that
    // counts in the guest's time is moved by the same amount, so the frame that
    // follows a pause is the next frame of the game and not a jump.
    clockShift += away;
    origin += away;
    if (audio) audio.resume().catch(() => {});
  }
  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", followPause);
  }

  // ---- the loop -----------------------------------------------------------

  /**
   * The guest's files as they stand, copied out of module memory.
   *
   * The name goes through the message buffer, which is what `wh_saved_name`
   * writes; the bytes are read where the guest left them, the same way the
   * framebuffer is.
   */
  function savedFiles() {
    const files = [];
    const count = host.wh_saved_count();
    for (let index = 0; index < count; index += 1) {
      const nameLength = host.wh_saved_name(index);
      if (!nameLength) continue;
      const name = decodeMessage(host);
      const length = host.wh_saved_len(index);
      const pointer = host.wh_saved_ptr(index);
      const bytes = length
        ? new Uint8Array(host.memory.buffer, pointer, length).slice()
        : new Uint8Array(0);
      files.push({ name, bytes });
    }
    return files;
  }

  let savedVersion = host.wh_saved_version();
  let savedAt = 0;

  /**
   * Copies the guest's files into browser storage.
   *
   * At most once a second while it runs: a game that writes its settings every
   * frame would otherwise hand the browser a write per frame, and a second is
   * finer than a save needs to be.
   */
  function persistSaves(force = false) {
    const version = host.wh_saved_version();
    if (!force && version === savedVersion) return;
    if (!force && performance.now() - savedAt < 1000) return;
    savedVersion = version;
    savedAt = performance.now();
    storeSaves(saveKey, savedFiles().filter((file) => keptBy(saves, file.name)));
  }

  let running = true;
  let paused = false;
  let flips = host.wh_flips();
  // Writes to the presented surface that no Flip counted: a guest that blits
  // straight onto the primary never moves the flip counter.
  let version = host.wh_frame_version();
  let instructions = 0;
  /**
   * What the last `wh_run` chunk cost in milliseconds. The frame loop decides
   * whether it can afford another chunk from this, because the cost of a chunk
   * is not a constant: it is a budget in instructions retired, and a guest that
   * enters a native replacement retires many times that budget in one call.
   */
  let chunkCost = 2;
  let handle = null;
  let lastStatus = "";

  // The pace, counted rather than timed: how many frames *should* have been run
  // by now is `elapsed * fps`, and a rAF callback runs one if it is behind. A
  // 50 fps target on a 60 Hz display is then five frames every six callbacks
  // on average -- a plain "has the interval elapsed" test would round down to
  // 30, because 20 ms never fits inside two 16.7 ms slots.
  const rate = Number(settings.fps ?? fps);
  const paced = Number.isFinite(rate) && rate > 0;
  // Seeded one interval in the past, so the first frame is due at once.
  let origin = performance.now() - (paced ? 1000 / rate : 0);
  let presented = 0;
  /** Milliseconds of pause the guest has not been told about. */
  let clockShift = 0;
  /** TEMP DIAGNOSTIC: paces the once-a-second mixer line in the pane. */
  let diagTick = 0;
  /** When the guest stopped running, or 0 while it is. */
  let frozenAt = 0;

  function status(text) {
    if (text === lastStatus) return;
    lastStatus = text;
    onStatus(text);
  }

  function frame(now) {
    if (!running) return;
    handle = requestAnimationFrame(frame);
    if (paused) return;

    if (paced) {
      const due = Math.floor(((now - origin) * rate) / 1000);
      // A stall -- the boot, a backgrounded tab, one very slow frame -- leaves
      // the count far behind the schedule. The pace resumes from where it is
      // rather than sprinting to make up the difference, which would play a
      // burst at the display rate the moment the game finished loading.
      if (presented < due - 1) presented = due - 1;
      // Not due yet: leave the guest where it is and keep the audio flowing.
      // The clock still moves, so the guest's own timers see real time either way.
      if (presented >= due) {
        host.wh_set_time(now - clockShift);
        drainAudio();
        return;
      }
    }

    host.wh_set_time(now - clockShift);
    const started = performance.now();
    let outcome = 0;
    // A frame's budget is wall time, but the clock can only be consulted between
    // chunks of guest work, and a chunk is not small. At 250,000 instructions one
    // `wh_run` measured 7.5 ms at the median on Hercules' menu and 18 ms at the
    // tail: the budget is a count of instructions retired, and a run of native
    // sprite rows retires several times its budget in one call. So "stop when the
    // slice is used up", checked *after* each chunk, really meant "stop somewhere
    // between the slice and the slice plus a chunk" -- and more than half the
    // callbacks then ran past the 16.7 ms display slot. A callback that crosses a
    // vsync does not shorten the next one; it loses it. Measured on the menu that
    // was 39 rAF callbacks a second instead of 60, and the display interval
    // swinging between 20 and 54 ms (p10 to p90) on a game that is otherwise a
    // steady 30 fps.
    //
    // So the chunk is small enough that the overshoot is a fraction of a vsync,
    // and the clock is consulted *before* the chunk that would overshoot, using
    // the cost of the previous one. That adapts by itself: a slower machine has
    // costlier chunks, so it asks for fewer of them. The same measurement then
    // gives 60 callbacks a second, none over the slot, and 93% of frames landing
    // on exactly two vsyncs instead of 43%.
    //
    // A paced game gets its own interval as the slice. It waits for real time
    // inside the guest -- `Sleep` against `GetTickCount`, which is how a game
    // written for 50 made its own frame rate -- so a slice shorter than a frame
    // cuts the wait in half and costs a frame. An unpaced one keeps the 12 ms
    // that has always been enough to keep a page responsive.
    //
    // A payload can raise that ceiling with `sliceMs` in config.json. An
    // interpreter-bound game spends its whole slice in wh_run, so 12 ms out of
    // every 16.7 ms rAF is only ~72% of the CPU. The ceiling has to stay clear of
    // a whole vsync, or the callback delays rAF itself and the extra work stops
    // buying frames: 16 ms was measured as 19.9 ms callbacks and 67% of them over
    // the slot, because the chunk granularity of the day was 4 ms.
    const CHUNK = 64_000;
    const configured = Number(settings.sliceMs);
    const slice = configured > 0 ? configured : paced ? Math.max(12, 1000 / rate) : 12;
    try {
      for (let chunk = 0; chunk < 64; chunk += 1) {
        // Half the slice is the most a chunk may be assumed to cost, so a single
        // hitch cannot make the loop refuse to run at all: a cost estimated at
        // more than the whole slice would stop it before its first chunk, and
        // nothing would ever run again to bring the estimate back down.
        const reserved = Math.min(chunkCost, slice * 0.5);
        if (performance.now() - started + reserved > slice) break;
        const at = performance.now();
        outcome = host.wh_run(CHUNK);
        chunkCost = performance.now() - at;
        instructions += CHUNK;
        if (outcome !== 0) break;
        if (host.wh_flips() !== flips) break;
        if (host.wh_frame_version() !== version) break;
      }
    } catch (error) {
      // A wasm trap would otherwise kill this frame and every one after it, with
      // nothing said. Name it once, in the diagnostic pane.
      onDiagnostic(`trap: ${error.message ?? error}`);
      throw error;
    }

    if (host.wh_flips() !== flips || host.wh_frame_version() !== version) {
      flips = host.wh_flips();
      version = host.wh_frame_version();
      presented += 1;
      present();
    }
    // Every frame, not only on a present: the guest writes its samples after
    // `Play`, and a sound must not wait for the next flip to be heard.
    drainAudio();

    // The guest has stopped -- `ExitProcess`, a fault -- and a real machine goes
    // silent with it: the buffers belong to the process, so the mixer dies with
    // the process. Nothing here does that on its own. This mixer is fed from
    // guest memory and keeps rendering whatever it holds, so the last sounds of
    // the session repeat forever after the game has gone, and a looping one is
    // the one that is still audible. Hercules leaves two of them: 1.57-second
    // ambience loops it never stops because it exits instead of winding its
    // sound system down.
    if (outcome !== 0 && mixer.size) {
      mixer.clear();
      onDiagnostic(`audio: guest halted (${outcome}); mixer cleared`);
    }

    // The mixer's live state, once a second, in the diagnostic pane: a session
    // that is silent names which link of the chain broke.
    diagTick += 1;
    if (diagTick >= 60) {
      diagTick = 0;
      const guestClock = host.wh_clock_ms ? host.wh_clock_ms() : -1;
      const instr = host.wh_instructions ? host.wh_instructions() : -1;
      // `looping` is how many voices the guest has left looping, and `oldest`
      // is the age of the oldest of them in seconds of its own audio. A looping
      // voice has no end of its own, so only a Stop or a Release can take it off
      // the mixer: an age of minutes on a one-second buffer is the whole of "a
      // sound effect that will not go off", and it names the fault without
      // anyone having to listen for it.
      const held = mixer.stats();
      const oldest = held.looping ? ` oldest=${held.longest.toFixed(1)}s#${held.voice}` : "";
      const level = `peak=${audioPeak.toFixed(2)} clipped=${audioClipped}`;
      onDiagnostic(
        audio
          ? `audio: ctx=${audio.state} voices=${held.count} looping=${held.looping}${oldest} ${level} aheadMs=${Math.round((queued - audio.currentTime) * 1000)} clock=${Math.round(guestClock)} instr=${instr} oc=${outcome}`
          : `audio: no context clock=${Math.round(guestClock)} instr=${instr} oc=${outcome}`,
      );
    }

    for (const notice of decodeNotices(host)) {
      onDiagnostic(notice);
    }
    // A write to a file the guest owns is a save; nothing else says so.
    persistSaves();

    if (outcome === 2) {
      status("stopped: a Win32 call has no shim");
      onDiagnostic(decodeMessage(host));
      running = false;
      return;
    }
    if (outcome === 3) {
      status("stopped: fault");
      onDiagnostic(decodeMessage(host));
      running = false;
      return;
    }
    if (outcome === 1) {
      status("the program exited");
      running = false;
      return;
    }
    // The depth is the surface the guest actually draws on, not the one the
    // artifact seeded: a game is free to ask for another mode, and Cave Story
    // does -- the payload says 8 and the game runs 16.
    const depth = host.wh_framebuffer_depth ? host.wh_framebuffer_depth() : 8;
    // A 3D payload presents through the device, whose back buffer is the size
    // that matters; `ddraw` never gets one.
    const width = host.wh_render_width ? host.wh_render_width() : 0;
    status(
      width
        ? `running — ${width}×${host.wh_render_height()}×32`
        : `running — ${host.wh_framebuffer_width()}×${host.wh_framebuffer_height()}×${depth}`,
    );
  }

  present();
  onStatus(`running — ${screen.width}×${screen.height}`);
  if (host.wh_import_count) {
    onDiagnostic(`${host.wh_import_count()} imports, ${provided} file(s) provided`);
  }
  handle = requestAnimationFrame(frame);

  // ---- input --------------------------------------------------------------

  function key(code, down) {
    const dik = scancode(code);
    if (!dik) return false;
    host.wh_key(dik, down ? 1 : 0);
    return true;
  }

  function pointer(x, y) {
    host.wh_pointer(Math.round(x), Math.round(y));
  }

  /**
   * Movement while the pointer is locked.
   *
   * A locked pointer has no position, only a delta, which is exactly what a game
   * reads from DirectInput's mouse or from `GetCursorPos` after parking it: the
   * host adds the delta to the cursor the guest thinks it has, so aiming keeps
   * working past the edges of the window instead of stopping there.
   */
  function pointerMove(dx, dy) {
    host.wh_pointer_move(Math.round(dx), Math.round(dy));
  }

  function pointerButton(button, down) {
    host.wh_pointer_button(button, down ? 1 : 0);
  }

  return {
    key,
    pointer,
    pointerMove,
    pointerButton,
    /** Keys the page should keep for the game rather than the browser. */
    captures: (code) => CAPTURED.has(code),
    /**
     * Whether clicking the frame should hand this game the pointer.
     *
     * The default is yes: a game that aims with the mouse reads movement, and a
     * locked pointer keeps moving past the edge of the window. A game that only
     * wants a cursor over its own menus says `"mouse": "absolute"` in its
     * `config.json` and keeps the ordinary pointer instead.
     */
    mouseLock: settings.mouse !== "absolute",
    frameSize: () => frameSize,
    artifact,
    imports: host.wh_import_count ? host.wh_import_count() : 0,
    instructions: () => instructions,
    /**
     * The clock the guest reads, in milliseconds.
     *
     * The page pushes real time; this is what the guest got. They differ when a
     * shim has moved the clock, which is exactly the bug a fast-playing game is:
     * the guest is being told time passed when it did not.
     */
    clockMs: () => (host.wh_clock_ms ? host.wh_clock_ms() : 0),
    setPaused(value) {
      if (paused === value) return;
      paused = value;
      followPause();
      onStatus(value ? "paused" : "running");
    },
    /**
     * Whether the guest wants the pointer shown.
     *
     * A game that draws its own cursor calls `ShowCursor(FALSE)`, and hiding the
     * page's pointer then leaves exactly one cursor on screen: the one the guest
     * positions from the movement the page forwards. With both visible, a click
     * lands on whatever the guest has under *its* cursor while the player aims
     * at the other one, which reads as clicks doing nothing.
     */
    cursorVisible() {
      return !host.wh_cursor_visible || host.wh_cursor_visible() !== 0;
    },
    stop() {
      // The last write of the session may be inside the throttle window.
      persistSaves(true);
      running = false;
      if (handle !== null) cancelAnimationFrame(handle);
      handle = null;
      mixer.clear();
      queued = 0;
      if (scene) scene.dispose();
      scene = null;
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", followPause);
      }
      if (audio) audio.close().catch(() => {});
      audio = null;
    },
  };
}
