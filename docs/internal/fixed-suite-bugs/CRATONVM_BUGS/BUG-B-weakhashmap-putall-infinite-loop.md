# Bug B — JIT miscompiles `WeakHashMap` iteration → `TestExpressionFactoryCache` hangs  ✅ FIXED

**Severity:** Medium (hang). **Status on CratonVM:** was hang, now PASS.
**HotSpot:** passes in <1s.
**Run date:** 2026-06-11 · **Fix commit:** `efa35d3` (branch `worktree-tomcat-fixes`)

## Symptom

`jakarta.el.TestExpressionFactoryCache` printed the JUnit banner then hung
forever (default 120 s watchdog; `rc=124`), producing no test output.

```
JUnit version 4.13.2
<hangs>
```

## Root cause — a JIT miscompile (not a WeakHashMap defect)

The decisive clue: **with `CRATONVM_DISABLE_JIT=1` the test passes** —
`OK (2 tests)`. So the interpreter is correct and the JIT mis-compiles a hot
method into an infinite loop.

`jakarta.el.ExpressionFactoryCache.getOrCreateExpressionFactory(ClassLoader)`
keeps the cache in an `AtomicReference<WeakHashMap<ClassLoader,
WeakReference<ExpressionFactory>>>` and updates it copy-on-write inside a CAS
retry loop:

```java
do {
    cache = factoryCache.get();
    ...
    newCache = new WeakHashMap<>(cache);   // copy ctor -> putAll -> entrySet().iterator()
    newCache.put(cl, factoryRef);
} while (!factoryCache.compareAndSet(cache, newCache));
```

The hot path is the `WeakHashMap` copy: `new WeakHashMap<>(cache)` →
`putAll(cache)` → iterate `cache.entrySet()`. The dispatch trace at hang time
cycles `WeakHashMap$HashIterator.hasNext → EntryIterator.next → put → getTable →
expungeStaleEntries → Entry.<init> → hasNext …` forever — i.e. the JIT-compiled
`HashIterator.hasNext()` never reaches the end of the table.

This is the **same miscompile signature already documented and banned for the
`HashMap$HashIterator` family** in `vm/src/jit/skip_list.rs` — allocate-then-
`putfield`-heavy iterator/entry methods (`HashIterator.<init>` stores
next/current/expectedModCount; `Entry.<init>` is `new` + `putfield`). Each was
individually verified to segfault/loop under JIT and pass under the interpreter.
`WeakHashMap` simply wasn't in that list.

Ruled out (all pass in isolation on CratonVM): a plain `WeakHashMap.putAll` of
30 ClassLoader keys with a resize; `AtomicReference.compareAndSet`;
`ExpressionFactory.newInstance()`; and the exact `getOrCreateExpressionFactory`
body driven directly. Only running it **under JUnit with the JIT warm** triggers
compilation of the iterator methods and the hang.

## Fix

Mirror the existing `HashMap$HashIterator` skip-list entries for `WeakHashMap`
(`vm/src/jit/skip_list.rs`):

```rust
| ("java/util/WeakHashMap$HashIterator", "<init>")
| ("java/util/WeakHashMap$HashIterator", "hasNext")
| ("java/util/WeakHashMap$EntryIterator", "next")
| ("java/util/WeakHashMap$KeyIterator", "next")
| ("java/util/WeakHashMap$ValueIterator", "next")
| ("java/util/WeakHashMap$Entry", "<init>")
| ("java/util/WeakHashMap", "getTable")
| ("java/util/WeakHashMap", "expungeStaleEntries")
```

These run interpreted (correct) while the rest of the method JIT-compiles.

## Verification

`TestExpressionFactoryCache` now passes **with JIT ON**: `OK (2 tests)`, rc=0.
Embedded Tomcat still serves HTTP 200 and other EL tests are unaffected (no
regression).

## Follow-up (not required for this fix)

The underlying JIT codegen bug for allocate-then-putfield-heavy iterator methods
is real (this is the 4th+ collection-iterator class to need the same ban). A
proper codegen fix would let these methods JIT and remove the skip-list entries;
tracked separately.
