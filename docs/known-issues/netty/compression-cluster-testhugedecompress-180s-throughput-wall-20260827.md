# `codec.compression.*IntegrationTest` — four defects fixed, three classes PASS, four are still over, and the rest of the wall is PRICED

## Status

**OPEN.** Every one of the eleven used to HANG at the suite's 180 s cap. Three
now PASS outright, four more COMPLETE inside the cap and fail only netty's own
120 s per-method timeout, and four are still over.

FOUR defects have been found and fixed — the fourth, `checkcast`, was named on
this page as a priced lever before it was one. What is left after it is not a
fifth: this page now carries a MEASUREMENT of the ceiling rather than the
description of it that earlier revisions carried, plus the price of every
remaining lever, and none of them closes a class.

## What the eleven share

All eleven inherit `AbstractIntegrationTest.testHugeDecompress`, which builds a
256 MiB buffer one byte at a time and pushes it through the class's codec:

```java
in.writeByte(byteValue);      // 268,435,456 times
digest.update(byteValue);     // 268,435,456 times, and again on the decompress side
```

## Defect 1 — 1.07 billion generic native dispatches

A `--dump-native-registry` census of `JdkZlibIntegrationTest`, which the earlier
investigation never took, named it exactly:

```
536 870 912  java/security/MessageDigest.update(B)V
269 768 030  java/lang/invoke/VarHandle.get(...)        <- ensureAccessible -> RefCnt
268 435 456  java/nio/DirectByteBuffer.put(IB)...       <- PooledDirectByteBuf._setByte
```

`perf` put the funnel and its receiver validation at ~45 % of the run. All three
have a thin direct bind that skips it, and FOUR mechanisms were each
independently keeping those binds away from these sites.

| | before | after | HotSpot |
|---|---:|---:|---:|
| `MessageDigest.update(byte)` | 216 ns | **25 ns** | 8.6 ns |
| `ByteBuf.writeByte` | 700 ns | **297 ns** | 5.1 ns |
| `ByteBuf.forEachByte` | 349 ns | **122 ns** | 5.9 ns |

Same-binary A/B on `JdkZlibIntegrationTest`, B arm
`CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS=0 CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER=0`:
**A 149/150/151 s against B 368/227 s**.

## Defect 2 — one `iinc_w` made a whole method permanently uncompilable

`FastLz.compress` is 1617 bytes whose entire job is one loop, so the
method-entry door never sees it hot and OSR is its only route to compiled code.
It contains three `iinc_w` instructions and `jit_scan` had no arm for the `wide`
prefix. Its catch-all `None` calls `mark_jit_bail_listed`, which bans the method
from EVERY compile door for the life of the process. 256 MiB of compression ran
in the **interpreter**.

    FastLz encode, 1 MiB, steady state:   5115 ms  ->  844 ms   (6.1x)

## Defect 3 — an `invokevirtual` to a `final` method was still a virtual dispatch

`CRATONVM_DBG_JITC=1` on a one-megabyte fastlz round trip reported
`ir-direct-call MISSED` **612 times** — the single most frequent line in the
log. One `ByteBuf.getByte(int)` on a pooled buffer is NINE of them:

```
FastLz.readU16          -> ByteBuf.getUnsignedByte      MISSED
  AbstractByteBuf.getUnsignedByte -> getByte            MISSED
    AbstractByteBuf.getByte       -> checkIndex         MISSED
                                  -> _getByte           MISSED
      AbstractByteBuf.checkIndex(int)     -> checkIndex(int,int)  MISSED
        AbstractByteBuf.checkIndex(int,int) -> ensureAccessible   MISSED
                                            -> checkIndex0        MISSED
          AbstractByteBuf.checkIndex0 -> capacity()               MISSED
      PooledHeapByteBuf._getByte -> idx(int)                      MISSED
```

