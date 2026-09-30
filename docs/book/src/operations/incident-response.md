# Incident Response

This runbook provides a repeatable way to reduce a CratonVM failure without
destroying the evidence that explains it.

## First response

1. Preserve the exact executable, JDK, application artifact, arguments, and
   relevant `CRATONVM_*` configuration.
2. Record exit status, stdout, stderr, timestamp, host load, RSS, and cgroup
   events.
3. Decide whether the symptom is a crash, hang, wrong result, resource failure,
   startup/compatibility failure, or performance regression.
4. Reproduce with the smallest safe workload.
5. Change one axis at a time.

Do not begin by disabling verification, changing collectors, lowering the heap,
and switching JDKs simultaneously. That may hide the defect while making the
result impossible to interpret.

## Diagnostic ladder

Use this order for a deterministic reproduction:

```text
original CratonVM command
    ↓
same command with --nojit
    ↓
same classes and arguments on the pinned HotSpot JDK
    ↓
focused logging or native audit
    ↓
minimal reproducer
```

Interpret the four outcomes:

| CratonVM JIT | CratonVM `--nojit` | HotSpot | Initial classification |
|--------------|--------------------|---------|------------------------|
| fail | pass | pass | JIT lowering, compiled runtime bridge, deopt, or JIT/GC interaction |
| fail | fail | pass | interpreter, loader/verifier, native, GC, or unsupported behavior |
| fail | fail | fail | likely application or test assumption; compare exact exceptions |
| pass | pass | pass | intermittent/resource/timing issue; repeat with captured load and seed |

Synthetic-JDK mode is a separate differential axis. A difference between real
and synthetic mode identifies library-path ownership; it does not by itself
prove which side is correct.

## Crash or abort

Preserve:

- the complete stderr and exit code;
- native core/minidump if enabled;
- OS kernel OOM or access-violation records;
- the executable checksum and symbols;
- the last JFR/log segment; and
- the minimal classpath that still reproduces.

On Linux, distinguish a VM abort from an OOM kill. Exit `137` or `SIGKILL`
with a kernel/cgroup OOM event is a resource failure, not a Rust panic or Java
exception.

If a release build is difficult to symbolize, reproduce with the same commit
and configuration in a build that retains line-table debug information. Do not
use debug-build performance to estimate release performance.

## Hang or deadlock

1. Capture process CPU and thread state.
2. Use `--stack-dump-on-timeout`.
3. Repeat once with `--nojit`.
4. If CPU is near zero, inspect blocking I/O and monitor ownership.
5. If one core is saturated, inspect interpreter/JIT loops and compilation.
6. If every runnable thread is delayed, record host load and CPU throttling.

The canonical VM lock order lives in `vm/src/runtime/lock_order.rs`. A proposed
lock-order fix must preserve that hierarchy rather than merely changing one
call site until the reproducer stops hanging.

## Wrong result or exception

A wrong result has priority over speed. Capture a deterministic checksum or
serialized output, and compare:

- CratonVM JIT;
- CratonVM `--nojit`; and
- HotSpot using the same Java classes and inputs.

Preserve exception class, message, cause chain, and the bytecode offset if
available. Do not compare only human-readable messages when the semantic
contract is exception type and control flow.

For a compiler-only mismatch, reduce the failing method without changing its
bytecode shape unnecessarily. Exception handlers, monitors, long/double slots,
and interface dispatch are all shape-sensitive.

## Out-of-memory or container eviction

Identify which limit failed:

- Java heap capacity;
- process address space;
- native allocation;
- thread stacks;
- code/class metadata;
- host memory; or
- cgroup memory.

`-Xmx` is not a process-RSS limit. Leave room for non-heap state. If the kernel
killed the process, increasing `-Xmx` makes the situation worse unless the
container limit also changes.

Compare GC diagnostics and live-set behavior before changing collectors. A
workload that retains most allocations needs capacity or application changes,
not a lower collection threshold.

## Startup, class-loading, and native failures

Verify:

1. `--java-home` points at the intended complete JDK;
2. the classpath/module path is identical to the HotSpot run;
3. the main class and manifest are correct;
4. verification is enabled;
5. the first missing class/method is preserved; and
6. the missing-native audit identifies the actual unsupported entry point.

Do not add a synthetic stub for a real application-visible JDK class merely to
make startup continue. The project policy is to execute real class bytecode and
implement only the genuine VM/native boundary.

## Performance regression

Use the procedure in [Performance Tuning](../performance/tuning.md):

1. verify checksums;
2. build pre-change and post-change commits the same way;
3. give them unique names;
4. alternate runs on one pinned host;
5. use medians and record every sample;
6. reject or annotate loaded-host measurements; and
7. profile both binaries.

Never re-anchor a performance baseline without an evidence document explaining
the semantic checks, host, binaries, samples, and accepted trade-off.

## Closing an incident

An incident is closed only when:

- the cause is identified or the unsupported boundary is explicit;
- a regression test or executable probe covers the failure;
- the fix passes JIT and interpreter modes where applicable;
- relevant documentation is updated; and
- the issue document moves from `docs/known-issues` to `docs/internal`.

Historical fixed documents remain useful evidence, but they must not stay in
the open-issues directory.
