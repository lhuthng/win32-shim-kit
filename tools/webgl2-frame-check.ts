// The end-to-end 3D check: a baked payload's first frame, through the page's
// renderer.
//
// `tools/webgl2-check.ts` drives the backend with a hand-built stream; this one
// drives it with a stream the *host* actually wrote, while a real Direct3D 8
// guest ran. It loads the payload the way the page does, runs until the guest
// presents, takes the command stream, and replays it through
// `demo-site/src/lib/webgl2.js` against a recording GL context. Every byte of
// guest memory the records point at -- the vertices, the texture -- is the real
// module's memory.
//
// What that proves, and what it does not: the whole path from the guest's
// `DrawPrimitive` to a `drawArrays` with the transformed triangle exists and
// carries the right numbers. Whether the shader compiles and what the pixels
// look like is the driver's business, and no check here can see it.
//
//   bun tools/webgl2-frame-check.ts [payload-dir]

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { createRenderer, OP } from "../demo-site/src/lib/webgl2.js";
import { recorder } from "./gl-recorder";

const payload = resolve(process.argv[2] ?? "demo/payloads/d3d8-triangle");

let failures = 0;
function check(condition: boolean, message: string): void {
  if (condition) {
    console.log(`  ok   ${message}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${message}`);
  }
}

const artifact = JSON.parse(readFileSync(join(payload, "artifact.json"), "utf8"));
const files: Array<{ name: string; bytes: number }> = JSON.parse(
  readFileSync(join(payload, "game/files.json"), "utf8"),
);
const module = readFileSync(join(payload, "host_wasm.wasm"));
const { instance } = await WebAssembly.instantiate(module, {});
const host = instance.exports as unknown as Record<string, any>;

const message = (): string => {
  const pointer = host.wh_message_ptr();
  const length = host.wh_message_len();
  if (!pointer || !length) return "";
  return new TextDecoder().decode(new Uint8Array(host.memory.buffer, pointer, length));
};

// ---- boot, exactly as the page boot does -----------------------------------

host.wh_set_screen(artifact.screen.width, artifact.screen.height, artifact.screen.depth);
const executableName: string = artifact.default ?? artifact.executables[0].name;
{
  const name = new TextEncoder().encode(executableName);
  const at = host.wh_alloc(name.length);
  new Uint8Array(host.memory.buffer, at, name.length).set(name);
  host.wh_set_exe_name(at, name.length);
}

const image = new Uint8Array(readFileSync(join(payload, "game", executableName)));
const imageAt = host.wh_alloc(image.length);
new Uint8Array(host.memory.buffer, imageAt, image.length).set(image);
check(host.wh_load(imageAt, image.length) === 0, `the guest loads (${message()})`);

for (const file of files) {
  if (file.name === executableName) continue;
  const contents = new Uint8Array(readFileSync(join(payload, "game", file.name)));
  const encoded = new TextEncoder().encode(file.name);
  const buffer = host.wh_alloc(encoded.length + contents.length);
  const memory = new Uint8Array(host.memory.buffer);
  memory.set(encoded, buffer);
  memory.set(contents, buffer + encoded.length);
  const provided = host.wh_provide_file(
    buffer,
    encoded.length,
    buffer + encoded.length,
    contents.length,
  );
  check(provided === 0, `${file.name} is provided`);
}

// ---- the guest's first frame ----------------------------------------------

let outcome = 0;
let ran = 0;
const budget = 40_000_000;
while (ran < budget) {
  host.wh_set_time(ran / 33_000);
  outcome = host.wh_run(500_000);
  ran += 500_000;
  if (outcome !== 0) break;
  if (host.wh_flips() > 0 && host.wh_render_len() > 0) break;
}
check(outcome === 0, `the guest ran without stopping (${message()})`);
check(host.wh_render_width() === artifact.screen.width, "the device is the payload's screen");
check(host.wh_render_draws() > 0, `the guest drew (${host.wh_render_draws()} draws)`);

const firstFrame = new Uint8Array(host.wh_render_len());
// Copied, not viewed: `render` clears the stream when it has replayed it, and a
// view into wasm memory goes stale if the module grows. A copy is the frame.
firstFrame.set(new Uint8Array(host.memory.buffer, host.wh_render_ptr(), firstFrame.length));

// ---- the page's replay ----------------------------------------------------

