# The per-call floor was one gate, not call overhead — FIXED 2026-07-31

**Status:** CLOSED. The gate is open by default; the floor is gone. This file
is retained for the measurement method and for the **false diagnosis** it
originally carried, which is the more useful half.

## What the symptom was

A call to a trivial leaf method cost 95-150 ns inside a fully-compiled loop
where HotSpot pays ~0, while the *same loop without a call* ran at HotSpot
parity. `jit/src/lib.rs::direct_jit_callee_calls_enabled()` returned `false`
whenever `x64::moving_young_enabled()` — the default — so the JIT planned no
`direct_calls` at all and every invoke site fell through to the generic
`jit_invoke_dispatch` round trip.

That much was measured correctly and still holds.

## The fix (not mine — `7f1b1f263`)

The moving-young predicate is gone from `direct_jit_callee_calls_enabled()`.
Three real defects had to be fixed first, all at the raw JIT-to-JIT edge:

* the inline PIC cascade's inter-slot `JNE` was a `rel8` sized by a stale
  comment ("a single slot body is ~30 bytes"); slot bodies had grown past 127
  bytes and `rel as u8` truncated the displacement behind a `debug_assert!`, so
  **release** builds branched to `JNE -128`, into the middle of the pre-call
  shadow-stack push. It ran as an infinite loop, overran the thread's 2 MiB
  shadow buffer, overwrote the allocator arena (including the `JvmThread`) and
  faulted ~170 MiB later. Now `rel32`, with `patch_rel8_or_bail` forbidding any
  truncating `rel8` patch;
* both backends now emit the `ShadowStack::END_OFFSET` overflow guard that was
  always documented but never emitted — an overrunning push bails instead of
  corrupting the heap;
* the single-pass **sibling tail call** now restores the shadow `top` watermark
  its `epilogue-without-ret` used to skip.

See `docs/internal/jit-raw-jit-to-jit-shadow-stack-overflow-FIXED-20260731.md`.

## Correction: the root cause this document originally asserted was WRONG

This file previously claimed the hazard was a **reclaimed live root** — that a
raw JIT-to-JIT callee's prologue overwrites the caller's chain-entry
`exact_rbp`, so the walker reads the caller's oop map at the callee's frame. It
inherited that story from the gate's own comment and reasoned forward from it.

Measured, that is wrong on both counts. The root scan behaves **correctly**:
`chain_entry_rbp_is_foreign` does detect the unguarded callee frame, the
per-cycle proof comes back incomplete, `moving_young_precise_only` refuses it,
and the collection diverts to the non-moving sweep. **Nothing was ever
reclaimed.** The process died from plain machine code, above.

Two lessons worth keeping:

* A plausible written root cause in a code comment is a hypothesis, not
  evidence. The `SIGSEGV`-on-a-zeroed-slot signature was read as confirming the
  reclaimed-root story when it was equally consistent with arena corruption.
  Falsify before building on it.
* The instinct that the **sibling tail call** was implicated was right, and for
  a structurally similar reason — `epilogue-without-ret` skipping bookkeeping
  the normal return path performs. But the bookkeeping was the shadow `top`
  watermark, not the oop map. Right suspect, wrong mechanism; only the
  measurement separated them.

## The win, verified

`probes/CallFloorProbe.java`, 20M iterations, same binary back-to-back
(absolute values are inflated — this host runs several other agents' VMs
concurrently — so read the ratio, not the ns):

| body | gate closed | default (open) |
|---|---|---|
| `arith` — no call (control) | 5.6 / 5.5 | 2.4 / 4.2 |
| `+ invokestatic` leaf | 228.5 / 239.4 | **12.5 / 12.5** |
| `+ invokevirtual` leaf | 363.7 / 369.5 | **76.8 / 79.6** |
| `+ invokeinterface` leaf | 355.4 / 390.5 | **79.9 / 80.1** |
| `+ String.length()` | 67.9 / 71.1 | 66.6 / 67.9 |

`String.length()` is unmoved because it is already served by a call-site
intrinsic and never wanted a direct call — that is the cross-check that the
lever is specific rather than a global speed knob.

`CallFloorProbe` runs every body twice — once as one call with a huge loop (OSR
only) and once as many calls with a small loop (compiled by invocation count).
The two columns agreeing is what rules out OSR code quality before anything
else is investigated.

## Not this bug

`docs/internal/fixed-suite-bugs/tomcat/23-charsetcache-pathological-slowdown.md` cited a
"~630 ns marginal cost of an un-inlined call". That figure came from
`NativeCallCostProbe`, whose timing loop sits inside a lambda invoked on a
freshly started thread, inflating every rung roughly uniformly. A plain
compiled loop runs the call-free body at HotSpot parity. Doc 23's *conclusion*
is unaffected and in fact sharpened: its control arm makes one call and both
cached arms make two, so opening this gate is what lets the cached arms win.
