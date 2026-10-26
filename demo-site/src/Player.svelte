<script>
  import { onDestroy } from "svelte";
  import { boot } from "./lib/player.js";

  export let slug;
  /** The files this game saves, from `payloads/index.json`; `null` keeps all. */
  export let saves = null;

  let canvas;
  let session = null;
  let status = "ready";
  let diagnostic = "";
  let error = "";
  /**
   * Whether the run ended for a reason, rather than still booting.
   *
   * The kit reports what it did as it boots -- a window, a DirectDraw object, a
   * timer -- and those notices are not errors: a game that is still loading must
   * not look broken. Only a stop is worth showing under the canvas.
   */
  let halted = false;
  let starting = false;
  let paused = false;
  /** Whether the pointer is held by the canvas, so movement arrives as a delta. */
  let locked = false;
  /** Whether the guest draws its own cursor, so the page hides its pointer. */
  let guestCursor = false;

  const payloadUrl = new URL(`./payloads/${slug}/`, document.baseURI).href;

  async function start() {
    if (session || starting) return;
    // `session` is null until the module has booted, so this cannot start audio
    // here: the browser's gesture rule is satisfied again by the first click on
    // the canvas, which is also what takes the pointer.
    session?.resumeAudio();
    starting = true;
    error = "";
    diagnostic = "";
    halted = false;
    try {
      session = await boot({
        canvas,
        payloadUrl,
        // One storage key per game, so two demos never share a save folder.
        saveKey: slug,
        saves,
        onStatus: (text) => {
          status = text;
          if (text.startsWith("stopped") || text === "the program exited") halted = true;
        },
        onDiagnostic: (text) => (diagnostic = text),
      });
    } catch (problem) {
      error = problem instanceof Error ? problem.message : String(problem);
      status = "could not start";
    } finally {
      starting = false;
    }
    guestCursor = session ? !session.cursorVisible() : false;
  }

  function stop() {
    session?.stop();
    session = null;
    held.clear();
    paused = false;
    status = "stopped";
    if (document.pointerLockElement === canvas) document.exitPointerLock();
  }

  /**
   * Takes the pointer, which only a click on the canvas does.
   *
   * Not on Play: starting a game and handing it the mouse are two different
   * decisions, and a player who only wants to watch the first screen should not
   * lose their cursor for it. Until the canvas is clicked the game gets an
   * absolute pointer, which is what a window with a visible cursor expects.
   */
  function lockPointer() {
    if (!canvas || document.pointerLockElement === canvas) return;
    // Browsers reject a lock taken without focus or after a refusal; the game is
    // still playable with an absolute pointer, so a refusal changes nothing.
    const request = canvas.requestPointerLock?.();
    if (request && typeof request.catch === "function") request.catch(() => {});
  }

  function releasePointer() {
    if (document.pointerLockElement === canvas) document.exitPointerLock();
  }

  function onPointerLockChange() {
    locked = document.pointerLockElement === canvas;
  }

  function togglePause() {
    if (!session) return;
    paused = !paused;
    session.setPaused(paused);
  }

  // ---- keyboard -----------------------------------------------------------

  function onKeyDown(event) {
    if (!session) return;
    if (event.target instanceof HTMLInputElement) return;
    if (session.key(event.code, true) && session.captures(event.code)) event.preventDefault();
  }

  function onKeyUp(event) {
    if (!session) return;
    if (session.key(event.code, false) && session.captures(event.code)) event.preventDefault();
  }

  // ---- pointer ------------------------------------------------------------

  const BUTTONS = { 0: 0, 1: 2, 2: 1 };
  /** Where the pointer was last, in guest pixels. */
  let last = null;
  /** Buttons the guest has been told are down, so it can be told they are up. */
  const held = new Set();

  /** Guest pixels per CSS pixel, for a canvas the page scales to fit. */
  function perPixel() {
    const frame = session?.frameSize() ?? { width: canvas.width, height: canvas.height };
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return { x: 1, y: 1 };
    return {
      x: (frame.width || canvas.width) / rect.width,
      y: (frame.height || canvas.height) / rect.height,
    };
  }

  function toGuest(event) {
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return { x: 0, y: 0 };
    const scale = perPixel();
    return {
      x: (event.clientX - rect.left) * scale.x,
      y: (event.clientY - rect.top) * scale.y,
    };
  }

  /**
   * The pointer moved. What the guest gets is the movement, not the position.
   *
   * A DirectInput mouse reports nothing but `lX`/`lY`, and a game that draws its
   * own cursor adds them up; sending it a position makes the step between two
   * positions, which leaves its cursor wherever the two histories disagree. So
   * the page forwards the movement itself, the same way the locked view does, and
   * the guest's cursor follows the mouse from wherever the game put it.
   */
  function onPointerMove(event) {
    if (!session || locked) return;
    guestCursor = !session.cursorVisible();
    const point = toGuest(event);
    if (last) {
      const dx = Math.round(point.x - last.x);
      const dy = Math.round(point.y - last.y);
      if (dx || dy) session.pointerMove(dx, dy);
    }
    last = point;
  }

  /**
   * The locked view: `movementX`/`movementY` are the browser's own deltas, in
   * CSS pixels, which is what DirectInput reports for a mouse.
   */
  function onMouseMove(event) {
    if (!session || !locked) return;
    const scale = perPixel();
    const dx = Math.round(event.movementX * scale.x);
    const dy = Math.round(event.movementY * scale.y);
    if (dx || dy) session.pointerMove(dx, dy);
  }

  function onPointerDown(event) {
    if (!session) return;
    session.resumeAudio();
    const button = BUTTONS[event.button] ?? 0;
    // No position: the button lands where the guest's cursor already is, which
    // is where the player put it. A window that acts on `WM_LBUTTONDOWN` reads
    // `lParam`, and that is the host's own tracked cursor, so the two agree.
    last = toGuest(event);
    held.add(button);
    // Sent before anything else in this handler. Pointer capture used to be
    // taken here first, and while the pointer was locked the browser refused it
    // with `InvalidStateError`; that threw out of the handler before the button
    // was sent, so every click after the mouse was locked did nothing at all.
    session.pointerButton(button, true);
    // The click that reaches the game is the one that hands it the pointer, and
    // only if this game asks for it: its `config.json` decides.
    if (session.mouseLock) lockPointer();
  }

  /**
   * The button came up.
   *
   * Heard on the window, not the canvas, so a release outside the frame still
   * reaches the guest and no button is left down. Only a button the guest was
   * told is down is released, so a click on one of the page's own controls
   * does not reach into the game.
   */
  function onPointerUp(event) {
    const button = BUTTONS[event.button] ?? 0;
    if (!held.delete(button)) return;
    if (session) session.pointerButton(button, false);
  }

  function onPointerLeave() {
    // Re-entering must not count the distance from where the pointer left.
    last = null;
  }

  // A lock belongs to the game that took it: switching demos, or leaving the
  // page, releases it here, or the next game would start with a pointer held by
  // a canvas that no longer exists.
  onDestroy(() => {
    session?.stop();
    if (document.pointerLockElement === canvas) document.exitPointerLock();
  });