const context = recorder();
const canvas = {
  width: 0,
  height: 0,
  getContext: () => context.gl,
};
const diagnostics: string[] = [];
const renderer = createRenderer({
  host,
  createCanvas: () => canvas,
  onDiagnostic: (text: string) => diagnostics.push(text),
});
const drew = renderer.render();

check(drew, "the backend replays the frame");
check(diagnostics.length === 0, `the backend reported nothing (${diagnostics.join("; ")})`);
check(host.wh_render_len() === 0, "the stream is cleared once replayed");
check(canvas.width === artifact.screen.width && canvas.height === artifact.screen.height,
  `the drawing buffer is ${artifact.screen.width}x${artifact.screen.height}`);

const clear = context.find("clear");
check(clear !== undefined, "the frame clears the colour and depth buffers");
const clearColour = context.find("clearColor")?.args as number[] | undefined;
check(
  clearColour !== undefined &&
    Math.abs(clearColour[0] - 12 / 255) < 0.01 &&
    Math.abs(clearColour[1] - 18 / 255) < 0.01 &&
    Math.abs(clearColour[2] - 36 / 255) < 0.01,
  "the clear colour is the guest's",
);

const draws = context.all("drawArrays");
check(draws.length > 0, `the triangle is drawn (${draws.length} drawArrays)`);
check(draws[0]?.args[2] === 3, "the draw is three vertices");

check(
  context.shaders.some((source: string) => source.includes("texture(uTexture")),
  "the textured fragment shader was built",
);

// The texture: the guest filled it from triangle.bmp through LockRect, so the
// uploaded pixels are the bitmap's four quadrants with red first.
const uploads = context
  .all("texImage2D")
  .filter((call) => call.args[8] instanceof Uint8Array) as Array<{ args: unknown[] }>;
const pixels = uploads.at(-1)?.args[8] as Uint8Array | undefined;
check(
  pixels !== undefined &&
    pixels.length === 8 * 8 * 4 &&
    pixels[0] === 255 && pixels[1] === 0 && pixels[2] === 0 && pixels[3] === 255,
  "the texture's top-left pixel is the bitmap's red",
);
check(
  pixels !== undefined &&
    pixels[4 * 4] === 0 && pixels[4 * 4 + 1] === 255 && pixels[4 * 4 + 2] === 0,
  "the next quadrant is the bitmap's green",
);

// The transform: the last matrix the shader received is the rotation the guest
// set this frame, and the triangle's vertices land inside the clip volume.
const matrices = context.all("uniformMatrix4fv");
const last = matrices.at(-1)?.args[2] as Float32Array | undefined;
if (last) {
  const values = Array.from(last);
  // A rotation about z: the first two rows are unit length, orthogonal, and the
  // third keeps z unchanged.
  const [a, b, c, d] = [values[0], values[1], values[4], values[5]];
  check(
    Math.abs(a * a + b * b - 1) < 0.01 && Math.abs(c * c + d * d - 1) < 0.01,
    "the matrix is the guest's rotation",
  );
  check(
    Math.abs(a * c + b * d) < 0.01 && Math.abs(values[10] - 1) < 0.01,
    "the rotation leaves z alone",
  );
  // Row-vector product, as Direct3D does it: clip = vertex * matrix.
  const triangle = [
    [-0.85, -0.75, 0.5],
    [0.85, -0.75, 0.5],
    [0.0, 0.85, 0.5],
  ];
  let inside = 0;
  for (const vertex of triangle) {
    const x = vertex[0] * values[0] + vertex[1] * values[4] + vertex[2] * values[8] + values[12];
    const y = vertex[0] * values[1] + vertex[1] * values[5] + vertex[2] * values[9] + values[13];
    const z = vertex[0] * values[2] + vertex[1] * values[6] + vertex[2] * values[10] + values[14];
    if (Math.abs(x) <= 1.5 && Math.abs(y) <= 1.5 && z >= 0 && z <= 1) inside += 1;
  }
  check(inside === 3, "all three vertices project inside the clip volume");
} else {
  check(false, "a transform reached the shader");
}

// The first record of the frame built the device, which is what makes the page
// choose this backend at all.
check(firstFrame.length > 0, `the frame is ${firstFrame.length} bytes of records`);
{
  const view = new DataView(firstFrame.buffer, firstFrame.byteOffset, firstFrame.length);
  check(view.getUint32(0, true) === OP.CREATE_DEVICE, "the stream opens with the device");
  check(view.getUint32(8, true) === artifact.screen.width, "the device width is the payload's");
}

console.log(failures === 0 ? "first frame drawn" : `${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
