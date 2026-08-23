# `ConfigurationPropertySourcesTests` is 245× HotSpot — decomposed

**Status: OPEN — Term 1 CLOSED 2026-08-23, Terms 2 and 3 open.** Rewritten from the 2026-08-21 first cut, which
called this "the native-collections floor, no leaf over ~8%, no dominant term
to attack" and left it there. That reading was **wrong in the way that matters**:
a flat profile does not mean a flat cause. Decomposed properly, the gap is three
named things, two of which are algorithmic divergences from the JDK rather than
constant-factor overhead.

Still open — nothing here is fixed. What follows is the decomposition, the
measurements that pin each term, and the two fixes with their predicted
payoffs, so the next session starts from arithmetic instead of from a profile.

## The workload, exactly

`environmentPropertyAccessWhenImmutableShouldBePerformant` and its two siblings
do 1000 property lookups across 100 `MapPropertySource`s of 1000 properties
each. Each lookup asks every source for its cache, and for a **mutable** source
`SoftReferenceConfigurationPropertyCache.hasExpired()` is true on every call
(`timeToLive == null` is the default), so `Cache.update` runs every time:

```java
String[] lastUpdated  = data.lastUpdated();
String[] propertyNames = propertySource.getPropertyNames();   //  <- (2)
if (lastUpdated != null && Arrays.equals(lastUpdated, propertyNames)) {
    return;                                                   //  <- (1)
}
```

Instrumenting that class and running it on **both** VMs gives byte-identical
control flow — this is not CratonVM taking a different path:

```
HotSpot   SICS_DIAG updateCalls=102114 fastPath=101910 rebuildFirst=204
          rebuildDiffered=0 lenDiffered=0 elemSameRef=100008000 elemEqual=0
CratonVM  SICS_DIAG updateCalls=102114 fastPath=101910 rebuildFirst=204
          rebuildDiffered=0 lenDiffered=0 elemSameRef=100018000 elemEqual=0
```

So the fast path fires 101 910 times out of 102 114 on both, and the real work
per test run is **~100 million** `Arrays.equals` element comparisons (all
resolved by reference identity) plus **~100 million** keySet steps inside
`getPropertyNames()`. HotSpot: 6–7 s. CratonVM: 475–580 s.

