# netty `io.netty.buffer` throughput gap — it is per-call cost, not the Unsafe path

**Status:** OPEN (gap is real and unfixed). **Root cause RE-MEASURED 2026-08-12**
on the Azure Linux host (`20.80.105.49`), dev `6d1bfd531`, quiet box.

The previous revision of this doc named the `sun.misc.Unsafe` bulk-access
natives and told the next reader to census them. **That is refuted below**, by
the census it asked for. Both of its central claims fail:

* its A/B compared a 126-passing run against a **17-passing** run and read the
  time difference as a speedup;
* `AdaptivePoolingAllocator` is not ~9× worse than the baseline. It is the same
  cost per call; its test class simply makes **23× more calls per test**.

The measurements that replace it are below. Nothing here is a guess: every
number is from an in-tree instrument or `perf`, and the two that disagreed with
a hypothesis of mine killed it.

## 1. What the cost actually is

`CRATONVM_DBG=jit-scan-prof` reports `jit_entries` — transfers of control into
compiled code, counted in `conservative_roots::push_entry_full`. Its own doc
comment says this is "the number that decides whether compiling short methods an
interpreted caller invokes is what makes the JIT a net negative on call-dense
classes". It had never been run. It has now:

| class | tests | CratonVM | `jit_entries` | entries/test | wall ÷ entries |
| --- | --- | --- | --- | --- | --- |
| `BigEndianHeapByteBufTest` | 414 | 35.3 s | 116,816,944 | 282 k | 302 ns |
| `AdaptiveBigEndianHeapByteBufTest` | 415 | 62.3 s | 120,381,490 | 290 k | 517 ns |
| `AdaptiveByteBufAllocatorTest` | 127 | 551 s | **826,764,658** | **6.51 M** | 667 ns |

**Wall time tracks call volume.** The allocator test is slow because it executes
826 million calls — 23× the per-test call density of the baseline class — and
CratonVM pays a few hundred nanoseconds on every one of them. HotSpot inlines
the same calls to roughly nothing, which is why its column barely moves
(~12 ms/test on *both* classes; HotSpot's time here is JUnit overhead, not
buffer work).

Note the middle row: `AdaptiveBigEndianHeapByteBufTest` makes essentially the
**same number of entries** as the non-adaptive baseline (120.4 M vs 116.8 M).
If `AdaptivePoolingAllocator` carried its own ~9× penalty, that row could not
exist.

## 2. Where the per-call cost goes

`perf record -F 499` (flat, no call graph — the DWARF unwind produced a 1.3 GB
file that resolved nothing), 120 s of `AdaptiveByteBufAllocatorTest`:

| share | family |
| --- | --- |
| ~15% | JIT entry/exit bookkeeping — `push_entry_full` 3.5, `pin_jit_code_range_owner` 3.1, `validate_code_ptr` 1.5, `gc_quiescence::leave` 1.4, `pop_jit_entry` 1.4, `jit_execution_leave` 1.2, `jit_activation::enter` 1.1, `record_transition` 1.0 |
| ~14% | the dispatch around it — `jit_invoke_dispatch` 2.7, `jit_invoke_virtual_mic` 2.5+1.3, `try_jit_site_cached_native_dispatch` 2.1, `forward_jit_reference_args` 1.5, `JitCache::get` 1.1, `try_call_compiled_entry_reentrant` 1.1, … |
| ~10% | the native-registry probe — `slot_for_exact` 3.8, `__memcmp_evex_movbe` 3.1, `safe_native_call_impl` 1.6, `slot_index_for_key` 0.8, `force_native_over_real_jdk_bytecode` 0.6 |
| 3.3% | `execute_frame_from_index` — the interpreter itself |

So ~30% of CPU is call-transfer machinery: ~200 ns of pure bookkeeping per
entry, 826 M times. The interpreter executing bytecode is 3.3%.

