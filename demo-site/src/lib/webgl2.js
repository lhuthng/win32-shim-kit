// A WebGL2 backend for the Direct3D 8 command stream.
//
// The host's Direct3D device does not draw: it records what the guest asked for
// (`crates/host/src/render.rs`) and a page replays it. The records are deltas, in
// call order, because Direct3D is stateful -- a draw means whatever the last
// transform, render state and stream source said at that moment. So this is a
// mirror of the device, not a scene: it holds the same state and applies each
// record as it is read.
//
// The parts of Direct3D 8 that are fixed-function have to be built on demand:
//
//   * the vertex layout is an `FVF` word, decoded with `layout()` below, which is
//     the same arithmetic as the host's `fvf_stride`;
//   * the position is either transformed (`XYZ`) or already in screen space
//     (`XYZRHW`), which is two vertex shaders;
//   * the texture combiner is a fragment shader keyed by the stage's `COLOROP`,
//     and the alpha test is `discard`, which GLSL cannot make a uniform.
//
// That is why programs are cached by a *pipeline key* of decisions rather than
// by state values: matrices, colours, the texture factor and the alpha reference
// are uniforms, so moving a camera or fading a screen costs uniform writes, not
// a compile. The key is `fvf|position|diffuse|textured|op|alphatest`, which is
// tens of variants in practice and thousands if state values were in it.
//
// # Coordinates
//
// Direct3D and OpenGL disagree about the clip-space y axis and about the depth
// range. Both are corrected once, in the vertex shader: y is negated and z is
// mapped from [0,1] to [-1,1]. Negating y turns every triangle inside out, so the
// winding is inverted in exactly one place too -- `frontFace` in `applyState`.
// Texture coordinates then need no correction: row 0 of a texture is the top of
// the picture in Direct3D and the first row uploaded in WebGL, and both mean v=0.

/** Record opcodes, from `crates/host/src/render.rs`. */
export const OP = {
  CREATE_DEVICE: 1,
  CLEAR: 2,
  SET_RENDER_STATE: 3,
  SET_TRANSFORM: 4,
  SET_FVF: 5,
  CREATE_VERTEX_BUFFER: 6,
  UPDATE_VERTEX_BUFFER: 7,
  SET_STREAM_SOURCE: 8,
  CREATE_INDEX_BUFFER: 9,
  UPDATE_INDEX_BUFFER: 10,
  SET_INDICES: 11,
  DRAW: 12,
  DRAW_INDEXED: 13,
  DRAW_UP: 14,
  DRAW_INDEXED_UP: 15,
  CREATE_TEXTURE: 16,
  UPDATE_TEXTURE: 17,
  SET_TEXTURE: 18,
  SET_TEXTURE_STAGE_STATE: 19,
  SET_VIEWPORT: 20,
  SET_MATERIAL: 21,
  PRESENT: 22,
  DESTROY: 23,
};

/** How many indices a strip or list of `primitives` primitives reads. */
function indexCount(topology, primitives) {
  switch (topology) {
    case 1:
      return primitives;
    case 2:
      return primitives * 2;
    case 3:
      return primitives + 1;
    case 4:
      return primitives * 3;
    case 5:
    case 6:
      return primitives + 2;
    default:
      return 0;
  }
}

/** `D3DPRIMITIVETYPE` to a GL draw mode. */
const MODES = {
  1: 0x0000, // POINTS
  2: 0x0001, // LINES
  3: 0x0003, // LINE_STRIP
  4: 0x0004, // TRIANGLES
  5: 0x0005, // TRIANGLE_STRIP
  6: 0x0006, // TRIANGLE_FAN
};

/** `D3DRENDERSTATETYPE`, only the ones a fixed-function pass reads. */
const RS = {
  ZENABLE: 7,
  ZWRITEENABLE: 14,
  ALPHATESTENABLE: 15,
  SRCBLEND: 19,
  DESTBLEND: 20,
  CULLMODE: 22,
  ZFUNC: 23,
  ALPHAREF: 24,
  ALPHAFUNC: 25,
  ALPHABLENDENABLE: 27,
  TEXTUREFACTOR: 60,
  FOGENABLE: 28,
  LIGHTING: 137,
  COLORVERTEX: 141,
  BLENDOP: 171,
};

/** `D3DTEXTURESTAGESTATETYPE`. */
const TSS = {
  COLOROP: 1,
  COLORARG1: 2,
  COLORARG2: 3,
  MINFILTER: 5,
  MAGFILTER: 6,
  MIPFILTER: 7,
  ADDRESSU: 13,
  ADDRESSV: 14,
};

