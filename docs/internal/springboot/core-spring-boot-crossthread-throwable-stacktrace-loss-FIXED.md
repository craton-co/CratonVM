# FIXED: `Throwable.getStackTrace()` returns empty when queried from a thread other than the one that filled it in

**Resolved 2026-07-18.** Captured Throwable frames now live in a VM-wide,
non-owning registry, so they survive producer-thread termination and can be
read from any Java thread. GC forwards live registry handles and sweeps dead
ones. `Throwable.printStackTrace` was completed as the associated residual:
it now renders suppressed throwables and common-frame elision like HotSpot.

Validated with the unique `cratonvm-crossthread-throwable-stacktrace-20260718.exe`:

- `StandardStackTracePrinterTests`: 23/23 PASS with JIT and `--nojit`.
- `StructuredLoggingJsonPropertiesTests`: 19/19 PASS with JIT and `--nojit`.

**Status: OPEN — found 2026-07-17 (root cause confirmed at file:line precision)**

## Symptom

2 classes, 15 individual test failures, all built on Spring Boot's shared
`TestException` test helper
(`apps/spring-boot/core/spring-boot/src/test/java/org/springframework/boot/logging/TestException.java`),
which deliberately constructs its exception chain **on a separate,
already-terminated thread** so the resulting stack trace is short and
deterministic:

```java
public static Exception create() {
    CreatorThread creatorThread = new CreatorThread();
    creatorThread.start();
    creatorThread.join();
    return creatorThread.exception;   // built entirely inside CreatorThread.run()
}
```

Every test that calls `TestException.create()` and later inspects the
resulting exception's frames via `Throwable.getStackTrace()` (directly, or
through `StandardStackTracePrinter`, which calls it internally) gets **zero
frames** back under CratonVM — even after AssertJ's own
"ignoring newline differences" normalization, which rules out a
line-separator/formatting issue (see the separate, unrelated
`core-spring-boot-structured-log-println-line-separator-bug.md` for that
class of bug):

```
JUnit Jupiter:StandardStackTracePrinterTests:withoutSuppressedHidesSuppressed()
  => org.opentest4j.AssertionFailedError:
Expecting actual:
  "java.lang.RuntimeException: exception
Caused by: java.lang.RuntimeException: cause
Caused by: java.lang.RuntimeException: root
"
to be equal to:
  "java.lang.RuntimeException: exception
	at org.springframework.boot.logging.TestException.actualCreateException(TestException.java:NN)
	at org.springframework.boot.logging.TestException.createException(TestException.java:NN)
	at org.springframework.boot.logging.TestException.createTestException(TestException.java:NN)
	at org.springframework.boot.logging.TestException$CreatorThread.run(TestException.java:NN)
Caused by: java.lang.RuntimeException: cause
	at org.springframework.boot.logging.TestException.createCause(TestException.java:NN)
	at org.springframework.boot.logging.TestException.createTestException(TestException.java:NN)
	... 1 more
Caused by: java.lang.RuntimeException: root
	at org.springframework.boot.logging.TestException.createTestException(TestException.java:NN)
	... 1 more
"
when ignoring newline differences ('\r\n' == '\n')
     org.springframework.boot.logging.StandardStackTracePrinterTests.assertThatCleanedStackTraceMatches(StandardStackTracePrinterTests.java:389)
     org.springframework.boot.logging.StandardStackTracePrinterTests.withoutSuppressedHidesSuppressed(StandardStackTracePrinterTests.java:128)
```

