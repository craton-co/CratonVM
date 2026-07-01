---
name: jit-deep-recursion-fault-recovery
description: Residual JIT infrastructure item split out from the archived SpringRepositoriesExtensionTests handoff. Stack-bang containment and guarded recursive dispatch are in place, but a native stack fault in compiled deep recursion still cannot be converted into a resumable deopt or catchable StackOverflowError.
metadata:
  type: known-issue
  area: jit, x64, stack, fault-recovery, throughput
---

# JIT deep-recursion fault recovery

**Status:** OPEN / LATENT. The user-visible
`SpringRepositoriesExtensionTests` hang is fixed and archived at
[`docs/internal/springrepos-extension-hang-jit-throughput-and-deep-recursion.md`](../internal/springrepos-extension-hang-jit-throughput-and-deep-recursion.md).
This remaining issue is the infrastructure half: compiled deep recursion now has
stack-bang containment, but not production-grade recovery from the fault context.

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

## Remaining Work

Finish the stack-overflow-aware fault path:

- Recognize `EXCEPTION_STACK_OVERFLOW` / stack-bang faults as recoverable when
  they occur in JIT code.
- Avoid doing stack-heavy work on the exhausted native stack.
- Convert the trapped JIT frame into a resumable deopt to the interpreter or
  raise a catchable `StackOverflowError`.
- Add a deterministic in-tree regression that does not depend on the out-of-tree
  Spring Boot runner artifacts.

Until that exists, keep this as a known issue even though the SpringRepos test is
green.
