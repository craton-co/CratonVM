# H2 three-GC-variant sweep — the shared-fault-site SIGSEGV was not a GC bug at all; FIXED

**Status:** **FIXED / RETIRED (2026-08-10).** The page this replaces was opened
the same day off a 218-class × 3-collector H2 sweep and reported "1 shared
SIGSEGV site, 1 GC-specific corruption guard, 55-57 non-passing classes per
variant". Everything it reported has now been reproduced, root-caused and either
fixed or attributed to an existing page.

Headline: **51 of the 53 SIGSEGVs in that sweep were not a garbage-collector
defect.** They were one NIO native reading a `java.nio.Buffer.address` — an
array-relative `Unsafe` offset — as if it were a process pointer. The original
page read them as GC-family because every non-passing class also carried GC
guard output; that was the background, not the signal.

## Result

The original sweep's non-passing union (65 classes as the runner's `--only`
regex resolves it), rerun under all three collectors from one `--features zgc`
binary selecting the collector by runtime flag, plus a stock-HotSpot-25 control
over the same list, same host, same classpath, 300 s per class, `--Xmx 1g`.

(The runs predate `dev`'s later flip to ZGC-as-default, so the "default" arm
below was the **Generational** collector — it took no flag, the other two took
`-XX:+UseG1GC` / `-XX:+UseZGC`. On current `dev` that arm needs
`-XX:+UseGenerationalGC` explicitly.)

| variant | CRASH before | CRASH after | PASS before | PASS after |
|---|---:|---:|---:|---:|
| default (Generational) | 18 | **0** | 2 | **19** |
| G1 | 20 | **4** | 2 | **19** |
| ZGC | 18 | **0** | 2 | **19** |

* **Zero `addr=0x10` faults remain** in any variant.
* The 4 that remain are G1-only and all fault at `addr=0x20084400000` — a
  1 MiB-aligned **region base**, i.e. the separate, already-OPEN G1 evacuation
  defect. Symbolized and written up on its own page (see Related).
* The end-of-sweep `LIVE_IN_DEAD_SPANS` guard the old page's §2 was about now
  reports **0 hits across all three variants** (was: 8 classes, repeatedly, on
  the same victim address).

Re-verified twice more after merging current `dev` in (including its flip to
ZGC-as-default): 8 of the previously-crashing classes under all three
collectors, **CRASH=0**, zero `addr=0x10` faults, zero precise-deopt
`InternalError`s. `cargo test -p cratonvm-native-builtins --lib` 3396 passed,
`cargo test -p cratonvm-gc` 1461 + 63 passed across every target.

One caveat worth stating rather than hiding: in the second of those smokes
`TestIndex` came back HANG on all three arms, which looked like a merge
regression. It was not — that smoke shared the host with two `cargo test` runs.
A/B'd sequentially afterwards on the same host, pre-merge binary vs post-merge
binary, generational collector:

```
post-merge  PASS  151.5 s        pre-merge  PASS  157.7 s
post-merge  PASS  139.8 s
```

The lesson generalises to every HANG figure in this document: at a 300 s cap
with classes that legitimately take 150-340 s, **host load decides the verdict**.
Read the HANG counts as "did not finish inside the cap on a loaded 8-core box",
not as "hung".

## 1. The SIGSEGV — a real-JDK `ByteBufferAs<T>Buffer` view read `address` as a pointer

The old page had the fault site right and the family wrong. Its own evidence
said so: `addr=0x10` is exactly `ARRAY_BYTE_BASE_OFFSET`, and the low PC bits
were identical across ASLR'd launches because the fault is inside `libc`.

Resolved end to end:

```
libc offset 0x1a152a  ->  __memcpy_avx512_unaligned_erms + 106
                          `mov (%rsi),%cl`  with %rsi = 0x10, length 1
```

(`__nss_database_lookup` is merely the nearest exported symbol; the
disassembly is the small-size tail of `memmove`.) Under `gdb`, one backtrace
named the caller in full:

```
#2 copy_from_native_memory ()  at vm/src/vm/vm_exec.rs:14768
#3 s2_bb_get_byte ()           at native-builtins/src/servlet.rs:3432
#4 s2_bb_read8 ()              at native-builtins/src/servlet.rs:3546
#5 s2_lb_get_bulk ()           at native-builtins/src/servlet.rs:6087
```

### Mechanism

`ByteBuffer.as<T>Buffer()` is **not** in `force_native_over_real_jdk_bytecode`,
so against a real JDK it runs the JDK's own bytecode and returns a genuine
`java/nio/ByteBufferAsLongBufferB`. Its bulk `get(long[],int,int)` **is**
forced — that accessor is declared on the abstract `java/nio/LongBuffer`, which
the view class does not override — so a real view reaches `s2_lb_get_bulk`.

