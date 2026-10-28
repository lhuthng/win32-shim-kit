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

import { recorder } from "./gl-recorder";

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
// whole question, and a headless run has no speakers to ask. It starts
// suspended, the way a browser starts one, so a run that never resumes it is a
// run whose sounds go nowhere.
let mixed: Float32Array[] = [];
let contexts = 0;
let resumes = 0;
let suspends = 0;
let context: RecordingAudioContext | null = null;
const opened = performance.now();
class RecordingAudioContext {
  state = "suspended";
  destination = {};
  sampleRate = 44_100;
  /** The loader queues blocks against this clock, so it has to run. */
  get currentTime() {
    return (performance.now() - opened) / 1000;
  }
  constructor() {
    contexts += 1;
    context = this;
  }
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
    resumes += 1;
    this.state = "running";
    return Promise.resolve();
  }
  suspend() {
    suspends += 1;
    this.state = "suspended";
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
  const { boot, resumeAudio } = await import("../demo-site/src/lib/player.js");
  // The page's Play button is a gesture, and a browser only starts audio from
  // one. A game that plays music on its first screen is silent for ever if the
  // context it mixes into was made after the click had been awaited away, so
  // the loader has to be able to start it before `boot` and then use it.
  resumeAudio();
  check(contexts === 1 && resumes === 1, "a click before boot starts the audio context");
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
  check(contexts === 1, "the session mixes into the context the click started");

  // A pause has to reach the mixer as well as the guest: the music is a buffer
  // the guest refills while it runs, so a context left playing repeats the last
  // half buffer it got -- one short fragment on a loop -- and a clock left
  // running makes the resume seek past where the music had got to.
  const before = session.clockMs();
  const resumesBefore = resumes;
  const suspendsBefore = suspends;
  session.setPaused(true);
  check(suspends > suspendsBefore, "pausing suspends the page's audio");
  await Bun.sleep(1200);
  check(session.clockMs() === before, "the guest's clock stops while paused");
  check(resumes === resumesBefore, "nothing resumes the audio while paused");
  session.setPaused(false);
  check(resumes > resumesBefore, "unpausing resumes it");
  await Bun.sleep(300);
  const advanced = session.clockMs() - before;
  check(advanced > 0, `the guest's clock moves again (${Math.round(advanced)} ms)`);
  check(advanced < 1000, `it did not jump by the pause (${Math.round(advanced)} ms for a 1200 ms pause)`);

  session.stop();

  // ---- the 3D payload, through the same loader -----------------------------
  //
  // A Direct3D game takes the other branch: the loader replays the host's
  // command stream into an offscreen WebGL2 canvas and copies it into the frame
  // canvas. Here that offscreen canvas is recorder-backed, so what this proves
  // is the wiring -- that a device is noticed, that the renderer is built once,
  // and that a presented frame reaches the canvas -- and not what the pixels
  // look like.
  const three = list.payloads.find(
    (entry: { slug: string }) => entry.slug === "d3d8-triangle",
  );
  check(Boolean(three), "the Direct3D payload is listed");
  if (three) {
    const context3d = recorder();
    const offscreen = {
      width: 0,
      height: 0,
      getContext: () => context3d.gl,
    };
    let copied: { width: number; height: number } | null = null;
    const frameCanvas = {
      width: 0,
      height: 0,
      getContext: () => ({
        createImageData: (width: number, height: number) => ({
          width,
          height,
          data: new Uint8ClampedArray(width * height * 4),
        }),
        putImageData: () => {},
        drawImage: (image: { width: number; height: number }) => {
          copied = { width: image.width, height: image.height };
        },
      }),
    };
    globals.document = {
      createElement: () => offscreen,
      addEventListener: () => {},
      removeEventListener: () => {},
    };

    const sceneStatus: string[] = [];
    const sceneDiagnostics: string[] = [];
    const scene = await boot({
      canvas: frameCanvas,
      payloadUrl: `${base}/payloads/${three.slug}/`,
      onStatus: (text: string) => sceneStatus.push(text),
      onDiagnostic: (text: string) => sceneDiagnostics.push(text),
    });
    const deadline3d = Date.now() + 20_000;
    while (Date.now() < deadline3d && copied === null) await Bun.sleep(50);
    check(copied !== null, "the 3D frame reached the canvas");
    const copy = copied as { width: number; height: number } | null;
    check(copy?.width === 640 && copy?.height === 480, `the 3D frame is ${copy?.width}x${copy?.height}`);
    check(
      context3d.all("drawArrays").length > 0,
      `${context3d.all("drawArrays").length} draw(s) reached WebGL2`,
    );
    check(
      frameCanvas.width === 640 && frameCanvas.height === 480,
      "the frame canvas is the device's back buffer",
    );
    check(
      sceneStatus.some((text) => text.startsWith("running")),
      "the 3D loader reports it is running",
    );
    check(
      !sceneDiagnostics.some((text) => text.includes("no shim") || text.includes("WebGL2:")),
      `the 3D loader reported nothing (${sceneDiagnostics.join("; ")})`,
    );
    scene.stop();
  }
} finally {
  server.stop(true);
}

if (failures > 0) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("the site's loader boots the demo");
