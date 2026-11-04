// End-to-end check of the demo site and its baked payloads, without a browser.
//
// The site under `demo/` is built output: an index, its assets, and one folder
// per payload. This serves it the way GitHub Pages will (a static tree, relative
// URLs), follows the same fetches the page's loader makes, and boots the payload
// through the module's own ABI until the guest presents a lit frame.
//
//   bun tools/demo-check.ts [demo-dir] [instructions] [slug]
//
// With a slug, only that payload is tested, listed in `index.json` or not: a
// payload baked beside the site for one machine is checked the same way as one
// that is published, and `tools/build-web.sh <entry.exe> demo/payloads/<slug>`
// is what puts it there.
//
// A lit frame is the first success condition: it means the baked executable
// runs, the data files arrived intact, and the shims answer what it asks for.
// The second is that the picture is *coherent* -- that neighbouring pixels land
// on neighbouring colours, which is what stops being true when a frame is
// coloured by a palette it was not drawn for. `lit` cannot see that: a frame of
// the wrong colours is still a frame full of lit pixels.
//
// A payload whose picture arrives correctly and then decays (Hercules' intro,
// where the palette slips a third of the way in) says so with `settle`: how many
// more instructions to run after the first picture, sampling as it goes. The
// worst frame seen while a picture is on screen is the one judged.

import { join, resolve } from "node:path";

import { mergedIndex, payloadFile, payloadRoots } from "./payloads";

/// How far apart neighbouring pixels may be, in summed channel distance, before
/// the picture is judged to be coloured wrongly. Measured: a correct frame comes
/// out at 3-13 and a frame coloured by the wrong palette at 30-69, so the bound
/// only has to sit in the gap between them.
const APART_LIMIT = 20;

