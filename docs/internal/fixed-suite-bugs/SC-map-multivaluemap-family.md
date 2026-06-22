# SC-map-multivaluemap-family — Map / MultiValueMap utility divergences

> **STATUS: RESOLVED (archived).** All three in-cluster native-collections Map
> bugs are fixed on `dev`:
> - **RC-1** (`keySet().contains` not delegating to the source map's overridable
>   `containsKey`) — fixed; `native_hs_contains` keySet-kind branch now dispatches
>   `source.containsKey(elem)` (`native-collections/src/lib.rs`, keySet branch of
>   `native_hs_contains`).
> - **RC-2** (`LinkedHashMap.putIfAbsent` not replacing a `null`-mapped value) —
>   fixed; `native_lhm_put_if_absent` now stores + returns null when the existing
>   value is `null`.
> - **RC-3** (`Map.equals` mishandling a non-native-layout `Map` operand) — fixed
>   `fix/sc-map-equals-foreign-rc3-v2` (`e115b0bd`): `native_map_equals` now routes
>   every `other` access through virtual dispatch (`other.size()`/`get()`/
>   `containsKey()`) behind an `instanceof Map` guard, restoring
>   `AbstractMap.equals` semantics for cross-implementation comparisons; also
>   tightened the null-value contract. Verified vs HotSpot (JDK 25) with
>   `test_classes/MapEqRepro` (pre-fix 12/15, post-fix 15/15).
>
> **RC-4 / RC-5 remain open as handoffs but are NOT Map-family defects** — they
> are the cross-cutting ByteBuddy `ClassInjector$UsingReflection` /
> Mockito-proxy + JUnit "TimeoutExtension multiple times" masking gaps tracked in
> the bug-E workstream (`SC-task-retry-util-misc.md`, and the "JUnit multiple
> times masks a VM linkage error" note). They do not block archiving this
> Map-family report.

## Title
CratonVM divergences in Spring's `LinkedCaseInsensitiveMap`, `LinkedMultiValueMap`,
and `(Unmodifiable)MultiValueMap` — three native-collections Map bugs plus two
cross-cutting ByteBuddy/Mockito-proxy gaps.

## Symptom
Six failures across the Spring `org.springframework.util` Map family. They split
into **three genuine native-collections Map bugs** and **two ByteBuddy/Mockito
class-injection artifacts** (already tracked elsewhere):

| Test | FAILCAUSE (log) | Class of bug |
|------|-----------------|--------------|
| `LinkedCaseInsensitiveMapTests.putAndGet` | `AssertionFailedError` | native-collections (RC-1) |
| `LinkedCaseInsensitiveMapTests.putWithOverlappingKeys` | `AssertionFailedError` | native-collections (RC-1) |
| `LinkedCaseInsensitiveMapTests.computeIfAbsentWithExistingValue` | `AssertionFailedError` | native-collections (RC-2) |
| `LinkedMultiValueMapTests.equals` | `AssertionFailedError` | native-collections (RC-3) |
| `MultiValueMapTests.canNotChangeAnUnmodifiableMultiValueMap` | `IllegalArgumentException: Could not create type` | ByteBuddy (RC-4, handoff) |
| `UnmodifiableMultiValueMapTests.delegation` | `JUnitException: Chain of InvocationInterceptors called invocation multiple times … TimeoutExtension` | masked VM error / Mockito (RC-5, handoff) |

## Affected tests
- `org.springframework.util.LinkedCaseInsensitiveMapTests` — `putAndGet`, `putWithOverlappingKeys`, `computeIfAbsentWithExistingValue` (3)
- `org.springframework.util.LinkedMultiValueMapTests` — `equals` (1)
- `org.springframework.util.MultiValueMapTests` — `canNotChangeAnUnmodifiableMultiValueMap` (1)
- `org.springframework.util.UnmodifiableMultiValueMapTests` — `delegation` (1)

---

## Root cause 1 — `keySet()` view `contains` does not delegate to the map's (overridable) `containsKey`

**Files:**
- `native-collections/src/lib.rs:6243` `native_hs_contains` — keySet branch at `:6296-6297`
- `native-collections/src/lib.rs:4732` `native_map_key_set` / `:18417` `native_lhm_key_set` (view builders)
- Spring: `LinkedCaseInsensitiveMap.java:114-132` (the inner anonymous `LinkedHashMap` subclass overriding `containsKey`), `:351-393` (`KeySet`), `:158-160` (`containsKey`)

`LinkedCaseInsensitiveMap` wraps an **anonymous subclass of `LinkedHashMap`** (`targetMap`)
that *overrides* `containsKey(Object)` to call back into the outer case-insensitive
`containsKey` (`LinkedCaseInsensitiveMap.java:117-120`). Spring's own `KeySet.contains`
delegates to `targetMap.keySet().contains(o)` (`:365-367`).

On real HotSpot, `LinkedHashMap.KeySet.contains(o)` is `return containsKey(o)` — a
**virtual** call, so it hits the subclass override and resolves case-insensitively.
`keySet().contains("KEY")` therefore returns `true` even though the stored key is `"key"`.

CratonVM models `keySet()` as a synthetic `HashSet` whose backing map stores the raw
keys (`native_lhm_key_set` → `make_view_set_of`). `native_hs_contains` for a **keySet-kind**
view does a *direct* lookup of the argument in that backing
(`native_map_contains_key(backing, elem)`, `lib.rs:6296-6297`), comparing via
`map_keys_equal` (String `equals`). Since `"KEY".equals("key")` is `false`, the view's
`contains` returns `false`, diverging from HotSpot.

Note the asymmetry: the **entrySet** branch of the same function (`lib.rs:6261-6294`)
*does* delegate to the source map's `containsKey`/`get` via `ctx.invoke_virtual(...)`;
the keySet branch does not. That is why `entrySetContainsIsCaseInsensitive` passes but
the keySet assertions fail.

**Failing assertions:** `putAndGet` lines 50-51 and `putWithOverlappingKeys` lines 67-68
(`map.keySet().contains("KEY")` / `("Key")`). (The direct `containsKey("KEY")`
assertions just above pass because they route through the plain `caseInsensitiveKeys`
HashMap, which works.)

---

## Root cause 2 — `LinkedHashMap.putIfAbsent` does not replace a `null`-mapped value

**File:** `native-collections/src/lib.rs:18530-18542` `native_lhm_put_if_absent`

```rust
if let Some(node) = lhm_find_node(ctx, this, &key)? {
    let existing = ctx.get_field(node, LHM_NODE_VALUE);
    Ok(Some(existing))            // <-- returns even when existing == null, and does NOT store
} else {
    native_lhm_put(ctx, args)
}
```

JDK contract (`Map.putIfAbsent`): *"If the specified key is not already associated with
a value **or is mapped to null**, associates it with the given value and returns null."*
CratonVM's LHM version returns the existing value unconditionally and never stores the
new value when the key is present-but-`null`. So a `null`-valued mapping is left at `null`.

`computeIfAbsentWithExistingValue` (`LinkedCaseInsensitiveMapTests.java:104-108`):
- L104 `put("null", null)` → `targetMap["null"] = null`.
- L105 `putIfAbsent("NULL", "value")` → Spring routes to `targetMap.putIfAbsent("null","value")`
  (`LinkedCaseInsensitiveMap.java:209-221`). HotSpot replaces the `null` with `"value"`
  and returns `null`. CratonVM returns `null` (assertion on L105 still passes) **but leaves
  `targetMap["null"] == null`**.
- L106 `assertThat(map.put("null", null)).isEqualTo("value")` — expects the value put by
  L105. On CratonVM the old value is still `null` → returns `null` → **AssertionFailedError**.

(The plain-HashMap analogue `native_map_put_if_absent`, `lib.rs:4880-4890`, happens to be
correct-by-accident: it can't distinguish absent from present-null via `native_map_get`,
so it re-puts in both cases. Only the LHM path is wrong. For full contract parity the LHM
path should mirror that behaviour.)

---

## Root cause 3 — `Map.equals` mishandles a non-natively-modelled `Map` operand

**File:** `native-collections/src/lib.rs:4936-4991` `native_map_equals` (registered on
`java/util/HashMap` only, `lib.rs:3832`; `LinkedHashMap` inherits it — there is no
`native_lhm_equals`)

`native_map_equals` reads both operands through *native-layout* helpers:
- `map_state(this)` and `map_state(other)` for the size check (`:4948-4949`)
- `native_map_get(other, key)` for each entry (`:4964-4965`)
- `map_collect_entries(this)` for `this`'s entries (`:4962`)

These only understand CratonVM's native map backends (HashMap buckets / LHM overlay /
TreeMap / CHM). When an operand is an arbitrary Java `Map` — e.g. Spring's
`LinkedMultiValueMap` (a plain class whose slot 0 is its `targetMap` field, not a bucket
array) — `map_state`/`native_map_get` read the wrong slots and report **size 0 / no
entries**, so equality silently returns `false`.

