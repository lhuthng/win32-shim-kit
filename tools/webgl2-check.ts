// Replays a Direct3D 8 command stream through the page's WebGL2 backend, with a
// recording stand-in for the GL context.
//
// There is no browser in this check, so the backend is given a fake `gl` that
// records every call and returns plausible objects. What that proves is the part
// a browser would not tell you either: that each record reaches the GL call it
// stands for, in order, with the coordinates converted rather than passed
// through. The host's side of the same ABI -- what it writes into the stream --
// is proved by `cargo test -p host`.
//
//   bun tools/webgl2-check.ts

import { createRenderer, layout, OP } from "../demo-site/src/lib/webgl2.js";
import { recorder } from "./gl-recorder";

let failures = 0;
function check(condition: boolean, message: string): void {
  if (condition) {
    console.log(`  ok   ${message}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${message}`);
  }
}

// ---- a command stream ------------------------------------------------------

class Stream {
  private bytes: number[] = [];

  u32(value: number): this {
    const buffer = new DataView(new ArrayBuffer(4));
    buffer.setUint32(0, value >>> 0, true);
    this.bytes.push(...new Uint8Array(buffer.buffer));
    return this;
  }

  f32(value: number): this {
    const buffer = new DataView(new ArrayBuffer(4));
    buffer.setFloat32(0, value, true);
    this.bytes.push(...new Uint8Array(buffer.buffer));
    return this;
  }

  record(op: number, payload: (stream: Stream) => void): this {
    const body = new Stream();
    payload(body);
    this.u32(op).u32(body.bytes.length).bytes.push(...body.bytes);
    return this;
  }

  data(): Uint8Array {
    return Uint8Array.from(this.bytes);
  }
}

// ---- a stand-in for the GL context ----------------------------------------

const GUEST_BASE = 0x1000;
const memory = new Uint8Array(0x10000);

const context = recorder();
const gl = context.gl;
const calls = context.calls;
const shaders = context.shaders;

const canvas = {
  width: 0,
  height: 0,
  getContext() {
    return gl;
  },
};

// ---- the host -------------------------------------------------------------

let stream = new Uint8Array();
let cleared = 0;
const diagnostics: string[] = [];

const host = {
  memory: { buffer: memory.buffer },
  wh_memory_ptr: () => GUEST_BASE,
  wh_render_ptr: () => 0x100,
  wh_render_len: () => stream.length,
  wh_render_clear: () => {
    cleared += 1;
    stream = new Uint8Array();
  },
};

const renderer = createRenderer({
  host,
  createCanvas: () => canvas,
  onDiagnostic: (text: string) => diagnostics.push(text),
});

const names = () => context.names();
const find = (name: string) => context.find(name);

function draw(commands: Stream): void {
  stream = commands.data();
  memory.set(stream, 0x100);
  context.reset();
  diagnostics.length = 0;
  renderer.render();
}

// ---- the checks -----------------------------------------------------------

console.log("layout");
{
  // XYZ | DIFFUSE | one two-float texture coordinate.
  const shape = layout(0x002 | 0x040 | 0x100);
  check(shape.stride === 24, "XYZ, diffuse and one coordinate take 24 bytes");
  check(shape.diffuse === 12, "the colour follows the position");
  check(shape.texcoords[0].offset === 16, "the coordinate follows the colour");
  check(layout(0x040) === null, "a layout with no position is refused");
}

console.log("device and a draw");
{
  const pixels = new Stream();
  pixels.record(OP.CREATE_DEVICE, (s) => s.u32(640).u32(480).u32(32));
  pixels.record(OP.SET_RENDER_STATE, (s) => s.u32(27).u32(1)); // ALPHABLENDENABLE
  pixels.record(OP.SET_TRANSFORM, (s) => {
    s.u32(1); // D3DTS_WORLD
    for (let index = 0; index < 16; index += 1) s.f32(index % 5 === 0 ? 2 : 0);
  });
  pixels.record(OP.SET_FVF, (s) => s.u32(0x142));
  pixels.record(OP.CREATE_VERTEX_BUFFER, (s) => s.u32(0x900).u32(96).u32(0x142));
  pixels.record(OP.SET_STREAM_SOURCE, (s) => s.u32(0).u32(0x900).u32(24).u32(0));
  pixels.record(OP.DRAW, (s) => s.u32(4).u32(0).u32(6));
  draw(pixels);

  check(canvas.width === 640 && canvas.height === 480, "the back buffer sizes the canvas");
  check(find("viewport")?.args[2] === 640, "the viewport matches the back buffer");
  check(names().includes("enable") && names().includes("blendFunc"), "alpha blending is enabled");
  const blend = find("blendFunc");
  // GL_SRC_ALPHA and GL_ONE_MINUS_SRC_ALPHA, which Direct3D's default blend is.
  check(
    blend?.args[0] === 0x0302 && blend?.args[1] === 0x0303,
    "the default blend is source alpha over one minus",
  );
  // Projection is identity, so the world matrix's 2 on the diagonal is what the
  // shader receives.
  const matrix = find("uniformMatrix4fv");
  const values = matrix?.args[2] ? Array.from(matrix.args[2] as Float32Array) : [];
  check(values[0] === 2 && values[1] === 0, "the world matrix reaches the vertex shader");
  const elements = find("drawElements");
  check(elements === undefined, "a non-indexed draw uses drawArrays");
  const arrays = find("drawArrays");
  check(arrays?.args[1] === 0 && arrays?.args[2] === 6, "six vertices are drawn from the base");
  check(cleared === 1, "the stream is cleared once it has been replayed");
  check(shaders.some((source) => source.includes("uWVP * vec4(aPosition")), "the transformed vertex shader is used");
}

