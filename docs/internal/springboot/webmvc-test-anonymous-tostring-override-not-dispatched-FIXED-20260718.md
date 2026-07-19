# `MockMvc*SpringBootTestIntegrationTests` — anonymous `toString()` overrides now dispatch and propagate correctly

**Status: FIXED — verified 2026-07-18**

## Symptom

`MockMvcSpringBootTestIntegrationTests.shouldNotFailIfFormattingValueThrowsException`
formats a session attribute whose anonymous `toString()` throws
`IllegalStateException("Formatting failed")`. Spring Boot's result printer catches
that exception and emits its `<< Exception ... occurred while formatting >>`
placeholder.

Before this fix, CratonVM printed `ExampleController1$1@...`, the default
`Object.toString()` representation, instead. The targeted JIT class therefore
failed 1 of 6 tests.

## Root cause and repair

Native `NativeContextImpl::invoke_virtual` resolved a receiver's class name and,
in the usual same-name case, dispatched again through the global name-to-class
map. For the method-local anonymous class reached by native `%s` formatting,
that path selected `java/lang/Object.toString()` rather than the receiver's
concrete override.

Ordinary object virtual calls now dispatch through the receiver's exact loaded
class via `invoke_on_class_shared`. That keeps native-method precedence while
resolving the actual receiver hierarchy.

The same investigation found two formatter residuals:

- `%s` collapsed an exception from `toString()` into the literal `"null"`.
- `invoke_to_string_opt` converted a virtual-dispatch exception into an identity
  fallback string.

Both paths now propagate `MethodCallFailed`; `native_string_format` propagates
it back to Java, allowing the caller's normal exception handling to run.

## Regression coverage

`vm/tests/string_format_throwing_tostring.rs` compiles a small Java probe with
an anonymous throwing `toString()` and verifies all of the following in JIT and
`--nojit` modes:

- direct virtual invocation throws;
- `"%s".formatted(value)` throws; and
- `String.format("%s", value)` throws.

## Validation

- Remote Java 25, targeted Spring Boot JIT run:
  `MockMvcSpringBootTestIntegrationTests` — **6/6 passed** (baseline: 5/6).
- Remote Java 25, targeted Spring Boot `--nojit` run:
  `MockMvcSpringBootTestIntegrationTests` — **6/6 passed**.
- Remote Java 25, targeted `MockMvcTesterSpringBootTestIntegrationTests`
  formatting assertion — **passed** in JIT and `--nojit` modes and printed the
  expected exception placeholder.
- `cargo test -p cratonvm-vm --test string_format_throwing_tostring -- --nocapture`
  using the unique release binary — **passed**, including its JIT and `--nojit`
  probe executions.
