# `Frame` was 296 bytes because of a variant the hot path never uses — FIXED 2026-08-19

**Status:** FIXED. `Frame` 296 → 232 bytes (−22%); `frame_push` −13%, call side
−4% (~2% end-to-end); no measurable regression on the workload that pays the
new allocation.

**Instruments:** `CRATONVM_DBG_INVOKE_PHASES=1` (now also reports the frame-kind
split), `probes/ZeroArgCallProbe.java`, `probes/OwnedFrameGrowthProbe.java`,
`probes/OwnedSplitProbe.java`.

## The defect

An enum is sized by its largest variant. `FrameInner` had two:

| variant | payload | built by |
|---|---|---|
| `Owned` | five fat pointers = **80 bytes** | reflection, JNI, JVMTI, tail calls |
| `Cached` | one `Arc` = **8 bytes** | **the hot interpreted call path** |

So every frame on the hot path carried 80 bytes for a variant it never uses.
`FrameInner` was 80 of `Frame`'s 296 — 27% — and `Frame` is built and then MOVED
by value into the frame stack on every call, so those are bytes written per
invocation and read again on pop.

Boxing the `Owned` payload takes `FrameInner` 80 → 16 and `Frame` 296 → 232.

## The trade, and two wrong answers before the right one

Boxing costs one heap allocation per `Owned` frame, so it is only correct while
`Owned` frames are rare. That is a claim about execution frequency, and
reasoning about it produced the wrong answer **twice**:

1. **Call-site counts suggested the opposite.** `Frame::new` has ~74 call sites
   against `new_pooled_cached`'s 8 — which, read naively, says `Owned`
   dominates. (Most of those are tests and cold paths, but "most" was an
   impression, not a measurement.)
2. **Two small runs suggested a fixed bootstrap cost.** The counter reported
   `owned=466` over 8,000,000 calls and `owned=478` over ~1,600 — near-identical
   absolutes, which reads as a constant paid at startup. Comfortable, and wrong.

Scaling the workload killed it: `Owned` frames grew at **exactly one per loop
iteration**, reaching a 49.7% share. Splitting the loop's operations found the
driver:

```text
  plain calls   owned=242    1.2%   bootstrap only
  lambdas/indy  owned=242    1.2%   bootstrap only — indy builds NONE
  throw/catch   owned=244           bootstrap only — unwinding builds NONE
  reflection    owned=20241 97.5%   one per `Method.invoke`
```

**Reflection is the sole driver.** The first version of this file's own doc
comment claimed `Owned` was reached from "reflection, JNI, JVMTI, indy and tail
calls" and was cold — wrong about indy, and understating reflection. A doc claim
that survives because nobody measured it is the same defect this audit keeps
finding, and it was in the comment written to justify this very change.

**The allocation is nevertheless invisible where it lands.** A reflective invoke
costs ~9µs in this interpreter against ~25ns for a malloc — 0.3%. A
reflection-dominated A/B measured no regression: medians 367ms (old) against
365.5ms (new) over six interleaved rounds, 4 worse / 2 better, all inside a
1-6% noise band.

## Measurements

**Phases** (zero-argument `invokestatic`, two independent passes):

| phase | OLD | NEW | |
|---|---|---|---|
| **frame_push** | 31.7 / 31.0 | **27.5 / 27.2** | **−13%** |
| frame_build | 78.4 / 78.5 | 80.8 / 81.1 | +3% |
| **call side total** | 235.0 / 230.3 | **224.9 / 223.4** | **−4%** |

`frame_push` is the phase that MOVES the frame into the frame stack, and it is
the one that improved — which is what a 64-byte-smaller struct should do, and a
reason to believe the effect is the intended one rather than drift.
`frame_build` is marginally worse, consistently across both passes; the struct
is smaller but the construction is otherwise unchanged, so this is most likely
layout noise rather than a real cost.

**Wall clock could not resolve it, and that is expected.** The call side is
about half the fixed per-call cost, so −4% there is ~2% end-to-end, against a
wall-clock spread of ~25% on this host. Eight interleaved rounds read 3 better /
5 worse — a wash. **The instrument is the evidence here, not the stopwatch**, and
the two do not disagree: 2% is simply below what the stopwatch can see.

An earlier attempt at this measurement was discarded outright: run immediately
after the build, it produced a 2.5x spread (422ms → 170ms across rounds) as the
host settled. Numbers from a host that is still cooling are not slow numbers,
they are meaningless ones.

## What this leaves

`Frame` is 232 bytes. The remaining large residents are `ValueStack` (64), three
`Vec`s at 24 each (`locals`, `local_kinds`, `osr_attempt_counts`) and
`Arc<[u8]> code` (16). `osr_attempt_counts` is almost always empty and could
become an `Option<Box<_>>` for another 16 bytes; folding `local_kinds` into the
`locals` allocation would save 24 more but is a real change to the GC's local
scanning.

And the standing caveat is unchanged: roughly **half** the fixed per-call cost
is outside these phases entirely, in the callee body and the return / frame-pop
path, which nothing here measures.
