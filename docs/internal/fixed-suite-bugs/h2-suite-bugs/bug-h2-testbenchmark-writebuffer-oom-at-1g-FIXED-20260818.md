# `TestBenchmark` — a 10 MB `WriteBuffer` grow threw `OutOfMemoryError` under `-Xmx1g` — FIXED

| | |
|---|---|
| **Status** | **FIXED 2026-08-18**, found the same day. |
| **Symptom** | `java.lang.OutOfMemoryError: Capacity: 10616832` raised inside `MVStore`'s background writer, which then calls `MVStore.panic`. |
| **Cause** | `java/nio/ByteBuffer.allocate` is shadowed by a native. Its backing array made **one** allocation attempt and reported OOM — it never ran the force-a-GC-and-retry ladder that both bytecode allocation paths run. |
| **Fix** | `NativeContext::reclaim_before_alloc_retry`, called by `s2_bb_alloc` at its first allocation. |

## The number was the whole point, and it was not a full heap

The failing allocation was **10,616,832 bytes — about 10 MiB — under a 1 GiB
heap**, and the heap was not out of memory. Instrumenting H2's own
`WriteBuffer.grow` catch block to report the heap at the moment of the throw,
and then to repeat the identical allocation, settled it in one run:

```
PROBE-WB original OOM: java.lang.OutOfMemoryError: Java heap space
PROBE-WB heapUsed=29M total=1024M max=1024M
PROBE-WB retrying the same allocation 10616832 ...
PROBE-WB retry 0 SUCCEEDED
MVLOAD OK total 113954 ms
```

**29 MB used out of 1024 MB, and repeating the same `ByteBuffer.allocate` one
Java statement later succeeded on the first attempt** — after which the whole
workload ran to completion. Everything else agreed: the class passed under
`--nojit`, at `-Xmx2g`, and on the generational collector, and failed only with
the JIT on, at 1g, on ZGC.

## What was actually wrong

Three paths in this VM allocate a caller-sized array. Two of them, on a failed
request, run a **ladder**: retire the calling thread's TLAB, force a collection,
retry, run `last_ditch_reclaim`, retry again, and only then throw. Those are
`gc_alloc_array` (interpreter) and `jit_newarray` (JIT).

The third is a native. `ByteBuffer.allocate` is shadowed by CratonVM in
real-JDK mode as well as synthetic mode, so its backing `new byte[n]` never
executes `newarray` and never sees that ladder. `s2_bb_alloc` called
`ctx.try_new_array` exactly once and, on `None`, threw.

That single attempt is correct **for the allocator**, and `runtime::native_oom`
explains why in detail: a native holds raw `ObjectRef`s in Rust locals that no
GC root set covers, so collecting underneath one would relocate them (dangling
the locals) or sweep them (freeing live objects). The allocator cannot know
whether its caller is holding any. But the *caller* can, and at its first
allocation `s2_bb_alloc` provably holds none — so that is where the retry
belongs.

### Why the request could not be served at that instant

The ZGC arena is a non-compacting bump region with a free list, and at the
moment of the refusal it was shredded. Its own diagnostic, at the failing
request:

```
request=10616848  used=1063784040  capacity=1073741824
free_list_bytes=497919440  largest_free_block=900664  free_spans=44638
```

**475 MB free, largest hole 880 KB.** The fragmentation report went further and
named the cheapest window that could have served the request:

```
window_bytes=10616920  window_free=10502632  wall_bytes=114288  walls=1960
```

— 122 KB of live data in ~2000 runs standing between 10.5 MB of free bytes.
Compaction would clear that, and compaction never ran: `compaction_cycles=0
objects_relocated=0 relocation_skipped_jit=11`, because ZGC declines to relocate
while a compiled frame is live (its registers and spill slots cannot be
rewritten). That refusal is correct and stays.

But it is also **not what made this fatal**. `--nojit` passes with
`CRATONVM_ZGC_RELOCATE=0` too — i.e. with compaction off — so contiguity here is
not what compaction buys. What was fatal was giving up after one attempt: the
collection that ran moments later left the heap 97% free, and the retry that
would have used it was never made.

## The fix

