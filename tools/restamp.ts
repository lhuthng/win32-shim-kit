// Replaces a payload's `host_wasm.wasm` and restamps `artifact.json`.
//
// A payload is a module plus a game plus an `artifact.json` that says what the
// build produced. `module.bytes` and `module.build` are not decoration: the
// page keys its fetch on the stamp, so a module swapped in without it can be
// served from a cache that still holds the old shims. `shimkit-bake` writes both
// when it bakes, but re-baking needs the game install; this updates a payload
// that already has one.
//
//   bun tools/restamp.ts <payload-dir> <wasm>
//
// The stamp is the same FNV-1a 64 that `crates/bake/src/main.rs` writes, so a
// payload restamped here and one baked by the kit are indistinguishable.

import { readFileSync, statSync, writeFileSync, copyFileSync } from "node:fs";
import { join } from "node:path";

const [payloadDir, wasmPath] = process.argv.slice(2);
if (!payloadDir || !wasmPath) {
  console.error("usage: bun tools/restamp.ts <payload-dir> <wasm>");
  process.exit(2);
}

const bytes = readFileSync(wasmPath);
let hash = 0xcbf29ce484222325n;
for (const byte of bytes) {
  hash ^= BigInt(byte);
  hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
}
const build = hash.toString(16).padStart(16, "0");

copyFileSync(wasmPath, join(payloadDir, "host_wasm.wasm"));

const artifactPath = join(payloadDir, "artifact.json");
const artifact = JSON.parse(readFileSync(artifactPath, "utf8"));
const before = artifact.module?.build ?? "(none)";
artifact.module = { name: "host_wasm.wasm", bytes: bytes.length, build };
writeFileSync(artifactPath, `${JSON.stringify(artifact, null, 2)}\n`);

console.log(
  `${payloadDir}: ${bytes.length} bytes, build ${before} -> ${build} ` +
    `(file ${statSync(join(payloadDir, "host_wasm.wasm")).size} bytes)`,
);