Each `MISSED` reads `ir_direct=true static=false special=false`: the direct-bind
door is gated `is_static || is_special`, so an `invokevirtual` never reaches it.
But five of those nine — `checkIndex(int)`, `checkIndex(int,int)`, `checkIndex0`,
`ensureAccessible` and `PooledByteBuf.idx` — are declared **`final`**. A `final`
method has exactly one possible target at every site that names it, and unlike
class-hierarchy speculation it needs **no invalidation dependency**, because no
class that could ever be loaded may override it.

Fixed by `perf(jit): bind an invokevirtual whose target is final`.
`invoke::invokevirtual_site_final_owner` owns the rule; the three
`cp_invokespecial_owner_resolver` closures (one per compile door) call it, which
is why one change reaches both the single-pass and the optimizing tier.
`CRATONVM_JIT_FINAL_DEVIRT=0` reverts it.

**It engages, and it buys nothing.** The census prints
`JIT invokevirtual pinned non-virtual: private=438 final=438`, every one of the
five links leaves the `MISSED` list, `ir-direct-call MISSED` falls 612 -> 533 —
and fastlz encode does not move (arm A 578-953 ms, arm B 493-945 ms over eight
reps; within noise, and the host was not quiet).

**That negative result is the most useful thing on this page.** It retires the
hypothesis the previous revision was built on. The chain is not slow because the
calls are *dispatched*; it is slow because they are *calls*. A direct `CALL` to a
tiny method still pays the frame, the spill, the boundary note and the checked
work inside it. Only removing the call removes that.

## What the wall actually is, in numbers

netty gates both halves of the guard chain on its own system properties, so the
chain can be switched off in JAVA and the workload still runs. That makes it
measurable without a VM change — and it gives the OLD binary a shape it can run,
which an inlining fix could never be A/B'd against.

`-Dio.netty.buffer.checkAccessible=false -Dio.netty.buffer.checkBounds=false`,
fastlz, 1 MiB, steady state, ON arms bracketing the OFF arm:

| | encode | decode | CPU user (6 reps) |
|---|---:|---:|---:|
| CratonVM, checks ON | 463 / 501 ms | 305 / 310 ms | 5.12 / 5.41 s |
| CratonVM, checks OFF | **213 ms** | **141 ms** | **2.66 s** |
| HotSpot, checks ON | 8 ms | 5 ms | 1.47 s |
| HotSpot, checks OFF | 5 ms | 3 ms | 1.40 s |

**The guard chain is 54 % of CratonVM's fastlz time and about 5 % of
HotSpot's.** That is the wall, stated as a number rather than as a description.
It is also very close to the whole remaining gap: the four classes need 2.4-4.1x
and the chain is worth 2.2x.

## Every remaining lever, priced

Measured on the same 1 MiB fastlz round trip (768 ms/MiB encode+decode).

| lever | share | status |
|---|---:|---|
| `checkcast` has no inline fast path | ~10 % | **FIXED** — see below, measured **1.21x** on this workload |
| the `RefCnt` `VarHandle` read in `ensureAccessible` | ~8 % | open — `jit_varhandle_read_direct::<4>` + `varhandle_instance_field_read_bits` + `direct_receiver_facts` |
| the `"index"` string literal in `checkIndex0` | ~6 % | open — `jit_ldc_string_cp` -> `MemberResolver::probe_constant` per byte, for a message never used |
| compiled `getfield` | **not a factor** | 1222 helper calls in 3 MiB — the compact inline path is working |

They sum to about 1.3x. **No combination of them reaches 2.4x**; the profile is
flat by construction, because the cost is one small fixed charge repeated nine
times per byte.

## Defect 4 — `checkcast` was an unconditional helper call, at 136x

`bytecode_walk.rs`'s `0xc0` arm emitted an **unconditional** call to
`jit_checkcast`: a `flush_scratch_registers`, an `emit_pre_safepoint_spill`, the
CALL, an `emit_oop_map_for_safepoint` and a post-invoke exception check. Inside
the helper, every call took a heap-membership walk (`ZObjectStarts::contains` ->
`ZgcRealHeap::is_object_address`) before it could read the header. There was no
inline class-id compare anywhere on the path — though the guarded-virtual inline
arm has emitted `CMP DWORD [recv+0], class_id` for months.

