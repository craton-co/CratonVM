# Hibernate ORM — `HqlParserMemoryUsageTest`: the 2.5x allocation multiplier, and the two counter bugs that hid it

**Status:** 1 confirmed CratonVM-specific FAIL, root cause **partially**
identified (2.5x allocation vs HotSpot; the mechanism is not yet fully
attributed). Two measurement defects found underneath it are **fixed** — before
them, no allocation figure this VM reported was trustworthy, including the one
this record was originally opened with. 0 HANGs on the default collector; the 3
previously-flagged classes rechecked earlier still do not reproduce (see
"Previously flagged" below).

**Verified on:** Azure Linux (`20.80.105.49`, JDK 25 Temurin at
`/data/toolchain/jdk-25`), worktree `perf/hql-parser-alloc-20260817` off
`dev@090416c56`.

## The failure

`org.hibernate.orm.test.hql.HqlParserMemoryUsageTest` (upstream `HHH-19240`)
parses one complex nested-CASE/subquery HQL string and asserts the parse
allocates under 256 MiB, measured via
`com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes()`.

| | reported | result |
|---|---|---|
| HotSpot JDK 25 | 250,123 KB | PASS (2% under budget) |
| CratonVM | ~627,000 KB | **FAIL** |

Note how little headroom HotSpot has: this test is tight on the VM it was
written for.

## First, two counter bugs — because the original figure was not measuring allocation

The earlier revision of this record put the gap at 1.8–1.9x. That number came
from a counter that was not counting allocation.

**1. `getTotalThreadAllocatedBytes()` returned a heap-occupancy gauge.** It
answered with `heap_allocated_bytes()`, which is derived from `used - free` and
therefore *falls at every collection*. A caller measuring a window containing a
GC gets the difference of two occupancies. The identical HQL parse read:

| configuration | reported |
|---|---|
| ZGC, 2 GB heap | 488 MB |
| Generational, 2 GB heap | **49 MB** |
| Generational, 8 GB heap | 458 MB |

Same bytecode, three answers, none cumulative — and the 49 MB one *passed the
test*. Replaced with a real process-wide accumulator fed from every TLAB retire
and every non-TLAB allocation. After the fix the same six configurations
(3 collectors × 2 heap sizes) report **626–629 MB**, a 0.5% spread.

**2. `Tlab::retire` charged the thread for the whole chunk.** It installed the
tail filler — which sets `cursor = end` by design — and *then* read
`consumed_bytes()`, so every retire credited the unused tail as allocated.
Separate from bug 1 and fixed alongside it.

The per-thread counters were never wrong: `probes/AllocCounterFidelity` measures
a known allocation volume at **1.000x expected** on all three collectors at two
heap sizes. Only the process-wide one was broken.

**The lesson this record should carry:** the original investigation verified the
metric "against CratonVM's `heap.allocated_bytes()` semantics" — which is the
very quantity that was wrong. Verifying a gauge against its own source proves
only that it is consistently itself.

## The real number, and what is attributable so far

With a trustworthy counter the multiplier is **2.5x**, not 1.8–1.9x.

**Where the allocation goes** (JFR `ObjectAllocationSample` on HotSpot; the
allocation *counts* are VM-independent because it is the same bytecode, so this
profile localises the work for both VMs):

| class | share |
|---|---|
| `org.antlr.v4.runtime.atn.ATNConfig` | 30.9% |
| `byte[]` | 25.1% |
| `ConcurrentHashMap$Node[]` | 6.4% |
| `int[]` | 4.3% |
| `Object[]` | 3.8% |
| `SingletonPredictionContext` | 2.8% |
| everything else | < 2% each |

**Per-shape cost, CratonVM vs HotSpot** (`probes/AllocShapeProbe`, bytes/op):

| shape | HotSpot | CratonVM | ratio |
|---|---|---|---|
| plain `Object` | 16 | 16 | 1.0 |
| `int[8]` | 48 | 48 | 1.0 |
| `Object[4]` | 32 | 48 | 1.5 |
| `ArrayList` empty | 24 | 96 | 4.0 |
| `HashMap` empty | 48 | 304 | 6.3 |
| `HashMap` 1 entry | 160 | 304 | 1.9 |
| `HashMap` 4 entries | 256 | 304 | 1.2 |
| `HashSet` empty | 64 | 272 | 4.3 |
| `LinkedHashMap` empty | 64 | 384 | 6.0 |

Two mechanisms are visible there:

* **Reference slot width.** `Object[4]` is 48 here and 32 on HotSpot — 8-byte
  reference slots against 4-byte compressed oops.
* **Collections have a large FIXED cost.** A `HashMap` costs 304 bytes empty,
  with one entry, and with four alike, because `native_map_init` eagerly
  allocates a 16-slot bucket array where the JDK allocates none until the first
  `put`; `native_al_init` does the same with a 10-slot buffer. The JDK's laziness
  is not a detail here, it is 4–6x on small collections.

## What was ruled OUT

