# 2026-07-08 closure

Status: FIXED / RETIRED. The loader-faithful enhancement/lazy/lazytoone family tracked here is closed on dev by `codex/hib-enhancement-loader-retire-20260708-121047`. The fix plugs the remaining runtime layers that still collapsed loader-private enhanced classes back to same-named global classes: exact mirror `Class.newInstance`, receiver-exact method retry, CP-interface default-method rescue, receiver-exact instance field retargeting, exact enum/static initialization, exact reference-array component checks, and loader-aware type-test fallback for same-named copies.

Verification on Azure host `/data/data/cratonvm` worktree `/data/data/wt-hib-enhancement-loader-retire-20260708-121047`, binary `/data/data/bin/cratonvm-hib-enhancement-loader-retire-20260708-121047-fix8` unless otherwise noted:

- Baseline dev sample before this fix: `lazy_lazytoone_sample.txt` was `total=18 pass=10 fail=8`, with `asManagedEntity`/`asPersistentAttributeInterceptable` default-method `NoSuchMethodError`, `$HibernateInstantiator` no-arg-constructor failures, enum `$VALUES`/container failure in `BasicAttributesLazyGroupTest`, and `FetchGraphTest` lazy field-slot failure.
- Post-fix focused residuals: `BasicAttributesLazyGroupTest` is `found=5 started=5 ok=5 failed=0`; `FetchGraphTest` is `found=17 started=17 ok=17 failed=0`.
- Post-fix representative sample: `lazy_lazytoone_sample.txt` is `SUMMARY total=18 pass=18 fail=0 hang=0`.
- Post-fix broader lazy/lazytoone subset: `lazy_lazytoone_subset.txt` is `SUMMARY total=69 pass=69 fail=0 hang=0` with the final fix8 binary.
- Additional 131-subset same-name checkcast regression: `LoadAndFetchGraphAssociationNotExplicitlySpecifiedTest` is fixed by the final type-test fallback (`found=14 started=14 ok=14 failed=0`).

The remaining non-green `gated_subset.txt` cases were not this loader-linking/lazytoone family. They were tracked separately and later retired in [`hib-bytecode-enhancement-basic-merge-version-residuals-FIXED.md`](hib-bytecode-enhancement-basic-merge-version-residuals-FIXED.md).

---

# Hibernate `bytecode.enhancement.*` — loader-faithful supertype linking + dispatch (gate `CRATONVM_LOADER_AWARE_RESOLUTION`)

| | |
|---|---|
| **Status** | 🟡 **PARTIALLY FIXED 2026-07-06** — root cause of the 2026-07-04 REOPENED regression found and fixed: `CRATONVM_LOADER_AWARE_RESOLUTION` is implemented as **three independent copies** of the same gate function (`../../../../classloading/src/class_manager.rs`, `../../../../native-builtins/src/classloader.rs`, `../../../../vm/src/runtime/env_cache.rs`), each with a doc comment asserting it "stays in lock-step" with the others. Only the `vm` crate's copy was ever flipped from default-OFF to default-ON (the `context.groovy` fix, `docs/known-issues/hib-proxyclassreuse-loader-blind-class-resolution.md`); the other two silently stayed default-OFF, so most of the actual loader-faithful fixes for THIS bug — which live in `classloading` (superclass/interface linking, verifier hierarchy lookup) and `native-builtins` (`preload_supertypes_via_loader`, `inherit_lookup_loader`, annotation Class-value resolution, `descriptor_to_class_mirror_via_loader` for reflective Field/Method/Constructor types) — were disabled by default despite being genuinely landed and correct. This fully explains the 2026-07-04 "REOPENED" finding: nobody had actually exercised the gate as globally on. Fix (branch `fix/hib-enhancement-annotation-classvalue-loader-20260706`): flipped the two stale copies' `Err(_) => false` to `Err(_) => true`, restoring the lock-step invariant. Verified on a fresh worktree (Azure host, dev `9f1db39d`+): `gated_subset.txt` (131 classes) went from **PASS 40 / HANG 13 / FAIL 78** (baseline) to **PASS 86 / HANG 7 / ABORTED 2 / FAIL 36** (fixed), with the dominant `targetEntity=X, but the attribute is declared as X` class-identity-mismatch signature **eliminated from the FAIL set entirely**. Status is "partially fixed" (not fully closed) because ~36 FAILs + 7 HANGs remain — see "UPDATE 2026-07-06" below for the residual breakdown; these are distinct, separate bugs one layer downstream of the one just fixed, matching this doc's established pattern of layered fixes. |
| **Area** | VM core — real-JDK-mode loader-faithful resolution: superclass/interface *linking* (not just `new`/checkcast/ldc), `invokespecial` owner dispatch, and link-time verification of trusted runtime-generated classes. |
| **Builds on** | [hib-proxyclassreuse-loader-blind-class-resolution.md](hib-proxyclassreuse-loader-blind-class-resolution.md) — the three-layer `CONSTANT_Class` / `defineClass`-namespace / `findLoadedClass` fix and the `resolve_class_loader_aware` mechanism. That doc's gate is now **default-on** (flipped 2026-07-03) and validated via a Hibernate app-gauntlet soak; this doc describes the *linking/dispatch* layer built on top of it. Both docs describe the same loader-identity mechanism at different depths — read the other doc first for the gate's base three-layer fix, this one for the enhancement-specific linking/dispatch/SessionFactory-build work. |