console.log("a texture");
{
  // Two BGRA pixels, so the conversion to RGBA is observable.
  memory.set([0x11, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0x80], GUEST_BASE + 0x200);
  const pixels = new Stream();
  pixels.record(OP.CREATE_DEVICE, (s) => s.u32(320).u32(240).u32(32));
  pixels.record(OP.CREATE_TEXTURE, (s) => s.u32(0x800).u32(2).u32(1).u32(1).u32(21).u32(8));
  pixels.record(OP.UPDATE_TEXTURE, (s) => s.u32(0x800).u32(0).u32(0x200).u32(8).u32(2).u32(1).u32(21));
  pixels.record(OP.SET_TEXTURE, (s) => s.u32(0).u32(0x800));
  pixels.record(OP.SET_FVF, (s) => s.u32(0x142));
  pixels.record(OP.CREATE_VERTEX_BUFFER, (s) => s.u32(0x901).u32(24).u32(0x142));
  pixels.record(OP.SET_STREAM_SOURCE, (s) => s.u32(0).u32(0x901).u32(24).u32(0));
  pixels.record(OP.DRAW, (s) => s.u32(4).u32(0).u32(3));
  draw(pixels);

  const upload = calls.filter((call) => call.name === "texImage2D").at(-1);
  const bytes = upload?.args[8] as Uint8Array | undefined;
  check(
    bytes !== undefined &&
      bytes[0] === 0x33 &&
      bytes[1] === 0x22 &&
      bytes[2] === 0x11 &&
      bytes[3] === 0xff,
    "BGRA becomes RGBA",
  );
  check(bytes !== undefined && bytes[7] === 0x80, "the alpha byte is kept");
  check(shaders.some((source) => source.includes("texture(uTexture")), "a textured program is compiled");
}

console.log("a screen-space draw");
{
  const pixels = new Stream();
  pixels.record(OP.CREATE_DEVICE, (s) => s.u32(640).u32(480).u32(32));
  pixels.record(OP.SET_FVF, (s) => s.u32(0x004 | 0x040)); // XYZRHW | DIFFUSE
  pixels.record(OP.DRAW_UP, (s) => s.u32(4).u32(20).u32(0x044).u32(0x300).u32(3));
  memory.set(new Uint8Array(60), GUEST_BASE + 0x300);
  draw(pixels);

  check(
    shaders.some((source) => source.includes("aPosition.x * uHalf.x")),
    "XYZRHW vertices take the screen-space shader",
  );
  const arrays = find("drawArrays");
  check(arrays?.args[2] === 3, "DrawPrimitiveUP draws its primitive's vertices");
}

console.log("an index buffer");
{
  const pixels = new Stream();
  pixels.record(OP.CREATE_DEVICE, (s) => s.u32(640).u32(480).u32(32));
  pixels.record(OP.SET_FVF, (s) => s.u32(0x002 | 0x040));
  pixels.record(OP.CREATE_VERTEX_BUFFER, (s) => s.u32(0x902).u32(60).u32(0x042));
  pixels.record(OP.SET_STREAM_SOURCE, (s) => s.u32(0).u32(0x902).u32(20).u32(0));
  pixels.record(OP.CREATE_INDEX_BUFFER, (s) => s.u32(0x903).u32(12));
  pixels.record(OP.SET_INDICES, (s) => s.u32(0x903).u32(0));
  pixels.record(OP.DRAW_INDEXED, (s) => s.u32(4).u32(0).u32(4).u32(0).u32(2));
  draw(pixels);

  const elements = find("drawElements");
  check(elements?.args[1] === 6, "two indexed triangles are six indices");
  check(elements?.args[3] === 0, "the first index starts the draw");
}

console.log("robustness");
{
  const pixels = new Stream();
  pixels.record(OP.CREATE_DEVICE, (s) => s.u32(64).u32(64).u32(32));
  // A record this backend does not know must be skipped by its length, not read
  // as whatever follows it.
  pixels.record(999, (s) => s.u32(7).u32(8));
  pixels.record(OP.DESTROY, (s) => s.u32(0x777));
  draw(pixels);
  check(diagnostics.some((text) => text.includes("999")), "an unknown record is reported once");
  check(names().includes("deleteBuffer") === false, "destroying an unknown handle is not an error");
}

console.log(failures === 0 ? "all checks passed" : `${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
