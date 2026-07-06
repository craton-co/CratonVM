# Hibernate `DomainParameterXref` — `LinkedHashMap` `removeEldestEntry` dispatch resolves to `Object`, not `LinkedHashMap`

| | |
|---|---|
| **Status** | 🔴 OPEN — not yet root-caused or fixed. Hypothesis below is unverified. |
| **Area** | VM — native `LinkedHashMap`/`HashMap` shim (`native-collections/src/lib.rs`), `removeEldestEntry` eviction-hook dispatch |
| **Symptom** | `java.lang.NoSuchMethodError: java/lang/Object.removeEldestEntry(Ljava/util/Map$Entry;)Z` |
| **Severity** | blocks `org.hibernate.orm.test.jpa.criteria.InPredicateTest` (and likely any other criteria/HQL-parameter test that constructs `DomainParameterXref`) from completing, once the earlier `values`-null NPE is fixed — see [`docs/internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md`](../internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md). |
| **Discovered** | 2026-07-06, while verifying the `InPredicateTest` `values`-null fix. Reported both under `--nojit` and with JIT/OSR enabled — not JIT-specific. |

## Symptom

`org.hibernate.query.sqm.internal.DomainParameterXref`'s constructor
(`hibernate-core/src/main/java/org/hibernate/query/sqm/internal/DomainParameterXref.java`)
builds a plain field:

```java
private final LinkedHashMap<QueryParameterImplementor<?>, List<SqmParameter<?>>> sqmParamsByQueryParam
        = new LinkedHashMap<>(...);
```

This is **not** a subclass and does **not** override `removeEldestEntry`.
Yet inserting into it throws:

```
NoSuchMethodError method="java/lang/Object.removeEldestEntry(Ljava/util/Map$Entry;)Z"
  caller="org/hibernate/query/sqm/internal/DomainParameterXref.<init>(...)  @pc=109"
```

i.e. CratonVM's native `LinkedHashMap.put` internals attempted a virtual
dispatch of `removeEldestEntry` that resolved against `java/lang/Object`
(which has no such method) rather than against `LinkedHashMap` (whose
protected `removeEldestEntry` always returns `false` and should either be
found correctly, or — per the existing plain-`LinkedHashMap` fast-path guard
— never be invoked at all for an un-subclassed instance).

Repro (Azure host, harness at `/data/data/apps/hibernate-orm-harness/hib-suite-runner`):
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 [CRATONVM_JIT_OSR=1] \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.jpa.criteria.InPredicateTest) 0
```
(reproduced both with and without `--nojit`).

## What's already in place (and apparently not enough)

`native-collections/src/lib.rs`, end of `native_lhm_put` (see
[[reference_lhm_removeeldestentry_eviction]] internal memory / dev commit
`83ac5788`), already has an explicit guard meant to cover exactly this case:

```rust
let this_cid = ctx.class_id_of_object(this);
let is_plain_lhm = ctx.class_name_of_id(this_cid).as_deref() == Some("java/util/LinkedHashMap");
if !is_plain_lhm {
    // ... invoke_virtual("removeEldestEntry", ...) ...
}
```

For a genuinely plain `java.util.LinkedHashMap` instance, `is_plain_lhm`
should be `true` and the whole `invoke_virtual` block should be skipped
entirely — so this `NoSuchMethodError` should not be reachable for
`DomainParameterXref`'s field as currently written. That it IS being hit
means either this guard's `this_cid`/class-name lookup is wrong for this
specific instance, or a *different* call site (not this one) is performing
the virtual dispatch without the same guard.

## Hypothesis (not yet verified)

The receiver's `ClassId` may be misreported as `0` for this instance. The
class store maps `ClassId(0)` to `java/lang/Object` in some contexts (see
the already-fixed, but call-site-specific, JIT virtual/interface MIC bug in
[`hib-jpalargeblobtest-object-read-nosuchmethod.md`](../internal/fixed-suite-bugs/hib-jpalargeblobtest-object-read-nosuchmethod.md)
— `java/lang/Object.read()I` from the exact same "ClassId(0) → Object"
misclassification pattern, fixed only in the JIT's `jit_invoke_virtual_mic`
helper, not necessarily in this native-collections call site). If this
`LinkedHashMap`'s header/ClassId is `0` (or otherwise resolves to
`java/lang/Object`) at the point `native_lhm_put` reads it, `is_plain_lhm`
would incorrectly be `false`, the `invoke_virtual` branch would run, and the
dispatch would search `Object`'s methods for `removeEldestEntry` — exactly
matching the observed error.

Not yet investigated / next steps:
- Confirm with a targeted debug print (or a minimal standalone repro
  constructing a plain `LinkedHashMap` and inserting many entries in the
  same allocation pattern `DomainParameterXref` uses) whether `this_cid`
  really is `0`/misresolved at the failing call, or whether `is_plain_lhm`'s
  string comparison itself has a subtler bug (e.g. interned-string identity
  vs. equality, or a loader-qualified name mismatch).
- If it is a `ClassId(0)` issue: determine why this particular
  `LinkedHashMap` instance's header ends up that way — is it specific to how
  `DomainParameterXref` constructs/copies the map, or general to any
  `LinkedHashMap` allocated in a similar context (e.g. inside a hot
  JIT-compiled/OSR-compiled constructor, per the neighboring
  `InPredicateTest` OSR bug — worth checking if these two bugs share a
  common "object header corruption under JIT" ancestor, or are unrelated).
- Check whether `native_lhm_remove`, `native_hm_put`, or other native
  collection natives have the same `is_plain_lhm`-style guard, in case this
  is a second, un-guarded call site rather than a bug in the existing guard.