On such a receiver:

* `s2_bb_arr` finds no array. The view's own `hb` is null (storage lives on the
  backing `bb` ByteBuffer), indexed slot 0 is `Buffer.mark`, and slot 5 is
  `Buffer.segment`, null for a heap buffer.
* `s2_bb_direct_addr` then reads `Buffer.address`. For a view over a **heap**
  ByteBuffer that value is `ARRAY_BYTE_BASE_OFFSET + byteIndex` — an `Unsafe`
  offset, not a pointer. For a fresh buffer it is exactly **16**.
* `copy_from_native_memory(0x10, 1)` dereferences it. SIGSEGV.

`org.h2.mvstore.Chunk.readToC` is `buff.asLongBuffer().get(toc)` — one line, on
every MVStore chunk read. That is why 16-18 classes per variant crashed with a
byte-identical fault, under every collector, and why it looked collector-shaped
when it was collector-independent.

### Fix (`native-builtins/src/servlet.rs`)

* `s2_bb_view_backing` resolves `(array, byte index of element 0)` from the
  view's `bb` + `address`, mirroring `bbacb_read_underlying_bytes`
  (native-builtins/src/lib.rs), which already did exactly this for the
  `ByteBufferAsCharBuffer{B,L}` family.
* `s2_bb_heap_window` is the single resolution point; `s2_bb_get_byte`,
  `s2_bb_put_byte`, `s2_bb_storage` and the typed-view `slice`/`slice(II)`/
  `duplicate` paths all go through it, so a derived view carries the real
  backing window instead of no storage at all.
* `s2_bb_direct_addr` declines any `address` below the first mappable page
  (64 KiB — Linux `vm.mmap_min_addr`, and Windows reserves the low 64 KiB).
  A backstop, not the contract: a wild pointer is unrecoverable, "no storage"
  degrades to this module's existing benign zero.
* `s2_bb_order` takes endianness from the view's class name (`…BufferL` vs
  `…BufferB`), which is where the JDK keeps it. The old `mark`-slot fallback
  answered BIG_ENDIAN for every such view and would byteswap every read through
  a little-endian one — a silent-wrong-data bug on the same path.

Three regression tests pin the contract (`servlet.rs`, `real_typed_view_*`):
a heap-backed view is not direct, it reads **and writes** through its backing
array at `address - ARRAY_BYTE_BASE_OFFSET` (a view is not a copy), and its
endianness comes from its class name. All three fail against the pre-fix
helper. `cargo test -p cratonvm-native-builtins --lib`: 3396 passed, 0 failed.

## 2. The young non-moving sweep guard — caught at the header now, not at the merge

The old page's §2 reported the end-of-sweep guard firing on 8 classes with
`spans=1 span_bytes=197216 span_head_class_id=4044482304`: one ~192 KB
"object" subsuming live ones, the same victim address milliseconds apart, and
all 8 classes then blowing the 300 s cap.

That guard is sound but **late**. It compares the reclaim set against the mark
set after the walk, retains the offending span, and unwinds — by which point
every decision from the phantom to the end of the arena was taken on a grid that
never resynchronised, so the cycle reclaims almost nothing.

Live objects never nest, so a marked base strictly inside a header's claimed
extent proves the walk left the object grid **at that header**. That is now
checked there (`gc/src/gen_heap.rs`), with the same recovery the
unlisted-all-zero-span path already used: unwind the decisions since the last
anchor and re-anchor at the next allocator-recorded object start. Cost is one
monotone index advance per object over a list the mark phase already built and
sorted. The parallel chunk walker gets the same test and abandons its attempt,
handing report and recovery to the sequential walk as it does for every other
grid anomaly.

Measured over the same 65 classes × 3 collectors:

| | before | after |
|---|---:|---:|
| end-of-sweep `LIVE_IN_DEAD_SPANS` reports | 8 classes, repeated | **0** |
| walk-time phantom detections | (no such check) | 69, across 10 classes |

So the desync itself is **not fixed** — it still happens 69 times — but it is
now contained at the header instead of corrupting the rest of the walk, and it
reports far sharper evidence. The 192 KB phantom is gone; what the detector
catches instead is small and specific, e.g.

```
offset=237907032 span_bytes=272 span_head_class_id=1026 kind_byte=0
num_slots=16 victim=0x200506e2c98 victim_interior_offset=64
```

