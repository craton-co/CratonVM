# `ParentAwareNamingStrategyTests` — `ObjectName._getKeyPropertyList`/`getKeyPropertyListString`/`appendToObjectName` still NPE on `_ca_array`/`_kp_array` — residual of an already-FIXED `ObjectName` gap, different accessor methods

**Status: OPEN — found 2026-07-17 (residual of a FIXED sibling bug)**

## Symptom

All 4 tests in `ParentAwareNamingStrategyTests` fail with
`NullPointerException`s reading `this._ca_array`/`this._kp_array` from
inside real `javax.management.ObjectName` bytecode, reached via Spring's
`JmxUtils.appendIdentityToObjectName` or CratonVM-autoconfigured
`ParentAwareNamingStrategy` itself:

```
=> java.lang.NullPointerException: Cannot read the array length because "this._ca_array" is null
   javax.management.ObjectName._getKeyPropertyList(ObjectName.java:1495)
   javax.management.ObjectName.getKeyPropertyList(ObjectName.java:1520)
   org.springframework.jmx.support.JmxUtils.appendIdentityToObjectName(JmxUtils.java:219)
   org.springframework.boot.autoconfigure.jmx.ParentAwareNamingStrategy.getObjectName(ParentAwareNamingStrategy.java:72)
```

```
=> java.lang.NullPointerException: Cannot read the array length because "this._kp_array" is null
   javax.management.ObjectName.getKeyPropertyListString(ObjectName.java:1535)
   org.springframework.boot.autoconfigure.jmx.ParentAwareNamingStrategyTests.lambda$objectNameMatchesManagedResourceByDefault$0(ParentAwareNamingStrategyTests.java:45)
```

```
=> java.lang.NullPointerException: Cannot read the array length because "this._ca_array" is null
   javax.management.ObjectName._getKeyPropertyList(ObjectName.java:1495)
   javax.management.ObjectName.getKeyPropertyList(ObjectName.java:1520)
   org.springframework.boot.autoconfigure.jmx.ParentAwareNamingStrategy.appendToObjectName(ParentAwareNamingStrategy.java:98)
   org.springframework.boot.autoconfigure.jmx.ParentAwareNamingStrategy.getObjectName(ParentAwareNamingStrategy.java:75)
```

`tests=4 failed=4`. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.jmx.ParentAwareNamingStrategyTests.out.log`

## Root cause — confirmed mechanism, but this is a genuine residual of an already-FIXED doc, not a new bug from scratch

This is the *exact same underlying defect* documented and marked FIXED in
[`docs/internal/fixed-suite-bugs/wildfly-standalone-boot-objectname-ca-array-npe-FIXED.md`](../../internal/fixed-suite-bugs/wildfly-standalone-boot-objectname-ca-array-npe-FIXED.md)
(fixed 2026-07-14, commit `974c0838`): CratonVM's native `javax/management/ObjectName`
`<init>` (`NativeKind::Bridge`, `native-builtins/src/jmx.rs::register_object_name`)
constructs a bare 1-field synthetic object (`alloc_concurrent_synthetic(ctx,
"javax/management/ObjectName", 1)`) that stores only the canonical name
string — real fields like `_ca_array` (`Property[]`, the parsed
canonical-array cache) and `_kp_array` are never populated. Several
`ObjectName` accessor methods are natively intercepted against that 1-field
model and work fine; any method **not** covered falls through to real JDK
bytecode, which reads the real (never-populated, `null`) private field and
throws.

**The 2026-07-14 fix only added natives for `getCanonicalKeyPropertyListString()`,
`isPattern()`, `isDomainPattern()`, `isPropertyPattern()`, and
`isPropertyListPattern()`** — the specific methods that were blocking
WildFly's `Repository.addMBean` path. This test hits **three different,
still-uncovered `ObjectName` accessors** that read the same never-populated
private fields:

- `_getKeyPropertyList()` / `getKeyPropertyList()` (line 1495/1520) — reached
  via Spring's `JmxUtils.appendIdentityToObjectName`.
- `getKeyPropertyListString()` (line 1535) — reached directly by the test.
- The same `_getKeyPropertyList()` path again, this time via
  `ParentAwareNamingStrategy.appendToObjectName` → `getObjectName`.

None of these were in the 5-native list the FIXED doc added, so they still
fall through to real bytecode and still NPE on the same never-populated
`_ca_array`/`_kp_array` fields — a genuine, real residual of the same
class-layout gap, not a regression or a re-occurrence of the fixed bug
itself.

**Per this session's triage instructions:** this is exactly the "class still
fails despite a doc claiming FIXED" case — noted explicitly here rather
than silently re-filed as an unrelated new bug or silently treated as a
duplicate. The FIXED doc's own fix-approach section (option (b),
"natively implement the specific un-intercepted methods against the
synthetic layout") directly describes the fix this residual needs: add
`getKeyPropertyList()`/`_getKeyPropertyList()`/`getKeyPropertyListString()`
natives to `register_object_name` in `native-builtins/src/jmx.rs`, derived
from the same canonical-string text model (`object_name_parts()`) the
existing 5 natives already use — same file, same pattern, just more
accessor coverage.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.jmx.ParentAwareNamingStrategyTests` |
