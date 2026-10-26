import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

// The output directory is `demo/`, which GitHub Pages serves from a
// repository sub-path, so every URL the build writes is relative. It is not
// emptied first: the baked payloads (`demo/payloads/`) live beside the site and
// are written by `tools/build-web.sh`, not by this build.
export default defineConfig({
  plugins: [svelte(), tailwindcss()],
  base: "/",
  build: {
    outDir: "../demo",
    emptyOutDir: false,
  },
});
