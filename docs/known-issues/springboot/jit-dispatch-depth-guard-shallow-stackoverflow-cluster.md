# 3-class `StackOverflowError` cluster: shallow, non-repeating traces point at the JIT-dispatch depth guard, not genuine deep recursion

**Status: OPEN**

## Symptom

Three unrelated, well-tested Spring Boot test classes each fail with exactly
one (or two) `java.lang.StackOverflowError` out of an otherwise-passing run,
in a single CratonVM process (JIT on), `crashfail-20260714`:

- `core/spring-boot` — `org.springframework.boot.context.properties.EnableConfigurationPropertiesRegistrarTests`
  (shard1, 6 tests, 1 failed, 134.0s wall)
- `core/spring-boot` — `org.springframework.boot.logging.logback.LogbackLoggingSystemPropertiesTests`
  (shard2, 5 tests, 2 failed, 164.5s wall)
- `module/spring-boot-restclient` — `org.springframework.boot.restclient.RestTemplateBuilderTests`
  (shard7, 52 tests, 1 failed, 112.6s wall)

`EnableConfigurationPropertiesRegistrarTests.registrationWithNoTypeShouldNotRegisterAnything`:

```
=> java.lang.StackOverflowError
   org.springframework.beans.factory.support.AbstractBeanFactory.cacheMergedBeanDefinition(AbstractBeanFactory.java:1502)
   org.springframework.beans.factory.support.DefaultListableBeanFactory.cacheMergedBeanDefinition(DefaultListableBeanFactory.java:1031)
   org.springframework.beans.factory.support.AbstractBeanFactory.getMergedBeanDefinition(AbstractBeanFactory.java:1464)
   org.springframework.beans.factory.support.AbstractBeanFactory.getMergedBeanDefinition(AbstractBeanFactory.java:1400)
   org.springframework.beans.factory.support.AbstractBeanFactory.getMergedBeanDefinition(AbstractBeanFactory.java:1383)
   org.springframework.beans.factory.support.AbstractBeanFactory.getMergedLocalBeanDefinition(AbstractBeanFactory.java:1369)
   org.springframework.beans.factory.support.AbstractBeanFactory.getMergedLocalBeanDefinition(AbstractBeanFactory.java:1365)
   org.springframework.beans.factory.support.DefaultListableBeanFactory.doGetBeanNamesForType(DefaultListableBeanFactory.java:627)
   org.springframework.beans.factory.support.DefaultListableBeanFactory.getBeanNamesForType(DefaultListableBeanFactory.java:604)
   org.springframework.beans.factory.support.DefaultListableBeanFactory.getBeanNamesForType(DefaultListableBeanFactory.java:598)
   org.springframework.boot.context.properties.EnableConfigurationPropertiesRegistrarTests.registrationWithNoTypeShouldNotRegisterAnything(...:95)
```
(11 frames total, no repetition.)

`LogbackLoggingSystemPropertiesTests` (2 of 5 tests, same shape):

```
=> java.lang.StackOverflowError
   org.springframework.boot.logging.LoggingSystemProperties.getConsole(LoggingSystemProperties.java:104)
   org.springframework.boot.logging.logback.LogbackLoggingSystemProperties.getConsole(LogbackLoggingSystemProperties.java:78)
   org.springframework.boot.logging.logback.LogbackLoggingSystemPropertiesTests.consoleCharsetWhenNoPropertyUsesSystemConsoleCharsetWhenAvailable(...:110)
```
(3 frames total — `getConsole` isn't even recursive in Spring's real source.)

`RestTemplateBuilderTests.customizersShouldBeAppliedLast`:

```
=> java.lang.StackOverflowError
   org.springframework.http.client.support.HttpAccessor.setRequestFactory(HttpAccessor.java:78)
   org.springframework.http.client.support.InterceptingHttpAccessor.setRequestFactory(InterceptingHttpAccessor.java:87)
   org.springframework.boot.restclient.RestTemplateBuilder.configure(RestTemplateBuilder.java:680)
   org.springframework.boot.restclient.RestTemplateBuilderTests.customizersShouldBeAppliedLast(...:425)
```
(4 frames total.)

