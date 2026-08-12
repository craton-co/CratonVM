# SIGSEGV (rc=139) in JNI-backed compression codec tests — Zstd, LZ4, checksum, HTTP content-decode

**Status:** OPEN (2026-08-12). Found on Windows (`C:\craton\CratonVM`) during a
3-GC-variant (default/G1/ZGC) full-suite (657-class) run of the netty test
suite, built from an isolated worktree at commit `70c8b8cd6`.

## Symptom

Four classes crash the CratonVM process outright (no stdout/stderr, no
Java exception — the process dies with SIGSEGV, `rc=139`, before any
`@@RESULT` line is emitted):

- `io.netty.handler.codec.compression.ZstdDecoderTest`
- `io.netty.handler.codec.compression.Lz4FrameDecoderTest`
- `io.netty.handler.codec.compression.ByteBufChecksumTest`
- `io.netty.handler.codec.http.HttpContentDecoderTest`

```
run-netty-suite.sh: line 247: <pid> Segmentation fault  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 60 <cv> ... CratonRunner <class>
status: CRASH=1  sum_class_ms=0
```

All 4 reproduced identically in every one of the 3 GC-variant shard runs
(default/G1/ZGC — GC-independent), and each was individually re-run in
isolation (`--shards 1`, single class) to rule out shard-contention
artifacts: **deterministic crash every time**, all 4 classes.

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

Only 4/657 classes crash outright, but a process-level SIGSEGV is more
severe than a normal test failure — it kills the entire fork (no partial
results for that class) and, if this pattern extends to any JNI-native-
library-dependent code in a real application (not just these test
classes), would be a hard crash rather than a graceful failure.

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
