// Serves `demo/` the way GitHub Pages will: a static tree, relative URLs, no
// rewriting. Run it with Bun while working on the site:
//
//   bun demo-site/serve.ts [port]
//
// The build writes into `demo/`, so this is the same bytes a visitor gets,
// payloads and all -- with one addition. `local/payloads/` is read too, and its
// entries are merged into the list this server answers for `index.json`. That
// folder is gitignored: it holds the payloads whose game files are not the
// repository's to ship, so they can be played here and never be published.
//
// Nothing about that is visible to a visitor, because a published `demo/` has no
// local tree beside it and the merge finds nothing. `tools/payloads.ts` owns the
// lookup for both this server and the two `tools/demo-*.ts` checkers.

import { resolve } from "node:path";

import { mergedIndex, payloadFile, payloadRoots } from "../tools/payloads";

const root = resolve(import.meta.dir, "..", "demo");
const roots = payloadRoots(resolve(import.meta.dir, ".."));
const port = Number(process.argv[2] ?? "3000");

const server = Bun.serve({
  port,
  async fetch(request) {
    const path = decodeURIComponent(new URL(request.url).pathname);

    if (path === "/payloads/index.json") return Response.json(await mergedIndex(roots));
    if (path.startsWith("/payloads/")) {
      const file = await payloadFile(roots, path.slice("/payloads/".length));
      return file ? new Response(file) : new Response("not found", { status: 404 });
    }

    const file = Bun.file(resolve(root, path === "/" ? "index.html" : `.${path}`));
    if (!(await file.exists())) return new Response("not found", { status: 404 });
    return new Response(file);
  },
});

console.log(`serving ${root} on http://localhost:${server.port}`);
