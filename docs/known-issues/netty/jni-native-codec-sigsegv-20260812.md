# SIGSEGV (rc=139) in JNI-backed compression codec tests — Zstd, LZ4, checksum, HTTP content-decode

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
(`docs/known-issues/hibernate-reactive/jna-native-clinit-nativeversion-npe-20260812.md`)
found the same day, but both involve JNI-adjacent native-library
interaction on CratonVM — worth keeping in mind as a possible shared
theme (CratonVM's JNI/native-library support surface) even though the
concrete symptoms (NPE vs SIGSEGV) are unrelated at the code level.
