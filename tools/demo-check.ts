// End-to-end check of the demo site and its baked payloads, without a browser.
//
// The site under `demo/` is built output: an index, its assets, and one folder
// per payload. This serves it the way GitHub Pages will (a static tree, relative
// URLs), follows the same fetches the page's loader makes, and boots the payload
// through the module's own ABI until the guest presents a lit frame.
//
//   bun tools/demo-check.ts [demo-dir] [instructions]
//
// The first lit frame is the success condition: it means the baked executable
// runs, the data files arrived intact, and the shims answer what it asks for.

import { join, resolve } from "node:path";

const root = resolve(process.argv[2] ?? "demo");
const limit = Number(process.argv[3] ?? "120000000");

let failures = 0;
function check(condition: boolean, message: string): void {
  if (condition) {
    console.log(`  ok   ${message}`);
  } else {
    failures += 1;
    console.log(`  FAIL ${message}`);
  }
}

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
  console.log(`serving ${root} on ${base}`);

  const page = await fetch(`${base}/`);
  check(page.ok, `the page loads (${page.status})`);
  const html = await page.text();
  check(/<script[^>]+assets\//.test(html), "the page points at a built script bundle");
  const styles = html.match(/assets\/[A-Za-z0-9._-]+\.css/);
  if (styles) {
    check((await fetch(`${base}/${styles[0]}`)).ok, `the stylesheet ${styles[0]} is served`);
  }

  const listed = await (await fetch(`${base}/payloads/index.json`)).json();
  // A payload may say how it boots: `instructions` is the budget one screen
  // takes to appear, and `lit` the share of the frame that screen fills. A game
  // that renders every music track at load needs far more than the default, and
  // a title screen on black does not fill a tenth of the picture.
  const payloads: Array<{ slug: string; title: string; instructions?: number; lit?: number }> =
    listed.payloads ?? [];
  check(payloads.length > 0, `${payloads.length} demo(s) listed`);

  for (const payload of payloads) {
    const at = `${base}/payloads/${payload.slug}`;
    const artifact = await (await fetch(`${at}/artifact.json`)).json();
    const files: Array<{ name: string; bytes: number }> = await (
      await fetch(`${at}/game/files.json`)
    ).json();
    check(files.length > 1, `${payload.title}: ${files.length} files in the payload`);

    const executableName = artifact.default ?? artifact.executables?.[0]?.name;
    const executable = files.find(
      (file) => file.name.toLowerCase() === executableName.toLowerCase(),
    );
    if (!executable) {
      check(false, `${payload.title}: files.json names ${executableName}`);
      continue;
    }

    const wasm = await fetch(`${at}/host_wasm.wasm`);
    check(wasm.ok, `${payload.title}: the module is served (${wasm.status})`);
    const { instance } = await WebAssembly.instantiate(await wasm.arrayBuffer(), {});
    const host = instance.exports as unknown as Record<string, any>;

    const message = (): string => {
      const pointer = host.wh_message_ptr();
      const length = host.wh_message_len();
      if (!pointer || !length) return "";
      return new TextDecoder().decode(new Uint8Array(host.memory.buffer, pointer, length));
    };

    host.wh_set_screen(artifact.screen.width, artifact.screen.height, artifact.screen.depth);
    const name = new TextEncoder().encode(executableName);
    let pointer = host.wh_alloc(name.length);
    new Uint8Array(host.memory.buffer, pointer, name.length).set(name);
    host.wh_set_exe_name(pointer, name.length);

    const image = new Uint8Array(
      await (await fetch(`${at}/game/${encodeURIComponent(executable.name)}`)).arrayBuffer(),
    );
    pointer = host.wh_alloc(image.length);
    new Uint8Array(host.memory.buffer, pointer, image.length).set(image);
    check(host.wh_load(pointer, image.length) === 0, `${payload.title}: loads (${message()})`);

    let provided = 0;
    for (const file of files) {
      if (file.name.toLowerCase() === executableName.toLowerCase()) continue;
      const response = await fetch(`${at}/game/${encodeURIComponent(file.name)}`);
      if (!response.ok) {
        check(false, `${payload.title}: ${file.name} fetches (${response.status})`);
        continue;
      }
      const contents = new Uint8Array(await response.arrayBuffer());
      const encoded = new TextEncoder().encode(file.name);
      const buffer = host.wh_alloc(encoded.length + contents.length);
      const memory = new Uint8Array(host.memory.buffer);
      memory.set(encoded, buffer);
      memory.set(contents, buffer + encoded.length);
      if (host.wh_provide_file(buffer, encoded.length, buffer + encoded.length, contents.length) === 0) {
        provided += 1;
      }
    }
    check(provided === files.length - 1, `${payload.title}: ${provided} data files provided`);

    /** How many palette entries are not black. */
    const paletteColours = (): number => {
      let colours = 0;
      for (let index = 0; index < 256; index += 1) {
        if (host.wh_palette(index) !== 0) colours += 1;
      }
      return colours;
    };

    /** Pixels that are not the cleared colour, in the frame the page presents. */
    const drawn = (): { lit: number; width: number; height: number; depth: number } => {
      const width = host.wh_framebuffer_width();
      const height = host.wh_framebuffer_height();
      const pitch = host.wh_framebuffer_pitch();
      // The offset is relative to the guest image base, which lives at the start
      // of the module's linear memory.
      const offset = host.wh_memory_ptr() + host.wh_framebuffer_offset();
      const depth = host.wh_framebuffer_depth();
      if (!width || !height || !host.wh_framebuffer_offset()) {
        return { lit: 0, width, height, depth };
      }
      const memory = new Uint8Array(host.memory.buffer);
      const bytes = depth / 8;
      let lit = 0;
      for (let y = 0; y < height; y += 1) {
        for (let x = 0; x < width; x += 1) {
          if (memory[offset + y * pitch + x * bytes] !== 0) lit += 1;
        }
      }
      return { lit, width, height, depth, colours: 0 };
    };

    // The page's own clock: thirty frames per million instructions, which is what
    // the probe and the browser's budget amount to. A demo that fades in presents
    // black frames first, so the run continues until the picture is actually
    // there rather than stopping at the first flip.
    const budget = payload.instructions ?? limit;
    const fills = payload.lit ?? 0.1;
    let ran = 0;
    let outcome = 0;
    let frame: { lit: number; width: number; height: number; depth: number; colours: number } = drawn();
    while (ran < budget) {
      host.wh_set_time(ran / 33_000);
      outcome = host.wh_run(2_000_000);
      ran += 2_000_000;
      if (outcome !== 0) break;
      // A frame is either a `Flip` or a write to the presented surface: a guest
      // that blits straight onto the primary presents without ever flipping.
      if (host.wh_flips() > 0 || host.wh_frame_version() > 0) {
        frame = drawn();
        if (frame.lit > frame.width * frame.height * fills) {
          // Lit indices are not a picture until the palette colours them: a game
          // that fades in presents a full screen of indices against black first.
          frame.colours = paletteColours();
          if (frame.depth !== 8 || frame.colours > 16) break;
        }
      }
    }
    check(outcome !== 3, `${payload.title}: did not fault (${message()})`);

    const presented = Math.max(host.wh_flips(), host.wh_frame_version());
    check(
      presented > 0,
      `${payload.title}: presented ${presented} frame(s) after ${ran} instructions`,
    );
    check(
      frame.width === artifact.screen.width && frame.height === artifact.screen.height,
      `${payload.title}: frame is ${frame.width}x${frame.height}x${frame.depth}`,
    );
    check(
      frame.depth === 8 || frame.depth === 16 || frame.depth === 32,
      `${payload.title}: depth ${frame.depth}`,
    );
    if (frame.depth === 8) {
      check(
        frame.colours > 16,
        `${payload.title}: the palette holds ${frame.colours} colours`,
      );
    }
    check(
      frame.lit > frame.width * frame.height * fills,
      `${payload.title}: ${frame.lit} of ${frame.width * frame.height} pixels are drawn`,
    );
  }
} finally {
  server.stop(true);
}

if (failures > 0) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("the demo boots");
