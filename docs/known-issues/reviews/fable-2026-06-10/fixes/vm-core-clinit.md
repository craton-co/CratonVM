# Fix: vm-core-clinit — invert the `<clinit>`-failure swallow default to JVMS-correct

**ID:** vm-core-clinit
**File owned/edited:** `vm/src/vm/vm_util.rs` (only)
**Report refs:** `docs/reviews/fable-2026-06-10/vm-core.md` — Bugs B1 & B2; Feature Suggestion #1.

## Finding

On a `<clinit>` exception, `initialize_class_shared` (in `vm_util.rs`) **swallowed**
the failure for a broad framework/JDK allowlist (`java/`, `jdk/`, `sun/`, `javax/`,
`org/jboss/`, `io/quarkus/`, `org/wildfly/`, `io/smallrye/`,
`org/springframework/boot/loader/`, `org/slf4j/impl/`, `ch/qos/logback/`, `com/sun/`),
marked the class `Initialized` anyway, and then called `post_clinit_fixup` to
**fabricate synthetic static state** (LogManager.manager, a pass-through
`NoopNormalizer2` Unicode normalizer, VarHandle `FORM`, Quarkus `DELAYED_HANDLER`,
JBoss/Spring/WildFly singletons). This violates JVMS §5.5 (the class must become
*Erroneous* and throw `ExceptionInInitializerError`/`NoClassDefFoundError`) and the
project's no-synthetic-stubs policy. It was the **default**; only
`CRATONVM_STRICT_SWALLOWS=1` produced correct behavior.

## Root cause

The two swallow blocks in the `Err(e)` arm of the `<clinit>` result match were
unconditional:
1. the `is_stack_underflow` swallow (stack-error/invokedynamic) →
   `finalize_init(Initialized); return Ok(())`,
2. the `is_swallowable` swallow (recoverable exception types × framework allowlist)
   → `finalize_init(Initialized); post_clinit_fixup(...); return Ok(())`.

When neither block ran, control already fell through to the JVMS-correct
`finalize_init(... InitializationError)` + propagate/wrap path (lines ~1064+). So
inverting the default is achieved purely by gating those two swallow blocks.

## Exact change

1. **Module doc comment** added at the top of `vm_util.rs` documenting the policy:
   strict (mark `InitializationError` + throw, no synthetic backfill) is the
   **default**; `CRATONVM_LENIENT_CLINIT=1` restores the legacy lenient mode and is
   the single env var to flip back.
2. **New gate helper** `lenient_clinit()` — a `OnceLock<bool>` cached read of
   `CRATONVM_LENIENT_CLINIT` (only literal `"1"` enables), mirroring the existing
   `env_cache::strict_swallows()` convention. Defined locally because `env_cache.rs`
   is not owned by this agent.
3. **Gated the two swallow blocks** on `&& lenient_clinit()`:
   - `if is_stack_underflow && lenient_clinit()`
   - `if is_swallowable && lenient_clinit()`
   Each now emits a one-line `tracing::warn!` ("CRATONVM_LENIENT_CLINIT: swallowing
   <clinit> failure …(JVMS-divergent)") before swallowing, so the divergence is
   visible. With the gate OFF (default), both blocks are skipped and the existing
   JVMS-correct `InitializationError` + `ExceptionInInitializerError` path runs.
   `post_clinit_fixup`'s **app-specific synthetic backfills are now reachable only
   under the lenient gate** (the only caller after a swallowed clinit is inside the
   gated `is_swallowable` block).

**Intentionally left unconditional** (NOT after a swallowed clinit, and the report
classifies them as legit JDK-layout repairs, not app-faking):
- the `Ok(_)` success-path fixups for `java/math/BigInteger` and
  `java/nio/file/attribute/PosixFilePermission`,
- the no-`<clinit>` / synthetic-stub-only `post_clinit_fixup` call in the `else`
  branch (`is_synthetic_stub` path).

The `CRATONVM_STRICT_SWALLOWS=1` escalation gate (BigDecimal `recoverable_silent`
arm) is preserved; it is now effectively inert because swallowing no longer happens
by default.

## Files touched
- `vm/src/vm/vm_util.rs` — module doc, `lenient_clinit()` helper, two gated swallow
  blocks + warns, one new `#[cfg(test)]` test.

## Tests added
- `lenient_clinit_defaults_off` (in the existing `#[cfg(test)] mod tests`): asserts
  the gate reads `false` when `CRATONVM_LENIENT_CLINIT` is unset (and honors `"1"` if
  a polluted env sets it). It does not mutate the env because the gate is a
  process-lifetime `OnceLock`. Compiles against `super::*` (helper is module-private
  and in scope). The report's deeper suggested tests (swallow-vs-strict end-to-end
  with a failing framework `<clinit>`) need a real loaded class + thread harness; left
  as follow-up.

## Follow-up & risk

**behavioral_risk (LOUD):** This **changes default boot behavior for many framework
apps** — Quarkus, JBoss, Spring, WildFly, SLF4J/logback, ICU, and any other class on
the old allowlist whose real `<clinit>` currently fails will now throw
`ExceptionInInitializerError`/`NoClassDefFoundError` at first use instead of silently
continuing with null/synthetic statics. The app gauntlet (`apps/TARGET_APPS.md`, the
10–15 pool) and the keycloak/H2/Tomcat/WildFly suites **MUST be re-validated before
merge**; expect regressions from classes that were previously only "working" because
their failure was masked. To restore prior behavior in one step:
`CRATONVM_LENIENT_CLINIT=1`. The orchestrator is expected to flag this change for
gauntlet validation.

Recommended follow-ups (out of scope here): convert each lenient `post_clinit_fixup`
arm into a tracked `docs/gaps/` entry with a gap id surfaced in the warn line
(report Feature Suggestion #2); add the end-to-end strict-vs-lenient differential
test once a loaded-class harness is available.
