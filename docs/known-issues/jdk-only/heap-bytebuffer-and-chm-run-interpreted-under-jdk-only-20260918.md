# `--jdk-only` throughput: the real JDK bytecode that used to sit behind natives

| | |
|---|---|
| **Status** | **PARTLY FIXED 2026-09-19, still OPEN.** Two of the page's three original claims were wrong or incomplete and are corrected below. Two fixes landed (a JIT admission and a 19-row retirement) and close the *micro-benchmark* gap by 1.3x-60x per operation — **but they move the netty classes by 0-15 %, inside run-to-run noise, because those classes are not bound by these paths** (see *Per-class attribution*). One attempted retirement was **refused on measurement** and is recorded so it is not tried again. The remaining gap is itemised, in order of value, at the end. Not a correctness defect. |
| **Found** | 2026-09-18, while retiring `netty-jdkonly-failures-and-hangs-20260918.md` (now `../../internal/fixed-suite-bugs/netty/netty-jdkonly-failures-and-hangs-FIXED-20260919.md`). |
| **Where it shows** | Netty classes that finish in 20-230 s under the default mode take 2-6x longer under `--jdk-only`; at a flat 120-240 s cap they read as `HANG`. |
| **Landed in** | `../../../jit/src/lib.rs` (`precise_frame_publishing_opcode`: protected `ldc`), `../../../native-api/src/retired_shadow.rs` (`RETIRED_SHADOW_JOTP_TRIPLES`, `RETIRED_SHADOW_JOTP_UNALIGNED_TRIPLES`), the kind-map baseline, and eleven probes under `../../../tools/probes`. |

## What the first version of this page got wrong

The first version blamed two things. Re-measured on a release binary (quiet host, load ≈ 4, only the mode differing):

1. **"`HeapByteBuffer.putInt`/`getInt` are never sealed, because a `session()` shadow blocks their compilation" — wrong.**
   `jit-skip-seal ... calls-native-shadowed-method` is real, but it gates only the interpreter's *first-call*
   compile door. The background pipeline compiles them anyway: `CRATONVM_DBG=jitc` shows
   `full-compile java/nio/HeapByteBuffer.putInt(II)` and `getInt(I)`, each followed by a C2 supersede, and
   `ScopedMemoryAccess.putIntUnalignedInternal` and its siblings compile too. The accessors are compiled and
   still 20-40x slower than the default mode, so the cost is in what they *call*.
2. **"`ConcurrentHashMap.putVal` bails out of compilation because of a resolver limitation" — right about the
   bail, wrong about the cause.** The bail is `rbc6-handler-reads-unsafe-local(pc=372,op=0x12)`. Op `0x12` is
   an `ldc`, and the `ldc` is the `"Recursive update"` in `throw new IllegalStateException("Recursive update")`
   inside `synchronized (f)`. It was refused because a comment said the `ldc` lowering "reaches its helper
   through the shared sentinel stub" and publishes no precise frame. That was true before
   `emit_post_alloc_oom_check` learned to publish, and was never revisited.
3. **The page's premise — that heap `ByteBuffer` and `ConcurrentHashMap` explain the netty table — was not
   checked against the classes in it.** They explain almost none of it. See *Per-class attribution*.

## Measured decomposition

`../../../tools/probes/NativeCrossingPerf.java`, `UnsafeCrossingPerf.java`, `HeapBufferChmPerf.java`. ns per call unless
noted; both columns are the pre-change release binary.

| operation | default | `--jdk-only` | ratio |
|---|---:|---:|---:|
| `Objects.checkIndex` | 4.5 | 278 | 62x |
| heap `ByteBuffer.get(int)` | 20.7 | 341 | 16x |
| heap `ByteBuffer.getInt(int)` | 54 | 1966 | 36x |
| heap `ByteBuffer.putInt(int,int)` | 55 | 1996 | 36x |
| `Unsafe.getInt(byte[],long)` (`ACC_NATIVE`) | 267 | 268 | 1x |
| `Unsafe.getIntUnaligned(byte[],long,boolean)` | 277 | 849 | 3x |
| `ScopedMemoryAccess.getIntUnaligned` | 39 | 1945 | 50x |
| `ConcurrentHashMap` 500 k put + get (ms) | 1483 | 6257 | 4.2x |

