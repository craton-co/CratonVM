# FIXED: Spring Boot `ObjectName` key-property accessor and constructor residuals

**Status: FIXED 2026-07-18 (fix commit pending integration)**

## Symptom

`ParentAwareNamingStrategyTests` initially failed all four tests with
`NullPointerException` from real `javax.management.ObjectName` bytecode
reading `_ca_array` or `_kp_array`. CratonVM's native bridge constructor uses
a one-field synthetic ObjectName representation, so any unregistered accessor
that falls through to the real JDK implementation reads cache fields that do
not exist in that representation.

## Root cause and closure

This was a genuine residual of the 2026-07-14 ObjectName fix, which covered
`getCanonicalKeyPropertyListString()` and the pattern accessors but not the
remaining key-property family.

`native-builtins/src/jmx.rs` now natively implements:

- private `_getKeyPropertyList()` with a fresh `HashMap`;
- public `getKeyPropertyList()` with the required defensive `Hashtable`;
- `getKeyPropertyListString()` with source-order semantics;
- quote-safe key/value parsing shared by `getKeyProperty` and pattern logic.

Validation exposed the adjacent `ObjectName(String, Hashtable)` residual: it
previously copied only `name` and `type`, silently dropping arbitrary
properties (`name1`, `name2`, `context`, and `identity`). It now snapshots all
String table entries from both the real JDK `Hashtable$Entry` layout and the
CratonVM native map layout.

## Validation

An isolated worktree produced the unique release executable
`cratonvm-objectname-keyproperty-residual-20260718-019f75ac.exe`. The exact
`core/spring-boot-autoconfigure`
`ParentAwareNamingStrategyTests` fixture ran through `SbRunner` against the
shared Spring Boot checkout on JDK 25:

- JIT: PASS, 4 tests, 0 failed.
- `--nojit`: PASS, 4 tests, 0 failed.

Focused native registration and quote-safe parsing tests also pass with the
`experimental-jmx` feature enabled.
