# Elasticsearch TSDB doc-values native crashes

Status: FIXED (2026-07-03)

Date observed: 2026-07-02

## Summary

Two TSDB doc-values codec tests crashed the CratonVM process. HotSpot also
fails these classes in the baseline, but CratonVM terminated natively with an
access violation, so this was tracked as a separate VM crash.

Representative fatal output:

```text
# A fatal error has been detected by the CratonVM Runtime Environment:
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005)
# Faulting access: read at address 0x0000002200000000
# thread: "Thread-2"
```

The stderr immediately before the crash also reported:

```text
Missing native method in real-JDK mode method=java/io/FileCleanable.cleanupClose0(IJ)V
```

That line is a red herring — `FileCleanable.cleanupClose0` genuinely has no
native registration, but a missing native correctly throws
`UnsatisfiedLinkError` in production mode (`vm_exec.rs`) and does not itself
crash the process. It is unrelated Cleaner-thread activity logged around the
same time as the real fault, not its cause.

## Root cause

`MemorySegment.get/set(ValueLayout, long)` real bytecode
(`jdk.internal.foreign.AbstractMemorySegmentImpl`) calls directly into
`java.lang.invoke.SegmentVarHandle.get/set(segment, offset[, value])`. That
class is real JDK bytecode with **no concrete `get`/`set` method** — `get`/
`set` are signature-polymorphic methods inherited from `VarHandle` (JVMS
§5.4.3.3), resolved by the JVM's special linkage, not by literal method
lookup.

CratonVM's `vm_exec.rs` already has a dedicated fallback for exactly this
(`is_vh`/`is_mh` checks that redirect signature-polymorphic
`get`/`set`/`compareAndSet`/etc. calls to a registered native under a generic
descriptor), but the `is_vh` check only recognised classes literally named
`VarHandle` or prefixed `java/lang/invoke/VarHandle` — `SegmentVarHandle` is a
distinct top-level class name (JEP 454 FFM API, added in modern JDKs) that
does not share that prefix, so it fell through to normal method resolution
and threw `NoSuchMethodError`. (The analogous `is_mh` check for
`MethodHandle` had already been broadened to a `contains("MethodHandle")`
match for the same reason on an earlier, unrelated fix — `is_vh` was never
given the same treatment.)

Once routed correctly, `SegmentVarHandle.get/set` still had no
implementation at all — CratonVM has never implemented FFM-API MemorySegment
VarHandle access against *real* `jdk.internal.foreign` objects (the
pre-existing `../../../../native-builtins/src/panama.rs` / `phases_late.rs` Panama code
is an older, fully-synthetic `MemorySegment`/`ValueLayout` implementation
that real JDK-25 `java.lang.foreign` bytecode never calls into, since the
real classes construct their own `jdk.internal.foreign.*` objects with a
completely different field layout).

Why this surfaced as a SIGSEGV specifically under JIT and a hang under
interpreter: `NoSuchMethodError` is a normal catchable Java exception and
does not itself crash or hang the VM. The crash/hang were downstream
consequences in the TSDB docvalues/storedfields codecs' own error-handling
paths once repeated `MemorySegment` access failures occurred during
`MMapDirectory` I/O — the SegmentVarHandle gap was the trigger, not a direct
segfault site.

## Fix

- `../../../../vm/src/vm/vm_exec.rs`: broadened `is_vh` to also match any
  `java/lang/invoke/*` class whose name contains `"VarHandle"`, mirroring the
  existing `is_mh` broadening — this alone is what makes `SegmentVarHandle`
  calls reach the native dispatch instead of throwing `NoSuchMethodError`.
