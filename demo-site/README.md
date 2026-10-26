# demo-site

The Svelte source of the page served from `demo/`. `demo/` holds only build
output: the site this folder produces, and the payloads `tools/build-web.sh`
writes beside it.

```sh
bun install
bun run build          # -> ../demo/index.html, ../demo/assets/
bun run serve          # the built tree on http://localhost:3000
```

`vite.config.js` sets `base: "./"`, so the built page works from a repository
sub-path (`https://<user>.github.io/win32-shim-kit/`) with no rewrite. The build
does not empty `demo/`, because `demo/payloads/` is written by the bake step, not
by this one.

## Styling

Tailwind CSS v4 through `@tailwindcss/vite`. The palette lives in the `@theme`
block of `src/app.css` — charcoal, violet, yellow, orange, teal, plus a muted
gray and a light line — so `bg-violet`, `text-charcoal` and `border-line` are
utilities, and `shadow-panel` comes from `--shadow-panel`. Components carry no
`<style>` blocks; everything is utilities on the markup, and the canvas keeps
`image-rendering: pixelated`.

## Payloads

One folder per demo, produced by the kit:

```sh
tools/build-web.sh /path/to/install demo/payloads/<slug>
```

and named in `demo/payloads/index.json`:

```json
{
  "payloads": [
    { "slug": "elimination-3", "title": "Elimination III", "note": "DirectDraw, 2000" }
  ]
}
```

The page boots whichever payload `?game=<slug>` names, or the first one listed.

## Per-bundle settings

Each payload carries a `config.json` beside its `artifact.json`, written by
`tools/build-web.sh` and editable afterwards:

```json
{ "fps": 50, "mouse": "lock" }
```

- `fps` holds the guest to that many frames a second. It has to be written down
  because the guest cannot say it: Cave Story's only timer is the 128 ms BGM
  refill, and the 50 fps it was drawn for came from the display's retrace, not
  from a number in its code. A payload with `null` runs as fast as the machine
  and the display allow.
- `mouse` is `"lock"` (the default) to hand the game the pointer when the frame
  is clicked, or `"absolute"` to leave the ordinary pointer alone.

The file is the payload's own, so it is not something a visitor can change from
the address, and re-baking a game leaves it in place.

`src/lib/player.js` is the loader: it follows `docs/loading-a-payload.md` in the
kit and speaks only the module's `wh_*` ABI, including `wh_framebuffer_depth()`
for the 8-, 16- and 32-bit surfaces an SDL game can ask for.

## Saving

The payload is a folder, and the game writes in it. Its saves are kept in
`localStorage`, one key per game (`win32-shim-kit:saves:<slug>`), restored before
the guest starts and stored again as it writes — at most once a second, and once
more on `Stop`. Only the files a payload names survive: Cave Story's
`Profile.dat` and `290.rec` come back, a rewritten `Config.dat` does not, so a
payload update is not shadowed by last week's screen mode. A payload that names
no saves keeps whatever the guest writes. Storage is best effort — a browser in
private mode throws, and the game plays without saving rather than not at all.

Clearing a game's saves is clearing its key:

```js
localStorage.removeItem("win32-shim-kit:saves:cavestory");
```

## Playing

`Play` boots the payload and starts the guest on `requestAnimationFrame`. The
canvas takes the keyboard while it has focus; the click also lets the browser
start audio. `Pause` stops presenting new frames without dropping the session,
`Stop` ends it — reload or press `Start` again to boot a fresh one.

Sounds are mixed one Web Audio voice per DirectSound buffer, reading the samples
where the guest left them (`wh_audio_samples`) rather than a copy taken at
`Play`, with the buffer's volume, pan, frequency and loop as the game set them.
This demo's game keeps its soundtrack on a CD (`open cdaudio`), which a payload
cannot carry, so it has effects but no music.

The mouse is **locked to the canvas** when the game starts, so the guest gets
movement (`wh_pointer_move`) instead of a position that stops at the edge — which
is what DirectInput's mouse and `GetCursorPos` after parking both expect. Escape,
or the `Release mouse` button, gives the cursor back; a locked pointer that the
browser refuses simply falls back to movement from the canvas pointer's own
positions, which is the same delta with the cursor still visible.

A game that draws its own cursor calls `ShowCursor(FALSE)`, and the loader passes
that on through `wh_cursor_visible()`: the canvas then shows `cursor: none`, so
the guest's arrow is the only pointer on the page. Two pointers would be two
different places — the guest moves its own from the movement it reads, not from
the page's position — and a click aimed at one lands on whatever the other has
under it.
