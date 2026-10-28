// A recording stand-in for a WebGL2 context.
//
// The checks that exercise `demo-site/src/lib/webgl2.js` run without a browser,
// so the backend is handed an object that answers the handful of questions a
// real context answers (a shader compiled, a location, an object handle) and
// records every other call in order. That is enough to prove which GL call a
// record turns into, with which arguments, and -- for a texture -- which bytes
// reached the upload.
//
// There is no rasteriser here. Whether a shader compiles and whether a triangle
// covers the pixels it should are the two things only a driver can answer, and
// they are checked by looking at a real frame.

export type Call = { name: string; args: unknown[] };

export type Recorder = {
  gl: any;
  /** Every call, in order, since the last `reset`. */
  calls: Call[];
  /** The source of every shader compiled, since the last `reset(true)`. */
  shaders: string[];
  /** GL enum values, one per constant queried, so two are never equal by accident. */
  constants: Map<string, number>;
  /** The first call with this name since the last reset. */
  find(name: string): Call | undefined;
  /** Every call with this name. */
  all(name: string): Call[];
  names(): string[];
  reset(shadersToo?: boolean): void;
};

export function recorder(): Recorder {
  const calls: Call[] = [];
  const shaders: string[] = [];
  const constants = new Map<string, number>();
  let next = 0x1000;

  const responses: Record<string, (...args: any[]) => unknown> = {
    createShader: () => ({ shader: true }),
    createProgram: () => ({ program: true }),
    createBuffer: () => ({ buffer: true }),
    createTexture: () => ({ texture: true }),
    getShaderParameter: () => true,
    getProgramParameter: () => true,
    getShaderInfoLog: () => "",
    getProgramInfoLog: () => "",
    getAttribLocation: (_program: unknown, name: string) =>
      ["aPosition", "aDiffuse", "aTexcoord0"].indexOf(name),
    getUniformLocation: (_program: unknown, name: string) => ({ uniform: name }),
  };

  const gl: any = new Proxy(
    {},
    {
      get(_target, property) {
        const name = String(property);
        // An upper-case name is a GL constant: hand out a number and remember it.
        if (/^[A-Z][A-Z0-9_]*$/.test(name)) {
          if (!constants.has(name)) constants.set(name, (next += 1));
          return constants.get(name);
        }
        return (...args: unknown[]) => {
          if (name === "shaderSource") shaders.push(String(args[1]));
          calls.push({ name, args });
          return responses[name]?.(...args) ?? undefined;
        };
      },
    },
  );

  return {
    gl,
    calls,
    shaders,
    constants,
    find: (name) => calls.find((call) => call.name === name),
    all: (name) => calls.filter((call) => call.name === name),
    names: () => calls.map((call) => call.name),
    reset(shadersToo = false) {
      calls.length = 0;
      if (shadersToo) shaders.length = 0;
    },
  };
}
