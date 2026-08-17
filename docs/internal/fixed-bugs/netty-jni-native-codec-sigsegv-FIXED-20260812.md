# SIGSEGV (rc=139) in JNI-backed compression codec tests — FIXED 2026-08-12

**Status: ✅ FIXED (2026-08-12), branch `fix/netty-jni-sigsegv-20260812`.**
All five classes now match stock HotSpot JDK 25 exactly. Three defects, all at
the JNI boundary, all reproduced on Linux and closed with a HotSpot oracle
beside each one.

The original report (Windows, 3-GC-variant 657-class run at `70c8b8cd6`) is
preserved verbatim in §6 below. Its working hypothesis — "a JNI-boundary fault
… is the leading hypothesis, not confirmed" — was right about the boundary and
did not name the mechanism; the mechanism turned out to be three separate ones.

---

## 1. What the five classes do now

| class | before | after | HotSpot |
|---|---|---|---|
| `io.netty.handler.codec.compression.ZstdDecoderTest` | **CRASH** rc=139 | 8/8 ok | 8/8 ok |
| `io.netty.handler.codec.compression.Lz4FrameDecoderTest` | **CRASH** rc=139 | 16/16 ok | 16/16 ok |
| `io.netty.handler.codec.compression.ByteBufChecksumTest` | **CRASH** rc=139 | 2/2 ok | 2/2 ok |
| `io.netty.handler.codec.http.HttpContentDecoderTest` | **CRASH** rc=139 | 24/24 ok | 24/24 ok |
| `io.netty.test.udt.nio.NioUdtByteRendezvousChannelTest` | 0 ok / 2 failed | 1 ok / 1 failed | **1 ok / 1 failed** |

Two classes outside the original five moved as a side effect and are listed
here so the change's reach is on the record rather than discovered later:
`io.netty.handler.codec.compression.ZstdDecompressorTest` CRASH → PASS, and
`io.netty.test.udt.nio.NioUdtMessageRendezvousChannelTest` FAIL → PASS.

The UDT row is complete: `basicEcho()` fails on **stock HotSpot too**, on this
host, for the same reason (the test needs two UDT sockets to rendezvous over
the loopback within 10 s). CratonVM's *extra* failure — `metadata()` — is
fixed. Matching HotSpot is the bar; passing a test HotSpot fails is not.

## 2. Defect 1 — `GetDirectBufferAddress` returned a pointer C cannot dereference

**This is the SIGSEGV.** It is one mechanism behind all four crashes.

`Unsafe.allocateMemory` on this VM does not return an OS pointer. It returns a
*handle* into a Rust-side arena (`native-builtins/src/unsafe_natives_ext.rs`,
`mod unsafe_arena`), carrying `ARENA_TAG` (bit 62) precisely so that it can
never be confused with a real pointer. `ByteBuffer.allocateDirect` runs the real
JDK's bytecode, which allocates through `Unsafe`, so **every direct buffer this
VM hands to Java has a tagged handle in `Buffer.address`** — measured:

```
             ByteBuffer.allocateDirect(64).address
HotSpot      137873551742736          (a real pointer)
CratonVM     4611686087146864640      (= ARENA_TAG | 0x10_0000_0000)
```

Inside the VM that is invisible: the `Unsafe` get/put natives route a tagged
address back to the arena. It stops being invisible at the JNI boundary, because
`GetDirectBufferAddress` returns a bare `void*` that the *native* dereferences.
Both libraries do exactly that, in one line and with no null check:

* lz4-java `XXH32BB` → `XXH32(GetDirectBufferAddress(env, buf) + off, len, seed)`
* zstd-jni `getDirectByteBufferFrameContentSize` → same shape

The faulting frame, under `gdb`, is unambiguous and names the arena tag in the
register:

```
Thread 2 "main-vm" received signal SIGSEGV
#0  0x00007fffe3956eb0 in XXH32 () from /tmp/liblz4-java-….so
#1  call_jni_fn_ptr_int () at vm/src/native/jni.rs:6079
#2  call_jni_marshalled () at vm/src/native/jni.rs:5756
#3  dispatch_jni_native () at vm/src/native/jni.rs:5614
rdi  0x400000100005fcb1     <-- ARENA_TAG | … , not an address
```

**Fix.** `unsafe_arena_real_ptr` (`unsafe_natives_ext.rs`) translates a live
handle into the real address of its block's backing bytes, and
`direct_buffer_native_address` (`vm/src/native/jni.rs`) calls it whenever the
`address` field is tagged. Writes by the native land in the arena directly —
the aliasing a direct buffer is defined to have — so there is no copy-back. A
tagged address no live block owns (use-after-free, or an offset past the end)
resolves to `None` and the caller answers JNI NULL, which is the sentinel the
spec already reserves for "not a direct buffer".

The translation is deliberately at the JNI boundary and nowhere else. Making
`Unsafe.allocateMemory` return real memory would fix the same symptom by
deleting the arena's bounds checking and use-after-free detection for every
caller in the VM.

## 3. Defect 2 — `GetDirectBufferAddress` answered for buffers that are not direct

Found by the regression probe written for defect 1, not by netty.

The address getter tested `address != 0` to decide whether a buffer was direct.
That was correct on JDK 8 and is **wrong on JDK 21+**: `Buffer.address` was
repurposed, and a heap buffer now carries `ARRAY_BYTE_BASE_OFFSET` there —
literally `16` — as the base for the `Unsafe` accesses that pair it with `hb`.
Measured on Temurin 25.0.3, and the two VMs *agree about the field*:

```
                                    HotSpot   CratonVM
ByteBuffer.allocate(16).address      16        16
GetDirectBufferAddress(that)         NULL      (void*)16      <-- the defect
GetDirectBufferCapacity(that)        -1        16
```

So `GetDirectBufferAddress` on any heap buffer handed native code the pointer
`0x10`. Same crash family as defect 1, different cause, and reachable by any
library that passes a `ByteBuffer` without checking `isDirect()` first.

**Fix.** `is_direct_buffer` in `vm/src/native/jni.rs` applies the JDK's own
rule — the buffer implements `sun.nio.ch.DirectBuffer` — and both the address
and the capacity getter refuse anything else. The class-name check on
`java/nio/DirectByteBuffer` sits beside the interface check because a buffer
minted by `NewDirectByteBuffer` under `--synthetic-jdk` has the class but need
not have the interface.

## 4. Defect 3 — a `java.lang.Class` parameter was unusable as a `jclass`

This is the UDT `metadata()` failure, and it is the widest of the three.

A class reaches native code under two different encodings:

* `FindClass`, `GetObjectClass`, `GetSuperclass` and the implicit `jclass` of a
  static native all produce `class_id_to_jclass` — a `ClassId` under
  `JCLASS_TAG`. Every `jclass`-consuming entry point decodes that with
  `ClassId::new(h as u32)`, which works because the truncation drops the tag;
* a `Class` that arrives as an ordinary **parameter** — the native is declared
  `foo(int, Class<?>, Object)` — is marshalled like any other reference, as
  `obj_to_jobject` of its mirror. Truncating *that* to a `u32` yields a fragment
  of a heap address.

Nothing reconciled the two. barchart-udt's
`SocketUDT.setOption0(int code, Class<?> klaz, Object value)` dispatches on
`IsSameObject(klaz, <cached Boolean.class>)`, so it rejected **every** option and
netty's `NioUdtProvider` could not open a channel at all:

```
com.barchart.udt.ExceptionUDT: UDT Error : -3 : wrapper generated error :
    unsupported option class in OptionUDT [id: 0x22ff87ec]
  at com.barchart.udt.SocketUDT.setBlocking(SocketUDT.java:1416)
  at io.netty.channel.udt.nio.NioUdtProvider.newRendezvousChannelUDT(…:162)
```

