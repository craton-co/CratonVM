# `module/spring-boot-micrometer-tracing-opentelemetry`: AssertJ `WritableAssertionInfo.representation` NPE (broad, masks real failures) + OTel event-publisher residual

**Status: OPEN — investigated 2026-07-23/24, not root-caused to file:line.** Two
distinct residual FAILs remain in this module as of the 2026-07-23 rerun
(`RESULTS-20260723.md`'s 3-residual count for this module; a fresh run this
session found 2 concrete FAILs — see Residual 2 below for the third).

Worktree: `C:\craton\CratonVM-micrometer-tracing-otel-20260723`, branch
`fix/springboot-micrometer-tracing-otel-20260723`, binary
`artifacts/cratonvm-micrometer-tracing-otel-20260723.exe`.

## Residual 1 — `OpenTelemetryBaggagePropagationIntegrationTests` (5/8 FAIL) — AssertJ `representation` NPE masks the real assertion

### Symptom

Every genuinely-failing AssertJ assertion in this test class throws, instead
of a normal `AssertionError` with an expected/actual message:

```
java.lang.NullPointerException: Cannot invoke "org.assertj.core.presentation.Representation.toStringOf(Object)" because "this.representation" is null
	at org.assertj.core.error.ShouldBeEqual.actualAndExpectedHaveSameStringRepresentation(ShouldBeEqual.java:141)
	at org.assertj.core.error.ShouldBeEqual.smartErrorMessage(ShouldBeEqual.java:155)
	at org.assertj.core.error.ShouldBeEqual.newAssertionError(ShouldBeEqual.java:120)
	at org.assertj.core.internal.Failures.failure(Failures.java:108)
	at org.assertj.core.internal.Objects.assertEqual(Objects.java:223)   [or Objects.assertNull]
	at org.assertj.core.api.AbstractAssert.isEqualTo(AbstractAssert.java:380)
	at org.assertj.core.api.AbstractStringAssert.isEqualTo(AbstractStringAssert.java:419)
```

This is **not the real bug the test is checking for** — it's a CratonVM bug in
AssertJ's own error-formatting path that fires whenever a `String`/
`CharSequence` assertion genuinely fails, hiding the actual expected-vs-actual
values. The genuine underlying failures (MDC not matching
`span.context().traceId()`, MDC not cleared when the span is null-scoped) are
real and still need root-causing once this NPE stops masking them — see
"What this blocks" below.

### Minimal, 100% reproducing standalone repro (no Spring, ~15 lines)

```java
import static org.assertj.core.api.Assertions.assertThat;

public class Repro {
    public static void main(String[] args) {
        assertThat("actualStr").isEqualTo("expectedStr"); // throws NPE, not AssertionError
    }
}
```

Run: `cratonvm.exe -c <dir>;<assertj-core-3.27.7.jar> Repro`. Reproduces
**identically under JIT-on (default) and `--nojit`**, and required **zero GC
events** (`--verbose:gc` with `--Xmx 3g` on the full failing test class showed
0 GC lines) — both JIT miscompilation and moving-GC stale-reference are ruled
out.

### What's confirmed (via ~15 iterative repros, `javap -p -c` bytecode reading,
and a classpath-shadowed instrumented `WritableAssertionInfo`)

- `assertThat((Object) "actualStr").isEqualTo((Object) "expectedStr")` —
  **works fine** (proper `AssertionError`). Only the `String`/`CharSequence`
  assertion family is affected, not `assertThat(Object)`.
- `new org.assertj.core.api.StringAssert("actualStr").isEqualTo(expectedObj)`
  called **directly from `main`** — **works fine**, field inspected via
  reflection immediately after construction is correctly a
  `CompositeRepresentation`.
- `org.assertj.core.api.AssertionsForClassTypes.assertThat("actualStr")`
  (equivalently `Assertions.assertThat(String)`, which is a 1-line delegate to
  it) — the **same** `new StringAssert(s)` bytecode, called from *inside*
  `AssertionsForClassTypes.assertThat`, — **fails**: reflecting the `info`
  field immediately after the call returns shows
  `WritableAssertionInfo[...representation=null]` already, i.e. the
  corruption happens during construction, not later.
- `ConfigurationProvider.CONFIGURATION_PROVIDER` (queried via reflection right
  after triggering the bug) is **not null** and
  `.representation()` correctly returns a `StandardRepresentation` — so the
  static singleton itself is fine by the time you can observe it externally.
- `java.util.Objects.requireNonNull(null, "msg")` throws correctly when
  called from the same program — so it isn't a blanket `requireNonNull`
  break.
- **Could not reproduce with any synthetic Java class** built to mirror the
  real shape: matching `AbstractAssert`'s exact 6-field layout (including the
  `info` field at slot/field-index 4, requiring the wide `aload 4` bytecode
  form rather than `aload_0..3`); matching the 4-level
  `StringAssert→AbstractStringAssert→AbstractCharSequenceAssert→AbstractAssert`
  inheritance depth with an intermediate class adding its own field
  (`strings`, like the real `AbstractCharSequenceAssert`); matching the
  2-hop static-method indirection (`Assertions.assertThat` →
  `AssertionsForClassTypes.assertThat` → `new StringAssert`); even using the
  **real** `ConfigurationProvider.CONFIGURATION_PROVIDER.representation()`
  call inside an otherwise-synthetic class hierarchy. All of these
  synthetic repros passed cleanly. **Only the fully-real assertj-core classes
  reproduce it** — the trigger requires something in the actual class
  graph/bytecode not yet isolated (ruled out: field-slot count/index alone,
  inheritance depth alone, static-method-indirection depth alone,
  `ConfigurationProvider`'s real ServiceLoader-backed construction alone —
  none of these individually reproduce in isolation).
