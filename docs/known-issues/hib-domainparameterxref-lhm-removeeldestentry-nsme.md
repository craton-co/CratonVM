# Hibernate `DomainParameterXref` — `LinkedHashMap` `removeEldestEntry` dispatch resolves to `Object`, not `LinkedHashMap`

| | |
|---|---|
| **Status** | 🔴 OPEN — root cause traced to the receiver's `ClassId` reading `0`, which matches this codebase's already-documented, unresolved "Layer 1 register-invisible roots" gap. Not independently fixable here; see analysis below. |
| **Area** | VM — JIT/GC precise-root-tracking ("Layer 1"), surfacing here via the native `LinkedHashMap` shim's `removeEldestEntry` guard (`native-collections/src/lib.rs`) |
| **Symptom** | `java.lang.NoSuchMethodError: java/lang/Object.removeEldestEntry(Ljava/util/Map$Entry;)Z` |
| **Severity** | blocks `org.hibernate.orm.test.jpa.criteria.InPredicateTest` (and likely any other criteria/HQL-parameter test that constructs `DomainParameterXref`) from completing, once the earlier `values`-null NPE is fixed — see [`docs/internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md`](../internal/hib-inpredicatetest-criteria-values-null-npe-FIXED.md). |
| **Discovered** | 2026-07-06. Root cause traced same day. |

## Symptom

`org.hibernate.query.sqm.internal.DomainParameterXref`'s 2-arg constructor
builds a plain field (not a subclass, no `removeEldestEntry` override):

```java
private final LinkedHashMap<QueryParameterImplementor<?>, List<SqmParameter<?>>> sqmParamsByQueryParam
        = new LinkedHashMap<>( sqmParamCount );
...
sqmParamsByQueryParam.computeIfAbsent( queryParameter, impl -> new ArrayList<>() ).add( parameter );
```

Inserting throws:
```
NoSuchMethodError method="java/lang/Object.removeEldestEntry(Ljava/util/Map$Entry;)Z"
  caller="org/hibernate/query/sqm/internal/DomainParameterXref.<init>(...)  @pc=109"
```

Repro (Azure host, harness at `/data/data/apps/hibernate-orm-harness/hib-suite-runner`):
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.jpa.criteria.InPredicateTest) 0
```

**Correction to the original report**: this was initially thought to reproduce
under `--nojit` too. Directly retested 2026-07-06 with a debug-instrumented
binary: **with `--nojit` the test passes cleanly (`ok=1`) and the guard below
never sees a bad ClassId.** The bug is JIT-dependent, contrary to the
original note — likely the `--nojit` control run in the original report used
a different binary/build.

## Root cause: confirmed, and it is NOT a bug in the `is_plain_lhm` guard itself

`native-collections/src/lib.rs`, end of `native_lhm_put`, already has the
guard meant to cover exactly this case (see [[reference_lhm_removeeldestentry_eviction]]):

```rust
let this_cid = ctx.class_id_of_object(this);
let is_plain_lhm = ctx.class_name_of_id(this_cid).as_deref() == Some("java/util/LinkedHashMap");
if !is_plain_lhm {
    // ... invoke_virtual("removeEldestEntry", ...) ...
}
```

Instrumented this guard with a debug print (`CRATONVM_DBG_LHM_EVICT=1`, now
committed as a permanent opt-in diagnostic — zero cost when unset) and reran
the real `InPredicateTest`. Every `native_lhm_put` call on the actual
`DomainParameterXref` map correctly reports `ClassId(121)` /
`"java/util/LinkedHashMap"` / `is_plain_lhm=true` — **except the one call
immediately preceding the crash**, which reports:

```
[dbg-lhm-evict] this_cid=ClassId(0) class_name=Some("java/lang/Object") is_plain_lhm=false
```

`this` is the exact same object reference the surrounding calls resolved
correctly to `ClassId(121)` moments earlier — so the guard's logic is
correct; what it read was wrong. `ClassId(0)` reads back as `java/lang/Object`
(the class store's slot-0 convention), so `is_plain_lhm` is (correctly, given
what it saw) `false`, and the subsequent `invoke_virtual("removeEldestEntry", ...)`
dispatch searches `Object`'s methods and throws — the guard did exactly what
it should with the (bad) input it was given.

**This is not a new, independent bug.** A momentarily-`ClassId(0)`/all-zero
header read on an otherwise-live, correctly-typed object under JIT — while
the interpreter path never sees it — is precisely the symptom this
codebase's `dohead-jit-heap-corruption-register-invisibility.md` documents as
**"Layer 1 (register-invisible roots) — the SURVIVABLE all-zero-header
stale-receiver flood"**: a JIT-compiled hot path holds a live object
reference somewhere the VM's oop-tracking doesn't see (a register or a
non-precisely-mapped stack slot), so at some point a stale/zeroed view of
that memory gets read back where a real header is expected. That doc's own
status explicitly lists this exact class of symptom as an **accepted,
unresolved residual**: "Layer 1 ... is UNCHANGED — the real fix remains
precise oop maps / shadow stack," after this codebase's two attempted
mitigations (full-GPR safepoint spill, shadow stack) were "empirically
insufficient." A separate, LATER, fatal layer of the same investigation (the
young from-space walk-desync family) WAS fixed (`fix/dohead-sweep-freelist`,
commit `928cc5b3`) — but that fix explicitly does not touch Layer 1.

`DomainParameterXref`'s constructor loop (up to ~100,000 iterations for this
specific test's huge `IN` list, each iterating `fromSqm(parameter)` plus two
map inserts) is exactly the kind of hot, allocation-heavy loop likely to get
JIT-compiled and to keep `this` (the `sqmParamsByQueryParam` receiver) live
across many calls/safepoints in a register or stack slot the current
oop-map coverage doesn't reach precisely — the same shape of hazard as every
other "Layer 1" trigger already catalogued for this codebase (DoHead, and
several others this session independently reproduced in unrelated contexts).

## Why this isn't fixed here

Per the linked doc, the actual fix is a large, cross-cutting effort (fully
precise oop maps or a real shadow stack covering every live JIT register and
stack slot at every safepoint) that spans the whole JIT/GC subsystem and has
already resisted two dedicated mitigation attempts. A narrow patch scoped to
`native_lhm_put`'s guard (e.g. treating `ClassId(0)` specially, or retrying
the read) would only mask this one call site's symptom without addressing
the underlying stale-receiver hazard, which can surface anywhere a JIT-hot
loop holds a reference across safepoints — masking it here risks hiding a
real, load-bearing signal rather than fixing anything.

## Recommendation

Track this as another confirmed trigger site of Layer 1 in
`dohead-jit-heap-corruption-register-invisibility.md` rather than as an
independent native-collections bug. If/when Layer 1 gets a real fix (precise
oop maps or shadow stack), re-verify `InPredicateTest` — this doc's symptom
should disappear as a side effect, not require its own patch.

## Diagnostic tooling added

`native_lhm_put`'s `is_plain_lhm` guard now has an opt-in debug print,
`CRATONVM_DBG_LHM_EVICT=1`, logging `this`'s resolved `ClassId`/class-name and
the guard's verdict on every call. Zero cost when unset; useful for
confirming/ruling out this same hazard at other `LinkedHashMap`/`HashMap`
native call sites in the future.