The registry probe is a known, already-optimized site: `native_class_hash`'s own
comment records it at **8.75%** of a live H2 profile and explains the class-name
prefilter added for it. It is on the every-invoke path via `invoke_or_native`,
so it is a per-call tax on ordinary bytecode calls, not only on native ones.

## 3. What was refuted

### 3.1 The Unsafe bulk-access path — refuted by the census

`--dump-native-registry` over the same run, default arm:

```
distinct natives invoked: 838     total invocations: 17,689,213
of which sun.misc/jdk.internal.misc.Unsafe:  54 distinct,  15,281 invocations
```

**0.086% of native invocations.** At the tree's own measured ~120 ns per native
call that is under 2 ms of a 551 s run. The Unsafe natives cannot be the cost,
and no `Unsafe` entry appears anywhere near the top of the census:

```
3,007,383  java/util/concurrent/atomic/AtomicIntegerArray.lazySet(II)V
2,821,339  java/lang/Thread.currentThread()Ljava/lang/Thread;
2,541,499  jdk/jfr/Event.isEnabled()Z
1,410,898  java/util/concurrent/atomic/AtomicReference.get()Ljava/lang/Object;
1,261,881  java/lang/Math.min(II)I
1,231,668  java/util/ArrayList.add(Ljava/lang/Object;)Z
1,228,626  java/util/ArrayList$Itr.hasNext()Z
1,220,219  java/util/ArrayList$Itr.next()Ljava/lang/Object;
1,152,526  java/util/concurrent/atomic/AtomicIntegerArray.get(I)I
```

These are CratonVM shims standing in for ordinary JDK bytecode. `Math.min(II)I`
as a native call is one machine instruction turned into a registry lookup plus a
call transfer.

*(The 2.5 M `jdk/jfr/Event.isEnabled` calls were checked separately in case
CratonVM was answering `true` and making netty build JFR events HotSpot skips.
It is not — `native-builtins/src/jfr.rs:356` returns
`jfr_java_recording_active()`, which is false with no recording. Correct
behaviour, 2.5 M native calls anyway.)*

### 3.2 The old A/B was confounded

`-Dio.netty.noUnsafe=true` was reported as "7× faster, same 127 test methods".
The two arms do not do the same work:

| arm | wall | ok | failed | native invocations |
| --- | --- | --- | --- | --- |
| default | 450 s | **126** | 0 | 17,689,213 |
| `-Dio.netty.noUnsafe=true` | 76 s | **17** | 109 | 20,440,370 |

109 of 127 tests fail early on `MemorySegment.asByteBuffer()`, so arm B exits
most of the work rather than doing it faster — and it issues *more* native calls
than arm A while taking a sixth of the time. The arm is a fine
`MemorySegment` reproducer (see
memorysegment-asbytebuffer-unimplemented (retired: `memorysegment-asbytebuffer-unimplemented-20260812`))
and worthless as a throughput instrument.

### 3.3 My own first hypothesis — also refuted, by its own flag

`jit_method_calls_native_shadowed` seals a method out of the JIT if its bytecode
calls a native-shadowed target, and its comment records that on a Spring Boot
startup this seals more methods than reach C2. netty's allocator calls
`ArrayList.add` / `Math.min` / `AtomicIntegerArray.get` — all shadowed — so
"the hot methods can never be compiled" looked obvious. It is measurable in one
binary, so it was measured (interleaved, same box, same hour):

| arm | wall | methods sealed |
| --- | --- | --- |
| A default | 594 s | 1117 (`calls-native-shadowed-method`=632) |
| B `CRATONVM_JIT=-native-shadow-interface-blind` | 493 s | 1056 (shadowed=564) |
| C `CRATONVM_JIT=-native-shadow-caller-seal` (whole seal off) | **591 s** | 675 (shadowed=**6**) |

Turning the entire seal off compiles 626 more methods and changes the wall clock
by **0.5%**. The seal is not the wall. (Arm B's 493 s is within this box's load
drift — load averaged 5.9 to 16.7 across these runs; see §5.)

