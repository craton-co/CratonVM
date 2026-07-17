# `MockMvcSpringBootTestIntegrationTests`/`MockMvcTesterSpringBootTestIntegrationTests` — a throwing anonymous-class `toString()` override silently runs `Object.toString()` instead when formatted via `String.formatted`/`%s`

**Status: OPEN — found 2026-07-17 (hypothesis for the dispatch gap; the format-exception-swallowing part is confirmed against source)**

## Symptom

| Class | Failures |
|---|---:|
| `org.springframework.boot.webmvc.test.autoconfigure.mockmvc.MockMvcSpringBootTestIntegrationTests` | 1/6 (this cause; the class's other failure, `shouldTestWithRestTestClient`, is the unrelated `SpringExtension.isBeanOverride` `NoSuchMethodError` — see [`grpc-test-springextension-isbeanoverride-nosuchmethoderror.md`](grpc-test-springextension-isbeanoverride-nosuchmethoderror.md)) |
| `org.springframework.boot.webmvc.test.autoconfigure.mockmvc.MockMvcTesterSpringBootTestIntegrationTests` | 1/6 (same cause) |

```
JUnit Jupiter:MockMvcSpringBootTestIntegrationTests:shouldNotFailIfFormattingValueThrowsException(CapturedOutput)
    => java.lang.AssertionError:
Expecting actual:
  "
...
    Session Attrs = {attribute-1=org.springframework.boot.webmvc.test.autoconfigure.mockmvc.ExampleController1$1@10cae4}
...
"
to contain:
  "Session Attrs = << Exception 'java.lang.IllegalStateException: Formatting failed' occurred while formatting >>"
       org.springframework.boot.webmvc.test.autoconfigure.mockmvc.MockMvcSpringBootTestIntegrationTests.shouldNotFailIfFormattingValueThrowsException(MockMvcSpringBootTestIntegrationTests.java:95)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc-test.org.springframework.boot.webmvc.test.autoconfigure.mockmvc.Mock-13242e234abc.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc-test.org.springframework.boot.webmvc.test.autoconfigure.mockmvc.Mock-dde51d6ee296.out.log`

## Root cause

The test controller stores an anonymous `Object` whose `toString()` is
overridden to always throw, as a session attribute
(`ExampleController1.java:49-58`):

```java
Object formattingFails = new Object() {
    @Override
    public String toString() {
        throw new IllegalStateException("Formatting failed");
    }
};
request.setAttribute("attribute-1", formattingFails, RequestAttributes.SCOPE_SESSION);
```

`MockMvc`'s result printer (`SpringBootMockMvcBuilderCustomizer.java`'s
`Printer.printValue`, lines 203-214) formats every attribute value with
`"%17s = %s".formatted(label, value)` inside a `try { ... } catch
(RuntimeException ex) { ... << Exception ... occurred while formatting >> ... }` —
so the expected behavior is: the `%s` conversion calls `value.toString()`,
that throws, `RuntimeException` propagates out of `.formatted(...)`, and
the `catch` substitutes the placeholder text the test asserts for.

On CratonVM, the actual printed value is
`ExampleController1$1@10cae4` — **the default `Object.toString()` format**
(`getClass().getName() + "@" + Integer.toHexString(hashCode())`), not the
exception placeholder. Since the class name prefix (`ExampleController1$1`)
is correct, the object's runtime identity/class is not corrupted — but its
`toString()` override did not run; something dispatched to `Object`'s
inherited `toString()` instead.

`%s` formatting is implemented natively in
`native-builtins/src/lang_string.rs::format_arg` (lines 4857-4893, used by
both `String.format` and `String.formatted`). For a non-`String`,
non-`%b`/`%h` object it does use real polymorphic dispatch:

```rust
match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[]) {
    Ok(Some(Value::Object(Some(s)))) => {
        return ctx.read_string(s).unwrap_or_else(|| "null".to_string());
    }
    _ => return "null".to_string(),
}
```

Two distinct issues are visible in this snippet, one confirmed and one
hypothesized:

1. **Confirmed (secondary defect, not what's manifesting in this specific
   observed output, but a real bug in this code):** the `_ =>` arm
   collapses BOTH `Ok(None)` and `Err(...)` to `"null"`. If
   `invoke_virtual` correctly dispatched to the anonymous class's
   `toString()` and that call threw (as it's designed to), `invoke_virtual`
   would return an `Err`, and this code would silently swallow the Java
   exception and return the literal string `"null"` instead of letting it
   propagate — which would violate `String.format`'s real contract (a
   throwing `toString()` must propagate out of `%s` formatting, not be
   absorbed). This is not what the test actually observed (the output
   wasn't `"null"`, it was the default `Object`-format string), so this
   isn't the active cause here, but it is a second, independently
   confirmable defect in the same function worth fixing alongside the
   dispatch gap below.

2. **Hypothesis (the actual observed cause):** the printed default-format
   string is exactly what `Object.toString()` produces, meaning
   `ctx.invoke_virtual(*obj, "toString", ...)` most likely resolved to
   `java/lang/Object.toString()` instead of the anonymous inner class's
   override — a method-resolution/dispatch gap specific to this call
   shape (an anonymous class defined inline in a method body, invoked via
   native-code `invoke_virtual` rather than a bytecode `invokevirtual`).
   This session did not trace CratonVM's `invoke_virtual` native-dispatch
   implementation to confirm why an anonymous class's override would be
   skipped here — flagged as the strongest hypothesis given the evidence
   (correct class identity, wrong method behavior), not an independently
   verified root cause. This is the same general shape (native-code
   `invoke_virtual` calls occasionally resolving to the wrong candidate)
   as prior, separately-tracked dispatch-precedence bugs in this codebase
   (see memory reference `reference_invoke_virtual_native_dispatch_cache_quirk`
   and the fixed `wrong-receiver-virtual-dispatch-corruption-cluster`) —
   worth checking against those mechanisms first rather than assuming a
   new one.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webmvc-test` | `org.springframework.boot.webmvc.test.autoconfigure.mockmvc.MockMvcSpringBootTestIntegrationTests` (1/6 tests) |
| `module/spring-boot-webmvc-test` | `org.springframework.boot.webmvc.test.autoconfigure.mockmvc.MockMvcTesterSpringBootTestIntegrationTests` (1/6 tests) |
