# Bug 01 — `Stream.forEachOrdered(Consumer)` → AbstractMethodError "no Code attribute"

**Severity:** High — **CratonVM-only**. Any code path that calls
`java.util.stream.Stream.forEachOrdered(...)` threw at the call site and could not proceed.
HotSpot (JDK 25) runs the identical code fine. Boot JDK 25, both interpreter and JIT.

**Status: FIXED** (worktree `test/wildfly-suite`, `vm/src/runtime/interpreter.rs`). Verified
by standalone repro + the affected WildFly class (LOADERR → normal FAIL).

**Signature:**
```
java.lang.AbstractMethodError: method java/util/stream/Stream.forEachOrdered(Ljava/util/function/Consumer;)V has no Code attribute
```

## Minimal standalone repro (no WildFly needed)
[`ForEachOrderedRepro.java`](ForEachOrderedRepro.java):
```java
import java.util.List;
import java.util.stream.Stream;
public class ForEachOrderedRepro {
    public static void main(String[] a) {
        System.out.println("start");
        Stream.of("x","y","z").forEachOrdered(s -> System.out.println("ord:" + s)); // threw here
        List.of(1,2,3).stream().forEachOrdered(i -> System.out.println("li:" + i));
        Stream.of("a","b").forEach(s -> System.out.println("fe:" + s));              // forEach was FINE
        System.out.println("done");
    }
}
```

| VM | Output (before fix) |
|----|--------|
| HotSpot JDK 25 | `start / ord:x / ord:y / ord:z / li:1 / li:2 / li:3 / fe:a / fe:b / done` |
| CratonVM (before) | `start` then `AbstractMethodError: Stream.forEachOrdered … has no Code attribute` |
| **CratonVM (after fix)** | identical to HotSpot ✓ |

## Root cause (confirmed)
`Stream.of(...)` is intercepted by a CratonVM native that returns a synthetic stream whose
runtime class is the **interface** `java/util/stream/Stream` itself (no concrete pipeline).
Terminal/intermediate ops on it (`forEach`, `map`, `peek`, `takeWhile`, …) are recovered by
the interpreter's interface→concrete **receiver-walk rescue** (or real-JDK bytecode), so they
work. `forEachOrdered` is the exception:

- Its **only** native registration lives in `register_phase56_stream_extras`, which is reached
  **solely** from `register_synthetic_overrides`. That function is
  `#[cfg(feature = "synthetic-jdk")]` and is **compiled out of the real-JDK CLI build** (the
  default `cratonvm.exe`). So no `Stream.forEachOrdered` native is ever registered.
- The `invokeinterface Stream.forEachOrdered` therefore resolves to the **abstract** interface
  declaration (no `Code`), the native-on-resolved-class check misses (nothing registered), and
  the receiver-walk rescue finds no concrete override either → `AbstractMethodError` at
  `interpreter.rs` (the "has no Code attribute" path).

Confirmed by instrumentation: at the failure site,
`owner=java/util/stream/Stream m=forEachOrdered native_found=false`, whereas `peek`/`forEach`
never reach that path (they resolve to Code-bearing methods).

## Fix
`vm/src/runtime/interpreter.rs`, in the no-`Code` rescue inside `execute()`: before throwing
`AbstractMethodError`, redirect `Stream.forEachOrdered(Ljava/util/function/Consumer;)V` to
`forEach` (semantically identical for our sequential-only synthetic streams), which already
resolves correctly via the receiver-walk rescue:

```rust
if method_name == "forEachOrdered"
    && method_descriptor == "(Ljava/util/function/Consumer;)V"
{
    return execute(shared, thread, class_id, "forEach", method_descriptor, args);
}
```

This is the minimal, targeted fix at the exact failure point. (A fuller fix would also promote
the `register_phase56_stream_extras` stream natives out of the `synthetic-jdk`-only path so the
real-JDK build registers `forEachOrdered`/`forEachRemaining`-style ops directly; the redirect
covers the observed gap for the `(Consumer)V` terminal op.)

## Verification
- `ForEachOrderedRepro` and a `peek`/`takeWhile`/`forEachOrdered` combo (`StreamExtras.java`)
  now produce **identical output to HotSpot**; `forEach`/`map`/`peek`/`takeWhile` unchanged (no
  regression).
- `org.jboss.as.test.integration.ejb.security.AnnotationAuthorizationTestCase` under the fixed
  binary: **LOADERR (AbstractMethodError) → `RESULT found=11 … status=FAIL`** with the normal
  Arquillian `ConfigurationException: javaHome '${container.java.home}' must exist` — matching
  HotSpot's `found=11` exactly.

## Affected WildFly tests (≥27, all the same signature)
All in `integration/basic`, package `org.jboss.as.test.integration.ejb.security` (a shared
`@BeforeClass`/static init uses `forEachOrdered`), e.g. `AnnotationAuthorizationTestCase`,
`AuthenticationTestCase`, `EJBSecurityTestCase`, `EJBInWarDefaultSecurityDomainTestCase`,
`InherritanceAnnSFSBTestCase`, `InjectionAnnSFSBtoSFSBTestCase`, … These surfaced as **LOADERR**
(thrown during class load / JUnit discovery, before any test ran). The fix moves them to the
normal Arquillian FAIL, matching HotSpot.