`--nojit` was run as the other bound and is *slower* (>660 s, killed). Compiled
code is helping; there is simply not enough of the run inside it.

Which arm fires, for whoever does make the seal precise later:
`direct=474 interface-blind=97 inherited=60`. The class-blind arm the comment
worries about is 15% of the population, not the bulk.

## 4. Two in-tree open questions, now answered

Both are questions the source asks and nothing had run:

* **`push_entry_full`'s per-entry cost.** ~200 ns of bookkeeping (§2), 826 M
  times on this class. So yes — on call-dense code the entry machinery is a
  first-order cost, and it is paid per transfer, not per compiled method.
* **Does the conservative-root scan cache ever hit?** `note_jit_boundary()` is
  called from `push_entry_full`, so the boundary generation is bumped on every
  entry. Measured `cache_hits=0 (0.0%)` in **every** run above — 1,022 scans on
  the allocator class, 41,894 on `BigEndianHeapByteBufTest`, 60,163 on the
  adaptive heap class. The cache never hits, in any of them. It is not the cost
  here (`band_words=0` — no band scanning fires at all on these classes), but
  the answer is 0%, not "small".

## 5. Measurement hygiene

This box runs many concurrent agents; load averaged **1.0 to 16.7** across these
runs and the CratonVM column moves with it — the same class measured 450 s,
551 s, 559 s and 594 s on the same binary. Ratios here are order-of-magnitude.
HotSpot's column is stable only because its absolute times are seconds. Anything
that needs better than ±20% must be ABBA-interleaved and repeated; the seal A/B
above was interleaved for exactly that reason, and its B arm is still not
trustworthy at 17%.

## 5a. One defect found and FIXED out of this — the gate-pass memo

Following lever 2 with a frame-pointer build (the DWARF profile in §2 resolved
no callers; `-C force-frame-pointers=yes` does) attributed `slot_for_exact`
exactly: **4.11% total, 2.15% of it reached through
`jit_method_calls_native_shadowed`** — the static JIT-eligibility gate in
`execute()`, not the dispatch path.

