# HIB-CV-38 — `Boolean.TYPE`/`TRUE`/`FALSE` static-slot corruption — **FIXED**

**Status:** FIXED, branch `fix/hib-cv-38-dynamicbatchfetch-sigsegv`.
**Originally filed as:** `DynamicBatchFetchTest` SIGSEGV (rc=139), suspected
regression/reopening of HIB-CV-37's native-callback GC-root family.
**Actual root cause:** unrelated to HIB-CV-37 — a `java/lang/Boolean` static
field layout bug in `init_wrapper_type` (native-builtins), not a GC/JIT race.

## Symptom, as originally observed

```
process-died rc=139 (SIGSEGV)
```

on an isolated single-class rerun of `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`
via `apps/hib-suite-runner`, dev `d9cb7be8`.

## What the crash actually was

Re-running the identical repro on dev `62f1f39a` (real-JDK, JIT on) did not
reproduce a SIGSEGV — it reproduced a **100% deterministic** `NullPointerException`
at JUnit's own launcher bootstrap, before `DynamicBatchFetchTest` (or any test
class) ever runs:

```
java.lang.NullPointerException: Cannot invoke "java.lang.Boolean.booleanValue()"
  because the return value of "java.util.Optional.orElse(Object)" is null
	at org.junit.platform.launcher.core.LauncherFactory.collectLauncherInterceptors(LauncherFactory.java:153)
	at org.junit.platform.launcher.core.LauncherFactory.lambda$create$2(LauncherFactory.java:134)
	at org.junit.platform.launcher.core.SessionPerRequestLauncher.createSession(SessionPerRequestLauncher.java:86)
	at org.junit.platform.launcher.core.SessionPerRequestLauncher.execute(SessionPerRequestLauncher.java:66)
	at CratonRunner.main(CratonRunner.java:54)
```

Reproduced identically 4/4 with `--nojit` too — ruling out the JIT/GC-race
hypotheses in the original doc (options 1 and 3). A minimal standalone repro
(no Hibernate, no JUnit) isolated it further:

```java
Boolean.valueOf(false)   // returned null
Boolean.FALSE            // returned null
Boolean.valueOf(true)    // returned a java.lang.Class instance (!), not a Boolean
```

## Root cause

`java/lang/Boolean`'s `<clinit>` is natively overridden (`register_essential_natives`
→ `clinit_boolean` in `native-builtins/src/phases_early.rs`) — this REPLACES
the real bytecode `<clinit>` entirely (native registrations take priority over
bytecode), so the real `TRUE = new Boolean(true); FALSE = new Boolean(false);`
never runs.

`clinit_boolean` hardcoded `ctx.set_static_field(c, 0, ...)` to populate the
primitive-type mirror (`Boolean.TYPE`), assuming static field index 0 is
always `TYPE`. That holds for the other 7 primitive wrappers' sibling
`clinit_*` functions in the same file (Integer, Long, Float, Double,
Character, Byte, Short — their other `static final` fields like
`MIN_VALUE`/`MAX_VALUE` are primitives, not object references, so `TYPE` is
their only/first object-reference static) and for `Void` (whose only field is
`TYPE`). **`Boolean` is the one exception**: real JDK `Boolean.class` declares
object-reference statics in the order `TRUE, FALSE, TYPE` (confirmed via
`javap` against JDK 25's `java.lang.Boolean`) — so `TYPE` is index 2, not 0.

(This bug and fix originally lived in a shared `init_wrapper_type` helper in
`native-builtins/src/lib.rs` at the time of investigation; a concurrent dev
refactor inlined each wrapper type's `<clinit>` into its own function in
`phases_early.rs` before this branch merged, carrying the identical bug over
unchanged. The fix below targets the current post-refactor location.)

Effect: `clinit_boolean` wrote the `Class` mirror for `boolean` into
`Boolean`'s slot **0**, silently corrupting `Boolean.TRUE` into a `Class`
object. `Boolean.FALSE` (slot 1) and the real `Boolean.TYPE` (slot 2) were
never populated at all, staying at their default null.

Because `Boolean.valueOf(boolean)` has no native override in the default
real-JDK build (`register_wrapper_natives`, which does register one, is
`#[cfg(feature = "synthetic-jdk")]`-gated and not compiled into the default
`cratonvm-cli` build — see [[reference_essential_vs_synthetic_jdk_registration_split]]),
it runs as real bytecode: `return b ? TRUE : FALSE;` — reading the corrupted
`TRUE`/null `FALSE` statics directly. `Boolean.FALSE`/`Boolean.valueOf(false)`
being unconditionally null is about as foundational a corruption as CratonVM
can have — JDK/JUnit/Hibernate code calls it constantly, so it very plausibly
also explains sporadic/rotating crash signatures elsewhere (SIGSEGV in the
original report vs. this NPE here) depending on what happens to dereference
the bad `TRUE`-as-`Class` object or unbox the null `FALSE` at a given call
site and heap layout.

## Fix

`native-builtins/src/phases_early.rs`, `clinit_boolean`: resolve the `TYPE`
static slot **by field name** (`ctx.static_field_index_by_name(c, "TYPE")`,
falling back to 0 only if the lookup fails) instead of hardcoding index 0.
Additionally, since `Boolean`'s native `<clinit>` override bypasses the real
bytecode that would otherwise populate `TRUE`/`FALSE`, explicitly allocate and
set them (by name-resolved slot) to real `Boolean` instances wrapping `true`/
`false`.

Verified with a standalone repro (no longer null/wrong-typed):

```
Boolean.valueOf(false) = false
Boolean.FALSE = false
Boolean.valueOf(true) = true
empty.orElse(Boolean.valueOf(false)) = false   (was null before the fix)
```

And end-to-end: `DynamicBatchFetchTest.testDynamicBatchFetch` now passes
(`ok=1`) under `--nojit`; the JUnit-launcher NPE that made every isolated
single-class rerun fail before even reaching test code is gone.

## Residual — NOT covered by this fix

With JIT **on**, `DynamicBatchFetchTest.testMultiLoad` (2000-row batch
insert + multi-load) now runs far enough to hit a **different**, pre-existing
JIT/GC bug: repeated `GC: inconsistent header — kind=Object but
array_length=N; inline-alloc forgot to set kind=Array` warnings during heavy
allocation, eventually `OutOfMemoryError` after ~5-9 minutes (heap arena
regions abandoned by the walker's corruption re-sync, per
`gc/src/gen_heap.rs` around `gen_object_total_size`). This is the same
warning text as the bintrees18 inline-TLAB-allocation header race documented
in `docs/internal/app-jvm-bugs/jit-bintrees18-inline-alloc-and-bc-ec-round2.md`
(BUG 1, marked FIXED there for the plain-`new` fast path,
`emit_inline_tlab_new` in `jit/src/x64.rs`) — but that fix is confirmed
present on this dev tree and bt18 itself is correct/checksummed, so this is a
**different, not-yet-covered trigger** of the same corruption family, surfaced
by Hibernate's heavier/mixed allocation mix rather than bintrees' uniform
2-field `Node` allocation. Tracked as a fresh, narrow issue that has since
been fixed and archived:
`docs/internal/fixed-suite-bugs/jit-inline-alloc-array-header-corruption-hibernate-batch.md`.