**Reference width is not the main driver.** Compressed oops exist
(`CRATONVM_COMPRESSED_OOPS=1`, Generational backend only) and do work —
`Object[4]` drops to 32, matching HotSpot exactly. On this test they close only
**9%** of the gap (627,872 KB → 569,847 KB). That was a hypothesis formed
halfway through this investigation and refuted by measuring it rather than by
shipping it.

That leaves roughly 2.3x unattributed. `ATNConfig` at 31% is the obvious next
target: it is a plain 5-field object, and plain objects measure 1.0x, so
whatever inflates it is not the object header.

Also ruled out earlier and still ruled out: ANTLR's SLL prediction mode succeeds
on both VMs (no fallback to the expensive LL parse on either), and warm repeats
are cheap on both, so this is neither a dispatch divergence nor a leak.

## Next steps

* Attribute the `ATNConfig` share directly. Plain-object allocation is 1.0x in
  the shape probe, so a 31% share that is inflated must be inflated for a reason
  the shape probe does not reach — allocation *count* rather than size, or a
  per-instance side allocation.
* The collections' eager tables are independently worth fixing:
  `map_resize_inner` **already** materialises a null table at capacity 16 (it
  documents the JDK-bytecode-constructed case) and `native_map_put` already
  routes there, so the machinery exists. The obstacle is that
  `buckets.is_none()` is currently *also* the signal for "not one of our maps" in
  `native_map_size` / `native_map_is_empty`, which needs a different
  discriminator first. Expect ~10-15% of this test, not 60%.
* Judge whether this test can be a CratonVM target at all. HotSpot passes with
  2% headroom; matching it needs a 2.5x reduction, which is an object-layout and
  collection-representation programme, not a defect fix.

## Previously flagged, still not reproducing

`annotations.uniqueconstraint.UniqueConstraintBatchingTest`,
`query.hql.FunctionTests` and `query.hql.StandardFunctionTests` were flagged on a
Windows host as FAILs that also reproduced under real HotSpot there. Rechecked
against `dev` on Azure, all three pass on **both** VMs with byte-for-byte
matching `found/ok/failed/skipped` counts. If they resurface, re-verify against
real HotSpot on the *same* host before assuming a CratonVM regression.

## Harness gotcha (Azure host only) — still current

`/data/cratonvm/apps/hib-suite-runner/common.args` points at five
`target/libs/*.jar` files (`hibernate-testing`, `hibernate-ant`,
`hibernate-scan-jandex`, `hibernate-community-dialects`, `hibernate-reveng`)
that do not exist on this host — only `target/classes/java/main` was ever
compiled; the Gradle `jar` task for those five modules was never run. This is a
local build-completeness issue, not a CratonVM defect: the missing jars also mean
the `../../../apps/META-INF/services` files behind two `ServiceLoader` lookups are absent,
producing a `ServiceConfigurationError` for `CheckClearSchemaListener` and then
an `AssertionFailure` about `TestableLoggerProvider`.

Every result here used a corrected classpath substituting the raw
`target/classes/java/main` dirs (plus `src/main/resources` where present) for the
five missing jars. Not applied back to the shared `common.args` to avoid touching
another session's checkout.

## Addendum 2026-09-06 — today's 3-GC-arm run shows a Gen/G1 vs ZGC split, not a uniform spread

A full 3-GC-arm hib-suite run reported, for the textually-identical bytecode:
Generational ~382,755 KB, G1 ~383,181 KB, ZGC ~627,717 KB — Gen and G1 agree with
each other tightly (0.1% apart) but sit at roughly **0.61x** of ZGC, not the "0.5%
spread across three collectors" this doc's verification section claims for
626-629 MB.

The notable part: **ZGC's figure (627,717 KB) is inside this doc's own verified
626-629 MB range.** It is Generational and G1 that are the outliers here, both
reporting a figure close to the doc's earlier, since-fixed, wrong bug-1 reading for
Generational at a 2 GB heap (`49 MB` — not this number, but the same class of
"counter falls at a GC" symptom shape, since Gen/G1 collect far more eagerly than
ZGC's default `CRATONVM_ZGC_CONC_START=60` heuristic on a small parse-only
workload). This is consistent with, though not proof of, the process-wide
allocation accumulator (the doc's bug-1 fix) having a live gap specific to
Generational/G1 that this test's original two-collector table (which predates
G1's inclusion) never exercised — not a new problem in ZGC.

Not chased further here per this triage's scope (this is a data-quality note on an
already-open item, not a new investigation): worth a follow-up rerun of
`probes/AllocCounterFidelity.java` split out by collector before trusting any
Gen/G1 allocation figure from this test again.

## Repro

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/hib-suite-runner
CP="$(sed -n '2p' common.args)"   # then apply the five substitutions above
java -Xmx2g -cp "$CP" CratonRunner org.hibernate.orm.test.hql.HqlParserMemoryUsageTest
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 2g -c "$CP" \
  CratonRunner org.hibernate.orm.test.hql.HqlParserMemoryUsageTest
```

The two probes this record rests on, both runnable against either VM:
`../../../probes/AllocShapeProbe.java` (per-shape bytes/op) and
`../../../probes/AllocCounterFidelity.java` (does the counter report a known volume
correctly, across heap sizes and collectors).
