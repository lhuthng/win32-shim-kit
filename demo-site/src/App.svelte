<script>
  import Player from "./Player.svelte";

  let payloads = [];
  let selected = null;
  let meta = null;
  let problem = "";
  let loading = true;

  const asked = new URLSearchParams(location.search).get("game");

  async function loadIndex() {
    try {
      const response = await fetch(new URL("./payloads/index.json", document.baseURI));
      if (!response.ok) throw new Error(`payloads/index.json: HTTP ${response.status}`);
      const data = await response.json();
      payloads = Array.isArray(data) ? data : (data.payloads ?? []);
      selected = payloads.find((entry) => entry.slug === asked) ?? payloads[0] ?? null;
    } catch (error) {
      problem = error.message;
    } finally {
      loading = false;
    }
  }

  function choose(entry) {
    selected = entry;
    const url = new URL(location.href);
    url.searchParams.set("game", entry.slug);
    history.replaceState(null, "", url);
  }

  async function loadMeta(slug) {
    meta = null;
    if (!slug) return;
    try {
      const response = await fetch(new URL(`./payloads/${slug}/artifact.json`, document.baseURI));
      if (response.ok) meta = await response.json();
    } catch {
      // The player reports a payload it cannot read; the summary is optional.
    }
  }

  $: loadMeta(selected?.slug);

  loadIndex();
</script>

<main>
  <section class="relative overflow-hidden bg-charcoal px-6 pt-16 pb-24 text-white">
    <div class="absolute -top-40 -right-28 size-[420px] rounded-full bg-violet/90"></div>
    <div class="absolute right-40 -bottom-32 size-[220px] rounded-full bg-orange/85"></div>

    <div class="relative mx-auto max-w-6xl">
      <span
        class="inline-block rounded-full border border-yellow px-2.5 py-1 font-mono text-xs font-semibold tracking-widest text-yellow uppercase"
      >
        PE32 · wasm · shims
      </span>
      <h1 class="mt-5 mb-4 max-w-2xl text-4xl leading-tight font-bold sm:text-5xl">
        Windows games, one PE32 image at a time.
      </h1>
      <p class="mb-8 max-w-xl text-base text-white/70 sm:text-lg">
        The engine is the same module in every demo; what changes is the game's own executable and
        the calls its import table asks for.
      </p>
      <a
        href="#demos"
        class="inline-block rounded-lg bg-yellow px-5 py-3 font-semibold text-charcoal transition hover:-translate-y-px"
      >
        Boot a demo
      </a>
    </div>
  </section>

  <section id="demos" class="relative mx-auto -mt-12 max-w-6xl rounded-2xl bg-white p-6 shadow-panel sm:p-8">
    {#if loading}
      <p class="font-mono text-sm text-muted">reading the payload list…</p>
    {:else if problem}
      <div class="rounded-xl border border-dashed border-line p-6">
        <h2 class="text-base font-bold">No payload list</h2>
        <p class="mt-2 font-mono text-xs text-orange">{problem}</p>
        <p class="mt-3 text-sm text-muted">
          Bake a demo into the site with
          <code>tools/build-web.sh &lt;install-dir&gt; demo/payloads/&lt;slug&gt;</code>.
        </p>
      </div>
    {:else if payloads.length === 0}
      <div class="rounded-xl border border-dashed border-line p-6">
        <h2 class="text-base font-bold">No demo has been baked yet</h2>
        <p class="mt-2 text-sm text-muted">
          Bake one with
          <code>tools/build-web.sh &lt;install-dir&gt; demo/payloads/&lt;slug&gt;</code>, then add it
          to <code>demo/payloads/index.json</code>.
        </p>
      </div>
    {:else}
      <nav class="mb-6 flex flex-wrap gap-2" aria-label="Demos">
        {#each payloads as entry (entry.slug)}
          <button
            type="button"
            class="rounded-xl border px-4 py-2 text-sm font-semibold transition {selected?.slug ===
            entry.slug
              ? 'border-violet bg-violet/5'
              : 'border-line hover:border-violet/60'}"
            on:click={() => choose(entry)}
          >
            {entry.title ?? entry.slug}
          </button>
        {/each}
      </nav>

      <div class="min-w-0">
        <div class="mb-4 flex flex-wrap items-baseline gap-x-4 gap-y-1">
          <h2 class="text-xl font-bold">{selected.title ?? selected.slug}</h2>
          <p class="font-mono text-xs text-muted">
            {meta?.default ?? meta?.executables?.[0]?.name ?? "unknown executable"}
            {#if meta?.screen}
              · {meta.screen.width}×{meta.screen.height}×{meta.screen.depth}
            {/if}
            {#if meta?.module?.bytes}· {(meta.module.bytes / 1024).toFixed(0)} KB module{/if}
            {#if meta?.game?.bytes}· {(meta.game.bytes / 1024 / 1024).toFixed(1)} MB of game{/if}
          </p>
        </div>

        {#key selected.slug}
          <Player slug={selected.slug} saves={selected.saves ?? null} />
        {/key}
      </div>
    {/if}
  </section>

  <section class="mx-auto max-w-6xl px-6 pt-14 pb-6">
    <h2 class="text-lg font-bold">What a payload is</h2>
    <p class="mt-2 mb-6 max-w-[70ch] text-sm text-muted">
      A demo is a module, an artifact that says what to run, and the game's own files. Nothing in
      the game is recompiled.
    </p>

    <div class="grid gap-5 sm:grid-cols-2 lg:grid-cols-3">
      <article class="rounded-2xl border border-line bg-white p-6">
        <div class="mb-4 size-10 rounded-xl bg-violet"></div>
        <h3 class="mb-1.5 text-base font-semibold">One PE32 image</h3>
        <p class="text-sm text-muted">
          The guest runs from its own executable, decoded and executed a call at a time.
        </p>
      </article>

      <article class="rounded-2xl border border-line bg-white p-6">
        <div class="mb-4 size-10 rounded-xl bg-orange"></div>
        <h3 class="mb-1.5 text-base font-semibold">A table of shims</h3>
        <p class="text-sm text-muted">
          Every imported function has an answer; a call with none stops the run and names itself.
        </p>
      </article>

      <article class="rounded-2xl border border-line bg-white p-6">
        <div class="mb-4 size-10 rounded-xl bg-teal"></div>
        <h3 class="mb-1.5 text-base font-semibold">The same module everywhere</h3>
        <p class="text-sm text-muted">
          The page only speaks the <code>wh_*</code> ABI, so a new game is a new payload.
        </p>
      </article>
    </div>
  </section>

  <footer class="mx-auto max-w-6xl border-t border-line px-6 py-8">
    <p class="max-w-[80ch] text-sm text-muted">
      Everything under <code>demo/</code> is built output: the site from
      <code>demo-site/</code>, the games from <code>tools/build-web.sh</code>, which takes an
      install directory and writes the payload a demo is.
    </p>
  </footer>
</main>
