# `SerializationHelperTest.testSerializeDeserialize` — `Class.forName(name, false, null)` doesn't restrict to bootstrap-only classes

**Status:** FIXED 2026-07-21; validated against the isolated Hibernate harness in both JIT and --nojit modes.

**Test:** `org.hibernate.orm.test.util.SerializationHelperTest::testSerializeDeserialize`
(1 of the class's 2 tests; `testSerDeserClassUnknownToCustomLoader` passes).

**Found:** 2026-07-21, `apps/hib-suite-runner` "others" category rerun (50
known-historically-failing classes) against a binary built from
`C:\craton\CratonVM-hib-local-0712` (branch `test/hib-local-0712`, merged with
`origin/dev` HEAD `7aed580f0`), using a freshly-regenerated `common.args`
classpath. Not a classpath artifact of that regeneration — reproduces in
isolation (see below).

## Resolution and validation

Fixed in native-builtins/src/lang_class.rs::native_class_for_name. An explicit null loader in the three/four-argument overload now means bootstrap-only resolution: non-bootstrap names fail with ClassNotFoundException before CratonVM's flat class store can see them. Bootstrap resolution also now honors initialize=false without routing through ensure_class_initialized.

Validation on Azure used /data/cratonvm-hib-serhelper-null-loader-0721.bin and the clean /data/data/apps/hibernate-orm-harness/hib-suite-runner fixture. SerializationHelperTest passed 2/2 in normal JIT mode and 2/2 with --nojit; the focused native unit tests cover both rejecting an application class and accepting a bootstrap class for an explicit null loader.

## Symptom

```
org.opentest4j.AssertionFailedError: expected: <org.hibernate.orm.test.util.SerializableThing@16a2d> but was: <org.hibernate.orm.test.util.SerializableThing@16b25>
```

This looks superficially like an identity-hash-code / `toString()` mismatch
(cf. the FIXED `identity_hash_code()` GC-move-stability bug), **but it is
not**. JUnit 5's `AssertionFailureBuilder.formatClassAndValue` special-cases
`Class` objects: when `expected`/`actual` are both `java.lang.Class`
instances whose `toString()` is identical (same class *name*), it renders
each side as `<className>@<identityHashCode-of-the-Class-object-itself>`
instead of the normal `<value>` — i.e. the assertion is comparing **two
distinct `Class` objects for the same class name**, not two instances of
`SerializableThing`. Confirmed by disassembling
`junit-jupiter-api-6.0.3.jar`'s `AssertionUtils`/`AssertionFailureBuilder`
(`formatValues`/`formatClassAndValue`, ~line 44 in the decompiled
`AssertionFailureBuilder.formatValues`) and by disassembling the compiled
`SerializationHelperTest.class` itself, which matches the checked-in source
exactly (no stale-build confound) — the failing bytecode is literally:

```
aload_2  // instance.getClass()
aload    4  // instance2.getClass()
invokestatic Assertions.assertEquals:(Ljava/lang/Object;Ljava/lang/Object;)V
```

## Root cause

`SerializationHelper.deserialize(bytes, loader)`'s `CustomObjectInputStream.resolveClass`
(`hibernate-core/src/main/java/org/hibernate/internal/util/SerializationHelper.java:277-310`)
tries three loaders in order and only falls back to plain
`ObjectInputStream.resolveClass` if all three throw `ClassNotFoundException`:

```java
try { return Class.forName(className, false, loader1); }   // loader1 = null here
catch (ClassNotFoundException e) { ... }
if (!Objects.equals(loader1, loader2)) {
    try { return Class.forName(className, false, loader2); }  // loader2 = TCCL (custom)
    ...
}
```

The test calls `SerializationHelper.deserialize(bytes, Serializable.class.getClassLoader())`
— `Serializable.class.getClassLoader()` is `null` (bootstrap loader, a core
JDK class) — so `loader1 = null`. Per the JDK `Class.forName(String, boolean,
ClassLoader)` spec, an explicit `null` loader means "resolve strictly via the
**bootstrap** class loader" — it must **not** see application/test-classpath
classes, so `Class.forName("org.hibernate.orm.test.util.SerializableThing",
false, null)` should throw `ClassNotFoundException` on real HotSpot, letting
the code fall through to `loader2` (the custom `TCCL`, which already has
`SerializableThing` loaded/cached from the test's earlier
`custom.loadClass(...)` call via `findLoadedClass`) — producing the **same**
`Class` object both times, so the assertion passes.

On CratonVM, `Class.forName(name, false, null)` incorrectly **succeeds**,
resolving `SerializableThing` via CratonVM's flat/global classpath scanner
instead of throwing CNFE — so the code never reaches `loader2`, and the
`Class` object returned for `instance2` is a **different** object than the
one `instance` was created from (loaded by `custom`).

**Exact defect location:** `native-builtins/src/lang_class.rs`,
`native_class_for_name` (function starts ~line 1700). The `effective_loader`
computation:

```rust
// lines 1764-1775
let effective_loader = match args.get(2) {
    Some(Value::Object(Some(loader))) => { ... Some((*loader, initialize)) }
    None if args.len() == 1 => class_for_name_one_arg_caller_loader(ctx).map(...),
    _ => None,
};
```

collapses two semantically different cases into the same `_ => None` arm:
1. the 3-arg `Class.forName(name, init, loader)` overload called with an
   **explicit `null` loader** (`Some(Value::Object(None))`) — meaning
   "bootstrap only", and
2. no loader information at all (which only legitimately arises for the
   1-arg overload, already handled by the branch above it).

The comment block at lines 1798-1801 even documents the correct JDK 25
`forName0` argument layout (`args[2] = loader (ClassLoader, may be null =
bootstrap)`) but the match arm above it does not act on that distinction.

When `effective_loader` is `None`, the entire loader-routing block (`if let
Some((loader, initialize)) = effective_loader { ... }`, lines 1810-2128,
which is what would call `loader.loadClass(name)` / consult loader-scoped
visibility) is skipped, and execution falls straight through to:

```rust
// line 2130
match ctx.ensure_class_initialized(&internal_name) {
```

`ensure_class_initialized` → `self.shared.load_class_concurrent(name)`
(`vm/src/vm/vm_exec.rs:5359-5363`) is CratonVM's flat, loader-unaware global
class store — it has no bootstrap/application separation
(`native-builtins/src/classloader.rs:864-866` documents this explicitly:
"CratonVM has no separate bootstrap classpath (its class store is flat)").
This global lookup happily resolves `org.hibernate.orm.test.util.SerializableThing`
because it's on the classpath, even though a `null`-loader `Class.forName`
should be restricted to `java/`, `javax/`, `jdk/`, `sun/`, etc.

CratonVM *does* have exactly the right predicate for this —
`is_bootstrap_class_name` (`native-builtins/src/classloader.rs:867-880`) —
but it's used only in `cl_load_class_base_delegation_inner` (for
`ClassLoader.loadClass` parent-delegation decisions), never consulted by
`native_class_for_name`'s null-loader path.

