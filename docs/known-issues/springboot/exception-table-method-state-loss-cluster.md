# Suspected VM interpreter/JIT bug: real bytecode in methods with an exception table (try/catch or try/finally) silently loses or corrupts correctly-computed state on the NORMAL (non-exceptional) path

**Status: OPEN, HIGH PRIORITY — found 2026-07-24.** Discovered across three
independent, unrelated call sites during the Spring Boot core Cluster C
(logging bootstrap) batch. Not scoped to logging or to Spring Boot — this
looks like a genuine CratonVM interpreter/JIT correctness gap, wide enough
that it likely explains failures well beyond this cluster. Recommend a
dedicated VM-level investigation session before further Cluster C work,
since several of the still-uninvestigated failures in this same batch
(`LoggingApplicationListenerTests`, `Log4J2LoggingSystemTests`,
`SpringBootJoranConfiguratorTests`) call into the same
lock/try/apply/finally-shaped configuration-application methods and are
likely hitting this same bug rather than independent issues.

## The pattern

Three separate, unrelated real-JDK/Spring methods — each containing a
`try`/`catch` or `try`/`finally` block (i.e., a non-empty bytecode
exception table) — produce **wrong results on their normal, no-exception
path**, even though:
- every individual operation the method performs, hand-replicated outside
  the try/catch/finally shape (same classpath, same process), produces the
  CORRECT result, and
- no exception is thrown or expected at any point in the real run.

### Instance 1 — `java.util.logging.SimpleFormatter.format(LogRecord)`
See `javaloggingsystemtests-simpleformatter-args-drop.md`. Real bytecode
drops the date (`%1$tc`) and message (`%5$s`) `String.format` arguments;
every sub-operation (the date computation, `formatMessage()`, the final
`String.format` call itself) works correctly when replicated by hand. This
method doesn't actually have a `try`/`catch` in the FAILING call path
(only its unrelated throwable-formatting branch does) — flagged here as a
data point, not yet confirmed to share this exact mechanism; may be a
separate bug that happens to look similar.

### Instance 2 — `org.springframework.util.ClassUtils.forName(String, ClassLoader)`
See `classutils-forname-platform-loader-false-positive.md`. Real bytecode
returns a class as present when it is not; the method has TWO
`catch(ClassNotFoundException)` exception-table entries (a primary lookup
guarded by one handler, whose fallback lookup is guarded by a second). Both
individual `Class.forName` calls, replicated by hand, correctly throw.

### Instance 3 — `org.springframework.boot.logging.logback.DefaultLogbackConfiguration.apply(LogbackConfigurator)`
New this entry. `apply()` is:
```java
void apply(LogbackConfigurator config) {
    config.getConfigurationLock().lock();
    try {
        defaults(config);
        Appender<ILoggingEvent> consoleAppender = consoleAppender(config);
        ... config.root(Level.INFO, ...);
    }
    finally {
        config.getConfigurationLock().unlock();
    }
}
```
`javap -c` confirms a real exception table:
```
Exception table:
   from    to  target type
        7    75    85   any
       85    87    85   any
```
`defaults(config)` (called at the very start of the `try` block) sets
several `LoggerContext` properties, including `FILE_LOG_CHARSET` /
`CONSOLE_LOG_CHARSET` (resolved from a `${NAME:-default}` pattern via real
Logback `OptionHelper.substVars`, honoring a `System.setProperty` override
when present). After `apply()` returns normally (no exception, confirmed —
the process exits cleanly), **every property `defaults()` set is gone**:
`loggerContext.getProperty("FILE_LOG_CHARSET")` returns `null`, not even
the literal default.

Ruled out via direct repro (same classpath, package-private class access
via a same-package helper class):
- `LogbackConfigurator.getContext()` returns the identical `LoggerContext`
  instance passed to its constructor (`==` true) — not a copy/different
  object.
