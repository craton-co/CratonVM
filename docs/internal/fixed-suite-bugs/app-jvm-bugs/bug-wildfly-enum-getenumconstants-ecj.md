# WildFly — `Class.getEnumConstants()` misses ECJ `ENUM$VALUES` field

## Status
**FIXED** on branch `fix/wildfly-enum-constants` (`native-builtins/src/lang_class.rs`). **NOT** in `target-bench` main build as of 2026-06-05 post-fix rerun.

## Severity
**HIGH** — JUnit subsystem tests fail; `EnumSet.allOf` throws on ECJ-compiled enums.

## App / suite
- **App:** WildFly (`apps/wildfly/health`)
- **Test:** `HealthSubsystemTestCase` (Parameterized runner)
- **Class:** `org.wildfly.extension.health.HealthSubsystemSchema`
- **Logs:** `test-infra/suite-results/apps-four-20260604-233229/`, `apps-four-20260605-090220/`

## Symptom (before fix)

```
native_class_get_enum_constants: no $VALUES field for class=org/wildfly/extension/health/HealthSubsystemSchema
java.lang.ClassCastException: class org.wildfly.extension.health.HealthSubsystemSchema not an enum
```

JUnit: **initializationError**, 0 tests run.

## HotSpot behavior

`Class.getEnumConstants()` returns `{ VERSION_1_0, … }`. Parameterized runner discovers test parameters; 2 tests execute.

## CratonVM behavior (before fix)

`getEnumConstants()` returned **null** because implementation looked only for synthetic field **`$VALUES`** (javac naming). WildFly health module is compiled with **Eclipse JDT (ecj)**, which emits **`ENUM$VALUES`**:

```
private static final HealthSubsystemSchema[] ENUM$VALUES;
```

(`javap -p HealthSubsystemSchema.class`)

## Root cause (confirmed)

`native_class_get_enum_constants` in `lang_class.rs` only read `$VALUES`. ECJ uses `ENUM$VALUES`.

HotSpot invokes reflective `values()` and does not depend on field name.

## Fix

Accept both spellings: try `$VALUES`, then `ENUM$VALUES`.

**Result:** Parameterized runner runs 2 tests (deeper failures remain — see WF-5, WF-6).

## Reproduce

```bash
cd apps/wildfly/health
# CratonVM JUnit (requires cp from cratonvm-health-cp.txt)
target-bench/release/cratonvm.exe … org.wildfly.extension.health.HealthSubsystemTestCase
```

Or inspect enum class:

```bash
javap -p apps/wildfly/health/target/classes/org/wildfly/extension/health/HealthSubsystemSchema.class
```

## Verify fix merged

Post-fix rerun on **main target-bench** still showed `not an enum` — merge `fix/wildfly-enum-constants` and rebuild.

## Related

- [bug-wildfly-string-format-positional-index.md](bug-wildfly-string-format-positional-index.md) (next failure after enum fix)
- `apps/wildfly/CRATONVM_BUGS.md` Bug 1