`LinkedMultiValueMapTests.equals` (`:127-130`):
```java
Map<String,List<String>> o2 = new HashMap<>();
o2.put("key1", Collections.singletonList("value1"));
assertThat(o2).isEqualTo(map);   // o2.equals(map) — map is a LinkedMultiValueMap
```
`o2.equals(map)` → `native_map_equals(this=HashMap o2, other=LinkedMultiValueMap)` →
`map_state(other)` reads slot 0 of the Spring object (an LHM ref, not an array) → buckets
absent, `size_b` resolves to 0 → `size_a(1) != size_b(0)` → returns `false` →
**AssertionFailedError**.

The reverse direction `map.equals(o2)` works (Spring's `LinkedMultiValueMap.equals`
bytecode delegates to `targetMap.equals(o2)`, i.e. native LHM `this` vs native HashMap
`other`, both natively modelled). The bug only bites when a **native** map's `equals` is
invoked with a **non-native** Map argument. Correct behaviour is to fall back to virtual
dispatch (`other.size()`, `other.get(key)`, and iterating via `this.entrySet()`/the
arg's `entrySet`) per `AbstractMap.equals`.

---

## Root cause 4 (handoff) — `assertSoftly` ByteBuddy proxy fails: `Could not create type`

**Test:** `MultiValueMapTests.canNotChangeAnUnmodifiableMultiValueMap`
(`MultiValueMapTests.java:142-168`)

The test body is entirely `assertSoftly(softly -> …)`. AssertJ `SoftAssertions` builds a
proxy with **ByteBuddy**, whose `TypeCache.findOrInsert` instantiates
`net.bytebuddy.dynamic.loading.ClassInjector$UsingReflection`, which CratonVM cannot
load/link → surfaced as `java.lang.IllegalArgumentException: Could not create type`. The
`UnmodifiableMultiValueMap` class itself is fine — every mutator throws
`UnsupportedOperationException` as expected; the failure is purely the soft-assertion
proxy machinery. This is the **same** `ClassInjector$UsingReflection` gap already
documented for `TestGroupTests` in `spring-suite/bugs/SC-task-retry-util-misc.md`
(Root cause 6) and folds into the ByteBuddy/Mockito (bug-E) workstream. **Not a Map bug.**

## Root cause 5 (handoff, lower confidence) — `delegation()` masked VM error via TimeoutExtension

**Test:** `UnmodifiableMultiValueMapTests.delegation` (`:43-72`)

FAILCAUSE is `JUnitException: Chain of InvocationInterceptors called invocation multiple
times … TimeoutExtension` — the documented JUnit *masking* symptom (see memory
"JUnit 'multiple times' masks a VM linkage error"): a VM-raised `InternalError`
/`NoSuchMethodError` re-enters the interceptor chain, hiding the real exception. The
sibling mock-heavy tests in the same class (`entrySetDelegation`, `valuesDelegation`,
`entrySetUnsupported`, `valuesUnsupported`, `unsupported`) all **pass**, so this is not a
blanket Mockito-mock failure. `delegation()` is unique in chaining AssertJ
`assertThat(map).hasSize(1)` / `.isNotEmpty()` over the proxy and
`assertThat(result).containsExactly("bar")` over a `Collections.unmodifiableList(...)`
wrapping a Mockito-returned list, then `result.add(...)` expecting
`UnsupportedOperationException`. The raw stack is not in
`spring-suite/full-run/spring-core.log` (only the FAILCAUSE summary), so the precise
masked error is unconfirmed. Best handed off with the ByteBuddy/Mockito family; re-run
with raw-trace capture to extract the underlying exception.

---

## Reproduction sketch

RC-1 (keySet case-insensitive contains):
```java
var m = new org.springframework.util.LinkedCaseInsensitiveMap<String>();
m.put("key", "v");
System.out.println(m.keySet().contains("KEY")); // HotSpot: true; CratonVM: false
```

RC-2 (LHM putIfAbsent over null):
```java
var lhm = new java.util.LinkedHashMap<String,String>();
lhm.put("k", null);
Object r = lhm.putIfAbsent("k", "v");          // HotSpot: returns null AND stores "v"
System.out.println(r + " / " + lhm.get("k"));  // HotSpot: null / v   CratonVM: null / null
```

RC-3 (Map.equals with a foreign Map arg):
```java
var mvm = new org.springframework.util.LinkedMultiValueMap<String,String>();
mvm.set("key1", "value1");
var hm = new java.util.HashMap<String,java.util.List<String>>();
hm.put("key1", java.util.Collections.singletonList("value1"));
System.out.println(hm.equals(mvm));            // HotSpot: true; CratonVM: false
```

Command (suite harness convention; do NOT run while a suite is active):
```
cratonvm --java-home <jdk25> -cp <spring-core-test-cp> \
  org.junit.platform.console.ConsoleLauncher \
  -c org.springframework.util.LinkedCaseInsensitiveMapTests
```

## Suspected subsystem
`native-collections` (synthetic `HashMap`/`LinkedHashMap` natives in
`native-collections/src/lib.rs`) for RC-1/2/3. `classloading` / ByteBuddy
`ClassInjector$UsingReflection` (bug-E family) for RC-4/5.

## Severity
- RC-1: **Medium** — any `LinkedHashMap` subclass that overrides `containsKey` (or
  `get`/`equals` semantics) will have an inconsistent `keySet().contains`. Broad pattern
  (Spring `LinkedCaseInsensitiveMap` is widely used for HTTP headers).
- RC-2: **Medium** — silent `putIfAbsent` data loss on any `LinkedHashMap` (incl.
  subclasses) holding null values; a Map-contract correctness violation.
- RC-3: **Medium** — `nativeMap.equals(foreignMap)` wrongly returns `false`; affects any
  cross-implementation Map comparison (very common in tests and config merging).
- RC-4/5: Medium but out-of-cluster (existing ByteBuddy/Mockito gap).

## Confidence
- RC-1: **High** — exact code path (`lib.rs:6296-6297` keySet branch vs `:6261-6294`
  entrySet branch) and the overriding anonymous subclass (`LinkedCaseInsensitiveMap.java:117-120`)
  pinpoint the divergent assertion; consistent with `entrySetContains*` passing.
- RC-2: **High** — `native_lhm_put_if_absent` (`:18536-18538`) plainly returns existing
  without storing; JDK contract requires replace-on-null.
- RC-3: **High** — `native_map_equals` (`:4948-4949`, `:4964-4965`) reads operands via
  native-layout helpers with no virtual-dispatch fallback; `LinkedMultiValueMap` slot 0 is
  not a bucket array. (Assertion-line attribution inferred from the equals direction; the
  raw assertion text is blank in the log.)
- RC-4: **High** (matches the documented `ClassInjector$UsingReflection` failure).
- RC-5: **Medium** (masked symptom; raw stack unavailable).

## Recommendation
- **RC-1 — Fix.** In `native_hs_contains`, make the keySet-kind branch delegate to the
  source map's `containsKey` via `ctx.invoke_virtual(source, "containsKey", …)` (exactly
  as the entrySet branch already does at `lib.rs:6275-6276`) when the backing carries a
  `view_backing_source`. Falls back to the current direct lookup for plain (non-view)
  HashSets.
