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

const EVENT_WORDS = 11;
const EVENT_PLAY = 0;
const EVENT_UPDATE = 1;
const EVENT_STOP = 2;
const EVENT_RELEASE = 3;
const EVENT_DATA = 4;
/** The guest moved the playhead with `SetCurrentPosition`; its offset is in `bytes`. */
const EVENT_SEEK = 5;

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
  const pointer = host.wh_message_ptr();
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
    fetch(`${base}artifact.json`).then((response) => {
      if (!response.ok) throw new Error(`artifact.json: HTTP ${response.status}`);
      return response.json();
    }),
    fetch(`${base}game/files.json`).then((response) => {
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
  // reads as a fix that did not take. The size the artifact records changes with
  // every rebuild, so it is the cache key: the same build is fetched once, and a
  // rebuilt payload cannot be missed.
  const build = artifact.module?.bytes ?? 0;
  const wasmResponse = await fetch(`${base}host_wasm.wasm?v=${build}`).then((response) => {
    if (!response.ok) throw new Error(`host_wasm.wasm: HTTP ${response.status}`);
    return response.arrayBuffer();
  });

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
  const host = instance.exports;

  const screen = artifact.screen ?? { width: 640, height: 480, depth: 8 };
  host.wh_set_screen(screen.width, screen.height, screen.depth);

  const name = new TextEncoder().encode(executableName);
  const namePointer = host.wh_alloc(name.length);
  new Uint8Array(host.memory.buffer, namePointer, name.length).set(name);
  host.wh_set_exe_name(namePointer, name.length);

  const imagePointer = host.wh_alloc(executable.length);
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
    const pointer = host.wh_alloc(encoded.length + contents.length);
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
    const pointer = host.wh_alloc(name.length + file.bytes.length);
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
    const offset = host.wh_memory_ptr() + host.wh_framebuffer_offset();
    if (!width || !height || !pitch || !host.wh_framebuffer_offset()) return;

    const depth = host.wh_framebuffer_depth ? host.wh_framebuffer_depth() : 8;
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
    frameSize = { width, height };

    const memory = new Uint8Array(host.memory.buffer);
    const image = context.createImageData(width, height);
    const out = new Uint32Array(image.data.buffer);

    for (let y = 0; y < height; y += 1) {
      const row = offset + y * pitch;
      const line = y * width;
      if (depth === 8) {
        refreshPalette();
        for (let x = 0; x < width; x += 1) out[line + x] = palette[memory[row + x]];
      } else if (depth === 16) {
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
      } else {
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
   * One entry per sounding voice: its format, and where the playhead has got
   * to. The samples are not copied -- the mixer reads them where the guest
   * keeps them, so a buffer rewritten under a playing voice is heard as it is.
   */
  const voices = new Map();
  /** Seconds of audio rendered into one queued block. */
  const CHUNK = 0.05;
  /** How far ahead of the audio clock the queue is kept filled. */
  const AHEAD = 0.09;

  /** DirectSound volume is hundredths of a decibel: 0 is unity, -10000 silent. */  function gainOf(volume) {
    if (!volume) return 1;
    return Math.min(1, Math.pow(10, volume / 2000));
  }

  /** DirectSound pan is -10000 (left) to 10000 (right). */
  function panOf(pan) {
    return Math.max(-1, Math.min(1, pan / 10000));
  }

  /** One frame of a voice's buffer, as a fraction of full scale. */
  function sampleAt(memory, base, index, bytes, bits) {
    const at = base + index * bytes;
    if (bits === 8) return (memory[at] - 128) / 128;
    // 16-bit samples are signed and little-endian.
    return (((memory[at] | (memory[at + 1] << 8)) << 16) >> 16) / 32768;
  }

  /**
   * The bus every voice mixes into.
   *
   * A game hands DirectSound a dozen 8-bit voices at once and DirectSound adds
   * them up; the sum passes full scale often enough that adding them here
   * unguarded clips on every loud chord, which is heard as roughness over the
   * music. A limiter is what keeps the sum inside the range without turning the
   * game down.
   */
  let master = null;
  function output(context) {
    if (master) return master;
    master = context.createGain();
    if (context.createDynamicsCompressor) {
      // A safety limiter, not compression. DirectSound never compressed -- the
      // guest sets its own levels, and percussion is a transient by definition --
      // so this only has to catch the rare moment a dozen voices sum past full
      // scale. Held at -6 dB with a 6 dB knee it caught every drum hit instead:
      // a mix whose peaks reach 0.5 was reduced by six, twelve decibels of
      // punch stripped off the one sound that is nothing but attack.
      const limiter = context.createDynamicsCompressor();
      limiter.threshold.value = -1;
      limiter.knee.value = 0;
      limiter.ratio.value = 20;
      limiter.attack.value = 0.003;
      limiter.release.value = 0.25;
      master.connect(limiter);
      limiter.connect(context.destination);
    } else {
      master.connect(context.destination);
    }
    return master;
  }

  /** Where the queue ends, on the audio clock. */
  let queued = 0;

  /**
   * Renders one block of the mix and queues it.
   *
   * DirectSound is a mixer: it reads each voice's buffer at that voice's own
   * play position, wraps a looping buffer and stops at the end of a one-shot.
   * One `AudioBufferSource` per `Play` cannot express that -- it copies the
   * samples once and loops that copy, so a note retuned under it keeps the old
   * waveform, a buffer the guest rewrites is played stale, and every restart
   * puts a step in the waveform. A step inside a period is a click; at the rate
   * a low note plays, several of them per period is noise. So the sum is
   * computed here, straight out of guest memory, and these blocks are the only
   * thing Web Audio plays.
   */
  function mixBlock(context) {
    // Render at the rate the guest's own buffers use, not the device's, and let
    // Web Audio do the conversion: its resampler is band-limited, while mixing
    // straight into 48 kHz here folded images of the 22 kHz source -- Cave
    // Story's buffers are mono 8-bit at 22050 -- back into 11 to 22 kHz, which
    // is the hiss laid over the music that the original does not have.
    let sourceRate = 0;
    for (const entry of voices.values()) {
      if (entry.stopped) continue;
      if (entry.spec.rate > sourceRate) sourceRate = entry.spec.rate;
    }
    const rate = Math.min(context.sampleRate || 22_050, sourceRate || 22_050);
    const frames = Math.max(1, Math.round(CHUNK * rate));
    const buffer = context.createBuffer(2, frames, rate);
    const left = buffer.getChannelData(0);
    const right = buffer.getChannelData(1);
    const memory = new Uint8Array(host.memory.buffer);

    for (const [voice, entry] of voices) {
      if (entry.stopped) continue;
      const spec = entry.spec;
      const bytes = spec.channels * (spec.bits / 8);
      if (!spec.channels || !spec.bits || !spec.rate || !bytes) continue;

      // Live memory first: a streaming guest writes into the buffer it plays.
      let base = 0;
      let length = 0;
      const address = host.wh_audio_samples ? host.wh_audio_samples(voice) : 0;
      if (address) {
        length = host.wh_audio_samples_len(voice);
        base = host.wh_memory_ptr() + address;
      }
      if (!length) {
        // A voice the guest released: the host kept the copy it took.
        length = host.wh_audio_pcm_len ? host.wh_audio_pcm_len(voice) : 0;
        base = host.wh_audio_pcm_ptr ? host.wh_audio_pcm_ptr(voice) : 0;
      }
      const available = length ? Math.floor(length / bytes) : 0;
      if (!available) continue;

      // `frequency` is the playback rate in Hz against the buffer's own rate,
      // which is what makes DirectSound's tiny looping waveforms notes. The mix
      // leaves at the context's rate rather than the buffer's, so one source
      // frame lasts `frequency / output rate` output frames: dividing by the
      // buffer's rate instead plays a 22050 Hz track at 48000/22050 of its true
      // length -- fast, and wrapping its loop before the guest has refilled it.
      const step = (spec.frequency || spec.rate) / rate;
      const gain = gainOf(spec.volume);
      const angle = ((panOf(spec.pan) + 1) / 2) * (Math.PI / 2);
      const leftGain = Math.cos(angle) * gain;
      const rightGain = Math.sin(angle) * gain;

      let position = entry.position;
      if (position >= available) position = spec.looping ? position % available : available;
      for (let index = 0; index < frames; index += 1) {
        if (position >= available) {
          if (!spec.looping) break;
          position -= available;
        }
        const frame = Math.floor(position);
        const next = frame + 1 < available ? frame + 1 : 0;
        const between = position - frame;
        const first = sampleAt(memory, base, frame, bytes, spec.bits);
        const second = sampleAt(memory, base, next, bytes, spec.bits);
        const value = first + (second - first) * between;
        left[index] += value * leftGain;
        right[index] += value * rightGain;
        position += step;
      }
      entry.position = position;
    }

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
    const count = host.wh_audio_event_count();
    if (count) {
      const base = host.wh_audio_events_ptr();
      const memory = new Uint8Array(host.memory.buffer);
      const words = new Uint32Array(memory.buffer, base, count * EVENT_WORDS);
      for (let index = 0; index < count; index += 1) {
        const at = index * EVENT_WORDS;
        const kind = words[at];
        const voice = words[at + 1];
        if (kind === EVENT_PLAY) {
          const spec = {
            channels: words[at + 2],
            rate: words[at + 3],
            volume: words[at + 5] | 0,
            pan: words[at + 6] | 0,
            looping: (words[at + 7] & 1) === 1,
            frequency: words[at + 9],
            bits: words[at + 10],
          };
          const sounding = voices.get(voice);
          if (sounding) {
            // `Play` starts at the play cursor, so a voice that is already
            // sounding keeps playing: this is a sustained looping waveform
            // being retuned, not a new note.
            sounding.spec = { ...sounding.spec, ...spec };
            sounding.stopped = false;
          } else {
            voices.set(voice, { spec, position: 0, stopped: false });
          }
        } else if (kind === EVENT_STOP) {
          // A stopped buffer keeps its play cursor, in the guest and here: the
          // `Play` that follows resumes where it stopped.
          const entry = voices.get(voice);
          if (entry) entry.stopped = true;
        } else if (kind === EVENT_RELEASE) {
          voices.delete(voice);
        } else if (kind === EVENT_SEEK) {
          // The guest moved the playhead itself, which is how a drum restarts:
          // `iw` is the byte it asked for.
          const entry = voices.get(voice);
          if (entry) {
            const bytes = entry.spec.channels * (entry.spec.bits / 8);
            entry.position = bytes ? words[at + 8] / bytes : 0;
          }
        } else if (kind === EVENT_UPDATE) {
          const entry = voices.get(voice);
          if (entry) {
            // An update carries the frequency and not the buffer's own rate --
            // `SetFrequency` is how a game retunes a waveform it is already
            // playing, which is every note of Cave Story's music.
            entry.spec = {
              ...entry.spec,
              volume: words[at + 5] | 0,
              pan: words[at + 6] | 0,
              frequency: words[at + 9] || entry.spec.frequency,
            };
          }
        }
        // `EVENT_DATA` needs nothing here: the mixer reads the buffer as it
        // stands, so a write the guest makes is heard on the next block.
      }
      host.wh_audio_events_clear();
    }

    pumpAudio();
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
    // A frame's budget is instructions, not time: 250,000 per chunk, repeated
    // until the guest presents or a wall-clock slice is used up, so one slow
    // frame cannot freeze the page.
    //
    // A paced game gets its own interval as that slice. It waits for real time
    // inside the guest -- `Sleep` against `GetTickCount`, which is how a game
    // written for 50 made its own frame rate -- so a slice shorter than a frame
    // cuts the wait in half and costs a frame. An unpaced one keeps the 12 ms
    // that has always been enough to keep a page responsive.
    const slice = paced ? Math.max(12, 1000 / rate) : 12;
    for (let chunk = 0; chunk < 8; chunk += 1) {
      outcome = host.wh_run(250_000);
      instructions += 250_000;
      if (outcome !== 0) break;
      if (host.wh_flips() !== flips) break;
      if (host.wh_frame_version() !== version) break;
      if (performance.now() - started > slice) break;
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

    for (const notice of decodeNotices(host)) onDiagnostic(notice);
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
      voices.clear();
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