> **RETRY 2026-07-01:** Source audit on current `dev` confirms the named loader-aware pieces
> are present (`allocate_loader_id` starts at 3, `lambda_impl_dispatch_override` is wired,
> lookup-defined classes register their defining loader, and builtin `findLoadedClass` hides
> user namespaces). The focused builtin classloader test passed via
> `cargo test -p cratonvm-native-builtins test_builtin_find_loaded_class -- --nocapture`.
> A broader `cratonvm-vm` loader test build failed before execution because the linker ran out
> of disk space, and the Hibernate app fixture is gitignored outside this worktree.

## Re-verification 2026-07-04 (this doc moved back to `../../../known-issues`)

This doc was filed under `docs/internal/hibernate-bugs/` with status "FIXED /
ARCHIVED", contradicted by a full Hibernate ORM 8.0 suite run earlier the same
session (dev `81a31c08+`, real-JDK JIT-on, gate on, TIMEOUT=600s, Azure Linux
host, 4548 classes): PASS 4293/4548, with `bytecode.enhancement.lazy.*`
(~54 classes) + `mapping.lazytoone.*` (~12 classes) as the dominant remaining
FAIL cluster even with the gate on. Re-verified from scratch in a fresh
worktree (`C:/craton/CratonVM-enhdoc`, branch
`docs/unarchive-enhancement-loader-linking`) off current `dev` (`c20f6f15`):

**Fix audit — 6 of 6 linking/dispatch fixes + the `allocate_loader_id`
KEYSTONE confirmed present in current source** (read the actual function
bodies, not just grepped for the gate name):

1. `define_class_with_options` (`classloading/src/class_manager.rs:2715`,
   gate logic 2958-2985) — present exactly as described.
2. `preload_supertypes_via_loader` (`native-builtins/src/lang_system.rs:2779`,
   gate logic 2816-2831) — present exactly as described.
3. `ClassStoreHierarchy::lookup` (`classloading/src/class_manager.rs:242`,
   `UserDefined` branch 309-345) — present exactly as described.
4. `execute_invoke_kind` `is_special` override
   (`vm/src/runtime/interpreter.rs:15533`, override at 16573-16593) — present
   exactly as described.
5. Link-time verification skip-flag fix — behavior present and correct, but
   **the doc's function name is stale**: `link_and_initialize` does not exist
   anywhere in the repo. The actual logic lives in `initialize_class_shared`
   (`vm/src/vm/vm_util.rs:693`, skip-flag check at 744-761) — same file, same
   substantive behavior, wrong name below (left uncorrected inline to avoid
   rewriting historical narrative; this is the accurate pointer).
6. `inherit_lookup_loader` (`native-builtins/src/lookup_define.rs:184`, gate
   logic 209-223) — present exactly as described.
7. `allocate_loader_id` KEYSTONE (`vm/src/vm/vm_exec.rs:6815`) — starts at 3,
   ungated, confirmed present as described.

**Test re-verification — built `cvenhdoc.exe` from this dev HEAD
(`C:/craton/CratonVM/apps/hib-suite-runner`, real JDK 25, `common.args`
classpath, `CratonRunner` harness, gate is default-on so no env var needed).**
An 18-class representative sample of `gated_subset.txt`'s
lazy/lazytoone classes (chosen to include the specific classes this doc names
as historically PASS or as known residuals):

