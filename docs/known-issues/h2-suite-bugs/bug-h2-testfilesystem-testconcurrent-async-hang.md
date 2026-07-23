# `TestFileSystem.testConcurrent` hangs/OOMs against the `async:` filesystem — JIT OSR `CompilationPolicy` deadlock (suspected)

## Status
**OPEN.** Newly discovered 2026-07-23 while closing out
`bug-h2-files-setposixfilepermissions-FIXED.md`'s residual chain. Not fixed
in this session — root cause is narrowed to the JIT's on-stack-replacement
(OSR) subsystem, but the actual fix (correct locking discipline in
`jit/src/tiered.rs`'s `CompilationPolicy`/`request_osr`) needs dedicated JIT
investigation.

## Severity
**MEDIUM** — only reachable through `org.h2.test.unit.TestFileSystem`'s
`testConcurrent` sub-test running against the `async:` filesystem prefix (a
two-thread, 10000-iteration stress loop doing concurrent
`AsynchronousFileChannel.read`/`write`/`position`/`truncate`/`reopen`). This
specific combination — real concurrent Java threads driving a hot loop that
also triggers `LinkedHashMap`'s `removeEldestEntry()` eviction callback deep
inside a native method — was never reached by any other test class before
this session's `AsynchronousFileChannel` fixes (see below) unblocked
`TestFileSystem` far enough to get here for the first time.

## How this was found
Closing `bug-h2-files-setposixfilepermissions-FIXED.md`'s "testSimple/
testRandomAccess/testMoveTo still failing" residual chain required fixing a
sequence of real, independent bugs, each of which unblocked execution one
step further into `TestFileSystem`:

1. `sun/nio/ch/FileChannelImpl.truncate` (the original doc's headline fix)
   didn't check `writable` before truncating (concrete-class override
   shadows real bytecode's `NonWritableChannelException` check) — FIXED.
2. Same method didn't respect the "no-op when new size >= current size"
   contract, growing files instead of leaving them alone — FIXED.
3. `Files.move(Path, Path, CopyOption...)` used `std::fs::rename` directly,
   which is unconditional POSIX rename semantics — never checked
   `REPLACE_EXISTING`, so it silently overwrote existing targets instead of
   throwing `FileAlreadyExistsException` — FIXED.
4. `AsynchronousFileChannel.read`/`write` (native-io) wrapped a bare
   `Value::Int` in their completed `Future`, instead of a boxed
   `java.lang.Integer` — `Future.get()`'s real bytecode does `checkcast
   Integer` on the result, crashing the VM ("internal error: checkcast: not
   an object reference") the first time `TestFileSystem.testConcurrent`
   exercised the `async:` filesystem — FIXED.
5. `AsynchronousFileChannel.tryLock(long, long, boolean)` was completely
   unregistered (`AbstractMethodError`) — FIXED (builds a real
   `sun/nio/ch/FileLockImpl`; its `release()` needed a companion override
   since the real bytecode's `instanceof FileChannelImpl /
   AsynchronousFileChannelImpl` dispatch matches neither for our synthetic
   channel).
6. `AsynchronousFileChannel.write`/`truncate` didn't check writability,
   throwing a generic `IOException` instead of `NonWritableChannelException`
   — FIXED.

With all six fixed, `TestFileSystem` progresses past `testSimple`,
`testRandomAccess`, `testMoveTo`, `testDirectories`, `testTempFile`, and
reaches `testConcurrent` against the `async:` prefix — at which point it
**hangs** (300s harness timeout, 0% CPU, genuinely parked — not spinning).

## Root cause (partially narrowed, not fixed)
A live `gdb -batch -ex 'thread apply all bt'` attach to the hung process
(Azure Linux host, `--java-home` real-JDK mode, JIT on) shows 4 threads:

- **`main-vm`**: blocked acquiring a `parking_lot::RawMutex` guarding
  `cratonvm_jit::tiered::CompilationPolicy`, inside `request_osr()`
  (`jit/src/tiered.rs:1020`), called from `try_osr_with_backoff`
  (`interpreter.rs:7534`). This frame is reached from deep inside
  `native_lhm_put_evict`/`native_lhm_put`
  (`native-collections/src/lib.rs`) — i.e. a `LinkedHashMap.put()` call
  (likely H2's own internal LRU cache) that calls back into interpreted
  Java bytecode for its `removeEldestEntry()` override, and THAT bytecode
  is hot enough to trigger an OSR compile request mid-loop.
- **`org.h2.test.uni` (the H2 test's own background `Task` thread)**: blocked
  in `monitor_enter_blocking` (`vm_exec.rs:1461`) — a Java `synchronized`
  monitor-enter — reached via the *same* `{closure#168}` /
  `native_lhm_put_evict`-shaped call path as `main-vm`. It's waiting to
  enter a Java monitor that (by elimination — the only two "real" threads in
  the process being these two) `main-vm` must already hold, from further
  down its own stack, while it separately blocks trying to acquire the JIT's
  `CompilationPolicy` mutex.
- **`cratonvm-jit-co` (background JIT compiler thread)**: idle, parked on
  `compiler_loop`'s job-queue condvar (`jit/src/tiered.rs:1372`) — NOT
  currently holding the `CompilationPolicy` mutex in this snapshot.
- **`Common-Cleaner`**: idle in its normal `ReferenceQueue.remove` polling
  sleep — not a factor.

This shape (main-vm holds a Java monitor entered earlier in its own call
stack, then blocks on `CompilationPolicy`; the H2 background thread blocks
trying to enter that same monitor) is consistent with a genuine AB-BA-style
deadlock, but the actual holder of `CompilationPolicy` at the moment
`main-vm` blocks was not caught in the snapshot (the compiler thread was
idle) — so the missing piece is finding what *transiently* held it just
before `main-vm`'s `lock()` call parked, or whether `request_osr()`/
`CompilationPolicy` has a documented non-reentrant-locking hazard when
called from a native-method-reentrant context (LinkedHashMap eviction
callback) under real concurrent multi-thread contention.

**Rerunning with `--nojit` did NOT simply pass** — it hit a *different*
failure, `FATAL: G1: out of heap space for object allocation (56 bytes)`,
before any hang could be observed (that specific invocation didn't set
`--Xmx`, unlike the suite runner's `--Xmx 1g`, so this isn't a clean
apples-to-apples "OSR is/isn't the cause" comparison — it only rules out
"the interpreter-only path trivially succeeds," it does not confirm or
refute the OSR-lock hypothesis under fully matched heap conditions). A
proper `--nojit --Xmx 1g` control run is the next step for whoever picks
this up.

## Distinct from `bug-h2-files-setposixfilepermissions-FIXED.md`
This is a genuinely separate subsystem (JIT OSR / thread synchronization /
`LinkedHashMap` eviction-callback reentrancy) from that doc's FileChannel/
POSIX-permissions scope — the six fixes above are all independently correct
and verified (each one's fix was confirmed by observing `TestFileSystem`'s
failure move to the *next* distinct assertion/exception, never regressing to
an earlier one), they just happened to be required, one after another, to
even reach this new bug. Filed separately per this repo's known-issues
convention (a class-level residual chain closes only when the class is a
clean PASS; this is the item keeping `TestFileSystem` open now that the
POSIX-permissions-doc's own chain is otherwise exhausted).

## Repro
```
cd apps/h2database-suite-runner   # or a checkout with H2_ROOT set
TMPDIR=/data/data/tmp H2_ROOT=<h2 checkout>/h2 \
  CRATONVM_BIN=<fixed cratonvm binary> JDK25=/home/victor/jdk25 \
  bash run-h2-suite.sh run --category all --only 'TestFileSystem' \
  --jdk real --jit on --class-to 300
# -> HANG at 300.0s
```
A live-gdb attach (`sudo gdb -p <pid> -batch -ex 'thread apply all bt'`)
while it's hung reproduces the 4-thread snapshot described above.

## Suggested next steps
1. A proper `--nojit --Xmx 1g` control run (matching the suite runner's
   actual flags) to determine whether the hang is OSR-exclusive or a
   broader `LinkedHashMap`-reentrancy/monitor issue independent of JIT.
2. If OSR-exclusive: audit `jit/src/tiered.rs`'s `request_osr`/
   `CompilationPolicy` locking for a path where the lock is held across a
   call that can itself need to enter a Java monitor already held by
   another thread waiting on that same lock.
3. If not OSR-exclusive: look for where a `LinkedHashMap.put()`-triggered
   `removeEldestEntry()` callback (re-entering interpreted bytecode from
   inside a native method) can hold a Java monitor across a call that
   blocks on VM-internal machinery, under real multi-thread contention.
4. A minimal, non-H2 synthetic repro (two threads hammering a
   `LinkedHashMap` with an overridden `removeEldestEntry()` hot enough to
   trigger OSR) would isolate this from H2/AsynchronousFileChannel entirely
   and make it tractable to fix without needing the full H2 suite harness.
