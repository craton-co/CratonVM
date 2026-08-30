# `TestMVStoreCachePerformance` — `NoSuchMethodError` for `Page.isPersistent()` against a `Page$PageReference` receiver

## Status

**OPEN BUT NOT REPRODUCED, 2026-08-29/30.** See "2026-08-30: 50 clean runs"
below before spending another run on blind repetition: relocation has never
been observed to engage on this workload at any tried heap size, which
argues for chasing this as a JIT-only (no-GC) concurrency defect next, or
checking whether it already closed as a side effect of an unrelated fix.

Split out from
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`, which tracked
this class only because it was watching it for a fragmentation defect that is
now closed. This is not that defect: it failed with **no `OutOfMemoryError` and
no `arena allocation failed` at all**.

**And on the 2026-08-29 tip the class PASSES**, once, `--Xmx 1g`:

```text
org.h2.test.store.TestMVStoreCachePerformance  rc=0  secs=282  oom=0  arena=0
  compaction_cycles=0 relocation_skipped_jit=6 relocation_on_proven_jit=0
```

Zero `isPersistent` / `NoSuchMethodError` occurrences in the log. Note the
census: **the slide never ran** (`relocation_on_proven_jit=0`), so this run does
not even exercise the relocation path a wrong-receiver defect would most likely
live on — it is one clean run, not evidence of absence. The page is kept because
the signature was real when it was recorded and a receiver defect that appears
once in N runs is exactly what this family looks like; see the sibling page's
"a clean arm is worth nothing without a base rate".

## The signature

```text
NoSuchMethodError: 'boolean org.h2.mvstore.Page$PageReference.isPersistent()'
```

## Read the class name in that message before theorising

**It is the RECEIVER's class, not the constant pool's.** CratonVM builds this
message from `receiver_class_id` (`vm_exec.rs`, the `class_name` binding on the
virtual-dispatch slow path), which is why the same family of defects produced
`'java.lang.Object[] java.lang.Object.toArray()'` when a receiver's header read
as all-zero (`ClassId(0)` **is** `java.lang.Object`).

So this is not a resolution defect on `Page.isPersistent`. In H2's source
(`src/main/org/h2/mvstore/Page.java`):

* `isPersistent()` is `protected final boolean` **on `Page`**, at line 827.
* `Page$PageReference` is a `public static final class` that extends `Object`
  and holds `long pos`, `Page<K,V> page`, `long count`. It has no
  `isPersistent` and is not in `Page`'s hierarchy.

A `PageReference` therefore arrived where a `Page` was expected. The two are
allocated in lockstep all over `MVStore` — `Page$NonLeaf.children` is a
`PageReference[]` and `getChildPage(i)` goes through `children[i].getPage()` —
so a stale or mis-indexed reference between them is the shape to look for, and
`PageReference.page` (slot 1 of 3) is the field that would produce exactly this
if a `getfield` read the wrong receiver.

That makes this a **wrong-receiver** defect, the same family as
`bug-h2-testrandommapops-small-heap-corruption-20260829.md` and the G1 defect
its parent page fixed — not a method-resolution defect, which is what the
message's shape invites and what the page this was split out of called it.

## What to do first

`CRATONVM_DBG_CCE_BT=1` dumps the offending receiver's shape, frame stack and
move history at the dispatch miss; it is the instrument that settled the G1
receiver defect and it applies unchanged here. The question it answers is the
one that discriminates: is the address one a slide just vacated (the object was
overwritten), or was the header already wrong before any slide (a different bug
entirely)?

`CRATONVM_ZGC_RELOCATE=0` is the cheap first bisect — it restores non-moving
behaviour byte for byte, so a failure that survives it is not a relocation
defect.

## 2026-08-30: 50 clean runs, and the working hypothesis needs correcting

Fifty more attempts, zero reproductions, and one finding that redirects where
to look next.

### The runs

| batch | heap sizes | `CRATONVM_ZGC_CONC_START` | runs | outcome |
|---|---|---|---:|---|
| 1 | 1g | default (0) | 2 | clean |
| 2 | 300-500m | default (0) | 8 | 2 legitimate `OutOfMemoryError` (too small a heap for the 80 MB working set — `Capacity: 4718592` at 500m, a 10001-length reference array at 300m), 6 clean. Neither failure names `Page` or `PageReference`. |
| 3 | 700-1050m | default (0) | 25 | clean |
| 4 | 750-1050m | **60** (forces concurrent marking on — the default is 0, "never") | 15 | clean |

Zero occurrences of `NoSuchMethodError` naming `Page` or `PageReference` in any
of the 50. `CRATONVM_DBG_CCE_BT=1` was armed for every run in batches 1, 3 and
4 and never fired.

### The working hypothesis this corrects

The page's own reasoning pointed at ZGC relocation as the likely site
("this run does not even exercise the relocation path a wrong-receiver defect
would most likely live on"). Batch 4 tested that directly: forcing concurrent
marking on (`CRATONVM_ZGC_CONC_START=60`, the value the sibling ZGC pages use
for measurement, against a default of 0 -- concurrent marking never starts at
all otherwise) changes the collection count (`relocation_skipped_jit` drops
from 41 at the default to 9-15 with it on -- concurrent marking is doing real
work, reclaiming enough that fewer STW cycles are needed) but **still never
produces a single compaction**:

```
[GC] zgc-features: … compaction_cycles=0 objects_relocated=0
                      relocation_skipped_jit=10 relocation_on_proven_jit=0 …