Measured against a purpose-built JNI fixture (`probes/jni_native_interop_probe.c`),
CratonVM answered "no"/NULL to all of `IsSameObject`, `GetMethodID`,
`GetStaticFieldID`, `IsAssignableFrom` and `IsInstanceOf` for a `Class`
parameter, where HotSpot answered yes to each.

**Fix.** `jclass_class_id` normalises *any* handle that names a class, and the
16 `ClassId::new(clazz as u32)` decode sites now call it; `jni_identity` does the
same for `IsSameObject`. It is a reconciliation at **consumption**, not a
re-encoding at marshalling, and that choice is load-bearing: re-encoding `Class`
arguments as `jclass` handles would have fixed the reads and broken the writes,
because a native that hands the same reference back to Java — or to
`GetObjectClass` — needs the mirror `ObjectRef`, which a `jclass` handle does not
carry. The probe asserts both directions for exactly that reason.

Anything that is neither a `jclass` nor a mirror still decodes by truncation, so
no pre-existing decode changed behaviour.

## 5. Defect 4 — the lz4 native shim answered wrongly for direct buffers

Not a crash, and only visible once the crashes were gone: with defect 1 fixed,
`Lz4FrameDecoderTest` ran and failed 8 of 16.

`native-builtins/src/compression_native.rs` registers a `lz4_flex`-backed shim
for `net.jpountz.lz4.LZ4JNI` that wins the registry slot over the real `.so`.
Every LZ4JNI method takes a `(byte[] arr, ByteBuffer buf)` pair where exactly one
half is non-null; the shim was written for Kafka, which only ever takes the array
branch, and read `args[0]`/`args[4]` unconditionally. On the ByteBuffer branch it
therefore read an **empty** input and wrote its output **nowhere** — a silent
wrong answer, which surfaced one frame later as netty's
`stream corrupted: mismatching checksum` and
`lz4 decompress_safe: expected another byte, found none`. Netty takes the
ByteBuffer branch for every direct or pooled `ByteBuf`.

Measured against the same natives on HotSpot:

```
                              HotSpot    CratonVM before   after
LZ4_compressBound(4096)       4128       4525              4128
ARR decompress_fast           112 ✓      -1  ✗             113 ✓
BB  compress_limitedOutput    112 ✓      1   ✗             113 ✓
BB  decompress_safe           4096 ✓     throws ✗          4096 ✓
MIX decompress_safe           4096 ✓     throws ✗          4096 ✓
```

(The compressed *lengths* legitimately differ by a byte — `lz4_flex` and liblz4
are different encoders of the same block format. Every round trip matches.)

**Fixes**, all in `compression_native.rs`:

* `lz4_side`/`lz4_read`/`lz4_write` back both halves of the pair — a direct
  buffer through `copy_from_native_memory`/`copy_to_native_memory`, the same
  bridge `Inflater`'s direct-buffer natives use — and an unresolvable argument
  now throws instead of silently reading zero bytes;
* `lz4_compress_bound` is liblz4's own `n + n/255 + 16` rather than `lz4_flex`'s
  larger internal estimate, so the number lz4-java exposes as
  `LZ4Compressor.maxCompressedLength` matches HotSpot;
* `lz4_block_decode` walks the LZ4 block grammar directly, reporting both bytes
  consumed and bytes produced. `LZ4_decompress_fast` is told the *uncompressed*
  size and must report the *source* size, which `lz4_flex` has no entry point
  for — the old body handed it the caller's whole source array, whose tail is
  unwritten, and returned -1 for every call;
* a block the decoder cannot decode now returns a **negative int** rather than
  throwing `IOException`. That is liblz4's contract, and lz4-java turns it into
  `LZ4Exception` — which netty's `Lz4FrameDecoder` has an explicit `catch` arm
  for, and which an `IOException` bypassed.

## 6. Verification