- A classpath-shadowed instrumented `WritableAssertionInfo` (same package,
  compiled fresh, placed first on the classpath) confirmed its constructor
  body's `println`s never fire when invoked via `AbstractAssert`'s
  precompiled `invokespecial` — but DOES fire when called directly from
  `main` with the same shadow class on the classpath. This may be a real clue
  (constructor dispatch behaving differently when the resolved class is
  loaded from a different classpath entry than the caller was compiled
  against) or may be an artifact of the shadowing technique itself — **not
  conclusively distinguished this session**, flagged for whoever continues.

### Suspected impact beyond this module

`assertThat(someString).isEqualTo(...)` is one of the single most common
assertions across the whole Spring Boot suite. If this bug is as general as
it looks (triggers via `Assertions.assertThat(String)`, the ordinary static
import every test uses), it is plausibly responsible for **masking the real
error message on an unknown but potentially large fraction of the suite's 99
residual FAILs** documented in `RESULTS-20260723.md` — anywhere a `String`
assertion is the one that's actually failing, the log will show this NPE
instead of the real expected/actual diff. Worth a dedicated grep of other
FAIL logs for this exact NPE signature before assuming each is a distinct
root cause.

### Next steps for whoever continues

1. Try `--verbose:class` with a narrower/differently-placed flag position, or
   `CRATONVM_DBG_JIT_DISASM`, to see whether `WritableAssertionInfo.<init>`
   is even being *entered* in the un-shadowed real-jar case (the shadow
   experiment above suggests it might not be, but wasn't conclusive since the
   experiment itself introduces a classpath-shadowing variable).
2. Grep `vm/src/runtime/interpreter.rs` and `vm/src/vm/vm_exec.rs` for
   `invokespecial`/constructor-cache handling keyed by callsite rather than
   by (caller class, resolved class) pair — the shadowing result hints at a
   possible cross-class-boundary cache aliasing bug in constructor dispatch.
3. Alternatively bisect via `git bisect` against `dev` history using the
   15-line repro above (sub-second to run) rather than the 46s full test
   class — this is now cheap enough to bisect properly, unlike when this was
   only reproducible via the full suite.

## Residual 2 — `OpenTelemetryTracingAutoConfigurationTests` (1/36 FAIL) — `shouldPublishEventsWhenContextStorageIsInitializedEarly`

Different symptom, **not** the AssertJ NPE above — a genuine (proper-message)
`AssertionError`:

```
java.lang.AssertionError:
Expecting actual not to be empty
	at OpenTelemetryTracingAutoConfigurationTests.lambda$shouldPublishEventsWhenContextStorageIsInitializedEarly$0(OpenTelemetryTracingAutoConfigurationTests.java:523)
```

`listener.events` (an `OtelEventListener` test helper collecting OTel
`EventListener.onEvent` callbacks) is empty after starting a span, when it
should have recorded events from
`OpenTelemetryEventPublisherBeansApplicationListener`'s `ContextStorage`
wrapper (`io.opentelemetry.context.ContextStorage.addWrapper(Storage::new)`
in `OpenTelemetryEventPublisherBeansApplicationListener.java:132`).

This class was previously documented as `StackOverflowError` on this exact
test method (see
[`../../internal/fixed-suite-bugs/springboot/opentelemetry-contextstorage-early-init-recursion-cluster-FIXED.md`](../../internal/fixed-suite-bugs/springboot/opentelemetry-contextstorage-early-init-recursion-cluster-FIXED.md)),
confirmed fixed 2026-07-20. **The recursion/hang is still fixed** — this is a
newly-exposed, different residual (events not published) now that the method
runs to completion instead of overflowing.

**Speculative, unconfirmed link to Residual 1:** both bugs involve a
lazily-initialized static singleton reached through a chain of wrapper/
delegate objects (`ConfigurationProvider.CONFIGURATION_PROVIDER` for AssertJ;
OTel's own internal `ContextStorage` singleton wrapping mechanism for this
one) — plausibly the same underlying CratonVM bug class, but **not verified
this session**; treat as two separate OPEN items until one is root-caused
enough to confirm or rule out the connection.

Not further investigated this session due to time — Residual 1 consumed the
investigation budget given its much broader suite-wide impact.

## Reproduce / rerun

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "C:\craton\CratonVM-micrometer-tracing-otel-20260723\artifacts\cratonvm-micrometer-tracing-otel-20260723.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList apps\spring-boot-suite-runner\.suite\micrometer-otel-classes.tsv `
  -RunName <name> -Parallel 3 -TimeoutSec 300
```

The other 7 classes in this module (`CompositeTextMapPropagatorTests`,
`OpenTelemetryEventPublishingContextWrapperBeansTestExecutionListenerIntegrationTests`,
`OpenTelemetryTracingPropertiesTests`,
`otlp.OtlpTracingAutoConfigurationIntegrationTests`, `SpanExportersTests`,
`SpanProcessorsTests`, `otlp.OtlpTracingAutoConfigurationTests`,
`zipkin.ZipkinWithOpenTelemetryTracingAutoConfigurationTests`) all PASS clean
on current `dev` (confirmed this session, JIT-on).