/** `D3DBLEND` to a GL blend factor. */
const BLENDS = {
  1: 0x0000, // ZERO
  2: 0x0001, // ONE
  3: 0x0300, // SRC_COLOR
  4: 0x0301, // ONE_MINUS_SRC_COLOR
  5: 0x0302, // SRC_ALPHA
  6: 0x0303, // ONE_MINUS_SRC_ALPHA
  7: 0x0304, // DST_ALPHA
  8: 0x0305, // ONE_MINUS_DST_ALPHA
  9: 0x0306, // DST_COLOR
  10: 0x0307, // ONE_MINUS_DST_COLOR
  11: 0x0308, // SRC_ALPHA_SATURATE
};

/** `D3DCMPFUNC` to a GL depth function, and to the alpha test's expression. */
const COMPARISONS = {
  1: 0x0200, // NEVER
  2: 0x0201, // LESS
  3: 0x0202, // EQUAL
  4: 0x0203, // LEQUAL
  5: 0x0204, // GREATER
  6: 0x0205, // NOTEQUAL
  7: 0x0206, // GEQUAL
  8: 0x0207, // ALWAYS
};

/**
 * The *failing* condition of each `D3DCMPFUNC`, because the shader discards on
 * true. Writing the passing condition instead inverts every test: `D3DCMP_ALWAYS`
 * becomes "throw the sprite away".
 */
const ALPHA_TESTS = {
  1: "true", // NEVER: nothing passes
  2: "base.a >= uAlphaRef", // LESS
  3: "abs(base.a - uAlphaRef) >= 0.002", // EQUAL
  4: "base.a > uAlphaRef", // LESSEQUAL
  5: "base.a <= uAlphaRef", // GREATER
  6: "abs(base.a - uAlphaRef) < 0.002", // NOTEQUAL
  7: "base.a < uAlphaRef", // GREATEREQUAL
  8: "false", // ALWAYS: nothing fails
};

/** `D3DTADDRESS_*` to a GL wrap mode. */
const ADDRESS = {
  1: 0x2901, // REPEAT
  2: 0x8370, // MIRRORED_REPEAT
  3: 0x812f, // CLAMP_TO_EDGE
  4: 0x812f, // BORDER: GL has no border colour here, clamp is the closest
  5: 0x812f, // MIRROR_ONCE: likewise
};

/** `D3DFMT_*` this backend uploads, as bytes per pixel. */
const FORMATS = {
  21: 4, // A8R8G8B8
  22: 4, // X8R8G8B8
  23: 2, // R5G6B5
  24: 2, // X1R5G5B5
  25: 2, // A1R5G5B5
  26: 2, // A4R4G4B4
  28: 1, // A8
  50: 1, // L8
};

const VERTEX_SHADER = `#version 300 es
in vec3 aPosition;
#ifdef HAS_DIFFUSE
in vec4 aDiffuse;
#endif
#ifdef HAS_TEXTURE
in vec2 aTexcoord0;
#endif
uniform mat4 uWVP;
uniform vec2 uHalf;
#ifdef HAS_DIFFUSE
out vec4 vDiffuse;
#else
uniform vec4 uDiffuse;
out vec4 vDiffuse;
#endif
#ifdef HAS_TEXTURE
out vec2 vTexcoord0;
#endif
void main() {
#ifdef RHW
  // Already in screen space: pixels, origin at the top left, and a reciprocal
  // homogeneous w this pass does not use because a 2D quad's is one.
  gl_Position = vec4(aPosition.x * uHalf.x - 1.0, 1.0 - aPosition.y * uHalf.y,
                     aPosition.z * 2.0 - 1.0, 1.0);
#else
  vec4 clip = uWVP * vec4(aPosition, 1.0);
  // Direct3D's y points up in clip space and down on screen, and its depth runs
  // 0..1; OpenGL's y points up on screen and its depth runs -1..1.
  gl_Position = vec4(clip.x, -clip.y, clip.z * 2.0 - clip.w, clip.w);
#endif
#ifdef HAS_DIFFUSE
  // A D3DCOLOR vertex is BGRA in memory and GL reads an UNSIGNED_BYTE quad as
  // RGBA, so the red and blue channels have to be swapped back.
  vDiffuse = aDiffuse.bgra;
#else
  vDiffuse = uDiffuse;
#endif
#ifdef HAS_TEXTURE
  vTexcoord0 = aTexcoord0;
#endif
}
`;

const FRAGMENT_SHADER = `#version 300 es
precision mediump float;
in vec4 vDiffuse;
#ifdef HAS_TEXTURE
in vec2 vTexcoord0;
uniform sampler2D uTexture;
#endif
uniform vec4 uTextureFactor;
uniform float uAlphaRef;
out vec4 colour;
void main() {
  vec4 base = vDiffuse * uTextureFactor;
#ifdef HAS_TEXTURE
#ifdef OP_MODULATE
  base *= texture(uTexture, vTexcoord0);
#elif defined(OP_ADD)
  base += texture(uTexture, vTexcoord0);
#elif defined(OP_SELECTARG1)
  base = texture(uTexture, vTexcoord0) * uTextureFactor;
#else
  base = vDiffuse;
#endif
#endif
#ifdef ALPHA_TEST
  if (__ALPHA_TEST__) discard;
#endif
  colour = base;
}
`;