```

`compaction_cycles=0` and `relocation_on_proven_jit=0` on every one of the
25 + 15 = 40 GC-stats-instrumented runs across this session and the
2026-08-29 session's own 1g run. **This workload has never been observed to
relocate a single object, at any heap size from 300m to 1050m, with or
without concurrent marking forced on.** Whatever produced the recorded
signature either does not require a relocation cycle at all, or requires a
condition none of these 50 runs hit (a specific commit's binary, a specific
concurrent interleaving among the up-to-100 reader threads, or a heap
pressure shape a fixed-size synthetic sweep does not reproduce).

**This does not rule out the wrong-receiver family diagnosis** — `Page` and
`Page$PageReference` are still allocated in lockstep, `PageReference.page`
(slot 1 of 3) is still the field that would produce exactly this signature,
and the parent page's own G1/OSR-coverage fixes are proof that this codebase
has more than one way to hand a JIT-compiled frame or a GC cycle a stale
reference. It narrows where the NEXT session should look: not "run it again
and hope for a relocation cycle", since 40 instrumented attempts say that
cycle essentially does not happen on this workload's shape. The two
directions this leaves:

1. **A JIT-only mechanism, no GC required.** `PageReference.getPage()` is a
   one-line accessor (`return page;`) hit constantly by up to 100 concurrent
   reader threads inside `NonLeaf.getChildPage(i)` — exactly the shape a
   polymorphic inline-cache race under heavy concurrent first-compile
   pressure would need. If an inline cache's target can be corrupted between
   two racing installs (one thread's write half-lands over another's), a
   monomorphic hit for the wrong receiver type would produce this exact
   symptom with no GC involvement at all. Nothing in this page's evidence
   argues against it, and it would explain why relocation-forcing changed
   nothing.
2. **The historical binary is not the current one.** The original signature
   was recorded once, and every fix landed on `dev` since then (including the
   G1/OSR-coverage repairs this page's own parent chases) changes what a
   4-year-old-in-VM-time class actually exercises. Worth checking whether the
   commit the original signature was seen on is still an ancestor of current
   `dev`, or whether something upstream of this page already closed it as a
   side effect the way `bug-h2-testkillprocess-…-FIXED-20260829.md` records
   for four *other* classes' fragmentation crashes.

Neither is confirmed. Both are cheaper to chase than a sixth batch of blind
repetition: (1) is a standalone JIT concurrency probe (many threads, one
polymorphic getter, no H2 needed); (2) is a `git bisect`-shaped question, not
a repro-shaped one.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -c "$CP" org.h2.test.store.TestMVStoreCachePerformance
```

## Related

- `fixed-suite-bugs/h2-suite-bugs/bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of.
- `docs/known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`
  — the same family, with the two ends of it named (who freed it vs who
  allocated over it).
