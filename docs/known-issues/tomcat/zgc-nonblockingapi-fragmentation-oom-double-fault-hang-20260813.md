# `TestNonBlockingAPI` hangs under ZGC — fragmentation OOM cascades into a double fault, not a deadlock

| | |
|---|---|
| **Status** | OPEN, found 2026-08-13. **ZGC-specific** — G1 passes clean (44/44, zero OOM warnings), Generational's own result for this class in this run's shard was a plain HANG with no OOM signature (separate, not investigated here — plausibly the already-documented `[moving-young] fallback` throughput collapse). |
| **Symptom** | Process runs to the full timeout with no further output after a specific point — reads as a hang from the outside, but the log shows exactly why it stops progressing. |
| **Discovered** | 2026-08-12/13, complete 651-class Tomcat suite run under all 3 GC backends, cross-checked via a 93-class ZGC-only rerun. |

## It's not stuck — it's dead, and nothing downstream notices

`TestNonBlockingAPI` has 44 sub-tests. Under ZGC the run gets to test #42 (`Starting test case` printed 41 times, i.e. mid-way through the 42nd), then this:

```
WARN cratonvm::gc::guard: zgc: arena allocation failed — this heap does not compact, so the
bump cursor never rewinds and reclaimed space returns only as free-list holes.
`largest_free_block < request` with a large `free_list_bytes` means fragmentation, not
exhaustion. request=2789392 used=2147351584 capacity=2147483648 free_list_bytes=1992876920
largest_free_block=524192 free_spans=3892 free_span_sizes=309

WARN cratonvm_vm::runtime::native_oom: native allocation could not be satisfied — raising a
catchable java.lang.OutOfMemoryError length=1394686 what="primitive array"

ERROR cratonvm_vm::vm::vm_exec: Native method panic caught in <cb@0x...>: unknown native
method panic (native invoked from
org/apache/catalina/nonblocking/TestNonBlockingAPI$TestAsyncReadListener$1.run()V)

Thread Thread-4888 terminated with error: ExceptionThrown(ObjectRef { ptr: 0x... })
(dispatchUncaughtException also failed: ExceptionThrown(ObjectRef { ptr: 0x... }))
```

No further log output follows for the rest of the 3000s budget. Three things stacked here, worth reading separately:

1. **The allocation genuinely should have succeeded.** `used=2147351584` sits right at the `-Xmx 2g` `capacity=2147483648`, but `free_list_bytes=1992876920` — almost 2GB — is available as reclaimed space. The request (2,789,392 bytes) fails only because the *largest single contiguous* free span is 524,192 bytes: 3,892 free spans holding ~2GB add up fine but none of them is big enough alone. This VM's own `zgc::guard` diagnostic names this precisely: "this heap does not compact... fragmentation, not exhaustion."
2. **The resulting `OutOfMemoryError` triggers a native method panic**, inside `TestAsyncReadListener$1.run()` — a callback invoked from native code. Whatever this panic path does, it isn't a clean Java-level OOM the test's own error handling gets a chance to react to.
3. **The uncaught-exception path double-faults**: `Thread-4888` dies with the OOM-derived exception, and the JVM's own `dispatchUncaughtException` handler *also* throws while trying to report it. Whatever the main JUnit thread was waiting on this thread to do — signal completion, release a latch, close a connection the next sub-test depends on — never happens, and the process just sits there for the rest of the timeout window with nothing left to log.

## Confirmed ZGC-specific, not host load or volume

The direct control is in the same suite run: **G1 passes this exact class clean, `OK (44 tests)`, in the same host conditions, with zero OOM warnings anywhere in its log.** G1 compacts; this ZGC backend does not (`RELOCATION_REQUESTED = false` — see `docs/internal/arch-2026-07-26/moving-young-corruption-rootcause.md` and `gc-backend-3way-fullsuite-comparison-20260810.md` for the same non-compacting characterization elsewhere). This matches the already-known "ZGC needs measurably more heap for the same work" pattern noted for `ZipContentTests` (OOMs at `-Xmx 2g` under ZGC, passes at 3g, passes at 2g under the default collector) in that same 3-way comparison doc — this is the same structural cost, but with a much worse failure mode than a clean OOM: a double fault that reads as a hang instead of a catchable, test-visible failure.

## Two separate things worth fixing, not one

- **The proximate, easy one**: whatever `dispatchUncaughtException` does when handling an exception that itself needs to construct/throw should not be able to fail in a way that leaves the thread's waiters permanently blocked. At minimum this turns a recoverable OOM into an unrecoverable hang.
- **The underlying one**: ZGC's non-compacting allocator is fragmentation-prone under a workload with many varied-size, high-churn allocations (this is exactly what an async NIO read-listener callback chain looks like) well before it's actually out of memory in aggregate. Whether that's worth a real fix (some form of segregated free lists, or periodic compaction despite the design's current stance) or just a documented "ZGC wants more headroom than the other two backends" caveat is a product call, not something this doc resolves.

## Not chased further here

Root cause of the native method panic itself (what specifically in `TestAsyncReadListener$1.run()`'s native call path panics on OOM rather than propagating a normal Java exception) is not investigated. Neither is why `dispatchUncaughtException` itself throws.

## Reproduction

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
apps\tomcat-suite-runner\run-one.ps1 -Class org.apache.catalina.nonblocking.TestNonBlockingAPI -Exe <cratonvm.exe>
```

No `-XX` flag needed if ZGC is the default binary; pass `-XX:+UseZGC` explicitly otherwise. Given the fragmentation trigger depends on allocation history over ~42 sub-tests, expect this to be sensitive to heap size (`-Xmx`) and possibly to which sub-tests ran first.
