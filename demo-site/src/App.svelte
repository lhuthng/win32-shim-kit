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
      // `?game=` names a payload the page will play even when the list does not
      // mention it: a payload baked next to the site for one machine is how a
      // game is checked before it is published, and the list is what is
      // published. Without a name there is nothing to fetch, so the list still
      // decides.
      selected =
        payloads.find((entry) => entry.slug === asked) ??
        (asked ? { slug: asked, title: asked } : payloads[0] ?? null);
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

<main class="mx-auto max-w-5xl px-5 pt-8 pb-10 sm:pt-12">
  <header class="flex flex-wrap items-end justify-between gap-x-6 gap-y-4">
    <div class="min-w-0">
      <p class="font-mono text-[11px] font-semibold tracking-[0.18em] text-violet uppercase">
        win32-shim-kit
      </p>
      <h1 class="mt-1 truncate text-2xl font-bold sm:text-3xl">
        {selected?.title ?? selected?.slug ?? "demos"}
      </h1>
    </div>

    {#if payloads.length > 1}
      <nav class="flex flex-wrap gap-1" aria-label="Demos">
        {#each payloads as entry (entry.slug)}
          <button
            type="button"
            class="rounded-full px-3.5 py-1.5 text-sm font-semibold transition {selected?.slug ===
            entry.slug
              ? 'bg-white text-black'
              : 'text-white/55 hover:bg-white/10 hover:text-white'}"
            on:click={() => choose(entry)}
          >
            {entry.title ?? entry.slug}
          </button>
        {/each}
      </nav>
    {/if}
  </header>

  <p class="mt-2 font-mono text-xs text-muted">
    {#if selected}
      {meta?.default ?? meta?.executables?.[0]?.name ?? "unknown executable"}
      {#if meta?.screen}· {meta.screen.width}×{meta.screen.height}{/if}
      {#if meta?.game?.bytes}· {(meta.game.bytes / 1024 / 1024).toFixed(1)} MB{/if}
    {:else}
      &nbsp;
    {/if}
  </p>

  <div class="mt-6">
    {#if loading}
      <p class="font-mono text-sm text-muted">reading the payload list…</p>
    {:else if problem}
      <div class="rounded-xl border border-dashed border-line p-6">
        <h2 class="text-base font-bold">No payload list</h2>
        <p class="mt-2 font-mono text-xs text-orange">{problem}</p>
        <p class="mt-3 text-sm text-muted">
          Bake one with <code>tools/build-web.sh &lt;install-dir&gt; demo/payloads/&lt;slug&gt;</code>.
        </p>
      </div>
    {:else if payloads.length === 0}
      <div class="rounded-xl border border-dashed border-line p-6">
        <h2 class="text-base font-bold">No demo has been baked yet</h2>
        <p class="mt-2 text-sm text-muted">
          Bake one with <code>tools/build-web.sh &lt;install-dir&gt; demo/payloads/&lt;slug&gt;</code>,
          then list it in <code>demo/payloads/index.json</code>.
        </p>
      </div>
    {:else}
      {#key selected.slug}
        <Player slug={selected.slug} saves={selected.saves ?? null} />
      {/key}
    {/if}
  </div>
</main>
