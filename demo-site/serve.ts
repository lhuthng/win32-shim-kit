// Serves `demo/` the way GitHub Pages will: a static tree, relative URLs, no
// rewriting. Run it with Bun while working on the site:
//
//   bun demo-site/serve.ts [port]
//
// The build writes into `demo/`, so this is the same bytes a visitor gets,
// payloads and all.

import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..", "demo");
const port = Number(process.argv[2] ?? "3000");

const server = Bun.serve({
  port,
  async fetch(request) {
    const path = decodeURIComponent(new URL(request.url).pathname);
    const file = Bun.file(join(root, path === "/" ? "index.html" : path));
    if (!(await file.exists())) return new Response("not found", { status: 404 });
    return new Response(file);
  },
});

console.log(`serving ${root} on http://localhost:${server.port}`);