| Result | Count | Classes |
|---|---|---|
| PASS | 2/18 | `LazyBasicFieldAccessTest` (2/2, matches doc's historical claim), `LazyInCacheTest` (1/1) |
| FAIL (harness-counted "PASS", not genuinely green) | 1/18 | `BasicAttributesLazyGroupTest` — `found=5 started=0 ok=0 failed=0`: no test method actually ran (a container-level failure the harness's crude `failed=0 && aborted=0` heuristic miscounts as PASS) |
| FAIL | 14/18 | `BidirectionalLazyTest`, `LazyLoadingTest`, `LazyCollectionLoadingTest`, `EagerAndLazyBasicUpdateTest` (20/40, down from the doc's claimed 40/40), `OnlyLazyBasicUpdateTest` (10/20, down from the doc's claimed 20/20), `LazyGroupTest`, `LazyGroupOneToOneMappedByTests`, `FinalAccessorProxyFactoryTests`, `LazyGroupWithInheritanceTest`, `MergeProxyTest`, `InstrumentedProxyLazyToOneTest`, `ManyToOneAllowProxyTests`, `OneToOneAllowProxyTests`, `InverseToOneAllowProxyTests` |
| HANG | 1/18 | `FetchGraphTest` (450s timeout) — this doc's own "5th surface" section describes it as FAIL-with-NPE (a lazy-collection runtime bug), **not a hang**; this may be a new regression since that was last checked, not yet root-caused |

The dominant FAIL signature is the identical class-identity mismatch this
doc's own "Historical blocker" / "Precise root cause" sections already
describe as unfixed: `Could not build SessionFactory: To-one mapping [...]
was mapped with targetEntity=`X`, but the attribute is declared as `X`` (same
name string, different `Class` identity) → `Could not instantiate persister`
/ `no no-arg constructor in ...$HibernateInstantiator`. This is the spurious
un-enhanced Application-loader copy leaking into the name-keyed
`JavaTypeRegistry`, described in this doc's "So the real fix is upstream of
dispatch/linking" section — still not fixed.

**Conclusion:** the linking/dispatch fixes are real and landed; do not revert
them. But this doc's prior "FIXED / ARCHIVED" status was wrong — the
`enhancement.lazy.*`/`mapping.lazytoone.*` cluster remains the single largest
open Hibernate-enhancement gap, confirmed both by the full-suite run earlier
this session and by this fresh 18-class targeted sample. Filing back under
`../../../known-issues` where it belongs per the triage rule (fixed code →
`../..`; anything with a live, reproducing FAIL cluster →
`../../../known-issues`). This is a documentation-only correction (plus the
`git mv`) — the underlying `enhancement.lazy.*` bug itself was not
investigated or touched here.

## UPDATE 2026-07-06 — root cause of the REOPENED regression found: gate lock-step drift

Investigated the `enhancement.lazy.*`/`mapping.lazytoone.*` FAIL cluster from
the 2026-07-04 re-verification above (Azure host, worktree
`/data/data/wt-hib-enh-classvalue-20260706`, branch
`fix/hib-enhancement-annotation-classvalue-loader-20260706`, off dev
`9f1db39d`). Live instrumentation (a temporary debug print in Hibernate's
`ToOneAttributeMapping` constructor, `hibernate-core/src/main/java/org/hibernate/metamodel/mapping/internal/ToOneAttributeMapping.java:268-283`)
on the reproducing `LazyGroupTest`/`FetchGraphTest` classes showed the
`targetEntity=X, but the attribute is declared as X` mismatch is between:

- `declaredType = propertyAccess.getGetter().getReturnTypeClass()` → literally
  `Field.getType()` (`GetterFieldImpl.getReturnTypeClass()`) — resolved to the
  **un-enhanced** copy, `loader=jdk.internal.loader.ClassLoaders$AppClassLoader`.
- `targetType = entityMappingType.getMappedJavaType().getJavaTypeClass()` →
  Hibernate's `JavaTypeRegistry`-backed `EntityJavaType` — correctly resolved
  to the **enhanced** copy, `loader=...BytecodeEnhancedClassUtils$EnhancingClassLoader`.

This is the *opposite* of what the doc's "Precise root cause" section above
hypothesized (that the JavaType registry caches the wrong copy) — for this
code path, the registry's answer is right and the reflective `Field.getType()`
is wrong. But `../../../../native-builtins/src/lang_class.rs` already has a correctly
loader-aware helper for exactly this,
`descriptor_to_class_mirror_via_loader` (~line 2699), which `create_field_object`
(~line 3645) already calls — gated on `crate::classloader::loader_aware_resolution()`.
The fix was seemingly already written and wired up. Forcing
`CRATONVM_LOADER_AWARE_RESOLUTION=1` explicitly made `LazyGroupTest` pass
(`ok=2/2`) on the otherwise-unmodified dev binary, proving the logic was
correct but not engaging by default — which sent the investigation to the gate
itself rather than to reflection code.

**Root cause:** `loader_aware_resolution()` is implemented **three times**,
once per crate that needs it (no shared lower-level crate to put a single copy
in): `classloading/src/class_manager.rs:124`, `native-builtins/src/classloader.rs:731`,
and `vm/src/runtime/env_cache.rs:205`. Each doc comment explicitly says it
"mirrors"/"stays in lock-step with" the others. `vm::env_cache`'s copy was
deliberately flipped from default-off to default-on for the `context.groovy`
Groovy-DSL bug cluster (see `hib-proxyclassreuse-loader-blind-class-resolution.md`,
soak-tested 2026-07-03/04) — its own doc comment even cites this doc's
`bytecode.enhancement`/`lazytoone` cluster as an already-known, unaffected
pre-existing failure. But the OTHER two copies (`classloading`,
`native-builtins`) were never updated to match — they still read `Err(_) =>
false` and their doc comments still say "default OFF". Since most of the
concrete fixes for THIS bug (superclass/interface linking, verifier hierarchy
lookup in `classloading`; `preload_supertypes_via_loader`,
`inherit_lookup_loader`, annotation Class-value resolution, and
`descriptor_to_class_mirror_via_loader` for reflective Field/Method/Constructor
types in `native-builtins`) are gated by the STALE copies, they were silently
disabled by default the whole time — nobody had actually run the full suite
with the gate globally on, despite the 2026-07-04 re-verification and the
`env_cache` comment both believing it was.

**Fix:** flip the two stale copies' `Err(_) => false` to `Err(_) => true`
(`../../../../classloading/src/class_manager.rs`, `../../../../native-builtins/src/classloader.rs`),
restoring the "lock-step" invariant their own comments already assert. No
change to the `Ok(v)` branch (explicit `CRATONVM_LOADER_AWARE_RESOLUTION=0`
still forces gate-off everywhere, unchanged).

**Verification** (Azure host, `../../../../apps/hib-suite-runner`, real JDK 25, JIT on,
`gated_subset.txt` = 131 classes, `TIMEOUT=300s`, neither run set
`CRATONVM_LOADER_AWARE_RESOLUTION` — testing the actual default):

| | PASS | HANG | ABORTED | FAIL |
|---|---|---|---|---|
| baseline (dev `9f1db39d`, pre-fix) | 40 | 13 | 0 | 78 |
| fixed (this branch) | 86 | 7 | 2 | 36 |

The `targetEntity=X, but the attribute is declared as X` signature — the
dominant FAIL cluster in every prior re-verification of this doc — is **gone
from the FAIL set entirely**. `LazyGroupTest` and `FetchGraphTest` (this doc's
own repro classes) both now build their `SessionFactory` successfully.

**Residuals — distinct, separate bugs, not addressed by this fix** (from the
36 remaining FAILs + 7 HANGs in the fixed run):

1. **`InstantiationException: no no-arg constructor in .../$HibernateInstantiator`**
   (6 classes, e.g. `OnlyLazyBasicUpdateTest`, `EagerAndLazyBasicUpdateTest`,
   `SimpleLazyGroupUpdateTest`) — traced to a recurring
   `Lookup.defineClass: ... already defined by user-defined(N) loader`
   collision on repeat `SessionFactory` builds within the same test class
   (each `@Test` method rebuilds a fresh `SessionFactory`, and a subsequent
   `getReflectionOptimizer()` call re-attempts to define the SAME
   `$HibernateInstantiator` helper under the SAME loader namespace instead of
   reusing the first-built one). This looks like the same family as the
   "UPDATE (4th surface)" `Lookup.defineClass` duplicate-define fix above
   (`b2317b72`), but that fix does not cover this recurrence — not
   investigated further this session.
2. **`NoSuchMethodError: ...asManagedEntity()`/`...asPersistentAttributeInterceptable`**
   (~8 classes, e.g. `FetchGraphTest`, `BatchFetchProxyTest`,
   `LazyCollectionLoadingTest`) — `SessionFactory` now builds successfully
   (this fix's direct effect) but a downstream runtime dispatch gap remains.
   Likely the same "5th surface" lazy-collection enhancement-runtime bug this
   doc's `FetchGraphTest` residual note already describes, now newly visible
   on more classes because they get further than before.
3. A handful of `IllegalArgumentException: Object of type 'class ...'`
   mismatches in `enhancement.detached.*` tests — not investigated, may be a
   related but distinct class-identity gap in detached-entity merge/contains
   checks.

## Context

`org.hibernate.orm.test.bytecode.enhancement.*` tests load the test + entity classes
through Hibernate-testing's package-scoped **`EnhancingClassLoader`** (`BytecodeEnhancedClassUtils`),
which *overrides* `loadClass` to ByteBuddy-enhance and `defineClass` **every** in-package
class itself (it does NOT delegate in-package names to the parent). Each enhanced entity
implements `ManagedEntity` / `PersistentAttributeInterceptable` / `SelfDirtinessTracker`
(+ `$$_hibernate_*` accessors). With the loader-aware gate ON, `new`/checkcast/ldc already
resolve the per-loader enhanced copy (prior fix). But three further linking/dispatch sites
were still loader-blind, which (a) introduced two FAIL→CRASH regressions and (b) capped the
gate-on win.

## Root causes & fixes (all four loader-aware fixes gated; the 5th is a general correctness fix)

1. **Superclass/interface linking was loader-blind.** `define_class_with_options`
   (`../../../../classloading/src/class_manager.rs`) resolved a class's superclass/interfaces via
   `self.load_class(name)` → `get_loaded_class_id` → the *one global* (un-enhanced) copy.
   So an enhanced subclass (`Employee`) linked its super to the **un-enhanced** `Person`,
   whose vtable lacks the enhanced `$$_hibernate_read/write_<field>` accessors → hard
   `NoSuchMethodError` on `entity.anUnspecifiedObject` (`InheritedTest` /
   `MappedSuperclassTest` CRASH). **Fix:** under the gate, prefer a copy already defined by
   THIS defining loader's namespace (`loaded_classes_probe(loader_id, name)`), falling back
   to global `load_class`. Paired with:
2. **`preload_supertypes_via_loader`** (`../../../../native-builtins/src/lang_system.rs`) skipped
   driving the loader's `loadClass` for a supertype whenever a *global* copy existed —
   so the enhanced super was never defined under the loader's namespace. **Fix:** under
   the gate, key the "already present" check on the loader's OWN namespace
   (`class_id_defined_by_loader_exact`) so the enhanced supertype is defined first; fix #1
   then links it.
3. **Verifier hierarchy lookup was parents-first.** `ClassStoreHierarchy::lookup`
   (used by the link-time verifier) probed the built-in delegation chain before the user
   loader, so an enhanced subclass's `invokespecial <super>.<init>` saw the parents-first
   un-enhanced `Person` ≠ the (correctly enhanced) linked super → `VerifyError:
   uninitializedThis ... found Person` → define returns null → JDK `postDefineClass` NPE.
   **Fix:** under the gate, probe the user loader's OWN definitions first for an overriding
   loader (matches #1).
4. **`invokespecial` owner dispatch was loader-blind.** `execute_invoke_kind`
   (`../../../../vm/src/runtime/interpreter.rs`) used the CP-resolved owner NAME for `is_special`, so an
   enhanced `Employee.$$_hibernate_read_oca` calling `super.$$_hibernate_read_oca()` (mapped
   superclass `Person`) resolved `Person` to the un-enhanced copy lacking the accessor →
   `NoSuchMethodError`. **Fix:** extend the existing virtual divergence-dispatch override to
   `is_special`: resolve the owner through the caller's loader (`lookup_loader_initiated`)
   and override dispatch when it diverges from the name-resolved copy.
5. **Link-time verification ignored the per-class `skip_verification` flag** (general fix,
   NOT gated). `link_and_initialize` (`../../../../vm/src/vm/vm_util.rs`) re-ran Pass-2 structural
   verification on EVERY class (gated only on the *global* `config.skip_verification`),
   ignoring the per-class flag that ByteBuddy / `Lookup.defineClass` / `Unsafe.defineClass`
   classes were DEFINED with. So a trusted lazy-proxy `Entity$HibernateProxy` whose getter
   overrides a FINAL accessor — which HotSpot surfaces as a *catchable* error the proxy
   factory handles — hit `verify_final_method_constraint` and returned an UNCATCHABLE
   `InternalError` → process abort (`FinalAccessorProxyFactoryTests`; only reachable once
   fix #1 made the hierarchy correct). **Fix:** honor `class_skip_bytecode_verification`
   at link-time too, consistent with the define-time skip (`class_manager.rs ~3267`).

## Results (real-JDK, JIT on, `gated_subset.txt` = 131 enhancement+lazytoone classes)

- `InheritedTest`, `MappedSuperclassTest`: gate-on CRASH → **ok=3/4** (1 aborted = the
  `assumeTrue` skip in the eager `@CustomEnhancementContext`, matching HotSpot), 0 fail.
- `gated_subset` PASS **25 → 31** (+6 eager-enhancement classes); **0 CRASH** (was 1 new:
  `FinalAccessorProxyFactoryTests`, now FAIL via fix #5).
- Gate-OFF byte-identical (fixes #1–#4 all gated on `loader_aware_resolution()`; classloading
  unit tests green; `InheritedTest`/`MappedSuperclassTest` gate-off unchanged = FAIL not crash).

## Historical blocker — the `enhancement.lazy.*` cluster

The lazy cluster fails **earlier**, at SessionFactory build, with
`Object of type '<Entity>' can't be cast to PersistentAttributeInterceptable`
(`ManagedTypeHelper.asPersistentAttributeInterceptable`, keyed on `entity.getClass()`).
The entity instance Hibernate's runtime instantiates during metamodel/persister build is the
**un-enhanced** copy — i.e. Hibernate's view of the entity `Class` (via the `@DomainModel`
annotation `Class[]` element / `ClassLoaderService` / TCCL) is not the enhancing loader's
enhanced copy. This is a SessionFactory-integration root cause orthogonal to the dispatch/
linking fixes above (those are necessary but not sufficient).

### UPDATE 2026-06-30 — SessionFactory-build blocker FIXED (6th fix)

The `PersistentAttributeInterceptable` CCE at SessionFactory build is FIXED (commit
`f658fe12`). Root cause: `inherit_lookup_loader` (`../../../../native-builtins/src/lookup_define.rs`)
resolved a `Lookup.defineClass` target loader via `loader_id_of_class` — an i32 that maps
BOTH `Application` and `UserDefined(2)` to `2` — then a `< 3` threshold, so a class loaded
by a user loader whose namespace id is 1 or 2 (the first ids `allocate_loader_id` hands out)
was mis-routed to the Application namespace. ByteBuddy defines the entity's
ReflectionOptimizer `<Entity>$HibernateInstantiator` via `Lookup.defineClass` against the
ENHANCED entity (`UserDefined(2)`), but it landed under Application, so its generated
`new <Entity>` resolved the un-enhanced copy. Fix: under the gate, derive the namespace from
the lookup class's recorded defining-loader (`peek_loader_namespace_id`) — the same value
`defineClass1` computes — falling back to the legacy i32 path otherwise (byte-identical
gate-off). With it, `enhancement.lazy.*` now build the SessionFactory and run.

### UPDATE 2026-07-01 — lazy lambda fix (landed on dev via misc16) + crash→catchable NSME

The `s.persist(entity)` failure — `PropertyAccessException` / `IllegalArgumentException: Can
not set java.lang.Long field <Entity>.id to <Entity>` — is **NOT** a MethodHandle-subsystem
bug (the "MethodHandle field-setter" note above was a misdiagnosis). The IAE is thrown from
`jdk.internal.reflect.MethodHandleFieldAccessorImpl.ensureObj` **before** any `invokeExact`:
`declaringClass.isAssignableFrom(o.getClass())` is false because the enhancement-test
transaction **lambda** body ran the *un-enhanced* enclosing-class copy, so its `new <Entity>()`
produced an un-enhanced entity that mismatched the enhanced mapped class. This exact diagnosis
and the loader-faithful **lambda impl dispatch** fix landed on dev independently via the misc16
`@BytecodeEnhanced` sweep — see the "misc16 loader-faithful merge" update below (`27f647ff`),
the authoritative account. (Independently re-derived here via New-opcode / `Constructor.newInstance`
/ `isAssignableFrom` instrumentation: the test instance is the enhanced `UserDefined(2)` copy,
but the lambda body's `new <Entity>` resolved the un-enhanced `Application` copy — cid 3014 vs 413.)

**Net addition of this merge (`fix/lazy-enhancement-lambda-dispatch`, `vm_exec.rs` only):** a
missing `$$_hibernate_*` accessor at the terminal invoke path in `invoke_on_class_shared_inner`
now raises a **catchable** `java.lang.NoSuchMethodError` instead of an uncatchable
`InternalError(Linkage)` process abort. Once the enhanced lambda body runs, Hibernate's
HHH-16572 `InvalidPropertyNameTest` reaches `entity.$$_hibernate_read_property()` (an
intentionally-absent enhanced accessor); per JVMS §5.4.3.3 an unresolved method is a throwable
`LinkageError`. Scoped to gate-on + the `$$_hibernate_` family only (passing call sites never
reach the terminal) → CRASH → FAIL, gate-off byte-identical. Verified on this worktree's build:
gate-on `gated_subset` PASS **31 → 54** (19 `enhancement.lazy.*`, incl. `LazyBasicFieldAccessTest`
ok=2/2), **0 CRASH** (`InvalidPropertyNameTest` CRASH → FAIL ok=1/2); gate-off byte-identical;
MethodHandle integration tests green (`wp2_5_proxy`/`wave3_b2_dispatch`/`wave2_c_methodhandles`).

### Precise root cause (traced 2026-06-30) — original SessionFactory CCE characterization

The CCE is `ManagedTypeHelper.asPersistentAttributeInterceptable(entity)` ← persister build
(`AbstractEntityInstantiatorPojo.applyInterception` ← `UnsavedValueFactory` ←
`BasicEntityIdentifierMappingImpl.<init>`). Confirmed via instrumentation:

- `bootDescriptor.getMappedClass()` is the **enhanced** entity Class — `applyBytecodeInterception
  = isPersistentAttributeInterceptableType(getMappedClass())` is `true` (our `isAssignableFrom`
  uses exact mirror class_ids, so this is reliable).
- But the entity instance Hibernate creates is the **un-enhanced** copy (`instanceof
  PrimeAmongSecondarySupertypes` ⇒ 0; the class has zero interfaces).
- It is instantiated by a ByteBuddy ReflectionOptimizer `<Entity>$HibernateInstantiator`
  whose generated `new <Entity>()` resolves the entity to the un-enhanced copy because the
  optimizer class itself is **defined under the Application loader**, not the enhancing loader
  (observed with `--nojit`: `new …$LazyEntity from …$LazyEntity$HibernateInstantiator
  loader=Application -> un-enhanced`). `--nojit` does NOT fix it ⇒ this is the optimizer's
  defining loader, not a JIT `new`-resolution gap.
- The optimizer lands under Application because Hibernate hands ByteBuddy
  `mappedJtd.getJavaTypeClass()` (= `registry.resolveEntityTypeDescriptor(getMappedClass())`),
  and that JavaType's class is **un-enhanced** even though the argument was enhanced.
  `JavaTypeRegistry.resolveDescriptor` is keyed by `Class.getTypeName()` (the NAME string),
  so a previously-registered un-enhanced JavaType for the same name is returned; its
  `checkCached` `!=` guard does not fire (it should throw "Type registration was corrupted").

So the real fix is upstream of dispatch/linking: ensure Hibernate sees ONE entity Class (the
enhanced one). On HotSpot there is exactly one `<Entity>` (the enhancing loader's), because the
Application loader never loads the test-package entity — Hibernate only ever uses the enhanced
`Class` object from the `@DomainModel` annotation, never resolving it by name. On CratonVM a
spurious **un-enhanced** copy is created (Application-loader name resolution somewhere during
metadata build) and then leaks into the name-keyed `JavaTypeRegistry`. Candidate fixes:
(a) prevent the spurious Application-loader load of an entity that a user loader has enhanced;
(b) make the entity JavaType / `getReflectionOptimizer` use the enhanced `getMappedClass()`
directly; or (c) make CratonVM Class-mirror identity loader-faithful so the registry's `==`
guard distinguishes (and rejects) the un-enhanced copy.

### UPDATE 2026-07-01 — lazy cluster CORE FIXED by the misc16 loader-faithful merge (`27f647ff`)

The `enhancement.lazy.*` cluster is now **mostly green** gate-on, resolved by the
misc16 `@BytecodeEnhanced` work merged into dev (`fix/hib-misc-correctness`,
commit `a663624d`; see `HIB-misc16-correctness-sweep.md §4–9`). Two additive
fixes on top of `f658fe12`'s `inherit_lookup_loader` peek:

1. **KEYSTONE (ungated):** `allocate_loader_id()` now starts at **3**, so a user
   loader's namespace id can never alias the reserved `Extension(1)`/
   `Application(2)` encodings in the first place — the class-store side of the
   same collision `f658fe12` patched in `inherit_lookup_loader` (belt-and-suspenders).
2. **The "next OPEN layer" (field-setter `Field.set` CCE) is FIXED** — but the
   cause was NOT the MethodHandle accessor: the `s -> { entity = new X(); }`
   setup **lambda** ran the *un-enhanced* enclosing-class body (loader-blind
   lambda impl dispatch), so `new X` produced an un-enhanced receiver and
   `Field.set(unenhancedEntity, Long)` mismatched the enhanced mapped class.
   Fixed by loader-faithful lambda impl dispatch + reflective type resolution +
   a lambda-arg checkcast carve-out (all gated).

Sample (`lazycluster.txt`, gate-on): 7/9 PASS incl. `OnlyLazyBasicUpdateTest`
20/20, `EagerAndLazyBasicUpdateTest` 40/40, `LazyGroup*`, `LazyBasic*`.

### UPDATE (4th surface) — `Lookup.defineClass` optimizer/bridge duplicate-define FIXED (`b2317b72`)

Now that the ByteBuddy optimizer / access-optimizer bridge correctly lands in
the enhancing namespace, a second `getReflectionOptimizer` for a class sharing a
mapped superclass RE-defined the same helper →
`Lookup.defineClass … already defined by user-defined(4) loader`
(SessionFactory-build failure for `lazy.proxy.FetchGraphTest` / `SpecializedEntity`).
Root cause: `lk_define_class_b` never called `register_defining_loader`, so the
helper's `Class.getClassLoader()` returned the app-loader fallback, and
ByteBuddy's reuse-check `result.getClassLoader() == referenceClass.getClassLoader()`
failed → re-define → collision. FIX (gated): register the lookup class's defining
loader for the newly-defined class, matching `defineClass1` /
`ClassLoader.defineClass`. `FetchGraphTest` now builds the SessionFactory
(0→17 tests run) with **no regression** to the 5 misc16 classes.

**Residual (5th surface, NOT loader-faithful):** `FetchGraphTest` now fails at
runtime — `NPE: $$_hibernate_read_specializedEntities() is null` — a lazy
*collection* enhancement-runtime bug (the enhanced entity's lazy `Set` field is
not initialized), distinct from the loader-identity family. `BasicAttributesLazyGroupTest`
similarly has a residual (`Nested Jupiter execution failed: 1 failure(s)`).

## Repro

`../../../../apps/hib-suite-runner`: `export CRATONVM_LOADER_AWARE_RESOLUTION=1; CV=<cvlazy.exe>
TIMEOUT=600 bash rerun.sh gated_subset.txt gated`. Single class:
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home <jdk> --Xmx 1500m @common.args
-Dcraton.batch=1 CratonRunner <listfile> <idx>`. Per-class diagnostics: `CRATONVM_DBG_NSME=1`
(dispatch class vs receiver), `CRATONVM_DBG_CCE=1` (checkcast failures). TRAP: Windows
`timeout` does not kill a hung native cratonvm — taskkill before rebuild (it locks the binary).