Why, from the native census (`--dump-native-registry`, `--nojit` for exact counts): one heap `getInt` executes four
natives — `Preconditions.checkIndex`, `Buffer.session()`, `Reference.reachabilityFence` and `Unsafe.getIntUnaligned`
— and the `--jdk-only` report carries the same triple as **both** `[bytecode-won]` and `[native-won]`, depending
on which dispatch door reached it. Under `--jdk-only` the JIT's thin direct helper for `Preconditions.checkIndex`
is refused (`direct_native_helper` admits a helper only for a reviewed `Intrinsic`; this is a `Bridge`), so a
compiled caller pays the generic dispatch, 250-550 ns, for a bounds check.

**The dispatch-time dial does not model a retirement.** Arming
`CRATONVM_ENFORCE_NATIVE_SHADOW=jdk/internal/util/Preconditions` made `Objects.checkIndex` **3x slower**
(485 → 1576 ns): a method with a registered native is never compiled (`native_skip`), so the bytecode the dial
yielded to ran interpreted. Only a refused registration takes the native out of the JIT's sight, and
`CRATONVM_UNRETIRE_NATIVE_SHADOW` is the instrument for a one-binary A/B of one.

## What landed

### 1. A protected `ldc` publishes a precise frame (`../../../jit/src/lib.rs`)

`emit_ldc_string` and `emit_ldc_class` both end in `emit_post_alloc_oom_check`, the guard `new` was admitted on
(2026-08-17); it records a reason-9 frame at the `ldc`'s own bci whenever the pc is protected. Every arm of the
`0x12`/`0x13` lowering is a helper call ending in that guard, a bare numeric push that cannot throw, or a compile
bail. Admitted under the existing `CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW` switch, so the A/B is one binary.

* `../../../tools/probes/Rbc6LdcProbe.java`: five methods whose handler reads a local written inside the `try`, including
  `missingClass`, where the `ldc` itself throws `NoClassDefFoundError` (`Gone.class` is deleted after compile).
  Identical to HotSpot under default, `--jdk-only` and `--nojit`. With the switch **off** all five bail at the
  `ldc` (jitc trace); **on**, all five compile.
* The unit test that pinned `ldc` as "does not publish" is corrected, and a second pins the exact `putVal` shape.
* Effect, one binary: `ConcurrentHashMap` 500 k put + get, `--jdk-only`, **6257 → 4768 ms**. `putVal` no longer
  bails. `initTable`, `transfer`, `tryPresize` still do (`anewarray`) — see the residuals. (`dev`'s JIT review
  round 9 admitted a protected `arraylength` in parallel; with both, `StreamEncoder.write([CII)V` compiles too.)

### 2. `Preconditions.check*` and `Unsafe.*Unaligned` retired under `--jdk-only`

* **`RETIRED_SHADOW_JOTP_TRIPLES` (3)** — the three `int` `Preconditions.check{Index,FromToIndex,FromIndexSize}(…,
  BiFunction)`. The real bodies are HotSpot intrinsic candidates and honour the exception formatter themselves.
  The non-JDK `(II)I` spellings have no bytecode to yield to and stay.
