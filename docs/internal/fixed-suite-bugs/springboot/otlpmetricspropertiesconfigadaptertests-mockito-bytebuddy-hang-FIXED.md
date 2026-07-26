# `OtlpMetricsPropertiesConfigAdapterTests` HANG — Mockito inline-mock-maker interface-default `super` call infinite recursion — FIXED

**Status: FIXED (2026-07-20), branch `fix/otlp-mockito-bytebuddy-hang-20260720`.**

Retires `docs/known-issues/springboot/otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang.md`.
The original doc's "last activity is Mockito self-attach / ByteBuddy `Invoker$Dispatcher`"
symptom was a real observation but an unconfirmed hypothesis (no thread dump captured);
the actual root cause is unrelated to self-attach itself, which was already fully working
(see `docs/internal/kafka-suite-bugs/bug-09-mockito-inline-mockmaker-selfattach.md`,
FIXED 2026-06-12).

## Root cause

`native_mockito_mock_method_advice_is_overridden` (`native-builtins/src/lib.rs`) — the
Rust-native fast path CratonVM registers for Mockito's
`MockMethodAdvice.isOverridden(Object, Method)` — unconditionally returned `false`
("not overridden") whenever the queried `Method`'s declaring class was an **interface**:

```rust
if ctx.is_interface_class(declaring_class_id) {
    return Ok(Some(Value::Int(0)));
}
```

`isOverridden` is the mechanism Mockito's inline mock maker uses to decide, on every
advice-woven method entry, whether to intercept or to run the real body directly. It is
specifically needed for the shape `Interface.super.someMethod()` called from an
overriding, redefined method: when a spied/mocked concrete class overrides an interface
default method and internally calls `Interface.super.someMethod()`, that `invokespecial`
lands on the (also advice-woven) interface's own copy of the method. Mockito's woven
prologue on the interface's copy calls `isOverridden(this, InterfaceMethod)` to recognize
"this call arrived via a `super` call from an overriding class — the outer call was
already intercepted, don't re-intercept" and skip straight to the real (interface
default) body.

Forcing this to always return `false` made the interface's own copy ALWAYS decide "not
overridden, must intercept" — re-entering `MockMethodAdvice.handle()` →
`MockHandlerImpl` → (no matching stub) → `CallsRealMethods.answer()` →
`callRealMethod()` → reflectively re-invokes the concrete override → which itself calls
`Interface.super.someMethod()` again → forever. Confirmed via CratonVM's
`--stack-dump-on-timeout` watchdog: a repeating 18-frame Mockito/ByteBuddy cycle
(`MockMethodAdvice.handle` → `MockMethodInterceptor.doIntercept` → `...` →
`CallsRealMethods` → `callRealMethod` → `MockMethodAdvice.tryInvoke` →
`InstrumentationMemberAccessor` → the concrete override → the interface default →
repeat) that ran to thousands of frames before the watchdog aborted the process.

Both affected classes in this doc hit the pattern:
- `OtlpMetricsPropertiesConfigAdapterTests.whenPropertiesUrlIsNotSetThenUseOtlpConfigUrlAsFallback`:
  `spy(createAdapter())`, then `adapter.url()` (declared on
  `OtlpMetricsPropertiesConfigAdapter`) calls `OtlpConfig.super.url()`.
- `JacksonAutoConfigurationTests`: `mock(ObjectMapper.class)` — this class's reported
  HANG turned out to be a **different, unrelated** issue (severe pre-existing
  performance pathology, not a Mockito/redefinition bug at all — see Verification).

## The fix

Removed the `is_interface_class` early return in the concrete-mock branch of
`native_mockito_mock_method_advice_is_overridden`. The existing hierarchy-walk logic
just below it (added by `dee2e26f4` "fix(runtime): preserve Mockito inline real-method
dispatch", 2026-07-15, for the analogous `RestTemplate`/`InterceptingHttpAccessor`
concrete-superclass case) already handles an interface declaring class correctly with no
changes needed: it walks from the mock's concrete class up through `superclass_of` until
it reaches `declaring_class_id`. For an **interface** declaring class, that class never
appears in the concrete superclass chain, so the walk's `break` condition simply never
fires — it just checks every concrete class from the mock's own class up to `Object` for
a declared override, which is exactly the right structural test ("does the mock's actual
class hierarchy override this interface default method"). Minimal, surgical fix — one
`if` block deleted, no new logic.

## Standalone repro (no Spring Boot / JUnit needed)

`Mockito.spy()` on any concrete class that overrides an interface default method and
calls the interface's own body via `Interface.super.method()`:

```java
interface Greeter {
    default String greet() { return "interface-default"; }
}
class MyGreeter implements Greeter {
    @Override
    public String greet() { return Greeter.super.greet() + "-overridden"; }
}
// Mockito.spy(new MyGreeter()).greet() infinite-loops pre-fix, returns
// "interface-default-overridden" post-fix (matches HotSpot).
```

Reproduces identically with `--nojit` (interpreter-only) — not JIT-specific.

## Verification

Branch `fix/otlp-mockito-bytebuddy-hang-20260720`, worktree
`C:\craton\CratonVM-otlp-mockito-bytebuddy-hang-20260720`, unique binary
`cratonvm-otlp-mockito-bytebuddy-hang-20260720.exe`.

- Minimal repro (`../../../known-issues/repros/springboot/interface-default-supercall-mockito-spy-repro.java`):
  pre-fix infinite recursion / watchdog abort → post-fix
  `RESULT=interface-default-overridden`, matches real HotSpot exactly.
- `OtlpMetricsPropertiesConfigAdapterTests` (30 tests): pre-fix CRASH (watchdog abort,
  4600+ frame repeating cycle) → post-fix **29/30 PASS**, 8-10s wall (was: never
  completes). The 1 remaining failure
  (`whenPropertiesUrlIsNotSetThenUseOtlpConfigUrlAsFallback`) is a **separate,
  narrower** bug — not a hang, not related to `isOverridden` — tracked in
  `docs/known-issues/springboot/mockito-inline-nested-selfcall-stub-bypass.md`.
- `JacksonAutoConfigurationTests`: this was a separate severe-slowdown issue at
  the time of this Mockito fix. It was subsequently fixed; see
  `docs/internal/springboot/jacksonautoconfigurationtests-severe-slowdown-FIXED-20260722.md`.
  The earlier `--stack-dump-on-timeout` dumps showed a normal, non-repeating,
  slowly-progressing Spring-context-refresh call stack
  (`OnBeanCondition.getMatchingBeans` / `DefaultBindConstructorProvider`
  reflection work) — **zero** Mockito/ByteBuddy frames anywhere in any dump. CPU-sampled
  the live process twice 20s apart (per
  `[[reference_cpu_sample_deadlock_vs_slow_technique]]`): the worker thread's
  `TotalProcessorTime` grew by ~20s of CPU across the 20s wall-clock window — pegged at
  ~100% CPU, genuinely busy, not parked/deadlocked. So this is **not a hang** in the
  deadlock/infinite-recursion sense at all, and unrelated to the `isOverridden` bug this
  doc covers — it was miscategorized into this doc by the 2026-07-17 investigation
  because both classes' `.err.log`s happened to end near a Mockito self-attach log line.
  It's a genuine (and severe — 600s+ for a 6-test class is 100-1000x slower than
  HotSpot) performance pathology in this class's Spring Boot bean-condition-evaluation
  path, out of scope for this fix. Left as a residual open item — see
  `docs/known-issues/springboot/jacksonautoconfigurationtests-severe-slowdown.md`.
