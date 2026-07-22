# TestParameterMap — ParameterMap not reporting locked after lock() (FIXED)

**Status:** FIXED. **Severity:** low-medium (correctness of an immutability
guard). **HotSpot:** PASS (fresh-verified).

## Resolution (2026-07-11)

The original hypothesis below (a plain-`boolean` field-visibility bug) was
**wrong**. The real bug had nothing to do with `ParameterMap.locked` or field
visibility — `checkLocked()` was correctly observing `locked=true` on every
call. The actual defect: `org.apache.catalina.util.TestParameterMap`
line 147's failure is inside `testMapImmutabilityAfterLocked`'s
`paramMap.replaceAll(...)` assertion. `ParameterMap` doesn't override
`replaceAll` (or `putIfAbsent`/`merge`/`remove(k,v)`/`replace(...)`), so the
call runs `java.util.Map`'s **default** `replaceAll` method, whose body is:

```java
default void replaceAll(BiFunction<? super K, ? super V, ? extends V> function) {
    for (Map.Entry<K, V> entry : entrySet()) {
        ...
        entry.setValue(v);
    }
}
```

When locked, `ParameterMap.entrySet()` correctly returns
`Collections.unmodifiableMap(delegatedMap).entrySet()` — but CratonVM's
synthetic `Collections.unmodifiableMap` wrapper
(`cratonvm/internal/UnmodifiableMap`, in `native-collections/src/lib.rs`)
had `entrySet()`'s iterator hand back the **backing map's own, real, mutable**
`Map.Entry` objects unwrapped. `entry.setValue(v)` therefore silently
succeeded and mutated the "locked" map instead of throwing
`UnsupportedOperationException` — real JDK wraps each entry in
`Collections$UnmodifiableMap$UnmodifiableEntrySet$UnmodifiableEntry`, whose
`setValue` always throws.

This is why only `testMapImmutabilityAfterLocked` failed and
`testKeySetImmutabilityAfterLocked`/`testValuesImmutabilityAfterLocked`/
`testEntrySetImmutabilityAfterLocked` did not: those three call
`keySet()`/`values()`/`entrySet()` directly and only exercise **Set-level**
mutators (`add`/`remove`/`clear`/...), which were already correctly blocked.
Only `replaceAll`'s internal `entry.setValue()` — reached through the
iterator, not a Set-level call — went unguarded. A second, previously
undocumented failure was found on re-repro: because `entry.setValue()`
silently succeeded, `replaceAll`'s lambda
(`(a, b) -> TEST_PARAM_VALUES_REPLACED`) actually **overwrote every entry**
in the map, so `tearDown()`'s subsequent `assertArrayEquals` against the
original `param1`/`param2` values also failed
(`expected:<[value1]> but was:<[replaced]>`) — a residual of the same root
cause, not a second bug.

**Fix** (`native-collections/src/lib.rs`, `native-builtins/src/lib.rs`,
`vm/src/vm/vm_init.rs`): added a dedicated `cratonvm/internal/
UnmodifiableEntrySet` wrapper (distinct from the plain `UnmodifiableSet` used
for `keySet()`/`values()`) whose `iterator()`/`forEach()` wrap each yielded
`Map.Entry` in a new `cratonvm/internal/UnmodifiableMapEntry` view —
`getKey`/`getValue`/`toString`/`hashCode`/`equals` delegate to the real
entry, `setValue` throws `UnsupportedOperationException`. `Collections.
unmodifiableMap(...).entrySet()` (and `Map.of()`/`Map.copyOf()`, which share
the same wrapper class) now allocate this entry-set wrapper instead of the
plain Set wrapper. The new synthetic classes had to be registered as
implementing `java.util.Map$Entry`/`java.util.Set` in `vm_init.rs`'s
`unmod_specs` table — the first fix attempt broke
`testEntrySetImmutabilityAfterLocked` with a `ClassCastException` because
the wrapped entry didn't declare the `Map$Entry` interface, so the implicit
`checkcast` on `Iterator<Map.Entry>.next()`'s result failed.

Verified: `TestParameterMap` 4/4 PASS (stable across repeated runs). No
regressions found in a broader sweep of tests exercising
`Collections.unmodifiable*`/`entrySet()`
(`TestCaseInsensitiveKeyMap`, `TestMapELResolver`, `TestMembership`,
`TestImportHandlerStandardPackages` — 31/31 PASS). `TestExpiresFilter`'s
`testBug63909` fails independently of this change (`test/webapp` fixture
directory absent from the worktree — an environment/fixture gap, not a VM
bug; the directory doesn't exist in the main checkout either).

Fixed on branch `fix/parametermap-immutability-lock-20260711`.

## Original summary

`org.apache.catalina.util.TestParameterMap.testMapImmutabilityAfterLocked`
fails:
```
1) testMapImmutabilityAfterLocked(org.apache.catalina.util.TestParameterMap)
java.lang.AssertionError: ParameterMap is not locked.
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.Assert.fail(Assert.java:89)
	at org.apache.catalina.util.TestParameterMap.testMapImmutabilityAfterLocked(TestParameterMap.java:147)
```
Tomcat's `ParameterMap` (backs `ServletRequest.getParameterMap()`) has a
`setLocked(true)`/`isLocked()` mechanism to make the map immutable once
request parameter parsing is complete, guarding against post-parse
mutation. The test locks the map and then asserts `isLocked()` returns
`true` — on CratonVM it reports `false` (or an equivalent falsy/wrong
state), meaning the lock flag isn't sticking, isn't visible to the
subsequent read, or the specific `setLocked`/`isLocked` field pairing this
class uses behaves differently under CratonVM.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
PASSES on HotSpot.

### Original (incorrect) recommendation

Read `org.apache.catalina.util.ParameterMap.setLocked`/`isLocked` and
`TestParameterMap.java` around line 147 for the exact lock/assert sequence.
If the field is a plain `boolean` (not `volatile`/`AtomicBoolean`), check
whether CratonVM's field-write visibility or the specific JIT/interpreter
path this test exercises is losing the write — this pattern (a simple
boolean flag write not being observed by a subsequent same-thread read)
would be worth comparing against other plain-field visibility findings in
this codebase before assuming it's Tomcat-specific.

**This recommendation was a red herring** — see Resolution above for the
actual root cause (an unwrapped `Map.Entry` escaping an unmodifiable-map
view, not a field-visibility bug).
