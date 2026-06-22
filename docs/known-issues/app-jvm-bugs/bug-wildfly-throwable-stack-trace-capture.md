# WildFly — `Throwable.getStackTrace()` loses frames after throw/unwind

## Status
**OPEN** — meta-blocker for WildFly (and general) debugging.

## Severity
**CRITICAL (diagnostic)** — does not always crash apps directly but prevents fixing other bugs.

## App / suite
- **Observed while:** debugging `HealthSubsystemTestCase` (WildFly)
- **Scope:** general VM — any thrown exception inspected after propagation

## Symptom

`Throwable.getStackTrace()` returns **too few or zero** frames after an exception propagates:

| Scenario | CratonVM frames | HotSpot frames |
|----------|-----------------|----------------|
| `new RuntimeException()` inspected immediately | 1 | ≥1 |
| Exception thrown from nested method, caught in caller | **0** | 2 |
| JDK-thrown `NumberFormatException` | 1 | 4 |

JUnit often prints only exception **message** with no stack → failures look like `IllegalStateException: null`.

## HotSpot behavior

Stack trace captured at throw site, stored in exception object, stable for lifetime of throwable.

## CratonVM behavior

Traces stored in thread-local map keyed by throwable **identity hash** (`vm_exec.rs` `capture_stack_trace`; `lang_misc` `capture_throwable_trace` sets `backtrace=this`, `depth=len`; `getStackTrace` re-looks-up by `identity_hash_code(backtrace)`).

Key is **not stable** across throw → unwind → GC → identity hash reuse → trace **lost or wrong**.

## Root cause (suspected)

Identity-hash indirection instead of storing trace **in the throwable object** (or stable internal id). Unwind/GC invalidates lookup.

## Impact

- Cannot localize WildFly WF-5 (JAXP) or deeper `testSubsystem` failures
- All CratonVM app debugging degraded
- CI logs unusable for root-cause analysis

## Reproduce

```java
public class StackTraceProbe {
    static void inner() { throw new RuntimeException("inner"); }
    public static void main(String[] a) {
        try { inner(); } catch (RuntimeException e) {
            System.out.println("frames=" + e.getStackTrace().length);
            for (var f : e.getStackTrace()) System.out.println(f);
        }
    }
}
```

Compare frame count CratonVM vs HotSpot.

## What to fix

1. Store stack trace array (or opaque handle) **on the Throwable instance** at fillInStackTrace / throw time.
2. Stop relying on identity-hash side table for `getStackTrace()`.
3. Verify nested throw/catch, rethrow, and wrapped exceptions match HotSpot frame counts.
4. Re-run WildFly health tests with full stacks; revisit WF-5.

## Related

- [bug-wildfly-jaxp-premature-end-of-file.md](bug-wildfly-jaxp-premature-end-of-file.md)
- `apps/wildfly/CRATONVM_BUGS.md` Bug 9
