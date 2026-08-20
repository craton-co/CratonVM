# The return path is the largest phase of an interpreted call — measured 2026-08-19

**Status:** MEASURED. The instrument is merged; no optimization is claimed here.
Two of the three follow-ups this measurement was meant to justify are now
**rejected** by it, and one concrete target is named.

**Instrument:** `CRATONVM_DBG_INVOKE_PHASES=1` (off by default),
`probes/ZeroArgCallProbe.java`.

## The gap this closes

Every previous record in this series carried the same caveat: the call-side
phases summed to about HALF an uninstrumented call, and the remainder was
attributed to "the callee body and the return path" without either being
measured. That is a residual, and a residual absorbs whatever nobody looked at.

The `ireturn` arm is now instrumented as `ret_total`, with
`pop_and_recycle_frame` nested inside it as `ret_recycle`.

## The instrument had to be calibrated before it could rank anything

`ret_total` BRACKETS the `ret_recycle` probe pair, so its span contains three
`rdtsc` reads where every flat phase contains one. Comparing them raw is not
like-for-like, and the size of the correction decides the answer:

| phase | raw | if a read costs 25 cyc | measured (a read costs ~17.5) |
|---|---|---|---|
| ret_total | 168.6 | ~93 | **112.9** |
| frame_build | 121.4 | ~96 | 102.9 |

At an assumed 25 cyc/read `frame_build` wins; at the measured cost `ret_total`
does. **The ranking depended entirely on a number that had been assumed rather
than measured** — and the assumption came from this module's own doc comment.

So `P_CALIB` was added: two back-to-back `now()` calls measuring nothing,
charged per call. It reports **18.5 / 16.6 cyc** per pair on this host, and the
report now prints every phase raw AND corrected — flat phases minus one read,
`ret_total` minus three.

## The breakdown

Two independent passes, 6,400,348 zero-argument `invokestatic` calls, corrected:

| phase | corrected cyc/call (p1 / p2) |
|---|---|
| **ret_total** | **112.9 / 100.8** |
| ↳ ret_recycle | 63.9 / 57.6 |
| frame_build | 102.9 / 94.9 |
| ic_lookup | 66.6 / 60.2 |
| frame_push | 26.7 / 22.8 |
| args | 24.7 / 21.4 |
| guards | 24.6 / 21.4 |

The return path is the **largest single phase**, and it splits roughly evenly:
~64 cyc in `pop_and_recycle_frame`, ~49 cyc in the rest of the arm (popping the
return value, the JVMTI method-exit gate, pushing the value to the caller).

The return path is nevertheless already well optimized — a previous `perf` run
recorded in `pop_and_recycle_frame`'s own comment put `memcpy` under
`Vec::pop<Frame>` inside a frame-lifecycle group worth ~24.7% of the invoke arm,
and that was fixed: the frame is read THROUGH the stack, `truncate` drops it in
place, and no `Frame` is moved. What remains is not a missed memcpy.

## Two follow-ups this measurement REJECTS

Both were on the list of "next steps" before the numbers existed:

* **Boxing `osr_attempt_counts`** — 16 bytes off `Frame`.
* **Folding `local_kinds` into the `locals` allocation** — 24 bytes, and a real
  change to how the GC scans locals.

`frame_push` — the phase that moves the frame, and the one any size reduction
acts on — costs **~25 cycles in total**. Shrinking `Frame` further can win at
most a few cycles of that, so the GC-scanning change in particular is real risk
for negligible return. **Neither is worth doing.**

This also recalibrates the previous record honestly: the `Frame` 296→232 change
measured `frame_push` −13%, and that was 13% of a small number.

## The target this measurement NAMES

`ret_recycle`'s dominant resident is the `Frame`'s `Drop`: **two `Arc`
decrements**, `code` and the cached method. `code: Arc<[u8]>` is **redundant for
cached frames** — `inner: FrameInner::Cached(Arc<CachedBytecodeMethod>)` already
owns that exact slice. It is the same defect shape already fixed on the call
side (two refcount bumps where one is the minimum), surviving on the return
side.

Removing it is worth one increment on build plus one decrement on pop, ~35-40
cyc across `frame_build` and `ret_recycle` together.

It is **not** a small change. `FrameInner::Owned` carries no code, so the Owned
variant would have to hold it before `Frame` could keep a pointer derived from
`inner`, and a raw pointer into an `Arc` payload in the VM's hottest path is a
use-after-free if the invariant is ever broken (`reset_for_tail_call` replaces
`inner` and would have to re-derive). That is a change to make deliberately,
with the equivalence discipline the argument-tag scan got, not as a tail-end
edit.

## What is still not measured

The corrected phases sum to ~358 cyc against an uninstrumented call of ~720.
The remainder is the callee body — `iconst_1` and `ireturn`'s own dispatch, plus
the caller's `iadd` — and the interpreter loop overhead around them. That is now
a SMALL residual over known opcodes rather than a large one over unexamined
machinery, which is the difference this record was written to make.
