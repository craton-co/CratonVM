# `ClassUtils.isPresent`/`forName` (Spring Framework) falsely reports a class present via `ClassLoader.getPlatformClassLoader()` when the identical logic, hand-replicated, correctly reports absent

**Status: OPEN — found 2026-07-24.** Found via the Spring Boot core Cluster
C (logging bootstrap) batch. Blocks
`org.springframework.boot.logging.logback.LogbackRuntimeHintsTests
#doesNotRegisterHintsWhenLoggerContextIsNotAvailable`. Likely affects any
test asserting "hints/behavior X is absent when a library isn't on this
classloader" via `ClassLoader.getPlatformClassLoader()` (or any loader that
legitimately doesn't have the class) + Spring's `ClassUtils.isPresent`.

## Symptom

`new LogbackRuntimeHints().registerHints(hints, ClassLoader
.getPlatformClassLoader())` is supposed to no-op (`ch.qos.logback.classic
.LoggerContext` is an application dependency, never present on the
platform/bootstrap classloader) but instead registers the full set of
Logback reflection hints — meaning `ClassUtils.isPresent
("ch.qos.logback.classic.LoggerContext", platformClassLoader)` returned
`true`.

## What's ruled out (all via direct repro against the real test classpath)

- `Class.forName("ch.qos.logback.classic.LoggerContext", false,
  ClassLoader.getPlatformClassLoader())` called directly: correctly throws
  `ClassNotFoundException`, both as the first classloading operation in the
  process and after the class has already been resolved via the
  application classloader (ruling out cross-test/cross-loader caching
  contamination) and after full instantiation (`newInstance()`) via the app
  loader.
- `org.springframework.util.ClassUtils`'s static `commonClassCache` (a
  ~47-entry `Map<String, Class<?>>` of well-known JDK types Spring
  short-circuits lookups for) does not contain a `LoggerContext` entry --
  read directly via reflection, `containsKey` is `false`. Not a stale-cache
  hit.
- `Class.forName("ch.qos.logback.classic$LoggerContext", false,
  platformClassLoader)` (the dollar-sign inner-class-name fallback
  `ClassUtils.forName`'s bytecode falls back to on a first
  `ClassNotFoundException`, since `LoggerContext`'s preceding path segment
  starts with an uppercase letter) called directly: also correctly throws.

Every individual operation `ClassUtils.forName`'s disassembled bytecode
performs, replicated by hand in the same process/classpath, behaves
correctly. Only the REAL, compiled `ClassUtils.isPresent`/`forName` method
call (normal `invokestatic`, nothing exotic) produces the wrong (`true`)
result.

## Suspected root cause (not confirmed)

`ClassUtils.forName(String, ClassLoader)` has two overlapping/nested
`try`/`catch(ClassNotFoundException)` exception table entries (the
outer array-type check isn't relevant here, but the primary
`Class.forName(...)` call at offset 82-88 is guarded by one handler
targeting the inner-class-name fallback at 89, and that fallback's OWN
`Class.forName(...)` call at 159-166 is guarded by a second handler
targeting 167, which just falls through to `athrow` re-throwing the
original exception). This is exactly the shape of bytecode (a method with
more than one exception-table entry, one nested inside another's catch
handler) that a second, independently-discovered bug this session
(`javaloggingsystemtests-simpleformatter-args-drop.md` — real
`SimpleFormatter.format()`'s bytecode dropping args that hand-written
equivalent bytecode does not drop) also involves a real-JDK method with
non-trivial internal control flow producing wrong results despite every
sub-operation working correctly in isolation. Both point at the same
general suspect: CratonVM's interpreter/JIT has a correctness gap
somewhere in executing methods with **multiple exception-table entries**,
independent of the specific opcodes each entry guards — worth
investigating together, not as two unrelated one-off bugs.

## Why this is scoped OPEN rather than fixed

This needs a VM-level interpreter/exception-table investigation (likely
comparing the `athrow`/exception-dispatch path against the JVM spec's
exact matching rules — table lookup by PC range, then leftmost/first
matching entry, re-throw semantics) rather than a native-bridge patch;
out of scope for the logging-bootstrap batch. A minimal repro that
doesn't depend on Spring or Logback (a hand-compiled two-exception-table
method) would help isolate this from `SimpleFormatter`'s residual and
confirm/refute the shared-root-cause hypothesis above.

## Repro

Needs `core/spring-boot`'s own test classpath (has `spring-core` +
`logback-classic` on it):
```java
public class Repro {
    public static void main(String[] args) throws Exception {
        boolean present = org.springframework.util.ClassUtils.isPresent(
            "ch.qos.logback.classic.LoggerContext",
            ClassLoader.getPlatformClassLoader());
        System.err.println(present); // expect false, get true
    }
}
```
