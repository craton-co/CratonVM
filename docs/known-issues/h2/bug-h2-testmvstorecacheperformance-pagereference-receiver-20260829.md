# `TestMVStoreCachePerformance` — `NoSuchMethodError` for `Page.isPersistent()` against a `Page$PageReference` receiver

## Status

**OPEN BUT NOT REPRODUCED 2026-08-29**, split out from
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