* **`probes/JniNativeInteropProbe.java` + `probes/jni_native_interop_probe.c`**
  — 24 checks over both JNI families, self-contained (no netty, no lz4, no
  zstd). CratonVM's transcript is now byte-identical to HotSpot's;
  `probes/JniNativeInteropProbe.expected.txt` records it, together with which
  lines were green *before* the fix and why a positives-only probe would have
  missed this.
* **Rust unit tests.** `unsafe_arena_real_ptr_tests` (3) cover aliasing in both
  directions, interior offsets, and refusal of a freed / untagged / past-the-end
  address. `lz4_block_decode_*` (2) round-trip six block shapes with trailing
  garbage and assert the reported source length, and refuse malformed input
  without panicking; `lz4_compress_bound_matches_liblz4` pins the formula.
* **The five original classes**, re-run on the fixed binary: table in §1.
* **Rust regression.** `cargo test -p cratonvm-vm` 2503 passed / 0 failed;
  the same with `--features synthetic-jdk` 4016 passed / 1 failed;
  `-p cratonvm-native-io` 451 passed / 0 failed; `-p cratonvm-native-builtins`
  3500 passed / 3 failed. All four pre-existing failures
  (`vm::tests::object_output_stream_p70`, `logmanager::tests::t19_h3_*`) fail
  **identically on pristine `origin/dev` 1c4ce7d3a** — each verified in a
  separate worktree, not assumed.
* **`regression-suite/run.sh`** (diffs CratonVM against HotSpot): 42 passed,
  0 failed.
* **Netty A/B**, 245 classes (every third of `testlist.txt`), pristine
  `origin/dev` binary vs this branch, same shards and 120 s cap:

  | | pristine | this branch |
  |---|---|---|
  | CRASH | **3** | **0** |
  | PASS | 149 | **153** |
  | HANG | 13 | 8 |
  | FAIL | 59 | 63 |

  Per class, **no class regressed from PASS**. The transitions are
  `CRASH -> PASS` ×3 (`ByteBufChecksumTest`, `ZstdDecompressorTest`,
  `HttpContentDecoderTest` — note `ZstdDecompressorTest` was NOT in the original
  five and is fixed by the same change), `FAIL -> PASS` ×1
  (`NioUdtMessageRendezvousChannelTest`, the UDT sibling class, fixed by defect
  3), and `HANG -> FAIL` ×5, all in `io.netty.buffer`.

  Those five were checked rather than waved through, because "HANG became FAIL"
  can hide a new failure. They are the throughput-gap cluster sitting on the
  120 s cap. Run individually at a 900 s cap on **both** binaries, twice each,
  `BigEndianHeapByteBufTest` and `SimpleLeakAwareByteBufTest` give
  byte-identical results on both arms — same counts, same three failing test
  names (`testDuplicateBytesInArrayMultipleThreads`,
  `testSliceBytesInArrayMultipleThreads`, `testStreamTransfer1`). One earlier
  single run of `SimpleLeakAwareByteBufTest` on this branch showed 8 failures
  instead of 3; four subsequent runs did not reproduce it on either arm, so
  those three multithreaded tests are flaky under host load. Worth knowing
  before reading any future one-shot netty number as a signal.

Repro for any of it (Linux host, `/data/cratonvm/apps/netty-suite-runner`):

```bash
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 180 ./cratonvm \
  --java-home /data/toolchain/jdk-25 --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner io.netty.handler.codec.compression.ZstdDecoderTest
```

## 7. What this does NOT close

The original report's non-CRASH observations were explicitly deferred by it
("this doc covers only the fully-verified CRASH bucket"), and they are **not**
resolved here. They have been carried forward, unchanged, to
`docs/known-issues/netty/full-suite-fail-hang-buckets-20260812.md`:

* ~130 FAIL and ~50 HANG classes per GC variant, untriaged;
* the `io.netty.buffer` HANG cluster, characterized as a ~12x throughput gap
  rather than a deadlock on a sample of one;
