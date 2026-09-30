# G1 and ZGC: `-Xlog:gc` lines and ZGC's MXBeans are not JDK 25 HotSpot's

*Filed 2026-09-28 by gcd d10/o (lane oom10, GC observability on every
collector). Measured, not fixed: every difference below sits in a
backend-specific arm (G1 or ZGC), which this round does not change.*

- **Status:** OPEN.
- **Severity:** low -- observability. No wrong heap result; but a GC-log
  parser (GCeasy picks its parser from the `Using ...` line) or a JMX client
  keyed on HotSpot 25's names sees a different collector on G1 and ZGC.
  Generational (Serial's shape) matches HotSpot Serial on every row below.
- **Owners:** the G1 and ZGC collector rounds.

## Evidence

`tools/bench/Gcd1MxNamesProbe.java` (new, deterministic) on Temurin 25.0.3
and on the pre-round CratonVM binary (`target/release/cratonvm.exe` of
2026-09-27 15:59, Windows, `-Xmx256m`, both modes identical):

| collector | HotSpot 25 | CratonVM |
|---|---|---|
| Serial / Generational | `collectors=[Copy, MarkSweepCompact]`, `heap-pools=[Eden Space, Survivor Space, Tenured Gen]` | the same |
| G1 | `collectors=[G1 Concurrent GC, G1 Old Generation, G1 Young Generation]`, `heap-pools=[G1 Eden Space, G1 Old Gen, G1 Survivor Space]` | the same |
| ZGC | `collectors=[ZGC Major Cycles, ZGC Major Pauses, ZGC Minor Cycles, ZGC Minor Pauses]`, `heap-pools=[ZGC Old Generation, ZGC Young Generation]` | `collectors=[ZGC Cycles, ZGC Pauses]`, `heap-pools=[ZHeap]` |

`counted=true` everywhere (a `System.gc()` moves some collector's count).

The same program under `-Xlog:gc`:

| collector | HotSpot 25 | CratonVM |
|---|---|---|
| Serial / Generational | `Using Serial`, `GC(0) Pause Full (System.gc()) 9M->1M(247M) 2.864ms` | `Using Serial`, `GC(0) Pause Full (System.gc()) 4M->0M(18M) 6.875ms` |
| G1 | `Using G1`, `GC(0) Pause Full (System.gc()) 7M->1M(24M) 4.142ms` | no `Using` line; `GC(0) Pause Young (System.gc()) 5M->0M(256M) 13.014ms` |
| ZGC | `GC(0) Major Collection (System.gc())`, then `GC(0) Major Collection (System.gc()) 8M(3%)->4M(2%) 0.013s` | `GC(0) Garbage Collection (System.gc()) 4M->0M(16M) 13.188ms` |

## What is wrong, per backend

1. **ZGC bean names** (`gc/src/gc_metrics.rs`, `JMX_ZGC_CYCLES_COLLECTOR`,
   `JMX_ZGC_PAUSES_COLLECTOR`, `JMX_POOL_ZHEAP`, `BackendBeanShape::Zgc`):
   deliberately HotSpot's NON-generational ZGC (JDK 15-22), on the argument
   that this ZGC has no young generation. JDK 23 removed non-generational
   ZGC, so no JDK 25 HotSpot ever answers these names; code written against
   JDK 25 that looks up `ZGC Minor Cycles` / `ZGC Young Generation` finds
   nothing. The notification count differs too (`GcNotificationThreadProbe`:
   HotSpot ZGC 26, CratonVM 2), since HotSpot notifies per cycle and per
   pause of each generation.
2. **ZGC `-Xlog:gc` line** (the ZGC arm of the GC event plumbing,
   `vm/src/runtime/interpreter/gc_events.rs`): the JDK 21 non-generational
   shape `Garbage Collection (<cause>) a->b(c) t ms`; JDK 25 prints
   `Major Collection (<cause>)` (or `Minor Collection`) twice, the second
   with `a(p%)->b(q%) t s`.
3. **G1 `Using G1` line** (`vm/src/vm/vm_init.rs`, the `-Xlog` wiring: only
   `VmHeap::Generational` calls `unified_logging::log_gc_startup`): G1 and
   ZGC print no startup line. HotSpot prints `Using G1` under plain
   `-Xlog:gc` (ZGC prints its banner under `gc,init`).
4. **G1 `System.gc()`**: `Pause Young (System.gc())` with the heap's MAX as
   capacity; HotSpot runs `Pause Full (System.gc())` and prints the
   COMMITTED capacity. The cause label follows the collection this G1 runs
   for `System.gc()` (a young pause plus a marking cycle; it has no full
   collection -- see `g1-humongous-refusal-with-free-space-and-old-array-fixup-misses.md`),
   so only the capacity figure is a pure logging difference.

## Proposed fix

1. and 2. together, in the ZGC round: either describe the generational
   shape HotSpot 25 has (four collectors, two pools, `Major`/`Minor
   Collection` lines -- every CratonVM ZGC cycle is whole-heap, so it would
   report each as a Major cycle plus its pauses), or state in `docs/GC.md`
   that the ZGC beans follow JDK 21 on purpose. 3.: call `log_gc_startup`
   with `collector: "G1"` for `VmHeap::G1` (a one-line arm in `vm_init.rs`);
   HotSpot's ZGC `gc,init` banner is `Initializing The Z Garbage Collector`.
   4.: print the committed capacity on G1 lines, as Generational's
   `gc_event_heap_used` companion does.

## How to verify

```
javac -d out tools/bench/Gcd1MxNamesProbe.java
for gc in UseGenerationalGC UseG1GC UseZGC; do
  cratonvm -XX:+$gc -Xmx256m -Xlog:gc -cp out Gcd1MxNamesProbe
done
```

against `java -XX:+UseSerialGC|UseG1GC|UseZGC -Xmx256m -Xlog:gc -cp out
Gcd1MxNamesProbe` (the tables above). Retire when the ZGC rows and the G1
`Using` line match, or when `docs/GC.md` records the ZGC shape as intended.
