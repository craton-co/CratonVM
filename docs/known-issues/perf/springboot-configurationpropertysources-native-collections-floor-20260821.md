# `ConfigurationPropertySourcesTests` — decomposed, Term 1 fixed, ~23× HotSpot

**Status: OPEN — Term 1 CLOSED and RE-MEASURED 2026-08-24; Terms 2 and 3 open.**
Rewritten from the 2026-08-21 first cut, which called this "the
native-collections floor, no leaf over ~8%, no dominant term to attack" and left
it there. That reading was **wrong in the way that matters**: a flat profile does
not mean a flat cause. Decomposed properly, the gap is three named things, two of
which are algorithmic divergences from the JDK rather than constant-factor
overhead.

## The end-to-end number, measured

Term 1 (the map-view rebuild) was fixed on 2026-08-23 and this class was
re-measured on 2026-08-24. **One binary, `CRATONVM_MAP_VIEW_CACHE` as the A/B,
three interleaved rounds, reported in CPU time:**

| round | HotSpot | cache OFF | cache ON | OFF/ON |
|---|---:|---:|---:|---:|
| 1 | 8.3 s | 1291.0 s | 186.2 s | 6.93× |
| 2 | 6.6 s | 1274.9 s | 187.3 s | 6.81× |
| 3 | 8.4 s | 1304.3 s | 191.2 s | 6.82× |

**6.8× end to end**, and the class moves from ~156× HotSpot to **~23×**. All
three rounds pass 11/11 tests on both arms (one test is `@Disabled`, "for manual
testing", on both VMs).

**Why CPU time and not wall clock.** The box is shared and ran at load 17–37
with ten other `cratonvm` processes throughout. Wall clock there measures the
neighbours: one HotSpot run read 2.7 s idle and 15.4 s at load 35, a 5.7×
excursion on an unchanged binary. CPU time moved 1.5× over the same range, and
across the three rounds the OFF arm spans 2.3% and the ON arm 2.7% — the ON arm
also read 184.3 s at load 5, so it is flat from load 5 to 24. A first attempt at
this measurement in wall clock was discarded, not adjusted.

**The engagement counter, identical on all three ON runs** (deterministic):

```text
[MAP-VIEW-CACHE] EXIT resync_skipped=300132 resync_ran=66 elided=100.0%
                      view_reused=299758 view_built=415 switch=ON verify=OFF
```

300 132 rebuilds skipped against 66 run, and 299 758 view reuses against 415
builds — which is what says the fix is engaged on THIS workload rather than
merely present in the binary.

**The 245× in this page's old title was wall clock on a loaded host and is not
comparable to the numbers above.** The ~156× OFF figure here is the same
pre-fix behaviour priced the new way, on the same host, in the same window as
its own ON control.

What follows is the decomposition, the measurements that pin each term, and the
fixes, so the next session starts from arithmetic instead of from a profile.

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

## Term 1 — `keySet()` is O(n) per call; the JDK's is O(1)

`native_lhm_key_set` (and the `HashMap` twin) calls `lhm_collect_keys` and then
`make_view_set_of`, which allocates a carrier, allocates a backing map, and
**inserts every key through `native_map_put`** — hashing each key and probing
for duplicates. Per call. The JDK returns a cached live view and touches
nothing.

Measured on a 1000-entry map (`KeySetBench`), µs per call:

| rung | HotSpot | CratonVM |
|---|---|---|
| `map.size()` | 0.4 | 1.8 |
| **`map.keySet()` and nothing else** | **~0** | **2619** |
| iterate a *hoisted* view | 10.8 | 4434 |
| `keySet()` + iterate | 10.0 | 6151 |

`HashMap` behaves the same as `LinkedHashMap` (2852 µs). Spring calls this
101 910 times per test run.

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
HotSpot's ~10 ns. That constant is the floor, and profiling the isolated
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

Per test run, against the ~475 s (JIT, loaded host, WALL CLOCK) arm this was
decomposed on. See "The end-to-end number, measured" at the top for what the
class costs now and why that section prices it in CPU time instead:

| term | est. cost | note | after Term 1 |
|---|---|---|---|
| `keySet()` view construction | ~190 s | 101 910 calls × 1000 elem × ~1.9 µs | **gone** |
| iterating those views | ~230 s | 100M steps × ~2.3 µs | **reduced** |
| `Objects.equals` via `Arrays.equals` | ~21 s | 100M calls × ~215 ns | unchanged |

Terms 1 and 2 were ~90 % of it. **`Objects.equals` is only ~4 %** — worth
fixing for the whole VM, but it is not this test's problem.

**Checked against the measurement rather than left as arithmetic.** The
estimates above predicted ~140 s remaining; the measured ON arm is 186-191 s
CPU. The estimate was low, and the residue is Term 2: what the elision removes
is the per-READ rebuild, not the per-element cost of walking a view once it is
built. `Objects.equals` (~21 s) and the ~66 rebuilds that still run are the rest.
Term 2 is now the whole of the remaining gap on this workload rather than part
of it, which is the useful thing the re-measurement establishes.

## The two fixes, in value order

1. **Make map views lazy/live** (`keySet`/`values`/`entrySet`) — **DONE
   2026-08-23**, for `keySet` and `entrySet`, by a route this page did not
   anticipate. Rather than delegating reads to the source, the view's backing
   now carries the source's modification generation and a resync whose source
   has not moved returns immediately; and `keySet()`/`entrySet()` hand back the
   instance the map already has instead of building a new one, which is also
   what HotSpot does and restores `map.keySet() == map.keySet()`.

   MEASURED on `probes/KeySetBench`, `LinkedHashMap` of 1000 entries, one
   binary with `CRATONVM_MAP_VIEW_CACHE` as the A/B: `keySet()` alone
   1674.5 → **2.5 µs/call**; `keySet().size()` — the shape
   `StringUtils.toStringArray(map.keySet())` uses — 3125 → **4.5 µs/call**;
   `keySet()` + iterate 5949.5 → **1489.5 µs/call**.

   `values()` did NOT get it: its carrier is a list with a different backing
   scheme. Neither did entrySet READS — only their construction — because an
   entrySet's contents include values and a value-replacing `put` deliberately
   does not move `modCount`.

   Kill switch `CRATONVM_MAP_VIEW_CACHE=0`, verify mode
   `CRATONVM_VERIFY_MAP_VIEW_CACHE=1`, engagement census
   `CRATONVM_DBG=map-view-cache`.

2. **Cut the per-element constant** (Term 2). The natives read and write the
   nodes *they themselves allocated*, with a known layout, through the generic
   validated accessor. A trusted-access path for that case is where the 17 % +
   29 % lives. This is a change at the native/GC boundary and needs to be
   argued for GC-correctness, not just measured.

A cheaper partial for Term 1, if a lazy view is too big a step: build the view's
backing directly from the source's `(hash, key)` node pairs — the hash is
already stored in `NODE_FIELD_HASH` and the keys are already unique, so both
`map_hash_key` and the duplicate probe are pure waste. Constant-factor only; it
does not remove the O(n)-per-call. **Still worth doing** — the 2026-08-23 fix
removes the repeated builds but not the cost of the first one, so this is what
is left of Term 1 for a map whose key set really does keep changing.

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