The actual output has every header line (`Caused by:`/`Wrapped by:`/
`Suppressed:`) but **not a single `at ...` frame line anywhere** — 13/23
tests in `StandardStackTracePrinterTests` fail this exact way, and the same
shape appears for `StructuredLoggingJsonPropertiesTests$StackTraceTests`
(2 tests), which uses the identical `TestException.create()` helper.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot.org.springframework.boot.logging.StandardStackTracePrinterTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot.org.springframework.boot.logging.structured.StructuredLoggingJsonPropertiesTests.out.log`

## Root cause (confirmed at file:line precision)

`StandardStackTracePrinter` (`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/logging/StandardStackTracePrinter.java:415`)
obtains its frames the standard-library way:

```java
this.frames = (throwable != null) ? throwable.getStackTrace() : null;
```

In CratonVM, captured backtraces are **not** attached to the `Throwable`
object itself (as real JDK's opaque `backtrace` field effectively is —
thread-independent once captured). Instead they live in a **per-thread**
map:

```rust
// vm/src/threading/jvm_thread.rs:308
/// Stack traces captured by `fillInStackTrace`, keyed by identity hash.
pub throwable_stacks: HashMap<i32, Vec<StackTraceEntry>>,
```

Both the write side (`fillInStackTrace`, called from the real `Throwable`
constructor) and the read side (`getStackTrace()`) go through
`self.thread.throwable_stacks`, where `self.thread` is **whichever
`JvmThread` is currently executing the interpreter loop** —
`vm/src/vm/vm_exec.rs`:

```rust
// vm/src/vm/vm_exec.rs:4068 — write side, runs on the throwing thread
fn capture_stack_trace(&mut self, throwable_hash: i32) -> Vec<StackTraceEntry> {
    ...
    if throwable_hash != 0 {
        self.thread.throwable_stacks.insert(throwable_hash, trace.clone());
    }
    trace
}

// vm/src/vm/vm_exec.rs:4096 — read side, runs on whichever thread calls getStackTrace()
fn get_stack_trace(&self, throwable_hash: i32) -> Option<&[StackTraceEntry]> {
    self.thread.throwable_stacks.get(&throwable_hash).map(|v| v.as_slice())
}
```

`vm_init.rs` documents the same limitation explicitly a few lines above its
own caller:

```
// vm/src/vm/vm_init.rs:5577
// Returns frames from the main thread's `throwable_stacks` only; other
// threads' `throwable_stacks` are not aggregated here, ...
```

`TestException.create()` is exactly the shape that exposes this: the
exception chain is built entirely inside `CreatorThread.run()` — so
`fillInStackTrace()` inserts the captured frames into **`CreatorThread`'s**
`JvmThread.throwable_stacks` map, keyed by the exception's identity hash.
`creatorThread.join()` returns, `CreatorThread` terminates (its `JvmThread`
is torn down/no longer current), and the *main test thread* later calls
`throwable.getStackTrace()` (via `StandardStackTracePrinter`) — which reads
`self.thread.throwable_stacks` for the **main thread's** `JvmThread`. That
map never had an entry for this exception's identity hash (only
`CreatorThread`'s did), so the lookup misses and `getStackTrace()` returns
zero frames — exactly the observed "headers present, all frame lines
missing" shape. Real HotSpot's backtrace is thread-independent by
construction, so the identical test passes there.

The interpreter already has a comment acknowledging the failure mode from
the caller side (`vm/src/runtime/interpreter.rs:14942`: `// frames (its
"throwable_stacks" lookup having missed).`), confirming this is a known,
previously-observed gap shape, not a new hypothesis invented this session.

## What would fix it

`throwable_stacks` needs to be keyed/stored somewhere that survives the
throwing thread's lifetime and is visible to any thread — either a
process-wide `HashMap<i32, Vec<StackTraceEntry>>` (matching the fact that
identity hashes are already unique per-object, so no thread-scoping is
actually needed for correctness) or, more robustly, attached directly to
the `Throwable` object's own native-shadow state so it travels with the
object regardless of which thread later reads it. The per-thread structure
in `vm/src/threading/jvm_thread.rs:308` is the field to change; the two
call sites in `vm/src/vm/vm_exec.rs:4068` and `:4096` (and the main-thread
special-case in `vm/src/vm/vm_init.rs:5569-5584`) would need to move to
whatever shared store replaces it.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot` | `org.springframework.boot.logging.StandardStackTracePrinterTests` (13/23 tests) |
| `core/spring-boot` | `org.springframework.boot.logging.structured.StructuredLoggingJsonPropertiesTests` (`StackTraceTests` nested class, 2/2 tests: `createPrinterWhenStandardAppliesCustomizations`, `createPrinterWhenClassNameInjectsConfiguredPrinter`) |
