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

const EVENT_WORDS = 11;
const EVENT_PLAY = 0;
const EVENT_UPDATE = 1;
const EVENT_STOP = 2;
const EVENT_RELEASE = 3;
const EVENT_DATA = 4;

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
 * Whether a file is one the payload calls a save. `*` takes everything, and a
 * payload that names nothing keeps everything the guest writes -- which is the
 * case where the guest's own folder is the save.
 */
function keptBy(patterns, name) {
  if (!patterns || patterns.length === 0) return true;
  const lower = name.toLowerCase();
  return patterns.some((pattern) => {
    const wanted = pattern.toLowerCase();
    return wanted === "*" || wanted === lower;
  });
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

  const [artifact, fileList, wasmResponse, settings] = await Promise.all([
    fetch(`${base}artifact.json`).then((response) => {
      if (!response.ok) throw new Error(`artifact.json: HTTP ${response.status}`);
      return response.json();
    }),
    fetch(`${base}game/files.json`).then((response) => {
      if (!response.ok) throw new Error(`game/files.json: HTTP ${response.status}`);
      return response.json();
    }),
    fetch(`${base}host_wasm.wasm`).then((response) => {
      if (!response.ok) throw new Error(`host_wasm.wasm: HTTP ${response.status}`);
      return response.arrayBuffer();
    }),
    // Optional, and missing on every payload baked before it existed: the
    // settings that belong to this game rather than to the page.
    fetch(`${base}config.json`)
      .then((response) => (response.ok ? response.json() : {}))
      .catch(() => ({})),
  ]);

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

  const { instance } = await WebAssembly.instantiate(await wasmResponse, {});
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
        for (let x = 0; x < width; x += 1) {
          const at = row + x * 2;
          const value = memory[at] | (memory[at + 1] << 8);
          const red = (value >> 11) & 0x1f;
          const green = (value >> 5) & 0x3f;
          const blue = value & 0x1f;
          out[line + x] =
            0xff000000 |
            ((blue << 3) << 16) |
            ((green << 2) << 8) |
            (red << 3);
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

  let audio = null;
  /** One entry per sound the page is mixing, keyed by the DirectSound voice. */
  const voices = new Map();
  /**
   * Plays waiting for the guest's samples.
   *
   * A guest that fills its buffer on a timer of its own can call `Play` before
   * the samples are written, and the copy taken then is the silence that was
   * there. These are re-read for a few frames until they hold something.
   */
  const waiting = new Map();
  /** How long a play waits for its samples, in milliseconds. */
  const WAIT_MS = 250;

  function audioContext() {
    if (!audio) {
      const Constructor = window.AudioContext ?? window.webkitAudioContext;
      if (!Constructor) return null;
      audio = new Constructor();
    }
    if (audio.state === "suspended") audio.resume().catch(() => {});
    return audio;
  }

  /** DirectSound volume is hundredths of a decibel: 0 is unity, -10000 silent. */
  function gainOf(volume) {
    if (!volume) return 1;
    return Math.min(1, Math.pow(10, volume / 2000));
  }

  /** DirectSound pan is -10000 (left) to 10000 (right). */
  function panOf(pan) {
    return Math.max(-1, Math.min(1, pan / 10000));
  }

  /**
   * A voice's samples as they are in guest memory right now.
   *
   * A DirectSound buffer is memory the guest writes into — at creation, or
   * every frame when it streams — and `wh_audio_samples` hands back that
   * memory, so this plays the sound as it stands rather than as it was when
   * `Play` was called.
   */
  function samples(voice) {
    if (host.wh_audio_samples) {
      const address = host.wh_audio_samples(voice);
      const length = host.wh_audio_samples_len(voice);
      if (address && length) {
        const offset = host.wh_memory_ptr() + address;
        return new Uint8Array(host.memory.buffer, offset, length).slice();
      }
    }
    // A voice the guest released: the host kept the copy it took at `Play`.
    const length = host.wh_audio_pcm_len(voice);
    if (!length) return new Uint8Array(0);
    return new Uint8Array(host.memory.buffer, host.wh_audio_pcm_ptr(voice), length).slice();
  }

  /** Whether every sample is the format's zero, so the guest has not written yet. */
  function silent(pcm, bits) {
    const zero = bits === 8 ? 0x80 : 0x00;
    for (let index = 0; index < pcm.length; index += 1) {
      if (pcm[index] !== zero) return false;
    }
    return true;
  }

  function stopVoice(voice) {
    const entry = voices.get(voice);
    if (!entry) return;
    voices.delete(voice);
    entry.source.onended = null;
    try {
      entry.source.stop();
    } catch {
      // A source that already ended throws; the nodes are gone either way.
    }
    entry.source.disconnect();
    if (entry.panner) entry.panner.disconnect();
    entry.gain.disconnect();
  }

  function startVoice(voice, spec, pcm) {
    const context = audioContext();
    if (!context) return;
    const channels = spec.channels;
    const rate = spec.rate;
    const bits = spec.bits;
    const bytesPerFrame = channels * (bits / 8);
    if (!channels || !rate || !bytesPerFrame) return;
    const frames = Math.floor(pcm.length / bytesPerFrame);
    if (frames <= 0) return;

    const buffer = context.createBuffer(channels, frames, rate);
    const view = new DataView(pcm.buffer, pcm.byteOffset, pcm.byteLength);
    for (let channel = 0; channel < channels; channel += 1) {
      const out = buffer.getChannelData(channel);
      for (let frame = 0; frame < frames; frame += 1) {
        const at = frame * bytesPerFrame + channel * (bits / 8);
        out[frame] =
          bits === 8
            ? (view.getUint8(at) - 128) / 128
            : view.getInt16(at, true) / 32768;
      }
    }

    stopVoice(voice);
    const gain = context.createGain();
    gain.gain.value = gainOf(spec.volume);
    const panner = context.createStereoPanner ? context.createStereoPanner() : null;
    const source = context.createBufferSource();
    source.buffer = buffer;
    source.loop = spec.looping;
    // `frequency` is the playback rate in Hz, which `SetFrequency` can change.
    source.playbackRate.value = (spec.frequency || rate) / rate;
    if (panner) {
      panner.pan.value = panOf(spec.pan);
      source.connect(panner);
      panner.connect(gain);
    } else {
      source.connect(gain);
    }
    gain.connect(context.destination);

    source.onended = () => {
      if (voices.get(voice)?.source === source) voices.delete(voice);
      source.disconnect();
      if (panner) panner.disconnect();
      gain.disconnect();
    };
    source.start();
    voices.set(voice, { source, gain, panner });
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
          waiting.set(voice, {
            spec: {
              channels: words[at + 2],
              rate: words[at + 3],
              volume: words[at + 5] | 0,
              pan: words[at + 6] | 0,
              looping: (words[at + 7] & 1) === 1,
              frequency: words[at + 9],
              bits: words[at + 10],
            },
            until: performance.now() + WAIT_MS,
          });
        } else if (kind === EVENT_STOP || kind === EVENT_RELEASE) {
          waiting.delete(voice);
          stopVoice(voice);
        } else if (kind === EVENT_UPDATE) {
          const entry = voices.get(voice);
          if (entry) {
            entry.gain.gain.value = gainOf(words[at + 5] | 0);
            if (entry.panner) entry.panner.pan.value = panOf(words[at + 6] | 0);
            const rate = words[at + 3];
            const frequency = words[at + 9];
            if (rate && frequency) entry.source.playbackRate.value = frequency / rate;
          }
        }
        // EVENT_DATA needs nothing: the mixer reads the buffer itself, so a
        // guest streaming into it is already heard.
      }
      host.wh_audio_events_clear();
    }

    // Start what the guest has written. One frame is usually enough, because
    // `Play` comes back before the writes do, not after them.
    for (const [voice, entry] of waiting) {
      const pcm = samples(voice);
      if (!pcm.length) {
        waiting.delete(voice);
        continue;
      }
      if (silent(pcm, entry.spec.bits) && performance.now() < entry.until) continue;
      waiting.delete(voice);
      startVoice(voice, entry.spec, pcm);
    }
  }

  /**
   * The mixer follows the tab.
   *
   * A hidden tab gets no animation frames, so the guest stops and nothing calls
   * `drainAudio` -- but `AudioContext` is the browser's, not the page's, and on
   * macOS Brave and Chrome leave it running while the page is in the background.
   * A looping `AudioBufferSource` then repeats whatever it holds with nobody
   * refilling it, which is heard as one short sound stuck on a loop. Worse, the
   * context can be suspended on the way out and nothing here ever asked it to
   * resume, so the frozen note is still there on the way back.
   *
   * Suspending it ourselves makes both directions explicit: silence while the
   * tab is away, and the same position -- loops, playheads, gain -- when it
   * comes back.
   */
  function followVisibility() {
    if (!audio) return;
    if (document.hidden) audio.suspend().catch(() => {});
    else audio.resume().catch(() => {});
  }
  // `document` is absent in the headless harness that boots this loader.
  if (typeof document !== "undefined") {
    document.addEventListener("visibilitychange", followVisibility);
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
  const origin = performance.now() - (paced ? 1000 / rate : 0);
  let presented = 0;

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
        host.wh_set_time(now);
        drainAudio();
        return;
      }
    }

    host.wh_set_time(now);
    const started = performance.now();
    let outcome = 0;
    // A frame's budget is instructions, not time: 250,000 per chunk, repeated
    // until the guest presents or a small wall-clock slice is used up, so one
    // slow frame cannot freeze the page.
    for (let chunk = 0; chunk < 8; chunk += 1) {
      outcome = host.wh_run(250_000);
      instructions += 250_000;
      if (outcome !== 0) break;
      if (host.wh_flips() !== flips) break;
      if (host.wh_frame_version() !== version) break;
      if (performance.now() - started > 12) break;
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
    status(`running — ${host.wh_framebuffer_width()}×${host.wh_framebuffer_height()}`);
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
    setPaused(value) {
      paused = value;
      onStatus(value ? "paused" : "running");
    },
    /** Browser audio starts suspended; the page calls this from a click. */
    resumeAudio() {
      audioContext();
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
      for (const voice of [...voices.keys()]) stopVoice(voice);
      waiting.clear();
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", followVisibility);
      }
      if (audio) audio.close().catch(() => {});
      audio = null;
    },
  };
}