* the `found=N aborted=N` classes (`WrappedUnpooledUnsafeByteBufTest` and
  friends), never cross-checked against HotSpot.

---

# 6 (original report, verbatim as filed 2026-08-12)

**Status:** OPEN (2026-08-12). Found on Windows (`C:\craton\CratonVM`) during a
3-GC-variant (default/G1/ZGC) full-suite (657-class) run of the netty test
suite, built from an isolated worktree at commit `70c8b8cd6`. All 3
variants completed (~87min each); results below are from the completed
runs, not partial data.

## Symptom

Five classes crash the CratonVM process outright (no stdout/stderr, no
Java exception — the process dies with SIGSEGV, `rc=139`, before any
`@@RESULT` line is emitted):

- `io.netty.handler.codec.compression.ZstdDecoderTest`
- `io.netty.handler.codec.compression.Lz4FrameDecoderTest`
- `io.netty.handler.codec.compression.ByteBufChecksumTest`
- `io.netty.handler.codec.http.HttpContentDecoderTest`
- `io.netty.test.udt.nio.NioUdtByteRendezvousChannelTest`

```
run-netty-suite.sh: line 247: <pid> Segmentation fault  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 60 <cv> ... CratonRunner <class>
status: CRASH=1  sum_class_ms=0
```

All 5 reproduced **identically in all 3 completed GC-variant full runs**
(default/G1/ZGC — same 5 classes, byte-for-byte, in every variant, so this
is GC-independent), and the first 4 were also individually re-run in
isolation (`--shards 1`, single class) to rule out shard-contention
artifacts: **deterministic crash every time**. The 5th
(`NioUdtByteRendezvousChannelTest`, netty's UDT — UDP-based Data Transfer —
native transport, backed by the `barchart-udt` JNI library) surfaced in the
full run after the doc was first drafted from partial data; it fits the
same JNI-native-library hypothesis below even more directly than the
first four, since UDT has no pure-Java fallback at all.

## HotSpot-clean confirmation

Ran the identical 4 classes, same classpath, under stock HotSpot (JDK 25,
`--hotspot`): **all 4 PASS** (`status: PASS=4`, 34002ms total). Genuine
CratonVM-specific defect.

## JIT-independent

Re-ran `ZstdDecoderTest` with `--nojit` (interpreter-only): **still
crashes** with the identical SIGSEGV. Rules out a JIT-compilation-specific
cause; whatever is faulting happens in a path both the interpreter and the
JIT reach.

## Working hypothesis: JNI boundary with bundled native libraries