/**
 * Splits an `FVF` word into a stride and attribute offsets, mirroring the host's
 * `fvf_stride`. A size of 0 means the component is not in the layout.
 */
export function layout(fvf) {
  const result = {
    stride: 0,
    position: null,
    diffuse: null,
    texcoords: [],
    rhw: false,
  };
  const position = fvf & 0x00e;
  if (position === 0) return null;
  let size;
  if (position === 0x002) {
    size = 3;
  } else if (position === 0x004) {
    size = 4;
    result.rhw = true;
  } else {
    // XYZB1..XYZB5: a position of three floats and up to five blend weights.
    size = 3 + (position - 0x004) / 2;
  }
  result.position = { offset: 0, size };
  let offset = size * 4;
  if (fvf & 0x010) offset += 12; // NORMAL
  if (fvf & 0x020) offset += 4; // PSIZE
  if (fvf & 0x040) {
    result.diffuse = offset;
    offset += 4;
  }
  if (fvf & 0x080) offset += 4; // SPECULAR
  const count = (fvf >> 8) & 0xf;
  for (let set = 0; set < count; set += 1) {
    const size = 2 + ((fvf >> (16 + set * 2)) & 3);
    result.texcoords.push({ offset, size });
    offset += size * 4;
  }
  result.stride = offset;
  return result;
}

function defaultCanvas() {
  return document.createElement("canvas");
}

/**
 * Builds a renderer over the host's command stream.
 *
 * `host` is the module's exports; `createCanvas` exists so a check can run
 * without a DOM, and receives no arguments.
 */