* `NativeContext::reclaim_before_alloc_retry` (default `false`, so every other
  native and all mock contexts are byte-for-byte unchanged). The VM impl runs
  the interpreter's ladder — retire TLAB, `maybe_gc_forced`, **overhead-limit
  check**, `last_ditch_reclaim` — and returns `false` when the heap is genuinely
  GC-thrashing, so a wedged heap still reports OOM rather than spinning.
* `s2_bb_alloc` (the live real-JDK path) calls it at its **first** allocation
  and retries once. The second allocation in that function does not, and must
  not: `arr` is live by then, which is why it is pinned across it.
* `native_heap_bytebuffer_allocate` (the synthetic-mode twin) got the same
  treatment.
* The refusal now names itself — `Java heap space (ByteBuffer.allocate N)`.

### A note on the diagnosis cost, and two instrument fixes it paid for

Two diagnostics were changed because each of them hid the answer:

1. **The thrown message was a bare `Java heap space`.** So is the pre-allocated
   singleton OOME, and so are three unrelated natives; the string identified
   nothing, and each candidate site cost a run to rule out. The two bytecode
   paths already name themselves (`alloc_array length N`); this one now does too.

2. **`warn_alloc_failed_once` / `frag_report_once` were one-shot.** Half of that
   rule was right — once the arena is out, the failure repeats for every request.
   The other half was not: an allocation does not fail once, it fails at each
   rung of the caller's ladder. One-shot therefore always profiled the
   *pre*-collection arena and went silent for the rungs that decide whether the
   OOM is honest. Here the one-shot line reported `used` at 99% of capacity while
   the collection that ran a moment later left the heap 97% free. The one-liner
   now fires on a doubling schedule carrying `failure_seq`, and the long frag
   report fires twice. **That count is what proved the ladder was never run:
   exactly one arena failure in the entire run.**

## Verification

Interleaved on the Azure host, `-Xmx1g`, ZGC, one binary per arm:

| arm | `MvLoad` repro (1M puts) | `org.h2.test.store.TestBenchmark` |
|---|---|---|
| HotSpot 25 control | OK 1.1 s | rc=0, 147 s, no OOM |
| CratonVM before | **FAILED 5/5** | **rc=1 at 37 s, 8 OOM/MVStoreException lines** |
| CratonVM after | **OK 3/3** | **0 OOM lines**, runs the full workload |

The before/after binaries differ only by the `s2_bb_alloc` change.

## The residual, which is not this bug

With the OOM gone, `TestBenchmark` no longer fails — it runs the whole workload
and is slow, hitting the wall-clock cap instead. That is the throughput story
that covers most of the H2 non-passing set (see `!nonpassed-40-census-20260818.md`
§2a) and it is where this class now belongs. The distinguishing claim in the
original page — *"CratonVM fails FASTER than HotSpot passes … an arm that exits
early is doing less work, not the same work slower"* — was correct, and is no
longer true of it: it now does the same work, slowly.

## Reproduction (both the class and the 90-second repro)

```bash
# focused repro — MvLoad is testConcurrency's load phase, ~90 s
cd /data/h2fix-wd
CP="$(cat /data/h2fix-cp.txt)"
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1g -XX:+UseZGC \
    -c "/data/h2fix-probe/mv:$CP" MvLoad 1000000

# the class itself
cd /data/h2gc-20260817
export CRATONVM_BIN=<cratonvm>  JDK25=/data/toolchain/jdk-25
export H2_ROOT=/data/cratonvm/apps/h2database/h2
export OUTROOT=$PWD/out-probe  H2_GC_FLAG='-XX:+UseZGC'
bash run-h2-suite.sh run --category all \
    --only '^org\.h2\.test\.store\.TestBenchmark\$' --max-heap 1g --class-to 900 --tag probe
```

Useful switches while working on this area: `CRATONVM_GC_STATS=1` prints
`zgc-features` (`compaction_cycles`, `relocation_skipped_jit`) and `zgc-frag`;
`CRATONVM_DBG=gc-overhead` prints the productivity/streak numbers that show
whether the GC-overhead limiter is involved — here it was not
(`unproductive=false streak=0` on every cycle, so the limiter never fired, and
`CRATONVM_GC_OVERHEAD_LIMIT=0` did not change the outcome).
