# Mockito inline mock maker: a stub on a method reached through a nested self-call chain (interface default → utility helper → target method) is silently bypassed

**Status: FIXED 2026-07-20.** Formerly
`docs/known-issues/springboot/mockito-inline-nested-selfcall-stub-bypass.md`.
Found while fixing
`docs/internal/springboot/otlpmetricspropertiesconfigadaptertests-mockito-bytebuddy-hang-FIXED.md`.

## Symptom

`OtlpMetricsPropertiesConfigAdapterTests.whenPropertiesUrlIsNotSetThenUseOtlpConfigUrlAsFallback`:

```java
OtlpMetricsPropertiesConfigAdapter adapter = spy(createAdapter());
given(adapter.get("management.otlp.metrics.export.url")).willReturn("https://my-endpoint/v1/metrics");
assertThat(adapter.url()).isEqualTo("https://my-endpoint/v1/metrics");
// actual: "http://localhost:4318/v1/metrics" (the hardcoded default — the stub never fired)
```

This was the one residual failure out of 30 after fixing the `isOverridden`
infinite-recursion bug — no hang, no crash, just a wrong value.

## Actual root cause

The original hypothesis (Mockito's `MockMethodAdvice.SelfCallInfo` `ThreadLocal`
being mis-consumed across the 4-5 layer self-call chain) was **wrong**. The real
cause was unrelated to Mockito's own bookkeeping entirely: a dispatch-cache
staleness bug in CratonVM's interpreter, in the raw-opcode-level fast path
`execute_invokevirtual_vtable_fast` (`vm/src/runtime/interpreter.rs`).

CratonVM has three layered `invokevirtual`/`invokeinterface` dispatch tiers:

1. `execute_invokevirtual_cached` (thread-local + cross-thread promoted cache)
   — entries are `CachedInvokeTarget`, each carrying a `RedefineGate` checked
   via `is_stale()` on every hit. Sound w.r.t. JVMTI class redefinition.
2. `execute_invokevirtual_vtable_fast` — a raw-opcode (`0xb6`/`0xb9`) fast path
   backed by `VtableManager`/`Vtable`, a global per-class dispatch table whose
   entries (`VtableEntry.resolved_method: Option<Arc<CachedBytecodeMethod>>`)
   carry **no staleness/generation tracking at all**.
3. `execute_invoke_kind` (via `execute_invoke`/`try_stackless_invoke`) — the
   slow, fully redefine-aware path that populates tier 1.

A class's own vtable is refreshed when *it* is redefined
(`class_manager::redefine_class` → `vtable_install_adapter`), but a
*subclass's* vtable — built at ordinary class-load time by copying/inheriting
the slot from a declaring ancestor, long before any agent runs — is never
transitively refreshed when that ancestor is later redefined. Mockito's inline
mock maker redefines a spied class's whole hierarchy in place to weave
`MockMethodAdvice`. Once a receiver's vtable had cached an inherited slot for
`get(String)` (declared on an ancestor a few levels up), the later redefinition
of that ancestor left the cached entry pointing at the pre-redefinition
bytecode forever, for any call site that reached tier 2 before ever warming
tier 1.

In the failing repro, `adapter.url()`'s chain to `get(String)` passes through
an interface default method (`OtlpConfig.url()` → `PropertyValidator` →
`config.get(fullKey)`) whose call site's declared receiver type
(`MeterRegistryConfig`) is an interface the concrete spied class doesn't
directly implement — so it never happened to warm the sound tier-1 cache
before falling into the stale tier-2 entry, while ordinary direct calls to
`adapter.get(key)` (declared type = the concrete/spied class) always hit
tier 1 first and were unaffected. This explains why only 1/30 tests in the
suite were affected and why a **direct** call to `spyAdapter.get(key)` worked
fine while the **nested** call through `url()` didn't — nothing to do with
`SelfCallInfo` layering, purely which dispatch tier got there first.

## Fix

`vm/src/runtime/interpreter.rs`, `execute_invokevirtual_vtable_fast`: after
resolving a vtable entry's cached method, check whether the entry's own
*declaring* class has ever been redefined
(`class_manager.class_redefine_generation(declaring_class_id) > 0`, gated on
the existing global `any_class_redefined()` fast-path flag so the check is
free when no redefinition has ever happened in the process). If so, treat it
as a cache miss and cede to the slow, redefine-aware path — mirroring the
guard already used a few lines above for the native-shadow decision
(`receiver_redefined`).

## Verification

- Minimal standalone repro
  (`docs/known-issues/springboot/repros/mockito-nested-selfcall-stub-bypass-RealProbe.java`,
  no JUnit/Spring context needed): now correctly prints the stubbed value and
  records the invocation via `Mockito.verify(...).get(key)`.
- `OtlpMetricsPropertiesConfigAdapterTests`: 30/30 PASS (up from 29/30).
- Regression: all 82 test classes in `spring-boot-micrometer-metrics` (chosen
  for density of `*PropertiesConfigAdapterTests`-shaped classes exercising the
  same interface-default/vtable-fast pattern) — 78 PASS, 3 EMPTY (no runnable
  tests: `PushRegistryPropertiesTests`, `StepRegistryPropertiesConfigAdapterTests`,
  `StepRegistryPropertiesTests`), 3 FAIL. All 3 failures reproduce **identically**
  against a pre-fix `dev` baseline binary, confirming they are pre-existing and
  unrelated to this fix:
  - `OtlpMetricsExportAutoConfigurationTests.whenNoSslBundleDefaultHttpSenderHasDefaultSslContext`
    — `SSLContext.getDefault()` object-identity assertion, unconnected to method
    dispatch.
  - `Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests` (2 tests),
    `LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests` (1 test) — JUnit
    Platform `DiscoveryIssueException` (test discovery fails before any test
    code runs), unconnected to method dispatch.

## Broader implication

`execute_invokevirtual_vtable_fast`'s `VtableEntry.resolved_method` cache was
unsound with respect to JVMTI class redefinition for *any* caller, not just
Mockito — this is a correctness gap in a VM-wide hot dispatch path. Any code
path that redefines a class's bytecode after a descendant's vtable has already
cached an inherited slot for it (Mockito, `Instrumentation.redefineClasses`,
hot-reload agents, etc.) was affected. The fix is general, not Mockito-specific.
