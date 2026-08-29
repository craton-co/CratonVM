# `TestOpenClose` — `Exception in thread "main" java/lang/Object`, with no captured frames

## Status

**OPEN, split out 2026-08-29** from
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`, which tracked
this class only because it was watching it for a fragmentation defect that is
now closed. This is not that defect and never was — the page's own kill-switch
differential said so on 2026-08-21, and the separation is now as clean as it
will get: the class reproduces this failure on a binary with **zero**
`OutOfMemoryError`.

## The signature

```text
Exception in thread "main" java/lang/Object
```

No message, no captured frames. `java/lang/Object` is the CLASS of the thrown
object, so either something that is not a `Throwable` reached the throw path, or
a receiver's class id decoded as 0 — `ClassId(0)` **is** `java.lang.Object`, the
first class this VM loads, which is what an all-zero header reads as.

That second reading puts it in the same family as the stale-reference defects
`bug-h2-testrandommapops-classcastexception-20260821.md` worked through: a G1
receiver evacuated across a GC point read back as `java/lang/Object` and
surfaced as `NoSuchMethodError 'java.lang.Object[] java.lang.Object.toArray()'`.
It is a family resemblance and nothing more until somebody dumps the throwable.

## What has already been ruled out

* **Not the ZGC fragmentation defect.** Confirmed pre-existing and unrelated by
  a kill-switch differential on 2026-08-21, and reproduced afterwards on a
  binary where the fragmentation OOM was gone.

  **But the OOM is back on 2026-08-29, and for a reason that is not
  fragmentation.** Re-run on the tip with all four repairs, `--Xmx 1g`:

  ```text
  rc=1 secs=335 oom=4 arena=4
    compaction_cycles=1 objects_relocated=5436
    relocation_skipped_jit=16 relocation_on_proven_jit=1
    zgc-high-compaction: cycles=0 declined=1 vacated_spans=0 vacated_bytes=0
  ```

  **The slide ran ONCE in seventeen collections.** This class is JIT-busy
  enough that the per-cycle coverage proof refuses relocation on nearly every
  cycle, so no repair inside `relocate_stw` gets a chance — `vacated_spans=0`
  is the same statement from the other side. That is the
  `relocation_skipped_jit` obligation, not the fragmentation one, and it is a
  third failure on this class rather than a return of the first.

  The parent page's own rule applies: read `relocation_on_proven_jit` before
  `rc`. Any arm here that does not report it above zero is measuring the
  refusal, not the heap.
* **Not a hang.** An earlier Windows rerun classified this class as HANG at the
  1500 s cap; the Azure rerun crashes it in about two minutes. The predecessor
  page (`hangs-true-vs-perfcliff-RESOLVED-20260821.md`) flagged that shape
  mismatch and did not reconcile it; the reconciliation is that the failure is
  fast and deterministic once triggered, so whatever made the Windows run time
  out instead is a host/heap difference, not a second behaviour.

## Three failures, not one

As of 2026-08-29 this class carries three separable things, and a run that
fixes any one of them still fails:

1. the `Exception in thread "main" java/lang/Object` this page is named for;
2. an `MVStoreException` wrapping `OutOfMemoryError` on the MVStore background
   writer thread;
3. a compaction refusal rate of 16 in 17 collections, which is what lets (2)
   happen at all.

(3) is the cheapest to attack and is not this page's — it belongs with the
per-cycle coverage proof. (1) is what this page is for.

## What to do first

**Dump the throwable, not the frames.** "No captured frames" is the whole
difficulty and it is also the clue: the frame publication happens at the last
blocking/safepoint deposit, so a throw that reaches the top of `main` with none
is either very early or is not going through the ordinary throw path. Print the
object's class id, header words and `num_slots` at the point the launcher gives
up, before deciding whether this is a coercion bug, a stale receiver or a
genuinely non-`Throwable` throw.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "$CP" org.h2.test.db.TestOpenClose
```

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of, and the differential that separated them.
- `docs/known-issues/h2/correctness-issues-consolidated.md` — the index this
  class appears in.
