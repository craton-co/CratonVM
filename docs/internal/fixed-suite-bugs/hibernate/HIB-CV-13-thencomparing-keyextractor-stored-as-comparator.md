# HIB-CV-13 — `Comparator.thenComparing(Function)` stores the key-extractor as a raw nested comparator → `Function.compare` NSME → JUnit "InvocationInterceptors called invocation multiple times"

**Severity:** High — fails `@SecondaryTable` (join) and inheritance tests. 4 classes: `JoinTest`, `ManyToOneJoinTest`, `JoinedSubclassTest`, `SubclassTest`.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-collections/src/lib.rs` `comparator_compare`).
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

Every test method fails with:

```
org.junit.platform.commons.JUnitException: Chain of InvocationInterceptors called invocation
  multiple times instead of just once: org.junit.jupiter.engine.extension.TimeoutExtension
```

(This is the same surface error as the old HIB-CV-04, but a **different** root cause.)

## Root cause

The JUnit "multiple times" error is a **cascade**. The real fault is a recurring
`NoSuchMethodError: java/util/function/Function.compare(Ljava/lang/Object;Ljava/lang/Object;)I`,
thrown once per test method from
`org.hibernate.action.queue.internal.constraint.ConstraintModelBuilder.collectEntityTableGroupConstraints`,
which does:

```java
Stream.of(descriptor.getTableMappings()).sorted(ENTITY_TABLE_MAPPING_COMPARATOR).toList();
// ENTITY_TABLE_MAPPING_COMPARATOR = primarySort.thenComparing(EntityTableMapping::relativePosition)
```

`thenComparing(Function keyExtractor)` must wrap the key extractor in a `Comparator.comparing(keyExtractor)`
(apply the function, compare the keys). CratonVM instead **stored the raw `Function`** as a nested
comparator (`thenComparing(Function)`'s dispatch landed on the `(Comparator)` overload —
`native_comparator_then_comparing`, `inner_tag=None` — so the key-extractor was placed directly as
the secondary comparator). When `comparator_compare` reached that secondary it took the lambda path
and called `compare(a,b)` on the `Function` → `NoSuchMethodError: Function.compare`.

That `NoSuchMethodError` then unwinds through JUnit's MethodHandle/lambda interceptor chain in a way
that re-executes the base invocation's `proceed()`, so the chain validator reports "called invocation
multiple times" and the whole test method fails.

Minimal reproduction (`FaithfulCmpProbe`): a static-final comparator
`primary.thenComparing(TM::relativePosition)` used via `Stream.of(arr).sorted(cmp).toList()` →
CratonVM `linkage error: no such method: Function.compare`; the *single-expression* primarySort case
never reaches the secondary so it didn't crash, which is why an earlier probe missed it.

## Fix

In `comparator_compare`'s lambda path (non-factory comparator), if `invoke_virtual("compare")` does
not resolve, fall back to treating the object as a key extractor — `invoke_virtual("apply")` on both
operands and `natural_compare` the keys, i.e. `Comparator.comparing(keyFn)` semantics. A genuine
`Comparator` lambda implements `compare` (not `apply`), so an exception thrown by a real `compare`
still propagates (the `apply` retry fails and the original error is re-raised). This makes a
key-extractor `Function` that was stored raw behave correctly regardless of which `thenComparing`
overload the dispatch selected.

## Verification

`FaithfulCmpProbe` sorts correctly (`[TM(true,1), TM(false,2), TM(false,3)]`); `JoinTest` 9/9,
`ManyToOneJoinTest` 3/3, `JoinedSubclassTest` 6/6, `SubclassTest` 1/1 — all were FAIL.

## Note

The deeper root cause is that CratonVM dispatched `thenComparing(Ljava/util/function/Function;)` to the
`(Ljava/util/Comparator;)` native overload (descriptor-blind lambda→native routing). The fix here is a
robust value-level guard; a follow-up could make the lambda→default-method native dispatch
descriptor-aware so the correct `thenComparing` overload is selected up front.