export function createRenderer({ host, createCanvas = defaultCanvas, onDiagnostic = () => {} }) {
  const canvas = createCanvas();
  const gl = canvas.getContext("webgl2", {
    alpha: false,
    antialias: false,
    depth: true,
    stencil: false,
    // The canvas is never in the document: it is copied into the frame canvas
    // after every present. A frame whose stream is empty still has to blit what
    // is already there, so the drawing buffer outlives the frame.
    preserveDrawingBuffer: true,
  });
  if (!gl) throw new Error("WebGL2 is not available in this browser");

  const programs = new Map();
  const buffers = new Map();
  const textures = new Map();
  const renderStates = new Map();
  const stageStates = new Map();
  const transforms = new Map();
  const streams = new Map();
  let indices = { handle: 0, base: 0 };
  let fvf = 0;
  let device = { width: 0, height: 0 };
  let viewport = { x: 0, y: 0, width: 0, height: 0, minZ: 0, maxZ: 1 };
  let bound = new Map();
  let scratch = null;
  let broken = null;
  const said = new Set();

  function once(key, text) {
    if (said.has(key)) return;
    said.add(key);
    onDiagnostic(text);
  }

  // ---- guest memory ------------------------------------------------------

  function memoryView() {
    // The buffer is re-read on every use: a wasm memory that grew invalidates
    // every view into it, which is a truncation rather than an error.
    return new Uint8Array(host.memory.buffer);
  }

  function base() {
    return host.wh_memory_ptr();
  }

  // ---- shaders -----------------------------------------------------------

  function compile(source, type) {
    const shader = gl.createShader(type);
    gl.shaderSource(shader, source);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(shader);
      gl.deleteShader(shader);
      throw new Error(`shader: ${log}`);
    }
    return shader;
  }

  function programFor(key) {
    const cached = programs.get(key);
    if (cached) return cached;
    const [encodedFvf, rhw, op, alphaTest] = key.split("|");
    const shape = layout(Number(encodedFvf));
    const flags = {
      HAS_DIFFUSE: shape && shape.diffuse !== null ? 1 : 0,
      HAS_TEXTURE: op !== "none" && shape && shape.texcoords.length ? 1 : 0,
      RHW: rhw === "1" ? 1 : 0,
      ALPHA_TEST: alphaTest !== "0" ? 1 : 0,
      OP_MODULATE: op === "modulate" ? 1 : 0,
      OP_ADD: op === "add" ? 1 : 0,
      OP_SELECTARG1: op === "selectarg1" ? 1 : 0,
    };
    const defines = Object.entries(flags)
      .filter(([, on]) => on)
      .map(([name]) => `#define ${name} 1`)
      .join("\n");
    const alphaExpression = ALPHA_TESTS[Number(alphaTest)] ?? "false";
    const vertex = VERTEX_SHADER.replace(
      "#version 300 es",
      `#version 300 es\n${defines}`,
    );
    const fragment = FRAGMENT_SHADER.replace(
      "#version 300 es",
      `#version 300 es\n${defines}`,
    ).replace("__ALPHA_TEST__", alphaExpression);
    const program = gl.createProgram();
    gl.attachShader(program, compile(vertex, gl.VERTEX_SHADER));
    gl.attachShader(program, compile(fragment, gl.FRAGMENT_SHADER));
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      const log = gl.getProgramInfoLog(program);
      gl.deleteProgram(program);
      throw new Error(`program: ${log}`);
    }
    const entry = {
      program,
      attributes: {
        position: gl.getAttribLocation(program, "aPosition"),
        diffuse: gl.getAttribLocation(program, "aDiffuse"),
        texcoord0: gl.getAttribLocation(program, "aTexcoord0"),
      },
      uniforms: {
        wvp: gl.getUniformLocation(program, "uWVP"),
        half: gl.getUniformLocation(program, "uHalf"),
        diffuse: gl.getUniformLocation(program, "uDiffuse"),
        textureFactor: gl.getUniformLocation(program, "uTextureFactor"),
        alphaRef: gl.getUniformLocation(program, "uAlphaRef"),
        texture: gl.getUniformLocation(program, "uTexture"),
      },
      shape,
      textured: flags.HAS_TEXTURE === 1,
    };
    programs.set(key, entry);
    return entry;
  }

  function matrix(handle) {
    return transforms.get(handle) ?? null;
  }

  function identity() {
    return new Float32Array([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]);
  }

  /** Row-major `a * b`, which is Direct3D's order: a row vector multiplies left. */
  function multiply(a, b) {
    const out = new Float32Array(16);
    for (let row = 0; row < 4; row += 1) {
      for (let column = 0; column < 4; column += 1) {
        let sum = 0;
        for (let inner = 0; inner < 4; inner += 1) {
          sum += a[row * 4 + inner] * b[inner * 4 + column];
        }
        out[row * 4 + column] = sum;
      }
    }
    return out;
  }

  /** World, view and projection as one matrix, which is the vertex shader's. */
  function worldViewProjection() {
    const world = matrix(1) ?? identity();
    const view = matrix(2) ?? identity();
    const projection = matrix(3) ?? identity();
    return multiply(multiply(world, view), projection);
  }

  // ---- pipeline ----------------------------------------------------------

  function value(state, fallback) {
    return renderStates.has(state) ? renderStates.get(state) : fallback;
  }

  function stageValue(kind, fallback) {
    return stageStates.has(kind) ? stageStates.get(kind) : fallback;
  }

  /** The combiner this pass would use, as one of the shader's names. */
  function combiner() {
    const boundTexture = bound.get(0) ?? 0;
    if (!boundTexture) return "none";
    switch (stageValue(TSS.COLOROP, 4)) {
      case 1: // DISABLE
        return "none";
      case 2: // SELECTARG1
        return "selectarg1";
      case 7: // ADD
        return "add";
      case 4:
        return "modulate";
      default:
        once(
          `op:${stageValue(TSS.COLOROP, 4)}`,
          `WebGL2: texture stage operation ${stageValue(TSS.COLOROP, 4)} is not ` +
            `implemented, drawing MODULATE instead`,
        );
        return "modulate";
    }
  }

  function alphaTestKey() {
    if (!value(RS.ALPHATESTENABLE, 0)) return "0";
    return String(value(RS.ALPHAFUNC, 8));
  }

  function usePipeline(shape) {
    // The key is the decisions that change the shader, not the values that feed
    // it: a matrix, a colour or the alpha reference is a uniform.
    const key = `${shape.fvf}|${shape.rhw ? 1 : 0}|${combiner()}|${alphaTestKey()}`;
    const entry = programFor(key);
    if (entry.program !== usePipeline.current) {
      gl.useProgram(entry.program);
      usePipeline.current = entry.program;
    }
    return entry;
  }

  function applyState(entry, wvp) {
    const zEnabled = value(RS.ZENABLE, 1);
    if (zEnabled) gl.enable(gl.DEPTH_TEST);
    else gl.disable(gl.DEPTH_TEST);
    gl.depthFunc(COMPARISONS[value(RS.ZFUNC, 4)] ?? gl.LEQUAL);
    gl.depthMask(value(RS.ZWRITEENABLE, 1) ? true : false);

    const cull = value(RS.CULLMODE, 3);
    if (cull === 1) {
      gl.disable(gl.CULL_FACE);
    } else {
      gl.enable(gl.CULL_FACE);
      // The vertex shader negates y, so a counter-clockwise Direct3D face is
      // clockwise on screen. This is the only place the inversion is handled.
      gl.frontFace(cull === 3 ? gl.CW : gl.CCW);
      gl.cullFace(gl.BACK);
    }

    if (value(RS.ALPHABLENDENABLE, 0)) {
      gl.enable(gl.BLEND);
      gl.blendFunc(
        BLENDS[value(RS.SRCBLEND, 5)] ?? gl.SRC_ALPHA,
        BLENDS[value(RS.DESTBLEND, 6)] ?? gl.ONE_MINUS_SRC_ALPHA,
      );
      const equation = value(RS.BLENDOP, 1);
      if (equation === 2) gl.blendEquation(gl.FUNC_SUBTRACT);
      else if (equation === 3) gl.blendEquation(gl.FUNC_REVERSE_SUBTRACT);
      else gl.blendEquation(gl.FUNC_ADD);
    } else {
      gl.disable(gl.BLEND);
    }

    const colour = value(RS.TEXTUREFACTOR, 0xffffffff);
    const factor = [
      ((colour >> 16) & 0xff) / 255,
      ((colour >> 8) & 0xff) / 255,
      (colour & 0xff) / 255,
      ((colour >>> 24) & 0xff) / 255,
    ];
    if (entry.uniforms.textureFactor) {
      gl.uniform4fv(entry.uniforms.textureFactor, factor);
    }
    if (entry.uniforms.wvp) {
      // The array is row-major; GLSL reads a mat4 column-major, so uploading it
      // unmodified computes `rowVector * matrix`, which is Direct3D's product.
      gl.uniformMatrix4fv(entry.uniforms.wvp, false, wvp);
    }
    if (entry.uniforms.half) {
      gl.uniform2f(
        entry.uniforms.half,
        device.width ? 2 / device.width : 0,
        device.height ? 2 / device.height : 0,
      );
    }
    if (entry.uniforms.diffuse) gl.uniform4f(entry.uniforms.diffuse, 1, 1, 1, 1);
    if (entry.uniforms.alphaRef) {
      gl.uniform1f(entry.uniforms.alphaRef, value(RS.ALPHAREF, 0) / 255);
    }
    if (entry.textured) {
      gl.activeTexture(gl.TEXTURE0);
      const texture = textures.get(bound.get(0));
      gl.bindTexture(gl.TEXTURE_2D, texture ? texture.texture : null);
      // Applied per draw, not when the texture was bound: a game sets stage
      // states and binds textures in whichever order it likes, and the state at
      // draw time is the one Direct3D would have used.
      if (texture) applySampling(texture.texture);
      if (entry.uniforms.texture) gl.uniform1i(entry.uniforms.texture, 0);
    }
  }

  function bindAttribute(entry, name, offset, size, type, normalized, stride) {
    const location = entry.attributes[name];
    if (location === undefined || location < 0) return;
    gl.enableVertexAttribArray(location);
    gl.vertexAttribPointer(location, size, type, normalized, stride, offset);
  }

  function disableAttribute(entry, name) {
    const location = entry.attributes[name];
    if (location === undefined || location < 0) return;
    gl.disableVertexAttribArray(location);
  }

  /** Points the entry's attributes at a vertex buffer's bytes. */
  function bindVertices(entry, buffer, stride, baseOffset, shape) {
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    const floats = gl.FLOAT;
    bindAttribute(entry, "position", baseOffset + shape.position.offset, shape.position.size, floats, false, stride);
    if (shape.diffuse !== null) {
      // Direct3D's vertex colour is a BGRA byte quadruple, which GL takes as
      // UNSIGNED_BYTE with normalized channels and swapped order.
      bindAttribute(
        entry,
        "diffuse",
        baseOffset + shape.diffuse,
        4,
        gl.UNSIGNED_BYTE,
        true,
        stride,
      );
    } else {
      disableAttribute(entry, "diffuse");
    }
    const coordinate = shape.texcoords[0];
    if (coordinate && entry.textured) {
      bindAttribute(
        entry,
        "texcoord0",
        baseOffset + coordinate.offset,
        coordinate.size,
        floats,
        false,
        stride,
      );
    } else {
      disableAttribute(entry, "texcoord0");
    }
  }

  // ---- resources ---------------------------------------------------------

  function uploadBuffer(target, buffer, bytes) {
    gl.bindBuffer(target, buffer);
    gl.bufferData(target, bytes, gl.DYNAMIC_DRAW);
  }

  /** A texture's level 0 in RGBA8, converted from whichever format it was. */
  function pixels(bytes, base, record) {
    const { width, height, pitch, format } = record;
    const out = new Uint8Array(width * height * 4);
    for (let y = 0; y < height; y += 1) {
      const row = base + y * pitch;
      const line = y * width * 4;
      if (FORMATS[format] === 4) {
        for (let x = 0; x < width; x += 1) {
          const at = row + x * 4;
          const to = line + x * 4;
          out[to] = bytes[at + 2];
          out[to + 1] = bytes[at + 1];
          out[to + 2] = bytes[at];
          // X8R8G8B8 has no alpha channel at all: its fourth byte is padding.
          out[to + 3] = format === 22 ? 0xff : bytes[at + 3];
        }
      } else if (FORMATS[format] === 2) {
        for (let x = 0; x < width; x += 1) {
          const at = row + x * 2;
          const value = bytes[at] | (bytes[at + 1] << 8);
          const to = line + x * 4;
          if (format === 26) {
            const expand = (v) => (v << 4) | v;
            const red = (value >> 8) & 0xf;
            const green = (value >> 4) & 0xf;
            const blue = value & 0xf;
            out[to] = expand(red);
            out[to + 1] = expand(green);
            out[to + 2] = expand(blue);
            out[to + 3] = expand((value >> 12) & 0xf);
          } else {
            const red = (value >> 11) & 0x1f;
            const green = (value >> 5) & 0x3f;
            const blue = value & 0x1f;
            out[to] = (red << 3) | (red >> 2);
            out[to + 1] = (green << 2) | (green >> 4);
            out[to + 2] = (blue << 3) | (blue >> 2);
            out[to + 3] = format === 25 ? (value & 0x8000 ? 0xff : 0) : 0xff;
          }
        }
      } else if (format === 50) {
        // L8 is luminance: the same byte in all three channels.
        for (let x = 0; x < width; x += 1) {
          const value = bytes[row + x];
          const to = line + x * 4;
          out[to] = value;
          out[to + 1] = value;
          out[to + 2] = value;
          out[to + 3] = 0xff;
        }
      } else {
        // A8: alpha only, black elsewhere.
        for (let x = 0; x < width; x += 1) {
          out[line + x * 4 + 3] = bytes[row + x];
        }
      }
    }
    return out;
  }

  function applySampling(texture) {
    const filter = (kind) => (stageValue(kind, 1) === 2 ? gl.LINEAR : gl.NEAREST);
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter(TSS.MINFILTER));
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter(TSS.MAGFILTER));
    gl.texParameteri(
      gl.TEXTURE_2D,
      gl.TEXTURE_WRAP_S,
      ADDRESS[stageValue(TSS.ADDRESSU, 1)] ?? gl.REPEAT,
    );
    gl.texParameteri(
      gl.TEXTURE_2D,
      gl.TEXTURE_WRAP_T,
      ADDRESS[stageValue(TSS.ADDRESSV, 1)] ?? gl.REPEAT,
    );
  }

  // ---- records -----------------------------------------------------------

  function createTexture(record) {
    const texture = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, texture);
    gl.texImage2D(
      gl.TEXTURE_2D,
      0,
      gl.RGBA,
      record.width,
      record.height,
      0,
      gl.RGBA,
      gl.UNSIGNED_BYTE,
      null,
    );
    textures.set(record.handle, { ...record, texture, uploaded: false });
  }

  function updateTexture(record, memory) {
    const entry = textures.get(record.handle);
    if (!entry) return;
    const converted = pixels(memory, base() + record.guest, {
      width: record.width,
      height: record.height,
      pitch: record.pitch,
      format: record.format,
    });
    gl.bindTexture(gl.TEXTURE_2D, entry.texture);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.texImage2D(
      gl.TEXTURE_2D,
      0,
      gl.RGBA,
      record.width,
      record.height,
      0,
      gl.RGBA,
      gl.UNSIGNED_BYTE,
      converted,
    );
    applySampling(entry.texture);
    entry.uploaded = true;
  }

  function drawRecord(record, indexed) {
    if (!record.vertices) return;
    const shape = { ...(layout(fvf) ?? {}), fvf };
    if (!shape.position) {
      once(`fvf:${fvf}`, `WebGL2: FVF ${fvf.toString(16)} has no position this backend can read`);
      return;
    }
    const source = streams.get(record.stream ?? 0);
    if (!source || !source.handle) {
      once("stream", "WebGL2: a draw names a stream source this page has not been given");
      return;
    }
    const buffer = buffers.get(source.handle);
    if (!buffer) return;
    const stride = source.stride || shape.stride;
    const entry = usePipeline(shape);
    applyState(entry, worldViewProjection());
    // `BaseVertexIndex` is added to every index, so moving the vertex base by
    // that many strides is the same arithmetic without touching the indices.
    const vertexBase =
      (indexed ? indices.base : record.first) * stride + source.base;
    bindVertices(entry, buffer.buffer, stride, vertexBase, shape);
    if (indexed) {
      const index = buffers.get(indices.handle);
      if (!index) return;
      gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, index.buffer);
      gl.drawElements(
        MODES[record.topology],
        record.vertices,
        gl.UNSIGNED_SHORT,
        (record.first_index ?? 0) * 2,
      );
    } else {
      gl.drawArrays(MODES[record.topology], 0, record.vertices);
    }
  }

  function drawUp(count, topology, shape, stride) {
    const entry = usePipeline(shape);
    applyState(entry, worldViewProjection());
    if (!scratch) scratch = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, scratch);
    gl.bufferData(gl.ARRAY_BUFFER, count.bytes, gl.STREAM_DRAW);
    bindVertices(entry, scratch, stride, 0, shape);
    gl.drawArrays(MODES[topology], 0, count.vertices);
  }

  function render() {
    const length = host.wh_render_len();
    // Nothing recorded: the frame is whatever the last draw left in the drawing
    // buffer, so the caller still blits it.
    if (!length) return true;
    const memory = memoryView();
    const pointer = host.wh_render_ptr();
    const view = new DataView(host.memory.buffer, pointer, length);
    let at = 0;
    while (at + 8 <= length) {
      const op = view.getUint32(at, true);
      const size = view.getUint32(at + 4, true);
      const start = at + 8;
      const end = start + size;
      if (end > length) {
        once("truncated", `WebGL2: record ${op} runs past the end of the stream`);
        break;
      }
      const u32 = (index) => view.getUint32(start + index * 4, true);
      const f32At = (index) => view.getFloat32(start + index * 4, true);
      switch (op) {
        case OP.CREATE_DEVICE: {
          device = { width: u32(0), height: u32(1) };
          if (canvas.width !== device.width || canvas.height !== device.height) {
            canvas.width = device.width;
            canvas.height = device.height;
          }
          gl.viewport(0, 0, device.width, device.height);
          viewport = { x: 0, y: 0, width: device.width, height: device.height };
          gl.enable(gl.DEPTH_TEST);
          break;
        }
        case OP.CLEAR: {
          const flags = u32(0);
          const colour = u32(1);
          gl.disable(gl.SCISSOR_TEST);
          gl.depthMask(true);
          gl.clearColor(
            ((colour >> 16) & 0xff) / 255,
            ((colour >> 8) & 0xff) / 255,
            (colour & 0xff) / 255,
            ((colour >>> 24) & 0xff) / 255,
          );
          gl.clearDepth(f32At(2));
          gl.clear(
            (flags & 1 ? gl.COLOR_BUFFER_BIT : 0) | (flags & 2 ? gl.DEPTH_BUFFER_BIT : 0),
          );
          break;
        }
        case OP.SET_RENDER_STATE:
          renderStates.set(u32(0), u32(1));
          break;
        case OP.SET_TRANSFORM: {
          const values = new Float32Array(16);
          for (let index = 0; index < 16; index += 1) values[index] = f32At(index + 1);
          transforms.set(u32(0), values);
          break;
        }
        case OP.SET_FVF:
          fvf = u32(0);
          break;
        case OP.CREATE_VERTEX_BUFFER: {
          const buffer = gl.createBuffer();
          uploadBuffer(gl.ARRAY_BUFFER, buffer, u32(1));
          buffers.set(u32(0), { buffer, size: u32(1), fvf: u32(2), kind: "vertex" });
          break;
        }
        case OP.UPDATE_VERTEX_BUFFER:
        case OP.UPDATE_INDEX_BUFFER: {
          const target = op === OP.UPDATE_VERTEX_BUFFER ? gl.ARRAY_BUFFER : gl.ELEMENT_ARRAY_BUFFER;
          const entry = buffers.get(u32(0));
          if (!entry) break;
          gl.bindBuffer(target, entry.buffer);
          const offset = u32(1);
          const count = u32(2);
          const guest = u32(3);
          gl.bufferSubData(target, offset, memory.subarray(base() + guest, base() + guest + count));
          break;
        }
        case OP.SET_STREAM_SOURCE:
          streams.set(u32(0), { handle: u32(1), stride: u32(2), base: u32(3) });
          break;
        case OP.CREATE_INDEX_BUFFER: {
          const buffer = gl.createBuffer();
          uploadBuffer(gl.ELEMENT_ARRAY_BUFFER, buffer, u32(1));
          buffers.set(u32(0), { buffer, size: u32(1), kind: "index" });
          break;
        }
        case OP.SET_INDICES:
          indices = { handle: u32(0), base: u32(1) };
          break;
        case OP.DRAW:
          drawRecord({ topology: u32(0), vertices: u32(2), first: u32(1) }, false);
          break;
        case OP.DRAW_INDEXED:
          drawRecord(
            {
              topology: u32(0),
              vertices: indexCount(u32(0), u32(4)),
              first_index: u32(3),
              first: 0,
            },
            true,
          );
          break;
        case OP.DRAW_UP: {
          const topology = u32(0);
          const stride = u32(1);
          const shape = layout(u32(2));
          const guest = u32(3);
          const count = u32(4);
          if (!shape) break;
          const bytes = memory.subarray(base() + guest, base() + guest + count * stride);
          drawUp({ bytes, vertices: count }, topology, { ...shape, fvf: u32(2) }, stride);
          break;
        }
        case OP.DRAW_INDEXED_UP: {
          const topology = u32(0);
          const stride = u32(1);
          const encoded = u32(2);
          const shape = layout(encoded);
          const indexGuest = u32(3);
          const vertexGuest = u32(4);
          const count = u32(6);
          const primitives = u32(7);
          if (!shape) break;
          const entry = usePipeline({ ...shape, fvf: encoded });
          applyState(entry, worldViewProjection());
          if (!scratch) scratch = gl.createBuffer();
          gl.bindBuffer(gl.ARRAY_BUFFER, scratch);
          gl.bufferData(
            gl.ARRAY_BUFFER,
            memory.subarray(base() + vertexGuest, base() + vertexGuest + count * stride),
            gl.STREAM_DRAW,
          );
          bindVertices(entry, scratch, stride, 0, shape);
          const count2 = indexCount(topology, primitives);
          const element = gl.createBuffer();
          gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER, element);
          gl.bufferData(
            gl.ELEMENT_ARRAY_BUFFER,
            memory.subarray(base() + indexGuest, base() + indexGuest + count2 * 2),
            gl.STREAM_DRAW,
          );
          gl.drawElements(MODES[topology], count2, gl.UNSIGNED_SHORT, 0);
          gl.deleteBuffer(element);
          break;
        }
        case OP.CREATE_TEXTURE:
          createTexture({
            handle: u32(0),
            width: u32(1),
            height: u32(2),
            levels: u32(3),
            format: u32(4),
            size: u32(5),
          });
          break;
        case OP.UPDATE_TEXTURE:
          updateTexture(
            {
              handle: u32(0),
              guest: u32(2),
              pitch: u32(3),
              width: u32(4),
              height: u32(5),
              format: u32(6),
            },
            memory,
          );
          break;
        case OP.SET_TEXTURE:
          bound.set(u32(0), u32(1));
          if (u32(1)) {
            const entry = textures.get(u32(1));
            if (entry) applySampling(entry.texture);
          }
          break;
        case OP.SET_TEXTURE_STAGE_STATE:
          // Only stage 0 is drawn: the backend has one texture unit of state, so
          // a later stage's settings must not overwrite it.
          if (u32(0) === 0) stageStates.set(u32(1), u32(2));
          break;
        case OP.SET_VIEWPORT: {
          viewport = {
            x: u32(0),
            y: u32(1),
            width: u32(2),
            height: u32(3),
            minZ: f32At(4),
            maxZ: f32At(5),
          };
          // Direct3D's viewport origin is the top left of the buffer; GL's is the
          // bottom left of the window.
          gl.viewport(
            viewport.x,
            device.height - viewport.y - viewport.height,
            viewport.width,
            viewport.height,
          );
          gl.depthRange(viewport.minZ, viewport.maxZ);
          break;
        }
        case OP.PRESENT:
          break;
        case OP.DESTROY: {
          const handle = u32(0);
          const entry = buffers.get(handle);
          if (entry) {
            gl.deleteBuffer(entry.buffer);
            buffers.delete(handle);
          }
          const texture = textures.get(handle);
          if (texture) {
            gl.deleteTexture(texture.texture);
            textures.delete(handle);
            if (bound.get(0) === handle) bound.delete(0);
          }
          break;
        }
        default:
          once(`op:${op}`, `WebGL2: record ${op} is not part of this backend`);
          break;
      }
      at = end;
    }
    host.wh_render_clear();
    return true;
  }

  return {
    canvas,
    render() {
      if (broken) return false;
      try {
        return render();
      } catch (error) {
        broken = error;
        onDiagnostic(`WebGL2: stopped drawing: ${error.message ?? error}`);
        return false;
      }
    },
    /** The drawing buffer's size, which is the module's back buffer. */
    size() {
      return { ...device };
    },
    dispose() {
      for (const entry of buffers.values()) gl.deleteBuffer(entry.buffer);
      for (const entry of textures.values()) gl.deleteTexture(entry.texture);
      for (const entry of programs.values()) gl.deleteProgram(entry.program);
      buffers.clear();
      textures.clear();
      programs.clear();
    },
  };
}