</script>

<div>
  <!-- The frame is shown at the guest's own size and aspect: a game that asks for
       794x550 is not a 4:3 window, and stretching it to one moves every pixel. -->
  <div class="flex justify-center">
    <div class="relative overflow-hidden rounded-xl border border-line bg-charcoal">
      <canvas
        bind:this={canvas}
        width="640"
        height="480"
        on:pointermove={onPointerMove}
        on:pointerdown={onPointerDown}
        on:pointerleave={onPointerLeave}
        class="block h-auto max-w-full [image-rendering:pixelated]"
        class:cursor-none={guestCursor}
        class:cursor-crosshair={Boolean(session) && !guestCursor}
      ></canvas>

      {#if !session}
        <div class="absolute inset-0 grid place-content-center justify-items-center gap-3 bg-charcoal/85 px-6 text-center">
          <button
            type="button"
            on:click={start}
            disabled={starting}
            class="rounded-lg bg-yellow px-6 py-2.5 font-semibold text-charcoal transition hover:-translate-y-px disabled:opacity-60"
          >
            {starting ? "starting…" : "Play"}
          </button>
          {#if error}
            <p class="max-w-[46ch] font-mono text-xs text-orange">{error}</p>
          {/if}
        </div>
      {/if}
    </div>
  </div>

  <div class="mt-4 flex flex-wrap items-center gap-2">
    <button
      type="button"
      on:click={start}
      disabled={Boolean(session) || starting}
      class="rounded-lg bg-charcoal px-4 py-1.5 text-sm font-semibold text-white transition hover:bg-charcoal/85 disabled:opacity-40"
    >
      Start
    </button>
    <button
      type="button"
      on:click={togglePause}
      disabled={!session}
      class="rounded-lg border border-line px-4 py-1.5 text-sm font-semibold transition hover:border-violet disabled:opacity-40"
    >
      {paused ? "Resume" : "Pause"}
    </button>
    <button
      type="button"
      on:click={stop}
      disabled={!session}
      class="rounded-lg border border-line px-4 py-1.5 text-sm font-semibold transition hover:border-violet disabled:opacity-40"
    >
      Stop
    </button>
    <button
      type="button"
      on:click={() => (locked ? releasePointer() : lockPointer())}
      disabled={!session}
      class="rounded-lg border border-line px-4 py-1.5 text-sm font-semibold transition hover:border-violet disabled:opacity-40"
    >
      {locked ? "Release mouse" : "Lock mouse"}
    </button>
    <span class="ml-auto font-mono text-xs text-muted">{status}</span>
  </div>

  {#if session}
    <p class="mt-3 text-sm text-muted">
      The canvas forwards the keyboard and the pointer to the guest. Click inside the frame to
      hand the game the mouse: it then gets movement rather than a position, so its cursor never
      stops at the edge, and Escape returns it. Until then the game follows the pointer normally.
    </p>
  {/if}

  {#if diagnostic && halted}
    <p class="mt-3 font-mono text-xs break-words text-orange">{diagnostic}</p>
  {/if}
</div>

<svelte:window
  on:keydown={onKeyDown}
  on:keyup={onKeyUp}
  on:mousemove={onMouseMove}
  on:pointerup={onPointerUp}
/>
<svelte:document on:pointerlockchange={onPointerLockChange} />
