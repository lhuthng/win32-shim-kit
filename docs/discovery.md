# Finding what a target actually needs

A map is only as good as the evidence behind it. Three sources, in order of how
much they can be trusted.

## 1. The import table (what it *can* call)

A PE file lists every DLL and symbol it imports. This gives the ceiling, not the
requirement: a game importing `MessageBeep` does not mean it beeps.

For the Doraemon image, `tools/browser-port/manifest.ts` and the patch tooling
already walk imports; the same walk answers "which DLLs are in play" for any new
target in minutes. Start here because it names the files you will need.

## 2. A trace of a real run (what it *does* call)

The host already stops on any import without a binding and reports the slot,
the call site, and the stack. That diagnostic is what the shim tables grew from:

```
host call  KERNEL32!GetModuleHandleA  from 0x004012a0
```

Rules that make a trace usable:

- **Record the caller.** `0x004012a0` turns "somebody needs a module handle" into
  "this routine, once, at startup". Without it you cannot tell a per-frame call
  from a one-off.
- **Count the calls.** One call at startup can be stubbed with a constant.
  Per-frame calls need the right *shape*: allocation churn, key-state consume,
  palette revisions.
- **Replay the path.** A shim that works at the title screen may be untested on
  the board screen. The trace should cover every screen and both input devices.
- **Write it down.** That is the `evidence` field; a stub without one cannot be
  re-checked when the game starts behaving differently three months later.

The Doraemon host records call counts and notices for exactly this reason, and
the trace binary (`crates/web-host/src/bin/trace.rs`) prints the framebuffer
histogram and palette state next to the call, so a shim can be checked against
what the screen actually shows.

## 3. The source (what it *intends* to call)

For a source port like reVC, grepping the tree is faster and more complete than
tracing: the skeleton layer (`src/skel/`) is the only place Win32 types appear,
so the inventory is "what the skeleton replaces". Where a call was rewritten
instead of replaced, `replaced_by` points at the file that took the job.

## The lesson that costs the most: effects, not return values

A call can return the right value and still break the game. Two examples from
the Doraemon port:

- **Palette writes.** `IDirectDrawPalette::SetEntries` returns `DD_OK` and
  touches no pixels. Every screen fade in the game is *only* a run of
  `SetEntries` calls — no `Flip` between them. The host answered correctly, but
  the page repainted only when the flip count moved, so fades and the boot logo
  never appeared. The fix was to count palette revisions as well as flips. A map
  that recorded only "returns DD_OK" would have certified the broken behaviour.

- **Key state.** `GetAsyncKeyState` returns the right bit and clears the
  "pressed since last read" bit as a side effect. Reading it twice in a frame
  changes what the second read sees. Return value alone is not the contract.

So when writing a `description`, record the **observable effect** the game
depends on, not the return code. "Reports the palette changed so the presenter
repaints" survives a rewrite of the presenter; "returns DD_OK" does not.

## Checklist before adding a stub

1. Which DLL and symbol, exactly (including the trailing `A`/`W`)?
2. Where is it called from, and how often?
3. What does the guest do with the result — compare it, store it, ignore it?
4. What side effects does the real call have (memory, state, counters)?
5. Is the honest answer a constant, a real implementation, or a refusal?

If 1–4 are not all answerable, the entry is `unimplemented` with a reason, which
stops the run and names it. That is cheaper than a guess that boots.
