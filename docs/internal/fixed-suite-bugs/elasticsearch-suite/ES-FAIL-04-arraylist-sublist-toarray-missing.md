# ES-FAIL-04 — synthetic `cratonvm/internal/ArrayListSubList` missing `toArray(T[])` (DOMINANT server-test blocker)

**Status:** ✅ FIXED — branch `fix/es-fail-04-arraylist-sublist-toarray` (commit `ec979daf`, worktree `C:\craton\CratonVM-jitfix`). Not yet merged (see note).
**Severity:** HIGH — this is the **actual dominant blocker** for `:server` unit tests under CratonVM (every `ESTestCase` reaches a `subList(...).toArray(T[])`), *not* the native-access issue first hypothesised in the now-retracted ES-FAIL-03.
**VM:** `cratonvm.exe` from `dev`. **Baseline:** HotSpot JDK 25.0.1.
**Date:** 2026-06-18

## Symptom

```
NoSuchMethodError: cratonvm/internal/ArrayListSubList.toArray([Ljava/lang/Object;)[Ljava/lang/Object;
```

CratonVM's synthetic `ArrayList.subList()` view class (`cratonvm/internal/ArrayListSubList`) registered only the 0-arg `toArray()` native — not the generic `toArray(T[])` overload. Any `list.subList(a,b).toArray(new T[n])` throws `NoSuchMethodError`. ES test bootstrap hits this on essentially every `ESTestCase`.

## Scope (confirmed)
Every diverse server test sampled in isolation terminally fails with this exact `NoSuchMethodError` (`BuildTests`, `ByteSizeValueTests`, `ClusterNameExpressionResolverTests`, `KeywordFieldTypeTests`, `GeoUtilTests`, `TransportInfoTests`, the `BucketedSortFor*Tests`, …). In the parallel suite run the same classes also appeared as `rc=127` (a load artifact) — same root cause.

## Fix
Register the typed overload on the synthetic SubList as a snapshot delegation (the existing pattern for `contains`/`indexOf`/`stream`/`forEach`), forwarding to a fresh `ArrayList` built from the live parent slice → `native_al_to_array_typed`:

```rust
// native-collections/src/lib.rs, ASL_CLASS registration
r.register(c, "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;", |ctx, args| {
    asl_delegate_snapshot(ctx, args, "toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;")
});
```

**Verified:** `subList(1,4).toArray(new String[0])` and the oversized `new String[5]` form are byte-identical to HotSpot (length, contents, null terminator). With the fix, the `NoSuchMethodError` is gone and the affected server tests **execute** instead of dying immediately.

## Residual (separate, not this bug)

After this fix, server `ESTestCase` suites run but some still end with a **suite-level failure** (RandomizedRunner `1) <ClassName>`, no method) *after* executing. That is a *separate* downstream issue (not the native-access EIIE — which is correctly caught/logged "Unable to load native provider" → Noop fallback, identical to HotSpot; and not `ArrayListSubList`). Likely a thread-leak / `@AfterClass` check; needs its own investigation. ES-FAIL-04 is nonetheless the gating fix that lets execution proceed.

## Merge note
The fix touches `native-collections/src/lib.rs`, which currently has **uncommitted changes from another agent in the main worktree** — so merging into the main worktree's `dev` is blocked until that work is committed. The fix is preserved on its branch and is a single, cherry-pickable commit.