- Calling `config.getContext().putProperty(...)` / `dlc.putProperty(config,
  ...)` directly round-trips correctly.
- Calling `OptionHelper.substVars("${FILE_LOG_CHARSET:-UTF-8}", ctx)`
  directly, with the system property set, correctly resolves and the
  result round-trips through `ctx.putProperty`/`getProperty`.
- Calling `defaults(config)` **and then** `consoleAppender(config)** via
  reflection (`Method.setAccessible(true)` + `invoke`, bypassing the real
  `apply()` method's own bytecode and its try/finally entirely) leaves
  `FILE_LOG_CHARSET` correctly set afterward.

Only calling through the real `apply()` method itself — i.e., going
through its try/finally — loses the property. This blocks all 3 failures
in `DefaultLogbackConfigurationTests`
(`fileLogCharsetShouldUseSystemPropertyIfSet`,
`consoleLogCharsetShouldUseConsoleCharsetIfConsoleAvailable`,
`consoleLogCharsetShouldDefaultToUtf8WhenConsoleIsNull` — all three call
`apply()`), and is the leading suspect for
`LogbackConfigurationAotContributionTests`'s 2 failures too (unexpected
extra registered types leaking across test methods — a shape consistent
with "state set during one call silently escaping/surviving/corrupting
past where it should be scoped", though not yet confirmed to be the exact
same mechanism).

## What does NOT reproduce it

A minimal, standalone (no Spring/Logback) try/finally around a
field-setting call correctly preserves state:
```java
static class Holder { ReentrantLock lock = new ReentrantLock(); String value; }
static void applyWithTryFinally(Holder h) {
    h.lock.lock();
    try { setValue(h, "SET-VIA-TRY-FINALLY"); }
    finally { h.lock.unlock(); }
}
static void setValue(Holder h, String v) { h.value = v; }
```
`h.value` is correctly `"SET-VIA-TRY-FINALLY"` afterward. So "any
try/finally" is not sufficient to trigger this — something about the real
methods' larger size, deeper call chains, and/or JIT eligibility
(tiering/hotness thresholds; these are exercised many times across the
149-class-ish suite runs vs. a single cold call in the minimal repro) is
likely part of the trigger condition. **Whoever picks this up should try
scaling the minimal repro up** (more locals, more nested calls inside the
try block, calling it in a loop to force JIT compilation) to find the
actual threshold, rather than assuming the simplified case is
representative.

## Why this is scoped OPEN rather than fixed

This is a VM interpreter/JIT correctness investigation, not a native-bridge
patch — well outside a logging-bootstrap batch's scope, and risky to guess
at blindly. Given three independent real-JDK/Spring methods across
different libraries all involve an exception table and all lose state on
their normal path, this looks systemic enough to be worth a dedicated
session with proper tooling (bisect JIT vs interpreter via
`--nojit`/`CRATONVM_NO_PRECISE_JIT_MAPS`, check `CRATONVM_DBG_JIT_DISASM`
output for the affected methods, compare against the interpreter's
exception-table PC-range matching logic).

## Reproduction assets

All three repros need `core/spring-boot`'s own generated test classpath
(`apps/spring-boot/core/spring-boot/build/cratonvm-test-cp.txt`). Instance
3's full repro (package-private class access):
```java
// file: org/springframework/boot/logging/logback/FullApplyRepro.java
package org.springframework.boot.logging.logback;
import ch.qos.logback.classic.LoggerContext;
public class FullApplyRepro {
    public static void main(String[] args) throws Exception {
        LoggerContext loggerContext = new LoggerContext();
        LogbackConfigurator config = new LogbackConfigurator(loggerContext);
        System.setProperty("FILE_LOG_CHARSET", "ISO-8859-1");
        new DefaultLogbackConfiguration(null).apply(config);
        System.err.println(loggerContext.getProperty("FILE_LOG_CHARSET")); // expect ISO-8859-1, get null
    }
}
```