* **`RETIRED_SHADOW_JOTP_UNALIGNED_TRIPLES` (16)** — `Unsafe.{get,put}{Char,Short,Int,Long}Unaligned`, both
  spellings. The L5 lane recorded these as "permanently blocked, a slot index has no bytes to address". That is true
  of an offset from `objectFieldOffset`, and **no `*Unaligned` accessor is ever handed one**: they take array and
  off-heap offsets, which are byte offsets here, and the real bytecode composes them from
  `getInt`/`getShort`/`getByte(Object,long)` — `ACC_NATIVE`, bound to the very `*_mb` handlers the shadows are
  registered to. The shadows only stood in front of a composition that already reached the same code. The two tests
  that pinned the rows are amended with the reason; a new one pins the sub-word atomics and the four numbering
  methods, whose reason is unchanged, *out*.
  `../../../tools/probes/UnsafeUnalignedMatrixProbe.java` (every width, both byte orders, every alignment, spanning reads of
  `char[]`/`int[]`/`long[]`, `Arrays.mismatch`) is identical to HotSpot under default, `--jdk-only` and
  `--jdk-only --nojit` with the table armed.
* Kind map: 19 + 32 rows amended `bridge → synthetic-stub`. `no_retired_triple_survives_the_strict_boot` passes.

One-binary A/B (`CRATONVM_UNRETIRE_NATIVE_SHADOW` naming exactly these rows, then unset), release, `--jdk-only`:

| | 19 rows un-retired | retired | default |
|---|---:|---:|---:|
| `Objects.checkIndex` | 288 | 8-108 | 4.5 |
| heap `get(int)` | 344 | **27** | 21 |
| heap `getInt(int)` | 1988 | **1054** | 54 |
| heap 1 M `put`+`getInt` (ms) | 2166 | **1073** | 59 |

## Refused, measured: `Buffer.session()` / `checkSession()`

The first cut also retired those two on the ten buffer classes that register them (`CharBuffer` took the same pair in
wave 4), on the argument that real `session()` is `segment != null ? segment.sessionImpl() : null`. **It broke the
corpus in one run**, on the same release binary with only those 20 rows differing:

| | retired | kept as shims |
|---|---:|---:|
| `PcapWriteHandlerTest` | 8 ok / 17 failed | 25 / 0 |
| `AdaptiveBigEndianDirectByteBufTest` | 339 ok / 76 failed | 415 / 0 |

