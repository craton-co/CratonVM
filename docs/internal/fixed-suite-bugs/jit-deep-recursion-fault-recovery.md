---
name: jit-deep-recursion-fault-recovery
description: Retired JIT infrastructure item split out from the archived SpringRepositoriesExtensionTests handoff. Compiled non-tail self-recursion now trips a JIT stack-headroom guard and raises a catchable StackOverflowError before the native guard page.
metadata:
  type: internal
  area: jit, x64, stack, fault-recovery, throughput
---

# JIT deep-recursion fault recovery

**Status:** FIXED / RETIRED 2026-07-08. The user-visible
`SpringRepositoriesExtensionTests` hang was already fixed and archived at
[`spring/springrepos-extension-hang-jit-throughput-and-deep-recursion.md`](spring/springrepos-extension-hang-jit-throughput-and-deep-recursion.md).
The remaining infrastructure gap is closed by avoiding the exhausted-stack fault
context entirely for the compiled self-recursive shape that produced the native
failure: non-tail static self-recursive JIT call sites now stay as direct calls,
but the x64 backend emits a call to `jit_self_call_stack_guard` immediately
before the recursive `CALL`. The guard compares the current native stack pointer
with a per-thread floor, stashes a catchable `java/lang/StackOverflowError` when
headroom is exhausted, and returns the existing JIT exception sentinel so normal
interpreter exception routing handles Java `catch (StackOverflowError)` blocks.

Stack-bang probes remain as containment and diagnostics for oversized frames or
unexpected native faults. Production recovery no longer depends on doing
deoptimization or throwable construction on an already-exhausted OS stack.

## What Is Fixed

Current `dev` has these containments:

- Recursive same-method and compile-cycle direct-call edges route through guarded
  dispatch where possible.
- The guarded ANTLR validation lift keeps the known-bad
  `PredictionContext` equality/hash cluster interpreted while allowing other
  `groovyjarjarantlr4/` methods to JIT with
  `CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/`.
- The x64 single-pass backend emits page-by-page stack-bang probes in normal
  compiled method prologues, with one page of post-frame headroom.
- The Windows crash handler can name the faulting JIT method when
  `CRATONVM_DBG_JIT_NAMES=1`, including `EXCEPTION_STACK_OVERFLOW`.

## 2026-07-01 Validation

Using the unique retry binary
`C:\craton\CratonVM-codex-springrepos-jit-retry-20260701-1\cvspringretry-20260701-1.exe`
and the Spring Boot buildSrc runner classpath from
`C:\craton\CratonVM\apps\spring-boot\buildSrc`:

```powershell
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
$env:CRATONVM_JIT_ALLOW_PACKAGES = 'groovyjarjarantlr4/'
```

`RunJUnitV org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests`
passed:

```text
JUNIT_RESULT tests=11 passed=11 failed=0 skipped=0 aborted=0
```

`GroovyScriptProbe` also parsed the real `SpringRepositorySupport.groovy` under
the guarded ANTLR lift:

```text
WARMUP(trivial) parsed in 6654ms
SCRIPT parsed OK: SpringRepositorySupport in 43549ms
```

`GroovyNestProbe` did not reproduce the old native-stack crash in the historical
failure window. It reached:

```text
WARMUP 11662ms
depth=20 parsed 20421ms
depth=40 THREW CompilationFailedException: parsing failed
depth=80 THREW CompilationFailedException: parsing failed
```

The process was still CPU-bound at depth 160 after 526 seconds and was manually
stopped. Treat that as a remaining throughput/deep-recursion stressor, not as a
fixed correctness result.

## 2026-07-08 Fix Validation

Added an in-tree regression that does not depend on the Spring Boot runner
artifacts:

- `../../../vm/tests/resources/cratonvm/JitDeepRecursionFaultRecovery.java` provides a
  small non-tail self-recursive method and a Java `catch (StackOverflowError)`
  wrapper.
- `../../../vm/tests/jit_deep_recursion_fault_recovery.rs` lowers the JIT threshold,
  disables background compilation for determinism, warms the fixture until a
  JIT code range is published, then verifies the deep compiled recursion returns
  through the Java catch block.

Validated on the Azure host worktree:

```text
cargo test -p cratonvm-vm --test jit_deep_recursion_fault_recovery -- --nocapture
test compiled_non_tail_deep_recursion_throws_catchable_stack_overflow ... ok
```