There is no dual-registration split for this native between `classloader.rs`
and `classloader_real.rs` (a pattern that has bitten other classloading bugs
in this codebase) — `classloader_real.rs` has no `forName`-related code at
all; both real-JDK and synthetic modes route through the same
`lang_class.rs::native_class_for_name` (registered from `lib.rs:30303,
30316, 41082`).

## Relationship to prior fixes (not a duplicate)

Several already-landed fixes address adjacent-but-distinct null-loader bugs
in this same area — this is a genuinely separate residual, not a regression
of any of them:

- `20519be46` ("Fix Hibernate CacheKeyEmbeddedIdEnanchedTest") fixed
  `jdk/internal/misc/VM.latestUserDefinedLoader0()` always returning null,
  which broke the **fallback-to-`super.resolveClass`** path (only reached
  after all three explicit loaders fail) — a different code path than the
  one at fault here (loader1 succeeds when it should fail, so
  `super.resolveClass` is never reached).
- `56383c47a` fixed `ClassUtils.forName` (a *Spring* utility method with its
  own native shim) ignoring an explicit null loader — a different native
  entirely from `java.lang.Class.forName` itself.
- `d718d7b1b` fixed a **non-null**-loader case (`Class.forName(name, init,
  loader)` not re-checking the loader's own already-defined-class namespace
  before invoking its `loadClass` bytecode) — again a different branch of
  the same function, not the null-loader fallthrough.

None of these touch the `_ => None` fallthrough in `native_class_for_name`'s
`effective_loader` match that this bug lives in.

## Reproduction

Deterministic across 3 isolated runs (identical `Class` identity-hashes each
time, ruling out timing/flakiness):

```
cd C:/craton/CratonVM/apps/hib-suite-runner
echo "org.hibernate.orm.test.util.SerializationHelperTest" > /tmp/single-serhelper.txt
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner /tmp/single-serhelper.txt 0
```

Result each of 3 runs: `found=2 started=2 ok=1 failed=1`, same
`AssertionFailedError` (identical Class-object identity hashes across runs:
`@169e6` vs `@16ade`).

## Suggested fix direction (not applied — investigation only)

In `native_class_for_name`'s `effective_loader` match, distinguish
`Some(Value::Object(None))` (explicit null loader — 3-arg overload) from the
`_ => None` catch-all, and route it through a bootstrap-only resolution path
that consults `is_bootstrap_class_name` (already implemented in
`classloader.rs`) before ever reaching the flat `ensure_class_initialized`
global scanner — mirroring how `cl_load_class_base_delegation_inner` already
does this for `ClassLoader.loadClass`. This is a narrow, well-scoped change,
but touches a very hot/heavily-patched function (`native_class_for_name` has
accumulated many special cases for other frameworks — JBoss Modules, Spring
Boot's `LaunchedURLClassLoader`, Groovy CGLIB, WF7 entry-class synthesis,
i18n logger short-circuits) so any fix should be validated against that full
set of existing special-cased scenarios, not just this test.

## Prior history (different bug, already fixed, do not confuse)

This exact test/class was previously broken by an **unrelated** bug (a
custom-loader `getResourceAsStream` parent-delegation gap in
`native-builtins/src/classloader.rs::cl_get_resource`, causing a
`ClassNotFoundException` for `SerializableThing` before the test ever
reached its assertions) — fixed 2026-06-22, see
`docs/internal/hibernate-bugs/run-20260622/HIB-CV-35-cvonly-correctness-longtail.md`
section 3, which recorded `SerializationHelperTest 2/2 PASS` at the time.
That fix is still present and correct; this is a **different**, previously
undiscovered bug in a code path that fix's repro never reached (the earlier
bug threw CNFE before `Class.forName(name, false, null)` was ever called
with a *resolvable* class name).