Every failure: `NoSuchMethodError: 'void jdk.internal.foreign.ArenaImpl.checkValidStateRaw()'`. A buffer whose
`segment` is not null (a direct buffer over a `MemorySegment`, which netty's allocators make) has its real
`sessionImpl()` run against **this VM's own arena carrier**, which is laid out deliberately unlike the JDK's
(`the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`) and never declares the method. The constant-`null`
shim is what stops that bytecode running. The niche-0 concern the first version of this page raised ("an unwritten
reference slot reads back as `Int(0)`") was the wrong worry: it applies to by-name native reads, not to a real
`getfield`. The real hazard is the carrier. Un-retiring exactly those 20 rows on the same binary restored 25/25 and
415/415; `the_jotp_wave_refuses_the_buffer_session_pair` pins them out by name.

## Per-class attribution

`CRATONVM_PROFILE_SAMPLE_MS=5` (a safepoint-poll CPU profile; **native time is charged to the calling Java frame at
the next poll**, so read a Java frame that only calls natives as "the natives it calls"), plus
`--dump-native-registry` invocation counts. `perf` is unavailable on the host (`perf_event_paranoid=4`) and gdb
cannot interrupt the VM (it blocks `SIGINT`), so pure-compute classes cannot be attributed to a native.

| class | default | `--jdk-only` (before) | dominant cost | same-time A/B, old → new (s) |
|---|---:|---:|---|---|
| `DefaultHttp2ConnectionTest` | 36-51 s | 125-215 s | **Mockito stack capture**: 81 % of samples in `Java9PlusLocationImpl.lambda$static$0` / `MetadataShim.get`; 3.4 M `Method.invoke` and 25 M `ArraysSupport.mismatch` calls vs 2.4 M | 125 → 129 |
| `SizeClassedChunkCacheTest` | 12 s | 23-25 s | **Mockito again**: 91 % in `Java9PlusLocationImpl` / `createInvocation` / `ArrayList.iterator` | 23.3 → 23.1 |
| `AdaptiveBigEndianDirectByteBufTest` | 66 s | 66-128 s | test-body loops over `Channels$WritableByteChannelImpl.write`, `Recycler` | 65.7 → 58.8 |
| `PcapWriteHandlerTest` | 64 s | 79-142 s | `PcapWriteHandler.handleTcpPacket`, `writePcapGreaterThan4Gb` (4 GB of `ByteBuf` writes) | 78.7 → 76.8 |
| `SearchProcessorTest` | 68 s | 92-96 s | 91 % in netty's own `AhoCorasicSearchProcessorFactory.buildTrie` (pure compute) | 91.9 → 79.4 |
| `BigEndianHeapByteBufTest` | — | 20 s | buffer-heavy test bodies | 20.1, 20.1 → 20.4, 26.0 |
| `PooledBigEndianHeapByteBufTest` | — | 33 s | same | 34.2, 32.6 → 31.1, 31.3 |
| `AdvancedLeakAwareByteBufTest` | — | 30 s | same | 29.7, 33.6 → 27.1, 25.4 |
| `Pooled`/`AlignedPooledByteBufAllocatorTest` | 48 s | 121-124 s | `testThreadCacheDestroyedByThreadCleaner`'s own 20 s timeout | 124 → 124 |
| `CertificateBuilderTest` | 62 s | 283-403 s | Bouncy Castle SLH-DSA (`WotsPlus.pkGen` 43 %, `HT.treehash` 28 %) and `BigInteger.gcd`/`oddModPow` | not run |
| compression `*IntegrationTest` | 68-93 s | 395-577 s | 0.9 k samples in 468 s: the time is inside natives the profiler cannot see | not run |

The last column is old and new **release** binaries run back to back on one host. **Do not read the sweep-to-sweep
totals as this change's effect.** A full 733-class `--jdk-only` sweep on the merged tree against the earlier one reads
`578 → 585 PASS`, 33 classes more than 1.5x faster and none slower — and that is host load: the earlier sweep ran at
load 8-20, this one at 4-12, and the *old* binary now finishes `BigEndianHeapByteBufTest` in 20 s where the earlier
sweep recorded 48 s. Only same-time pairs isolate a change.

So the page's premise was mostly wrong: **heap `ByteBuffer` and `ConcurrentHashMap` account for the micro-benchmark
gap, not for the netty table** — which is why a 30-60x win on `checkIndex` and a 2x win on the heap accessors is
worth 0-15 % on the classes above. The two biggest ratios are Mockito. Frame counts through `StackWalker` and
`Throwable` match HotSpot in both modes (`../../../tools/probes/StackDepthCensus.java`), so the default mode is not faster
for doing less work; `--jdk-only` really is slower per frame:

| | default | `--jdk-only` | ratio |
|---|---:|---:|---:|
| `new Throwable()`, depth ≈ 6 | 3.1 µs | 6.3 µs | 2.0x |
| `new Throwable().getStackTrace()`, depth ≈ 6 | 6.5 µs | 17.5 µs | 2.7x |
| … depth ≈ 56 | 62 µs | 147 µs | 2.4x |
| `StackWalker.walk(collect)`, depth ≈ 8 | 9.5 µs | 21.7 µs | 2.3x |
| `Method.invoke` | 2.3 µs | 3.2 µs | 1.4x |

(Streams are not the cause: `list.stream().findFirst()` is *faster* under `--jdk-only`, 5.7 vs 25.9 µs.)

## What is left, in order of value

1. **The `ACC_NATIVE` `Unsafe.get/put{Byte,Short,Int,Long}(Object,long)` crossing, ≈ 267 ns in BOTH modes.** It is
   the floor under every heap `ByteBuffer` accessor and under netty's `PlatformDependent` on `byte[]`, in the default
   mode too. A thin direct helper (the `scoped_memory_*` pattern in `../../../vm/src/jit/helpers.rs`) is admissible under
   `--jdk-only` — an `ACC_NATIVE` method has no bytecode to shadow, unlike the `Bridge`s `direct_native_helper`
   refuses. Three compile doors (`lib.rs` single-pass, IR, `jit_bridge.rs` OSR).
2. **`Buffer.session()`'s thin helper declines every call under `--jdk-only`.** Census, one 1.2 M-call run:
   default `served=1196005 declined=0`; `--jdk-only` `served=0 declined=1198121`, sites bound and never answering,
   so each accessor pays a generic dispatch (~440 ns; retiring the shim measured 1054 → 580 ns before it was
   refused). It declines because its served-class table is filled only when the *shim* runs, and because
   `jit_direct_helper_refused` withholds a *shadow's* answer under strict policy. Neither reason applies to a buffer
   whose `segment` field is null: real `session()` returns null there too, so the helper could answer from the field
   — bytecode's own answer, not a shadow's — and decline when `segment` is set. Needs the `Buffer.segment` slot
   resolved independently of the shim.
3. **`StackTraceElement` materialisation and `Throwable`, ≈ 2.5-3x.** Four native crossings per `Throwable`
   (`fillInStackTrace`, `initStackTraceElements`, `computeFormat`, `Object.clone`); `getStackTrace` of a cached
   trace costs 3.0 µs vs 1.5 µs. Bounds the Mockito classes above.
4. **`anewarray` (and `aastore`, the integer divides, `multianewarray`) in a protected range.** These lowerings
   publish no frame at the bci, or have an unaudited `NegativeArraySizeException` edge. On the merged tree they
   keep `ConcurrentHashMap.initTable`/`transfer`/`tryPresize` interpreted
   (`compile-bail … reason=rbc6-handler-reads-unsafe-local(pc=…,op=0xbd)`) — cold per operation, hot for a map
   that keeps growing. `arraylength` was on this list until round 9 admitted it on `dev`; `anewarray` reaches the
   same publishing `emit_post_alloc_oom_check` `new` does and is the cheapest next candidate.
5. **The generic JIT → native dispatch, ≈ 250 ns for a trivial native.** `jit_invoke_dispatch_body` does a SATB
   flush, thread-local hash lookups keyed on the site, argument forwarding and several policy probes per call. Any
   general saving pays in both modes.
6. Unattributed: `buildTrie`, Bouncy Castle SLH-DSA, `BigInteger`, the compression natives — need a native-level
   profile the host does not allow.

Found on the way, both modes, not this page's: `StackWalker` with `SHOW_REFLECT_FRAMES` reports 2 frames through a
`Method.invoke` where HotSpot reports 4 (`StackDepthCensus`, last line).

## Reproduce

```bash
# probes are in tools/probes; javac them with the JDK 25 image, no build needed
cratonvm --java-home <jdk25> -cp . NativeCrossingPerf 200000               # per-crossing prices
cratonvm --jdk-only --java-home <jdk25> -cp . NativeCrossingPerf 200000
cratonvm --jdk-only --java-home <jdk25> -cp . HeapBufferChmPerf 4 500000   # heap/direct BB and CHM, per round

# the one-binary A/B of the retirement: name exactly the 19 rows
CRATONVM_UNRETIRE_NATIVE_SHADOW='jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I,...' \
    cratonvm --jdk-only ...                       # prints "armed, N rule(s)" and any rule matching nothing

# the ldc acceptance differential (Gone.class must be deleted after javac)
javac -d out Rbc6LdcProbe.java Gone.java && rm out/Gone.class
java -cp out Rbc6LdcProbe 200000  > hs.txt; cratonvm [--jdk-only|--nojit] -cp out Rbc6LdcProbe 200000 | diff hs.txt -
CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW=1 CRATONVM_DBG=jitc cratonvm ... Rbc6LdcProbe   # the five bails, at the ldc

# the session() helper census (bound vs answering)
CRATONVM_DBG=jit-method-stats cratonvm --jdk-only ... HeapBufferChmPerf 3 200000 2>&1 | grep 'Buffer.session direct'

# exact native counts for a workload
CRATONVM_DISABLE_INTRINSICS=1 cratonvm --jdk-only --nojit --dump-native-registry reg.json ...
```