const root = resolve(process.argv[2] ?? "demo");
const roots = payloadRoots(resolve(root, ".."));
const limit = Number(process.argv[3] ?? "120000000");
const only = process.argv[4] ?? null;

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
    if (path === "/payloads/index.json") return Response.json(await mergedIndex(roots));
    if (path.startsWith("/payloads/")) {
      const file = await payloadFile(roots, path.slice("/payloads/".length));
      return file ? new Response(file) : new Response("not found", { status: 404 });
    }
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

  // A payload may say how it boots: `instructions` is the budget one screen
  // takes to appear, and `lit` the share of the frame that screen fills. A game
  // that renders every music track at load needs far more than the default, and
  // a title screen on black does not fill a tenth of the picture.
  type Entry = { slug: string; title: string; instructions?: number; lit?: number; settle?: number };
  const listed: Entry[] =
    ((await (await fetch(`${base}/payloads/index.json`)).json()).payloads ?? []);
  const payloads: Entry[] = only
    ? [listed.find((entry) => entry.slug === only) ?? { slug: only, title: only }]
    : listed;
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
      const pointer = host.wh_message_ptr() >>> 0;
      const length = host.wh_message_len();
      if (!pointer || !length) return "";
      return new TextDecoder().decode(new Uint8Array(host.memory.buffer, pointer, length));
    };

    host.wh_set_screen(artifact.screen.width, artifact.screen.height, artifact.screen.depth);
    // The platform seed the page applies before it loads. A launcher written for
    // Windows 98 refuses to run on a machine that reports XP and exits during
    // start-up, so a check that skipped this would fail a payload the site boots.
    if (artifact.windows) {
      host.wh_set_windows(
        artifact.windows.major,
        artifact.windows.minor,
        artifact.windows.build,
        artifact.windows.platform,
      );
    }
    const name = new TextEncoder().encode(executableName);
    let pointer = host.wh_alloc(name.length) >>> 0;
    new Uint8Array(host.memory.buffer, pointer, name.length).set(name);
    host.wh_set_exe_name(pointer, name.length);

    const image = new Uint8Array(
      await (await fetch(`${at}/game/${encodeURIComponent(executable.name)}`)).arrayBuffer(),
    );
    pointer = host.wh_alloc(image.length) >>> 0;
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
      const buffer = host.wh_alloc(encoded.length + contents.length) >>> 0;
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
      const offset = (host.wh_memory_ptr() >>> 0) + host.wh_framebuffer_offset();
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

    /**
     * How far apart neighbouring pixels land in colour.
     *
     * A picture is drawn on the assumption that pixels next to each other look
     * alike: that is what dithering is, and it is why a palettised frame's
     * indices are neighbours in the palette as well as on the screen. When the
     * palette that colours a frame is not the one the frame was drawn for, every
     * adjacent pair jumps to an unrelated colour and the number goes up by an
     * order of magnitude -- which is what "the intro looks like static" is, in
     * one measurement.
     *
     * This is the check that `lit` cannot make. A frame of the wrong colours is
     * still a frame full of lit pixels: Hercules presented 209 wrong colours and
     * 6,476 lit pixels while its intro was unreadable noise.
     */
    const coherence = (): number => {
      const width = host.wh_framebuffer_width();
      const height = host.wh_framebuffer_height();
      const pitch = host.wh_framebuffer_pitch();
      const offset = (host.wh_memory_ptr() >>> 0) + host.wh_framebuffer_offset();
      const depth = host.wh_framebuffer_depth();
      if (!width || !height || !offset) return -1;
      const memory = new Uint8Array(host.memory.buffer);
      const palette = new Uint32Array(256);
      if (depth === 8) {
        for (let index = 0; index < 256; index += 1) palette[index] = host.wh_palette(index);
      }
      // The colour of one pixel, as three channels, in the layout the page reads.
      const at = (x: number, y: number): [number, number, number] => {
        const base = offset + y * pitch;
        if (depth === 8) {
          const colour = palette[memory[base + x]];
          return [(colour >> 16) & 0xff, (colour >> 8) & 0xff, colour & 0xff];
        }
        if (depth === 16) {
          const value = memory[base + x * 2] | (memory[base + x * 2 + 1] << 8);
          return [((value >> 11) & 0x1f) << 3, ((value >> 5) & 0x3f) << 2, (value & 0x1f) << 3];
        }
        return [memory[base + x * 4 + 2], memory[base + x * 4 + 1], memory[base + x * 4]];
      };
      let total = 0;
      let samples = 0;
      for (let y = 0; y < height; y += 2) {
        for (let x = 0; x + 1 < width; x += 2) {
          const a = at(x, y);
          const b = at(x + 1, y);
          total += Math.abs(a[0] - b[0]) + Math.abs(a[1] - b[1]) + Math.abs(a[2] - b[2]);
          samples += 1;
        }
      }
      return total / Math.max(samples, 1);
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
      // A Direct3D payload has no framebuffer to measure: the picture is drawn
      // by the page's WebGL2 backend from a command stream, so the evidence here
      // is the draw itself and a presented frame.
      if (host.wh_render_width && host.wh_render_width() > 0) {
        if (host.wh_render_draws() > 0 && host.wh_flips() > 0) break;
        continue;
      }
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

    // The first picture is not the picture. A title that arrives correctly and
    // then decays as the guest plays on is a broken demo, and the frame the loop
    // above stopped on is the one before it went wrong -- Hercules' palette does
    // not slip until the video is a third of the way in. A payload that wants
    // that caught says how many instructions to keep running for, and
    // `coherence` below measures the picture it settles on.
    //
    // The count is the guest's own (`wh_instructions`), not the sum of the
    // budgets: `wh_run` returns early whenever every thread parks, which it does
    // constantly here, so an accumulated total runs far ahead of what the guest
    // has actually executed -- 40M counted against 25M executed, on Hercules.
    // The size, depth and `lit` checks stay on the first picture: they ask
    // whether a picture arrived at all, which is a different question.
    const settle = payload.settle ?? 0;
    const stop = Math.round(host.wh_instructions()) + settle;
    // The clock has to move on its own here. A guest waiting on a deadline is
    // not running, so `wh_run` comes back with nothing done and an
    // instruction-derived clock would never reach the deadline -- the loop would
    // spin. One step per frame is what the page does, and it is what wakes the
    // waiters up. The cap is a guard, not a budget: a guest that stops making
    // progress should end the check, not hang it.
    let clock = Math.round(host.wh_instructions()) / 33_000;
    // The picture to judge is the worst one seen while a picture was on screen,
    // not the last one: a demo that ends on a black screen would otherwise score
    // zero and pass. A frame that has gone to noise keeps the high number, so
    // the worst case is what survives.
    let apart = coherence();
    for (let frame = 0; settle > 0 && outcome === 0 && frame < 4000; frame += 1) {
      if (Math.round(host.wh_instructions()) >= stop) break;
      clock += 16;
      host.wh_set_time(clock);
      outcome = host.wh_run(2_000_000);
      if (frame % 4 !== 0) continue;
      const now = drawn();
      if (now.lit > now.width * now.height * fills) apart = Math.max(apart, coherence());
    }

    const presented = Math.max(host.wh_flips(), host.wh_frame_version());
    check(
      presented > 0,
      `${payload.title}: presented ${presented} frame(s) after ${ran} instructions`,
    );
    // A 3D payload's frame is a command stream, not pixels in linear memory, so
    // the framebuffer checks below have nothing to measure. What it must have
    // done is draw through the device at the size the artifact declares.
    if (host.wh_render_width && host.wh_render_width() > 0) {
      check(
        host.wh_render_draws() > 0,
        `${payload.title}: drew ${host.wh_render_draws()} primitive(s) through the device`,
      );
      check(
        host.wh_render_width() === artifact.screen.width &&
          host.wh_render_height() === artifact.screen.height,
        `${payload.title}: back buffer is ${host.wh_render_width()}x${host.wh_render_height()}`,
      );
      continue;
    }
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
    // A picture whose neighbouring pixels land on unrelated colours is a picture
    // coloured by a palette it was not drawn for. The bound is loose on purpose:
    // a correct frame measures 3-8 and a mis-coloured one 30-69, so the number
    // only has to sit in the gap between them.
    if (apart >= 0) {
      check(
        apart < APART_LIMIT,
        `${payload.title}: neighbouring pixels are ${apart.toFixed(1)} apart in colour`,
      );
    }
  }
} finally {
  server.stop(true);
}

if (failures > 0) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("the demo boots");