- **RC-2 — Fix.** In `native_lhm_put_if_absent`, when `lhm_find_node` finds a node whose
  value is `Value::Object(None)`, store the new value (set `LHM_NODE_VALUE`) and return
  `null`; only return the existing value when it is non-null. Small, localized,
  contract-correct change.
- **RC-3 — Fix.** In `native_map_equals`, when an operand is not a natively-modelled map
  (detect via `is_lhm_receiver`/`is_chm_receiver`/`is_tree_map_receiver`/bucket presence),
  use virtual dispatch: size via `other.size()`, per-entry via `other.get(key)`, and
  collect `this`'s entries via `this.entrySet()` iteration as a fallback. This restores
  `AbstractMap.equals` semantics for cross-implementation comparisons.
- **RC-4 / RC-5 — Handoff** to the existing ByteBuddy/Mockito class-injection (bug-E)
  workstream; not Map-family defects.

## Open questions
1. The per-test `AssertionFailedError` messages are blank in `spring-core.log` (only
   `FAILCAUSE … ::` with no detail). RC-1/RC-2/RC-3 line attributions are derived by
   static trace; a raw-trace re-run would confirm which assertion line trips first
   (especially whether `putAndGet`/`putWithOverlappingKeys` die on the keySet assertions
   vs an earlier line).
2. RC-5 (`delegation`): capture the masked underlying exception (raw stack) to confirm it
   is the ByteBuddy/Mockito family and not a distinct `Collections.unmodifiableList`-over-
   mock or AssertJ-over-proxy gap.
3. Does any OTHER `LinkedHashMap`-subclass pattern in the suite (e.g. Spring
   `AnnotationAttributes`, `MultiValueMapAdapter`) also rely on the overridden-`containsKey`
   → `keySet().contains` path? RC-1's fix should be validated against those too.
4. Related but out-of-cluster: `MimeTypeTests.serialize` fails with
   `NotSerializableException: cratonvm.internal.UnmodifiableMap` (log line 249) — the
   synthetic unmodifiable-map wrapper is not `Serializable`. Confirms the sibling
   "UnmodifiableMultiValueMap may be non-Serializable" hint; worth a separate
   serialization-of-collection-wrappers item.