```
20,000,000 casts, before:  CratonVM 38.2 ns/iter   HotSpot 0.28 ns/iter   136x
```

Fixed by `perf(jit): inline fast path for checkcast`, in two shapes, each
proving its answer from the header alone:

* **plain object** — class-id equality. An object whose class id equals the
  target's is assignable to it, full stop. Subclasses, interfaces, lambda
  proxies and synthetics all have different ids and keep the helper, so this can
  only turn a slow YES into a fast YES.
* **1-D primitive array** — the `KIND_TAGS` byte. A class-id compare provably
  CANNOT serve `[B`: a primitive array carries `class_id == 0` and a reference
  array carries its COMPONENT's. The header's own `kind | (element_type << 2)`
  byte does, in one comparison — a `byte[]` is `0x21` and nothing else is. One
  dimension only, which is what makes it sound: `byte[][]` holds references, so
  its element type is `Reference`.

| same binary, kill switch the only difference | before | after | HotSpot |
|---|---:|---:|---:|
| plain-object cast | 20.04 ns | **1.55 ns** | 0.27 ns |
| `byte[]` cast | 38.11 ns | **1.35 ns** | 0.27 ns |
| netty fastlz, 1 MiB, CPU user, 3 interleaved pairs | 11.12 s | **9.16 s** | — |
| `membership walks by JIT site: checkcast=` | 12,002,214 | **276** | — |

**Two near-misses are the reusable part.**

`RJitArrayTypecheck` caught the first version reintroducing
BUG-JIT-ARRAY-INSTANCEOF-20260726 — an array's header does NOT hold its own
class id, so `checkcast java/lang/String` on a `String[]` receiver found
`String`'s id there, matched the baked target, and accepted a cast that must
throw. The vector written for that 2026-07-26 defect caught the same reasoning
returning by a different route, four weeks later.

And **three doors, again**. The first version patched only the single-pass
backend's `0xc0` arm and measured EXACTLY ZERO on a four-million-cast probe —
`checkcast=3,203,236` walks with it on against `3,203,316` with it off — because
every body that mattered had tiered up into the optimizing pipeline, whose
`Op::CheckCast` lowering is a separate emitter. Then the OSR door, the one a hot
LOOP reaches, turned out to be the last of the three still handing the runtime
helper a bare name with no `ClassId` recorded at all: `CcProbe2`, a
20-million-iteration loop whose body is one `(Node) o` cast, reported
`refused-no-target-id=2` and 63,989,000 unserved walks. Both are fixed. That is
the third time this page has recorded the shape — the thin native binds and the
`String` call-site intrinsics were the first two.

## Where the eleven stand

Solo, uncapped, one run each, merged `dev` 2026-08-28.
`regression-suite/run.sh` **72/72** on this binary.

**These rows predate defect 4.** They were taken on merged `dev` before the
inline `checkcast` fix, which is worth 1.21x on this workload's CPU time; no
quiet host has been available since to retake them, and a contended one cannot
(see immediately below). The three that PASS still pass — `JdkZlibIntegrationTest`
measured 87 s `ok=11` at load 5.1 on the fixed binary, against 94 s at load ~0
before it.

**Measure these on a QUIET host.** The same `FastLzIntegrationTest` binary
measured 322 s at load ~0 and 407 s at load ~6 — 26 %, which is larger than any
single lever below. A class time from a shared host is not evidence of anything.
The CPU-time A/B on `CodecProbe` is the instrument to use when the host is busy:
it separated "this change is neutral" (A 5.54/6.36/5.49 s user against B
7.06/5.44/5.33, interleaved) from the 26 % the wall clock was showing.

Contention does not merely blur these numbers, it **changes the verdict**.
`LengthAwareLzfIntegrationTest` sits just under netty's 120 s per-method
timeout, so load walks it across the line. One binary, one host, four runs:

| 1-min load | wall | verdict |
|---:|---:|---|
| 3.30 | 107 s | `ok=11 failed=0` |
| 4.56 | 115 s | `ok=11 failed=0` |
| ~6 | 143 s | `ok=10 failed=1` |
| 7.80 | 170 s | `ok=10 failed=1` |

Monotonic in the load, and it crosses netty's 120 s method timeout between the
second row and the third. Two of those runs would have been recorded as a
regression in whatever landed just before them. Check `uptime` AND
`ps aux --sort=-%cpu` — the competing load here was two other sessions running
their own `cratonvm` binaries, which a load average alone does not attribute —
before recording any row in the table below.

| class | wall | |
|---|---:|---|
| `JdkZlibIntegrationTest` | 94 s | **PASSES** `ok=11 failed=0` |
| `ZstdIntegrationTest` | 109 s | **PASSES** |
| `LengthAwareLzfIntegrationTest` | 110 s | **PASSES** |
| `JZlibIntegrationTest` | 142 s | completes |
| `BrotliIntegrationTest` | 142 s | completes |
| `Lz4FrameIntegrationTest` | 146 s | completes |
| `LzfIntegrationTest` | 148 s | completes |
| `FastLzIntegrationTest` | 322 s | over |
| `SnappyIntegrationTest` | 334 s | over |
| `SnappyJumboSizeIntegrationTest` | 408 s | over |
| `Bzip2IntegrationTest` | 489 s | over |

Against 455-528 s for `JdkZlibIntegrationTest` when this page was written.
"Completes" means the class finishes inside the harness's 180 s cap and reports
`ok=10 failed=1`: `TimeoutException: testHugeDecompress() timed out after 120
seconds`, which is netty's own per-method timeout, not the harness's.

## Not a bug to keep re-discovering

`TimeoutException: testHugeDecompress() timed out after 120 seconds` on one of
the seven that complete is the EXPECTED failure today. The number to watch is
the CLASS wall time; above ~250 s for one of the seven means something
regressed. **A HANG at the 180 s cap is new** — that was the old signature.

Four instruments earned their keep, each the only thing that could see its own
defect:

* `CRATONVM_DBG=jit-method-stats` prints bound-SITE and served/declined CALL
  counts for the thin helpers, the non-virtual `invokevirtual` census, and
  **`membership walks by JIT site`** — the line that named `checkcast` after the
  getfield hypothesis it also refuted (`getfield helper calls: 1222`).
* `[cratonvm-jitc] ir-direct-call MISSED …` names every call site the direct-bind
  door declined, and is what turned "the accessor chain is slow" into nine
  specific edges with a reason attached to each.
* `[cratonvm-jitc] OSR-compile FAILED … stage=` names which region of
  `compile_osr_artifact` refused. An OSR refusal is silent AND permanent.
* `[cratonvm-jitc] scan-bail op=0x…` names the byte `jit_scan` refused.

And one method note: **netty's own `checkAccessible`/`checkBounds` properties are
a free ablation of the thing this page is about.** Toggling them costs one run
and prices the whole guard chain, on a binary that does not have the fix yet.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.codec.compression.FastLzIntegrationTest
```

`TimedRunner` (same directory, same arguments) prints a per-TEST breakdown, which
is what separated `testHugeDecompress` from the rest of a class and made
`testLargeRandom` usable as a 30-second stand-in for a 600-second one.
`CodecProbe` (`-Dcodec=`, `-Dsize=`, `-Dreps=`, `-Dfill=0` to skip the
byte-at-a-time fill) runs one codec round trip standalone in seconds, which is
what every number on this page was measured with.

## Related

* `compression-testhugedecompress-shared-timeout-20260816.md`
* `varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`
* `longlonghashmaptest-npe-spliced-ctor-this-not-a-gc-root-FIXED-20260828.md`
* The per-call native-dispatch floor, priced independently elsewhere: the
  `HashedWheelTimerTest` page and the KFusion FFM-segment page.
