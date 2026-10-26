// Drives the site's own loader headlessly.
//
// `tools/demo-check.ts` speaks the module ABI directly; this one imports
// `demo-site/src/lib/player.js` -- the file the page ships -- and gives it the
// smallest canvas and clock a browser would. What it proves is the loader's own
// wiring: the boot order, the scratch-buffer discipline, and the pixel path from
// `wh_memory_ptr()` through the palette to `putImageData`.
//
//   bun tools/demo-player-check.ts [demo-dir]

import { join, resolve } from "node:path";

const root = resolve(process.argv[2] ?? "demo");

let failures = 0;
function check(condition: boolean, message: string): void {
  if (condition) {
    console.log(`  ok   ${message}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${message}`);
  }
}

// A canvas that only records what was presented: width, height and the last
// RGBA image the loader drew.
let presented: { width: number; height: number; data: Uint8ClampedArray } | null = null;
const canvas = {
  width: 0,
  height: 0,
  getContext() {
    return {
      createImageData(width: number, height: number) {
        return { width, height, data: new Uint8ClampedArray(width * height * 4) };
      },
      putImageData(image: { width: number; height: number; data: Uint8ClampedArray }) {
        presented = { width: image.width, height: image.height, data: image.data };
      },
    };
  },
};

// A Web Audio context that only records: what the loader hands the mixer is the
// whole question, and a headless run has no speakers to ask.
let mixed: Float32Array[] = [];
class RecordingAudioContext {
  state = "running";
  destination = {};
  createBuffer(channels: number, frames: number, rate: number) {
    const data = Array.from({ length: channels }, () => new Float32Array(frames));
    mixed.push(data[0]);
    return { channels, frames, rate, getChannelData: (channel: number) => data[channel] };
  }
  createBufferSource() {
    return { buffer: null, loop: false, playbackRate: { value: 1 }, onended: null, connect() {}, start() {}, stop() {}, disconnect() {} };
  }
  createGain() {
    return { gain: { value: 1 }, connect() {}, disconnect() {} };
  }
  createStereoPanner() {
    return { pan: { value: 0 }, connect() {}, disconnect() {} };
  }
  resume() {
    return Promise.resolve();
  }
  close() {
    return Promise.resolve();
  }
}

// The loader names `window` for audio and `requestAnimationFrame` for the clock.
const globals = globalThis as unknown as Record<string, unknown>;
globals.window = globalThis;
globals.AudioContext = RecordingAudioContext;
globals.requestAnimationFrame = (callback: (time: number) => void) =>
  setTimeout(() => callback(performance.now()), 4) as unknown as number;
globals.cancelAnimationFrame = (handle: unknown) => clearTimeout(handle as number);

const server = Bun.serve({
  port: 0,
  async fetch(request) {
    const path = decodeURIComponent(new URL(request.url).pathname);
    const file = Bun.file(join(root, path === "/" ? "index.html" : path));
    if (!(await file.exists())) return new Response("not found", { status: 404 });
    return new Response(file);
  },
});
const base = `http://localhost:${server.port}`;

try {
  const list = await (await fetch(`${base}/payloads/index.json`)).json();
  const payload = list.payloads?.[0];
  check(Boolean(payload), "a payload is listed");

  const statuses: string[] = [];
  const diagnostics: string[] = [];
  const { boot } = await import("../demo-site/src/lib/player.js");
  const session = await boot({
    canvas,
    payloadUrl: `${base}/payloads/${payload.slug}/`,
    onStatus: (text: string) => statuses.push(text),
    onDiagnostic: (text: string) => diagnostics.push(text),
  });

  check(session.imports > 0, `${session.imports} imports in the loaded image`);
  check(statuses.some((text) => text.startsWith("running")), "the loader reports it is running");
  // The pointer-locked path: a delta arrives as movement, which is what a game
  // that aims with the mouse reads. Nothing here needs the guest to react.
  check(typeof session.pointerMove === "function", "the loader takes pointer deltas");
  session.pointer(320, 240);
  session.pointerMove(7, -3);
  session.pointerButton(0, true);
  session.pointerButton(0, false);

  // Wait for the guest to present. The loader runs on the frame clock, so this
  // is real time, not instructions.
  // A palettised game fades in: the first frames it presents are black, and the
  // canvas frame carries an opaque alpha either way. So the wait is for colour,
  // not for any byte at all.
  const coloured = (): number => {
    if (!presented) return 0;
    let lit = 0;
    for (let index = 0; index < presented.data.length; index += 4) {
      if (presented.data[index] || presented.data[index + 1] || presented.data[index + 2]) lit += 1;
    }
    return lit;
  };
  /** Samples in the loudest buffer the mixer built, 0 when it built none. */
  const loudness = (): number => {
    let peak = 0;
    for (const samples of mixed) {
      for (const sample of samples) peak = Math.max(peak, Math.abs(sample));
    }
    return peak;
  };

  // Clicking the game's own controls is what makes it play a sound effect, and
  // the sound only exists once the guest has written it into the voice, so this
  // keeps clicking while it waits rather than clicking once at the start.
  const deadline = Date.now() + 30_000;
  let clicks = 0;
  while (Date.now() < deadline && (coloured() === 0 || loudness() === 0)) {
    await Bun.sleep(400);
    session.pointer(120 + (clicks % 5) * 100, 140 + (clicks % 3) * 80);
    session.pointerButton(0, true);
    session.pointerButton(0, false);
    clicks += 1;
  }

  check(presented !== null, "the loader presented a frame");
  if (presented) {
    const frame = presented as { width: number; height: number; data: Uint8ClampedArray };
    check(frame.width === 640 && frame.height === 480, `the frame is ${frame.width}x${frame.height}`);
    let lit = 0;
    for (let index = 0; index < frame.data.length; index += 4) {
      if (frame.data[index] || frame.data[index + 1] || frame.data[index + 2]) lit += 1;
    }
    check(lit > frame.width * frame.height * 0.1, `${lit} pixels reached the canvas`);
  }
  check(
    !diagnostics.some((text) => text.includes("no shim")),
    "no diagnostic reported a missing shim",
  );
  // The guest draws its own cursor and calls `ShowCursor(FALSE)` before it
  // presents anything. The page has to know, or it keeps showing a second
  // pointer that does not line up with the guest's, and a click aimed at one
  // lands on the other.
  check(session.cursorVisible() === false, "the loader reports the guest hid the pointer");
  // A mixer that never starts a voice, or starts it with the buffer's silence,
  // is indistinguishable from a working one until someone listens: this asks
  // Web Audio instead, with the samples the guest left in the buffer.
  check(mixed.length > 0, `${mixed.length} buffer(s) reached Web Audio after ${clicks} click(s)`);
  check(loudness() > 0.01, `the loudest mixed sample is ${loudness().toFixed(3)}`);

  session.stop();
} finally {
  server.stop(true);
}

if (failures > 0) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("the site's loader boots the demo");