All three `.err.log`s end the same way, with no panic/backtrace, just a clean
VM shutdown after the exception propagated to the top:

```
[cratonvm] System.exit(1) called — process terminating
```

## Root cause / Analysis

These traces are **anomalously shallow for a `StackOverflowError`**. None of
`getMergedBeanDefinition`/`cacheMergedBeanDefinition`,
`LoggingSystemProperties.getConsole`, or `HttpAccessor.setRequestFactory` are
recursive in real Spring Boot/Framework source, and none of these three test
classes exercise any known-deep call graph (no long circular-bean chains, no
deep proxy nesting) — a real, unbounded-recursion `StackOverflowError` on
HotSpot would show thousands of repeating frames; a real *bounded-but-deep*
recursion would show a long but sensible chain. Neither shape is present
here: each trace is just the ordinary, shallow call path into the failing
test method, repeated zero times. These are well-exercised, widely-used
Spring Boot APIs on trivial unit-test inputs — genuine infinite/deep
recursion in Spring's own code, reproducible on HotSpot too, is not a
credible explanation.

Tracing where CratonVM actually raises `StackOverflowError` turned up a
structural gap that fits this shape exactly:

1. CratonVM has **two independent, thread-local recursion-depth guards**:
   - `EXEC_DEPTH` in `vm/src/runtime/interpreter.rs` (`fn execute`, ~line
     3964) — guards *interpreted* re-entrant `execute` recursion. Every push
     is paired with a live `StackTraceEntry`/`Frame` on `thread.frames`
     (`vm/src/threading/jvm_thread.rs:289`), so a `StackOverflowError` raised
     here carries a real, complete Java stack trace.
   - `JIT_DISPATCH_DEPTH` in `vm/src/jit/helpers.rs` (~line 3984,
     `enter_jit_dispatch`/`jit_invoke_dispatch`/`jit_invoke_virtual_mic`,
     added as "BUG-1 fix: native-stack recursion guard for the JIT→JIT
     dispatch path" per the comment at helpers.rs:3962) — guards *JIT-compiled*
     recursive calls, which "never re-enter `interpreter::execute`" per that
     same comment. This counter is a bare `Cell<u32>` with **no accompanying
     frame metadata** — confirmed by grepping `vm/src/jit/helpers.rs` for
     `thread.frames.push`/`.pop`: there are zero call sites. Every
     `thread.frames.push(frame)` in the codebase is in `interpreter.rs`
     (plus a few init/GC/JVMTI paths), never in the JIT dispatch helpers.

2. Consequence: when `JIT_DISPATCH_DEPTH` trips and
   `raise_jit_stack_overflow` (helpers.rs:4041) constructs the
   `StackOverflowError` via `create_exception_object`/`fillInStackTrace`, the
   captured trace can only reflect whatever *interpreted* frames happen to
   be on `thread.frames` at that moment — none of the actual JIT-compiled
   call chain that drove the counter to its ceiling is represented. A
   `StackOverflowError` raised through this path is therefore **structurally
   incapable of showing its own cause**: the visible trace will always look
   like "the last few interpreted frames before entering compiled code",
   regardless of whether the underlying compiled recursion was 5 levels or
   50,000 levels deep. This matches all three observed traces (they all
   bottom out at a plain non-recursive call from the test into ordinary
   Spring code, with no repetition) and would look identical whether the
   real cause is (a) a genuine bug making JIT-compiled dispatch recurse
   infinitely (e.g. a self-call/dispatch-cache-reroute defect — see
   `reference_jit_selfcall_dispatch_reroute` in project memory for a past,
   already-fixed instance of this class of bug), or (b) the per-thread
   `JIT_DISPATCH_DEPTH`/`EXEC_DEPTH` counters carrying a residual, wrongly
   non-decremented count across earlier passing `@Test` methods in the same
   process (each class here runs `one-process-per-class`, so a per-thread
   leak accumulated over the class's earlier tests would explain why a
   plain, non-recursive later test trips the guard). Both hypotheses are
   consistent with the evidence gathered; distinguishing them needs a live
   repro with `CRATONVM_DBG_JIT_DISASM`/`CRATONVM_EXEC_DEPTH_CEILING`
   instrumentation or an isolated single-test run, which was not performed
   in this pass (see Repro below for the exact command to do that).

3. Secondary, independently-confirmed finding: `vm/src/runtime/call_stack.rs`
   defines a `CallStack`/`StackFrameEntry` type with a *named*, per-method
   frame vector and its own `max_stack_depth` guard (default `8192`,
   `RJ_MAX_STACK_DEPTH`-overridable, see `vm/src/config.rs:394` — itself
   raised from a prior spurious-`StackOverflowError` fix for
   `DefaultListableBeanFactoryTests.extensiveCircularReference`). This module
   is **dead code**: `vm/src/runtime/mod.rs` only declares it
   (`mod call_stack;`) and re-exports `CallStack` (`pub use
   call_stack::CallStack;`); no other file in `vm/src` calls
   `CallStack::push`/`pop`/`new`. The one guard mechanism in the codebase
   that *would* produce a complete, correctly-named stack trace on overflow
   is not wired into the live exception path at all.

No HotSpot vs. CratonVM `--nojit` comparison was run for these three classes
in this pass (worth doing as a fast next step — `-Jit off` on the same
`-ClassList` would confirm/refute the JIT-dispatch-guard theory directly: if
`--nojit` passes cleanly, the JIT-compiled path is implicated; if it also
fails, that points more toward a leaked/shared per-thread counter or a
genuinely-deep interpreted call the trace is similarly failing to capture).

## Repro

Each class is one CratonVM process (JIT on) via the shard runs already
captured under
`apps\spring-boot-suite-runner\.suite\results\crashfail-20260714\shard{1,2,7}\`.
To reproduce a single class in isolation:

```powershell
# EnableConfigurationPropertiesRegistrarTests
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -ClassList "core/spring-boot`torg.springframework.boot.context.properties.EnableConfigurationPropertiesRegistrarTests" `
  -Start 1 -Count 1 -Parallel 1 -Vm craton -Jit on -RunName repro-jitdispatch-20260714

# LogbackLoggingSystemPropertiesTests
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -ClassList "core/spring-boot`torg.springframework.boot.logging.logback.LogbackLoggingSystemPropertiesTests" `
  -Start 1 -Count 1 -Parallel 1 -Vm craton -Jit on -RunName repro-jitdispatch-20260714

# RestTemplateBuilderTests
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -ClassList "module/spring-boot-restclient`torg.springframework.boot.restclient.RestTemplateBuilderTests" `
  -Start 1 -Count 1 -Parallel 1 -Vm craton -Jit on -RunName repro-jitdispatch-20260714
```

(`-ClassList` takes a TSV path per `run-spring-boot-suite.md`; write the
`module<TAB>class` row(s) above to a small `.tsv` file first, or extract the
matching rows straight out of `.suite\all-tests.tsv`.) A useful follow-up:
add `-Jit off` (or `-CratonArgs @('--nojit')`) to the same invocation to test
the "genuine JIT-compiled recursion" hypothesis directly.

## Related

- `vm/src/jit/helpers.rs:3962` — "BUG-1 fix: native-stack recursion guard for
  the JIT→JIT dispatch path" comment; explicitly documents that JIT-compiled
  recursion bypasses `interpreter::execute` and its frame tracking.
- `vm/src/config.rs:373-393` — history of a prior spurious
  `StackOverflowError` from an under-calibrated hardcoded depth guard
  (`DefaultListableBeanFactoryTests.extensiveCircularReference`, fixed by
  raising `max_stack_depth` 1024 → 8192). Same failure *symptom* class
  (spurious SOE on ordinary Spring bean-factory code), different guard.
- Project memory: `reference_jit_selfcall_dispatch_reroute.md` (FIXED) — a
  previously-fixed bug in the same family (JIT self-call dispatch
  misbehaving and creating unintended recursion). Worth checking whether
  this cluster is a residual/regression of that fix or a distinct defect.
- `vm/src/runtime/call_stack.rs` — dead-code `CallStack`/`StackFrameEntry`
  module with named per-frame tracking and its own working
  `max_stack_depth` guard; not wired into any live call path. Confirmed via
  `grep -rn "CallStack|StackFrameEntry" vm/src` returning only its own file
  and the `mod.rs` declaration/re-export.
