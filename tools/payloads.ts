// Where payloads live, and how the two trees are merged into one list.
//
// A payload is a folder with `artifact.json`, `config.json`, `game/` and
// `host_wasm.wasm`. There are two places they can be:
//
//   demo/payloads/    published. Committed, built into `demo/`, served by
//                     GitHub Pages, and listed by its own `index.json`.
//   local/payloads/   this machine only. Gitignored, never built, and read by
//                     nothing but the local server and these checkers.
//
// The split exists so a game whose files belong to somebody else can be baked,
// played and checked here without entering the published tree. Keeping the
// lookup in one module is what stops the two trees drifting: `serve.ts` and the
// two `tools/demo-*.ts` checkers all resolve through this, so a payload plays
// the same in each.
//
// `local/payloads/index.json` has the same shape as the published one. It is
// what gives a local payload a title and its boot settings, since nothing else
// describes it -- the folder name is all the directory scan can see.

import { readdir } from "node:fs/promises";
import { join } from "node:path";

/** One entry of a payload `index.json`, as the page reads it. */
export type PayloadEntry = {
  slug: string;
  title: string;
  note?: string;
  instructions?: number;
  lit?: number;
  settle?: number;
  saves?: string[];
  /** Added by the merge: this entry is not in the published tree. */
  local?: boolean;
};

export type PayloadRoots = {
  published: string;
  local: string;
};

/** The two trees, given the repository root. */
export function payloadRoots(repoRoot: string): PayloadRoots {
  return {
    published: join(repoRoot, "demo", "payloads"),
    local: join(repoRoot, "local", "payloads"),
  };
}

/** `index.json` from one tree, or `null` when the tree has none. */
async function readIndex(dir: string): Promise<PayloadEntry[] | null> {
  const file = Bun.file(join(dir, "index.json"));
  if (!(await file.exists())) return null;
  const data = await file.json();
  const entries: PayloadEntry[] = Array.isArray(data) ? data : (data.payloads ?? []);
  return entries.filter((entry) => entry && typeof entry.slug === "string");
}

/** The bootable executable's name, minus `.exe`, as a stand-in title. */
async function titleOf(dir: string): Promise<string | null> {
  const artifact = Bun.file(join(dir, "artifact.json"));
  if (!(await artifact.exists())) return null;
  const build = await artifact.json().catch(() => null);
  const name = build?.default ?? build?.executables?.[0]?.name;
  if (typeof name !== "string") return null;
  return name.replace(/\.exe$/i, "");
}

/**
 * The published list, then the local one, then anything either tree holds that
 * neither list names.
 *
 * A folder baked by hand is worth listing without also being described by hand:
 * the directory scan catches it, and the title comes from its artifact so the
 * page has something to show. What it cannot invent is `instructions`, `lit` or
 * `settle`, which is why `index.json` still exists -- and why a local payload
 * that needs a long boot belongs in the local index.
 */
export async function mergedIndex(roots: PayloadRoots): Promise<{ payloads: PayloadEntry[] }> {
  const published = (await readIndex(roots.published)) ?? [];
  const payloads: PayloadEntry[] = [...published];
  const known = new Set(payloads.map((entry) => entry.slug));

  // The local index before the local scan: a described payload keeps its
  // description, an undescribed one gets a title from its artifact.
  const local = await readIndex(roots.local).catch(() => null);
  for (const entry of local ?? []) {
    if (known.has(entry.slug)) continue;
    payloads.push({ ...entry, local: true });
    known.add(entry.slug);
  }

  for (const dir of [roots.published, roots.local]) {
    let entries;
    try {
      entries = await readdir(dir, { withFileTypes: true });
    } catch {
      continue; // a fresh clone has no local tree at all
    }
    for (const entry of entries) {
      if (!entry.isDirectory() || known.has(entry.name)) continue;
      const title = await titleOf(join(dir, entry.name));
      if (title === null) continue; // no artifact: not a payload
      payloads.push({
        slug: entry.name,
        title,
        note: "Baked here: its files are not in the repository, so this entry is not in the published index.json.",
        local: dir === roots.local,
      });
      known.add(entry.name);
    }
  }

  return { payloads };
}

/**
 * The file a request for `/payloads/<path>` should be answered with, or `null`.
 *
 * `path` is relative to the payloads root: `hercules/game/files.json`. The
 * published tree is asked first, so a slug that exists in both is served from
 * the published copy.
 */
export async function payloadFile(
  roots: PayloadRoots,
  path: string,
): Promise<ReturnType<typeof Bun.file> | null> {
  // A URL path is attacker-controlled even on localhost; `..` would reach the
  // rest of the disk from either root.
  if (path.split("/").includes("..")) return null;
  for (const dir of [roots.published, roots.local]) {
    const file = Bun.file(join(dir, path));
    if (await file.exists()) return file;
  }
  return null;
}