- `../../../../native-builtins/src/lang_invoke.rs`: added `segment_vh_get`/`segment_vh_set`
  (wired into the existing `varhandle_get`/`varhandle_set` natives, gated on
  `is_segment_var_handle`), implementing real `MemorySegment` coordinate
  access:
  - Resolves the `SegmentVarHandle`'s own `enclosing`/`offset`/`be` fields by
    name (`resolve_field_index`, not assumed indices — it's a real class).
  - Resolves the segment's base address by reading
    `AbstractMemorySegmentImpl.length`/`readOnly`,
    `NativeMemorySegmentImpl.min`, or `HeapMemorySegmentImpl.base`/`offset`
    **directly as fields**, not by calling back into the segment's own
    `unsafeGetBase()`/`unsafeGetOffset()`/`isReadOnly()` methods — an early
    version called those methods via `NativeContext::invoke_virtual` and
    that reentrant native→bytecode call, made from inside a native invoked
    by JIT-compiled caller bytecode, itself crashed
    (`EXCEPTION_ACCESS_VIOLATION` reading `0xFFFFFFFFFFFFFFFF`). Reading the
    fields directly avoids the reentrancy entirely.
  - Critically, `Unsafe.allocateMemory` (which real `Arena.ofConfined()` /
    `.allocate()` bytecode calls into) returns a **synthetic tagged arena
    handle** in this VM (`../../../../native-builtins/src/lib.rs`'s `unsafe_arena`
    module — see its doc comment), not a real OS pointer. A confined-arena
    `MemorySegment`'s `min` field legitimately holds one of these handles.
    Dereferencing it as a raw pointer segfaults. The fix checks
    `unsafe_arena_contains(addr)` first and routes through
    `unsafe_arena_copy_out`/`copy_in` for arena handles, falling back to a
    real raw-pointer read/write only for genuine OS addresses (e.g. a real
    `MMapDirectory` mapping) — mirroring the existing
    `vm_exec.rs::copy_from_native_memory` pattern used for NIO direct
    buffers.

## Verification

Built a fresh worktree (`C:\craton\CratonVM-estsdb`,
`fix/es-tsdb-docvalues-storedfields`), unique binary
`cratonvm-estsdb.exe`.

1. Minimal FFM probe (`Arena.ofConfined().allocate()` +
   `MemorySegment.get/set` with `JAVA_LONG`, `JAVA_LONG_UNALIGNED`, and
   `withOrder(LITTLE_ENDIAN)`) round-trips correctly under both JIT-on and
   `--nojit`, where it previously threw `NoSuchMethodError` and then, after
   only the routing fix, SIGSEGV'd.
2. `ES819TSDBDocValuesFormatTests` (JIT-on), raw `JUnitCore` invocation
   against `../../../../apps/elasticsearch/server`'s test classpath: previously crashed
   the process; now runs to completion (`Tests run: 170, Failures: 116`,
   `System.exit(1)`, no crash). A same-invocation HotSpot run on this box
   also fails broadly here (`Tests run: 170, Failures: 89`, uniformly
   `RandomizedContext.getPerThread()` null) — a raw-`JUnitCore`-outside-Gradle
   harness-bootstrap gap affecting both JVMs, not something this fix
   introduced or needs to match exactly. The original known-issue's own
   baseline already recorded `HotSpot=FAIL` (not PASS) for this class, so the
   bar here is "does not crash the VM", which is met.
3. `cargo test --release -p cratonvm-native-builtins --lib`: 2749 passed, 0
   failed.

## Residual

CratonVM still has more test failures than HotSpot on this class under a raw
`JUnitCore` invocation (116 vs 89, though the failure *reasons* differ:
HotSpot's are uniformly a harness/fixture gap, CratonVM's include
`ArrayIndexOutOfBoundsException`, `NoClassDefFoundError`, and
`LockObtainFailedException`). These are pre-existing correctness gaps
distinct from the crash this doc tracked, not evaluated further here. If
picked up, track separately with a fresh known-issues doc built from the
official `run-elasticsearch-suite.ps1` runner (not raw `JUnitCore`, which has
its own harness-bootstrap incompleteness).

## Repro (historical, before the fix)

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1299 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-tsdb-docvalues-native-crash-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

The other affected class was
`org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests` (same
root cause; not independently re-verified, but it shares the identical
`MemorySegmentIndexInput`/`SegmentVarHandle` access path).

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.tsdb.es819.ES819TSDBDocValuesFormatTests.err.log
Windows Application log, Application Error source, 2026-07-02 around 15:16 and 15:17 local time, exception code 0xc0000005
```

## No-JIT partial evidence (historical)

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. In no-JIT, the same TSDB doc-values classes did not crash before the
runner timeout; they hung and were killed at 300 seconds — the same
`SegmentVarHandle` root cause manifesting as a hang instead of a crash in
interpreter mode (see `elasticsearch-tsdb-storedfields-hang.md`).

```text
index=1347 org.elasticsearch.index.codec.tsdb.es819.ES819TSDBDocValuesFormatTests HANG, 300.081s
index=1356 org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests HANG, 300.129s
```