**Two readings refuted before writing this up.** "The Spring cache never hits
under CratonVM" — it hits exactly as often, see above. "`SoftReference.get()` is
broken" — 300 000 reads of a strongly-held referent, `nullsImmediate=0
nullsFresh=0 nullsWeak=0 identityOk=true`, identical on both VMs.

## It is not codegen, and the JIT is nearly irrelevant here

| | ms |
|---|---|
| CratonVM, JIT | 475 178 |
| CratonVM, `--nojit` | 643 523 |
| HotSpot | 6 125 |

The JIT buys **1.35×** on a workload HotSpot runs 78× faster. Whatever is
expensive is not compiled code. `perf record --sort dso` agrees: **96.09 % of
samples are inside the `cratonvm` binary**, not in JIT-compiled code.

And plain compiled Java is fine — a hand-written loop over the same two arrays:

| loop | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `a[i] != b[i]` | 1.3 ns/elem | 4.9 | 3.8× |
| body of `Objects.equals` written out | 1.4 | 7.5 | 5.4× |
| `aaload` both sides only | 2.5 | 2.7 | 1.1× |

**~5× on ordinary compiled Java, 40–400× on anything that touches the native
collections.** That is the whole story, and it is where the first cut stopped.

## Term 1 — `keySet()` was O(n) per call; the JDK's is O(1) — **CLOSED 2026-08-23**

`native_lhm_key_set` (and the `HashMap` twin) called `lhm_collect_keys` and then
`make_view_set_of`, which allocates a carrier, allocates a backing map, and
**inserts every key through `native_map_put`** — hashing each key and probing
for duplicates. Per call. The JDK returns a cached live view and touches
nothing.

Measured on a 1000-entry map (`KeySetBench`), µs per call:

| rung | HotSpot | CratonVM (2026-08-21) |
|---|---|---|
| `map.size()` | 0.4 | 1.8 |
| **`map.keySet()` and nothing else** | **~0** | **2619** |
| iterate a *hoisted* view | 10.8 | 4434 |
| `keySet()` + iterate | 10.0 | 6151 |

`HashMap` behaved the same as `LinkedHashMap` (2852 µs). Spring calls this
101 910 times per test run.

**Fixed by `internal/performance/lazy-map-views-FIXED-20260823.md`**: a
`keySet()`/`values()`/`entrySet()` view is now cached on the source in the
field `java.util.AbstractMap` declares for it, exactly as HotSpot does, and
`resync_view_set` early-outs on a three-term generation stamp. Same probe, same
machine, one binary A/B'd by `CRATONVM_MAP_VIEW_CACHE`:

| rung | HotSpot | CV before | CV after | ratio to HotSpot, before -> after |
|---|---:|---:|---:|---|
| `viewOnly` (LHM) | ~0 | 1284.0 | **1.5** | — |
| `sizeOnly` (LHM) | ~0 | 3132.0 | **5.0** | — |
| `perCall` (LHM) | 9.0 | 5206.5 | **1192.5** | 578x -> **132x** |
| `hoisted` (LHM) | 9.5 | 2974.5 | **1217.5** | 313x -> **128x** |
| `perCall` (HM) | 15.5 | 5506.0 | **1246.5** | 355x -> **80x** |
| `hoisted` (HM) | 14.5 | 3079.5 | **1197.5** | 212x -> **83x** |

Term 1 is gone outright. Term 2 is what is left, and it is now visible without
the rebuild sitting on top of it — see below.

## Term 2 — the per-element constant is ~2 µs, and it is linear

The obvious next guess is a quadratic iterator. It is not. Holding total
elements fixed and scaling the map width:

| width | `keySet()` build | iterate hoisted |
|---|---|---|
| 250 | 1.84 µs/elem | 2.39 µs/elem |
| 500 | 1.72 | 2.54 |
| 1000 | 1.95 | 2.32 |
| 2000 | 1.88 | 2.24 |

Flat. Both are clean **O(n) with a ~2 µs per-element constant** — against
HotSpot's ~10 ns. (Those two rows were measured with the per-read rebuild still
in them. With Term 1 closed the `iterate hoisted` constant is **1.2 µs/elem**
against HotSpot's 9.5 ns — still ~128x, and now the whole of the remaining gap
rather than part of it.) That constant is the floor, and profiling the isolated
`keySet()` rung (no Spring, 13 s of pure view construction) shows what it is
made of:

| share | group |
|---|---|
| 17.4 % | object-address validation (`ZObjectStarts::contains`, `is_object_address`) |
| 28.8 % | field index/descriptor resolution + checked cell read/write |
| 7.3 % | native root pinning (`read_native_pin`, `pin_native_root`) |
| 4.9 % | `load_and_forward_inner` |
| 6.7 % | the map internals themselves |

i.e. the natives drive Java objects through a **validated, name/descriptor-resolving
heap API**, and pay that per field touch. No existing switch moves it much —
`CRATONVM_GC_NO_VALIDATE_ONCE=1` costs 6.6 % (so "validate once" is currently
worth ~6 %), `CRATONVM_COMPACT_REF_FIELDS=0` gains 6.8 %, and the Generational
collector is 33 % faster than ZGC on the same code.

## Term 3 — a static with a registered native can never be JIT-compiled

`java.util.Objects.equals` is registered as a `NativeKind::Intrinsic` native.
`java/util/Arrays.equals(Object[],Object[])` is **not** a native — it is
ordinary JDK bytecode, and it *is* compiled (`CRATONVM_DBG_JIT_COMPILED` shows
both a `put` and an `osr` body for it). Yet it runs at 214–224 ns/element while
the byte-identical hand-written loop runs at 7.5:

| rung | CratonVM ns/elem |
|---|---|
| `java.util.Arrays.equals` | 213.7 |
| the same loop calling `java.util.Objects.equals` | 231.5 |
| the same loop calling a **local** static with the identical body | **21.1** |
| the body written out inline | 7.5 |

`java/util/Objects.equals` never appears in the compiled-method list; the local
twin does. Directly, 10M calls per rung:

| | nativized | local twin | ratio |
|---|---|---|---|
| `Objects.equals` | 275.5 ns | 66.0 ns | 4.2× |
| `Objects.hashCode` | 323.3 ns | 73.7 ns | 4.4× |
| `Objects.requireNonNull` | 237.0 ns | 69.6 ns | 3.4× |
| `Objects.isNull` | 177.9 ns | 35.5 ns | 5.0× |

**Why**: the "prefer real JDK bytecode unless force-gated" rule lives in
`force_native_over_real_jdk_bytecode`, and that is read only from
`dispatch_virtual.rs` and `jit_bridge.rs` — **never from `dispatch_static.rs`**.
For a static, the registry always wins, so the real bytecode is never used, the
method is never compiled, and every call pays the full native-dispatch path.
`requireNonNull` is among the most-called methods in the JDK and in Spring, so
this is a VM-wide tax rather than a microbenchmark curiosity.

**A first attempt at the fix is recorded here because it FAILED, and the
failure is the useful part.** Making `dispatch_static` consult a curated
yield-list for `java.util.Objects` (behind `CRATONVM_NO_STATIC_REAL_BYTECODE`)
built clean and measured **nothing** — ON vs OFF was identical on the compiled
path *and* under `--nojit`. So the interpreter's `dispatch_static` is not the
only selector, or something downstream re-selects the native. Whoever picks
this up: **print an engagement counter before trusting the flag**, and start by
finding every place that chooses native-over-bytecode for a static, not just
that one. The change was reverted rather than shipped inert.

## Where the time actually goes

Per test run, against the ~475 s (JIT, loaded host) arm:

| term | est. cost | note | 2026-08-23 |
|---|---|---|---|
| `keySet()` view construction | ~190 s | 101 910 calls × 1000 elem × ~1.9 µs | **~0 — CLOSED** |
| iterating those views | ~230 s | 100M steps × ~2.3 µs | **~120 s** (1.2 µs/elem) |
| `Objects.equals` via `Arrays.equals` | ~21 s | 100M calls × ~215 ns | unchanged |

Terms 1 and 2 were ~90 % of it. **`Objects.equals` is only ~4 %** — worth
fixing for the whole VM, but it is not this test's problem.

**The ~475 s figure and every estimate in this table predate the map-view fix.**
The arithmetic above predicts ~140 s; that prediction has NOT been measured
end to end (the Azure host was unreachable when the fix landed), so re-run the
class before quoting a new wall. What HAS been measured is the isolated probe,
above.

## The two fixes, in value order

1. ~~**Make map views lazy/live**~~ — **DONE 2026-08-23**, though not the way
   this entry proposed. Delegating `size()`/`contains()`/`iterator()`/
   `toArray()` to the source is design B of the plan page, and it is blocked by
   the view backing being readable from JDK bytecode
   (`keySet().spliterator()` does `getfield map.table`). What landed is design
   A: cache the view on the source in the JDK's own field, and guard the
   per-read rebuild on a `(source modCount, source size, backing size)` stamp.
   Kill switch `CRATONVM_MAP_VIEW_CACHE=0`, verify mode
   `CRATONVM_VERIFY_MAP_VIEW_CACHE=1`, and a 138-row behavioural probe
   (`probes/MapViewCacheProbe`) byte-identical to HotSpot.

2. **Cut the per-element constant** (Term 2) — now the WHOLE of the remaining
   gap on this workload, at 1.2 µs/elem against HotSpot's 9.5 ns. The natives read and write the
   nodes *they themselves allocated*, with a known layout, through the generic
   validated accessor. A trusted-access path for that case is where the 17 % +
   29 % lives. This is a change at the native/GC boundary and needs to be
   argued for GC-correctness, not just measured.

A cheaper partial for Term 1, if a lazy view is too big a step: build the view's
backing directly from the source's `(hash, key)` node pairs — the hash is
already stored in `NODE_FIELD_HASH` and the keys are already unique, so both
`map_hash_key` and the duplicate probe are pure waste. Constant-factor only; it
does not remove the O(n)-per-call.

## Reproducing

Benches used here (all standalone, no Spring): `ArrEqBench`, `ArrEqBench2`,
`PropNamesBench`, `KeySetBench`, `StaticNativeBench`. The Spring-level
instrumentation was a classpath shadow of
`SpringIterableConfigurationPropertySource` counting fast-path vs rebuild — the
same "instrument the consumer" technique that cracked the BatchJdbc miscompile.

```bash
cd /data/cratonvm/apps/spring-boot/core/spring-boot
cratonvm --java-home /data/toolchain/jdk-25 --Xmx 2g --XX:UseGc ZGC \
  -cp "$(cat build/cratonvm-test-cp.txt)" SbRunner \
  org.springframework.boot.context.properties.source.ConfigurationPropertySourcesTests
```

`perf` on this host needs `sudo sysctl -w kernel.perf_event_paranoid=1` first.
**Read `/proc/loadavg` before believing any number** — this box is shared and
was seen at load 178 on 8 cores. Every ratio above is from a paired run on one
host at one load; the absolute walls are not comparable across sections.