Not yet root-caused at the instruction/stack-trace level (no crash dump or
`gdb`-equivalent captured yet on Windows — the process dies silently, no
`CRATONVM_DBG`-style output). But the four crashing classes share a
suggestive pattern: `codec-compression`'s `pom.xml` pulls in
`com.github.luben:zstd-jni` and `lz4-java`, both of which bundle
platform-native `.dll`/`.so` libraries and communicate via JNI (not pure
Java). `ByteBufChecksumTest` and `HttpContentDecoderTest` don't obviously
touch those two libraries directly by name, but content-decoding /
checksum paths commonly probe or fall through native zlib/CRC bindings
(JDK's own `java.util.zip` native calls) during class init or algorithm
negotiation — so a JNI-boundary fault (native-library loading, a native
upcall into the JVM via a JNI function CratonVM doesn't fully/correctly
implement, or memory-layout mismatch when native code touches a
CratonVM-managed byte array) is the leading hypothesis, not confirmed.
Worth checking specifically: does CratonVM crash on *any* class that loads
a third-party JNI native library on Windows, or is it isolated to these
four? A minimal repro (a bare class that just calls
`System.loadLibrary`/triggers `Zstd.isAvailable()` with no netty
scaffolding) would help isolate loader-time vs. call-time failure.

## Impact

Only 5/657 classes crash outright, but a process-level SIGSEGV is more
severe than a normal test failure — it kills the entire fork (no partial
results for that class) and, if this pattern extends to any JNI-native-
library-dependent code in a real application (not just these test
classes), would be a hard crash rather than a graceful failure.

Full-suite results, all 3 GC variants (657 classes each, ~87min wall each):

| variant | PASS | FAIL | HANG | CRASH | ABORTED | NOTESTS |
|---|---|---|---|---|---|---|
| default (`-XX:+UseGenerationalGC`) | 425 | 131 | 50 | 5 | 8 | 38 |
| g1 (`-XX:+UseG1GC`) | 425 | 126 | 55 | 5 | 8 | 38 |
| zgc (`-XX:+UseZGC`) | 427 | 130 | 49 | 5 | 8 | 38 |

The near-identical distribution across all 3 GC variants (CRASH, ABORTED,
NOTESTS counts are *exactly* identical; PASS/FAIL/HANG vary by only a few
classes) indicates the FAIL/HANG buckets are largely GC-independent —
i.e. algorithmic/harness-classpath issues rather than GC-triggered
correctness bugs. Not yet individually triaged (~130 FAIL and ~50 HANG
classes per variant); this doc covers only the fully-verified CRASH
bucket. The FAIL bucket needs its own pass (dedupe by exception
signature — the harness's own auto-extracted `sig` column was empty for
119/131 default-variant FAILs, so this needs a raw-log read, not just a
tsv scan) with a HotSpot cross-check per distinct signature before any of
it can be called a confirmed CratonVM bug.

**HANG bucket, partial characterization**: 25 of the ~50 HANG classes per
variant cluster in `io.netty.buffer` (allocator/pooled-ByteBuf test
classes with large parameterized/combinatorial method counts). Sampled one
(`PooledByteBufAllocatorTest`) at a 600s timeout instead of the suite's
180s default: it completed in **202s** (`ABORTED=1`, matching HotSpot's
result type), vs HotSpot's **17s** for the same class — so this is **not**
a true hang/deadlock, it's a ~12x throughput gap that happens to cross the
180s cutoff. Only 1 of 25 `io.netty.buffer` HANGs has been checked this
way; treat the rest as *likely* the same throughput-gap pattern, not
confirmed individually. The 16 `io.netty.handler.codec` HANGs and the
remaining smaller clusters (`util.concurrent`, `resolver.dns`,
`handler.ssl`, `handler.pcap`) haven't been sampled at all and could be
genuine hangs rather than slowness — don't assume the same explanation
without checking.

## Repro

```bash
cd apps/netty-suite-runner
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh \
  --list <(echo io.netty.handler.codec.compression.ZstdDecoderTest) \
  --gc default --shards 1 --timeout 60 --out /tmp/repro
# or --jit off for the interpreter-only repro
```

## Also observed in the same run (not yet investigated, lower priority)

Several classes report `aborted` counts equal to their full test count
(e.g. `WrappedUnpooledUnsafeByteBufTest`: `found=413 aborted=413`,
`LittleEndianUnsafeNoCleanerDirectByteBufTest`: `found=412 aborted=412`).
JUnit5 "aborted" (vs. failed) typically means every test hit a failed
`Assumption` — plausible given the "Unsafe"/"NoCleaner" naming that these
assert `PlatformDependent.hasUnsafe()` or similar and CratonVM's answer
differs from HotSpot's, causing a graceful skip rather than a crash or
failure. Not yet cross-checked against HotSpot; worth a follow-up doc once
the full run's `others.txt` is categorized.

## Related

None yet — first netty-specific finding this session. Distinct from the
hibernate-reactive JNA `Native.<clinit>` NPE
(`../../known-issues/hibernate/jna-native-clinit-nativeversion-npe-20260812.md`)
found the same day, but both involve JNI-adjacent native-library
interaction on CratonVM — worth keeping in mind as a possible shared
theme (CratonVM's JNI/native-library support surface) even though the
concrete symptoms (NPE vs SIGSEGV) are unrelated at the code level.