That gate is memoized on one side only. `jit_skip_set` records methods that
**fail** it — a 2026-07-15 fix whose own comment explains that without it "every
future call to `execute()` for it recomputed all three from scratch (worst case,
`jit_method_calls_native_shadowed`'s full O(bytecode-size) decode-and-scan)
forever". The mirror case was left open: a method that **passes** the gate was
recorded nowhere, so `already_skipped` could never fire for it and the whole
gate re-ran on every `execute()` entry — and it runs *before* the `JitCache`
consult below it, so a fully compiled, hot method paid it too.

Measured, and this is the load-independent part:

| class | gate evaluations before | after | ratio |
| --- | --- | --- | --- |
| `AdaptiveByteBufAllocatorTest` | 1,432,835 | **1,858** | 771× |
| `BigEndianHeapByteBufTest` | 264,235 | **2,319** | 114× |

`fills` is bounded by the number of distinct eligible methods, which is what a
per-method gate should cost. Everything above it was re-computation.

**Fix:** `JitRealm::jit_gate_pass`, the positive half of `jit_skip_set`,
stamped with `cratonvm_jit::redefine_epoch()`. The stamp is load-bearing in a
direction the negative set does not need: a stale *seal* only costs throughput
(the method stays interpreted), but a stale *pass* would let a redefined body —
whose new bytecode may call a native-shadowed target — reach the compiler, which
is precisely what the seal exists to prevent. `bump_redefine_epoch()` already
runs on every `redefineClass`, beside the `clear_all()` that evicts compiled
artifacts, so an older stamp is simply a miss.

**Worth, in CPU:** 20 ABBA-interleaved runs per arm in one binary
(`CRATONVM_JIT=-gate-pass-memo` is the off switch), `BigEndianHeapByteBufTest`,
`perf stat` task-clock because this box's wall clock is useless (§5) and its
cloud PMU reports `instructions: <not supported>`:

| arm | n | mean CPU | median | min | max |
| --- | --- | --- | --- | --- | --- |
| memo ON | 20 | **55.42 s** | 55.28 | 54.05 | 59.65 |
| memo OFF | 20 | 56.48 s | 56.05 | 54.87 | 62.56 |

**~1.9% less CPU.** Small, and honestly so: 771× fewer gate evaluations buys
2%, which is itself the §1 result restated — the run is dominated by the
*number of calls*, and no single per-call site is a large share of it. It is
recorded here as a fixed defect, not as a fix for this doc's gap.

## 6. What would actually move it

Not an `AdaptivePoolingAllocator` fix — there is no allocator-specific defect to
find. Two general levers, in order of measured size:

1. **The per-entry cost of `push_entry_full`/`pop_jit_entry`** (~15% here).
   Each entry does a boundary-generation bump, a TLS `RefCell` push, a striped
   global depth counter, a thread-state transition record, `jit_execution_enter`
   and `gc_quiescence::enter` — then the mirror image on the way out. Most of it
   exists so the GC can walk or defer around compiled frames.
2. **The native-registry probe on every invoke** (~10% here, 8.75% on H2).
   It hashes class+method+descriptor byte-at-a-time and then memcmps all three
   to verify. A `ClassId`-keyed "does this class register any native at all"
   test would replace the hash for the overwhelming majority of invokes, which
   miss; the call sites currently have only `&str`. **Partly addressed** — the
   2.15% of it that came from the eligibility gate is gone (§5a); the remaining
   ~2% is on the genuine dispatch path, where `NativeCallSite` already memoizes
   the sites that hold one.

A third thing this investigation did NOT find, and which the numbers rule out:
any single hot site worth more than a few percent. §1's arithmetic is the whole
answer — 826 M calls at a few hundred ns each. Anyone hoping to close this gap
should be sizing lever 1, not hunting for another §5a.

Both are VM-wide, not netty-specific, and both have prior optimization passes in
their comments — treat this doc as evidence for sizing that work, not as a netty
bug.

## 7. Repro

```bash
cd apps/netty-suite-runner
CLS=io.netty.buffer.AdaptiveByteBufAllocatorTest

# the cost, per class: jit_entries is the number that matters
CRATONVM_DBG=jit-scan-prof /usr/bin/time -f "%e s" <cratonvm> \
    --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner $CLS

# what is actually being called (NOT the Unsafe natives)
<cratonvm> --java-home <jdk25> --Xmx 1500m --dump-native-registry census.json \
    @common.args -Dcraton.batch=1 CratonRunner $CLS

# where the CPU goes — flat, not DWARF
perf record -F 499 -o p.data -- timeout 120 <cratonvm> … CratonRunner $CLS
perf report -i p.data --stdio --no-children -g none --percent-limit 0.4

# HotSpot oracle
CP=$(sed -n 2p common.args)
/usr/bin/time -f "%e s" <jdk25>/bin/java -cp "$CP:." \
    -Djunit.jupiter.execution.timeout.default=120s -Dcraton.batch=1 CratonRunner $CLS
```

## 8. Unchanged from the previous revision

**None of these classes hang.** Every one completes and passes when given room;
they are recorded as HANG only because they cross the suite's wall cap. The
three per-test JUnit 120 s timeouts
(`AdaptiveByteBufAllocatorTest.purgeScanShouldEvictIdleChunks`, the
`UseCacheForNonEventLoopThreadsTest` equivalent, and
`AdaptiveBigEndianDirectByteBufTest.testInternalNioBuffer`, which reads 64 MiB
one `ByteBuffer.get()` at a time) have the same cause and pass in a solo run.

This doc supersedes the one-sample "~12x" estimate in
`../../internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md`.