The report also now carries `last_anchor_off`, `objects_since_anchor` and the
previous object's `off/size/class_id/kind`, which turns "the grid broke
somewhere" into a decidable question: if `prev_off + prev_size != offset` the
previous stride IS the break, and `objects_since_anchor` bounds how far back to
look otherwise. **That is the next step and it is left open** — see the residual
section.

## 3. `TestScript` / `TestCrashAPI` — the two CRASHes with exit code 1

The old page flagged these as "worth checking separately before assuming
they're the same libc-fault mechanism". They are not the same mechanism. Both
died on a fatal `InternalError` from the JIT's first-call tier-up sink:

```
precise deoptimization unavailable for
org/h2/expression/function/StringFunction1.getValue(...) at bci 54
(the stashed frame belongs to a different method, stashed key
"org/h2/util/StringUtils.cache:(Ljava/lang/String;)Ljava/lang/String;",
inline callers 0, reason UnreachedCode); refusing side-effecting replay
```

`CRATONVM_DBG_DEOPT=1` named the producer: a deopt inside an **inlined** callee
stashes a frame keyed to the inlinee with an empty caller chain, so no call site
on the dispatch path can claim it and it is orphaned in the thread-local. The
sink then took someone else's frame and killed the VM over it.

Fixed narrowly, by aligning the outlier with its own sibling: when the stash is
foreign, de-speculate the frame's real owner, drop the orphan, and let this
(innocent) method fall through to interpreted execution — exactly what
`jit-callsite-b` (`vm/src/runtime/interpreter/jit_bridge.rs`) already did for
the same case.

**Both classes go CRASH → HANG, not CRASH → PASS.** They stop killing the VM and
then run out the 300 s cap — `TestScript` was dying at ~212 s, so surviving the
orphan buys it more work, not a pass. That residual is the throughput programme
in §4, not this defect. **The orphan-producing defect is upstream and still
open**; it has its own page (see Related).

## 4. The FAILs — a HotSpot control settles most of them

The old page left 15-22 FAILs per variant "not individually triaged". They are
now, against a stock HotSpot 25 control over the same class list.

**HotSpot fails or hangs on 21 of them**, so they are H2-suite / environment
issues, not CratonVM defects: `TestFunctions`, `TestLob`, `TestOutOfMemory`,
`TestSubqueryPerformanceOnLazyExecutionMode`, `TestCachedQueryResults`,
`TestRecoverKillLoop`, `TestWeb`, `TestMVStore`, `TestJoin`, `TestTimer`,
`TestMulti`, `TestClassLoaderLeak`, `TestExit`, `TestMemoryUnmapper`,
`TestTools`, plus the HANGs `TestBenchmark`, `TestKill`, `TestMultiThreaded`,
`TestPowerOffFs`, `TestPowerOffFs2`, `TestSynth`. That control is also what
closes the five-class 2026-08-07 page — see
`bug-h2-suite-fail-cluster-not-cratonvm-bugs-20260807-NOT-A-BUG.md` in this
folder, which now carries the table.

**Load caveat, stated rather than buried:** that control shared an 8-core host
with three concurrent CratonVM suite runs (load ~20). For the exception-shaped
rows that changes nothing. For the timing-shaped ones —
`TestSubqueryPerformanceOnLazyExecutionMode`, `TestMVStore`'s cache-ratio
assertion, and the six HANGs — it might, and they should be re-run idle before
being quoted as evidence.

The FAILs common to all three collectors that HotSpot **passes** are the real
CratonVM residue, and all three are throughput margins against a hardcoded
millisecond budget in the fixture, not discrete defects:

### `TestTransaction.testMergeUsing` — measured

Deterministic: 3/3 fail under CratonVM, 3/3 pass under HotSpot, and identical
with `--nojit`, so it is not a JIT miscompile. A fixture-faithful probe
(`testMergeUsing` verbatim, with the `catch (SQLException e) { // Ignore }`
replaced by a print) names it in 1.25 s:

```
org.h2.jdbc.JdbcSQLTimeoutException: Timeout trying to lock table "TEST"
```

The test's own `TestAll.lockTimeout = 50` ms is the budget. Two connections
each run a 50-statement `MERGE` batch; whichever loses the race must acquire
the table lock within 50 ms.

| `LOCK_TIMEOUT` | HotSpot batch | CratonVM batch | result |
|---|---|---|---|
| 50 ms (the fixture default) | 13 / 17 ms | 146 ms | **CratonVM FAILs**, other batch times out |
| 500 ms | 26 / 44 ms | 128 / 290 ms | both pass |
| 5000 ms | 28 / 48 ms | 207 / 2556 ms | both pass |

