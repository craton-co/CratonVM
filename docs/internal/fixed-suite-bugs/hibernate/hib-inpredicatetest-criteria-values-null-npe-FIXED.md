# Hibernate `InPredicateTest` — Criteria `In` predicate's value list is `null` at execution

| | |
|---|---|
| **Status** | ✅ FIXED 2026-07-06 (branch `fix/hib-inpredicate-criteria-values-null-20260706`, commit `084c8ffb`). Test now runs past this NPE; a separate, unrelated bug (`DomainParameterXref` / `LinkedHashMap.removeEldestEntry` `NoSuchMethodError`) blocks a full pass — tracked in [`hib-domainparameterxref-lhm-removeeldestentry-nsme.md`](../known-issues/hib-domainparameterxref-lhm-removeeldestentry-nsme.md). |
| **Area** | VM — JIT on-stack-replacement (OSR) uncommon-trap handling |
| **Symptom** | `java.lang.NullPointerException: Cannot invoke "java.util.Collection.size()" because "values" is null` |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage. Root-caused and fixed 2026-07-06 (Azure host, dev `9f1db39d`→`657ee914` base). |

## Symptom (original)

`org.hibernate.orm.test.jpa.criteria.InPredicateTest` builds a
`CriteriaBuilder.in(...)` predicate over a 100,000-element `List<String>` and
executes the resulting query. HotSpot passes; CratonVM threw
`NullPointerException: values is null` inside
`SqmCriteriaNodeBuilder.in(Expression, Collection<T> values)` — the very
first statement (`values.size()`), even though the caller's list was
genuinely non-null and fully populated.

## Root cause

Not a Hibernate bug, and not specific to Hibernate at all — reproduced with a
tiny standalone Java program (no Hibernate/JPA involved):

```java
static List<String> getNames() {
    List<String> names = new ArrayList<>(100000);
    for (int i = 0; i < 100000; i++) names.add("abc" + i);   // live invokedynamic (string concat)
    return names;
}
```

`getNames()`'s loop is hot enough to trigger on-stack-replacement (OSR)
mid-execution. The loop body contains a **live** `invokedynamic`
(`"abc"+i` lowers to `StringConcatFactory.makeConcatWithConstants`).
CratonVM's JIT does not implement `invokedynamic`; by design (see the `0xba`
arm in `../../../../jit/src/x64.rs`), any method containing one compiles fine but
unconditionally deopts via an "uncommon trap" (`jit_uncommon_trap`,
`DeoptReason::UnreachedCode`) the instant that instruction is actually
reached at runtime — intended to permanently punt such methods back to the
interpreter (harmless in the common case where the indy is dead code behind
a disabled `assert`, but this loop's indy is very much live).

The bug: `jit_uncommon_trap` signals the deopt via a generic
`set_jit_deopt_pending()` flag only — unlike the guard-based deopt
trampolines (`x64_deopt_entry` / `ir_deopt_entry`), it does **not** stash a
reconstructed frame into `cratonvm_jit::deopt::LAST_DEOPT`. `try_osr()` in
`../../../../vm/src/runtime/interpreter.rs` checked `take_last_deopt()` to distinguish "a
real deopt" from "a genuine `Long.MIN_VALUE` method return" (both signal via
the same `i64::MIN` sentinel), but never checked the generic deopt-pending
flag. So an uncommon-trap deopt with no stashed frame fell through to the
normal return-value conversion, which for a reference-typed method
(`Ljava/util/List;`) reinterprets the raw `i64::MIN` bit pattern as a heap
pointer — garbage, observed downstream as a null/broken reference once
something (e.g. `values.size()`) actually dereferences it.

This is unrelated to the CompactValue NaN-box / HIB-CV-20 register-staleness
theories floated in this doc's original version and in the
`others.txt` README entry — no moving GC, no register-oop-mask gap, and no
scalar-replacement was involved. `--nojit` masked it (trivially — no OSR, no
trap) but so would forcing eager (non-OSR) compilation of the same method,
which was not tried before root-causing this via the standalone repro.

## Fix

`../../../../vm/src/runtime/interpreter.rs`, `try_osr()`: drain
`crate::jit::helpers::take_jit_deopt_pending()` immediately after the OSR
call returns (mirroring the ordering already used by the regular,
non-OSR invoke path), and when set with no stashed frame, safe-reject
(`return None`, resume interpreting the same frame) instead of falling
through to the return-value conversion.

## Note on a previously-staged alternative fix

The `others.txt` README entry (2026-07-06) recorded a *different*, broader
candidate: cherry-picking an unmerged "OSR allocation-region gate"
(`4c3cf821`, branch `claude/practical-golick-ff73f1`) that rejects back-edge
OSR entirely for any loop containing an allocating/calling region, staged on
`fix/hib-inpredicate-criteria-values-null-20260705` (never merged). That
approach would also mask this symptom (by preventing OSR from ever
triggering for such loops), but at the cost of forcing many otherwise-fine
hot loops into the interpreter. The fix above is narrower — it only affects
the invokedynamic-uncommon-trap fallthrough — and the standalone repro now
passes cleanly with JIT/OSR fully enabled, so the broader gate is not needed
for this bug. `fix/hib-inpredicate-criteria-values-null-20260705` should be
treated as superseded and can be deleted once confirmed unused elsewhere.

## Verification

- Standalone repro (`MiniRepro.java`, no Hibernate): `values.size() = 100000`
  with JIT+OSR on, matching HotSpot; previously threw the NPE.
- `InPredicateTest` under CratonVM with JIT+OSR on: the original
  `NullPointerException: values is null` no longer occurs. The test still
  fails, but now on a distinct, later, unrelated defect — see
  [`hib-domainparameterxref-lhm-removeeldestentry-nsme.md`](../known-issues/hib-domainparameterxref-lhm-removeeldestentry-nsme.md).