A ~6-10x batch-throughput gap against a 50 ms budget. Same family as
`TestBnf`/`TestWeb` against H2's hardcoded 100 ms
`Sentence.MAX_PROCESSING_TIME` — already root-caused and still open in
`docs/known-issues/h2/!bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`,
which is the interpreter-throughput programme, not a scoped patch.

### `TestLargeBlob` — also a throughput margin, and the suite cap is what it misses

In the suite it fails with `OutOfMemoryError: Capacity: 23887872` rethrown by
`org.h2.mvstore.WriteBuffer.grow` — i.e. `ByteBuffer.allocate(~22.8 MiB)`
failed on a `--Xmx 1g` heap. Distinct from the direct-memory OOM fixed in
`bug-h2-largeblob-direct-memory-oom.md` (that one was `MaxDirectMemorySize`;
this is the Java heap).

But run standalone at **the same `--Xmx 1g`** it does not OOM at all — it
**passes**:

| | result | wall |
|---|---|---|
| HotSpot 25, `-Xmx1g` | PASS | **9.65 s** |
| CratonVM, `--Xmx 1g` | PASS | **320 s** |
| CratonVM, `--Xmx 2g` | PASS | 338 s |

A ~33x gap, and 320 s is past the suite's own 300 s per-class cap, so this
class cannot pass under the suite as configured no matter what the heap does.
The in-suite OOM is therefore a *secondary* effect of running that long under
concurrent load (three VMs on the host), not an allocation defect of its own —
which also explains why the same class read as HANG under the default collector
and FAIL under G1/ZGC in the same sweep. Same programme as `TestTransaction`:
throughput, not correctness.

## Residuals — what is deliberately still open

1. **Why the young non-moving sweep's object grid desyncs at all.** Contained,
   not fixed; 69 detections. The enriched report is landed for whoever picks it
   up. Related in family to the G1 page's "a full, never-recycled Eden region is
   not walkable".
2. **The orphaned deopt frame** — CLOSED 2026-08-10, and it was not inlining:
   a statically-bound JIT-to-JIT direct call carried no `JitInvokeInfo`, so no
   callee-deopt service check was emitted and the callee's stash propagated
   past the only site that could attribute it. See
   `../jit-direct-call-mints-an-orphaned-deopt-frame-20260810-FIXED.md`.
3. **The G1 region-aligned evacuation SIGSEGV** — 4 classes, G1 only. Findings
   added to `docs/known-issues/hibernate/g1-collector-fullsuite-crashes-hangs-fails-20260806.md`,
   including a cheaper H2 repro than the Hibernate one it had.
4. **Interpreter throughput against hardcoded fixture budgets** —
   `TestTransaction`, `TestBnf`, `TestWeb`. Measured above; the fix is the
   throughput programme.
5. **The HANG cluster** — unchanged and separately tracked in
   `docs/known-issues/h2/bug-h2-hang-cluster-lirs-trace-mvstore-compact-20260807.md`.
   HotSpot hangs on six of the same classes on this host, so part of that
   cluster may be fixture/host rather than VM; needs an idle-host control.

## Reproducing the verification

```bash
cd apps/h2database-suite-runner
ONLY='<the non-passing union, | -separated>'
for v in default g1 zgc; do
  JDK25=/data/toolchain/jdk-25 OUTROOT=/tmp/out CRATONVM_BIN=<cv-$v> \
    ./run-h2-suite.sh run --category all --only "$ONLY" \
      --tag fix2-$v --class-to 300 --max-heap 1g &
done
JDK25=/data/toolchain/jdk-25 OUTROOT=/tmp/out \
  ./run-h2-suite.sh hotspot --category all --only "$ONLY" \
    --tag ctrl --class-to 300 --max-heap 1g
```

## Related

- `bug-h2-suite-fail-cluster-not-cratonvm-bugs-20260807-NOT-A-BUG.md` (this
  folder) — the HotSpot control, now confirmed, for the FAIL cluster.
- `../../../known-issues/hibernate/g1-collector-fullsuite-crashes-hangs-fails-20260806.md`
  — the G1 region-aligned SIGSEGV, still OPEN, updated with this sweep's data.
- `../jit-direct-call-mints-an-orphaned-deopt-frame-20260810-FIXED.md`
  — the orphaned-deopt-frame residual, now closed (and re-titled: the page this
  sweep spawned blamed inlining, which was not the mechanism).
- `../s2-bytebuffer-natives-real-jdk-direct-buffer-gaps-FIXED.md` and
  `../nio-buffer-address-indexed-slot-aliasing-FIXED.md` — the two previous
  passes over this same `s2_bb_*` storage-resolution surface. This one closes
  the case they both stopped short of: a view whose storage is on *another*
  buffer.
