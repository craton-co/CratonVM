# CGLIB `@Configuration` singleton-cache regression — FIXED 2026-07-27

> **Residual round — 2026-07-28.** The original fix below is correct and still
> holds (`S03_ConfigProxy` 8/8, suite 10/10 / 95/95 re-verified on `dev`
> @ `d0a6c7987`), but it left four residuals, all now closed. See
> [Residuals (2026-07-28)](#residuals-2026-07-28) at the end of this file:
> a regression the sibling-site cleanup introduced in
> `Instrumentation.getInitiatedClasses`, the same *caller*-side loader-id
> defect at three more generated-subclass define sites (one of them
> observably divergent from HotSpot), the missing round-trip test coverage,
> and the 18 `runtime::lock_order` `--release` failures the Verification
> section had to explain away.

## Summary

Root-caused and fixed the regression documented in
`docs/known-issues/springboot/configproxy-cglib-singleton-regression-20260727.md`
(`S03_ConfigProxy` scenario regressed 8/8 -> 4/8 in the real Spring Boot
functional suite; `engineBuiltOnce` expected the `@Bean` factory method to
run once, it ran 3 times — an inter-`@Bean`-method call on `this` was not
routing through the shared singleton, re-running the real factory body
every time).

## Root cause

`vm/src/vm/vm_exec.rs`'s `NativeContextImpl::define_class_full` decoded its
`loader_id: u32` parameter back into a `ClassLoaderId` with:

```rust
let cl_id = if loader_id == 0 { ClassLoaderId::Application } else { ClassLoaderId::UserDefined(loader_id) };
```

This is NOT the inverse of `loader_id_of_class`'s encoding
(`Bootstrap=0, Extension=1, Application=2, UserDefined(id)=id`) a few
hundred lines below it in the same file. `native-builtins/src/cglib_enhancer.rs`'s
`cce_enhance` fetches `super_loader_id = ctx.loader_id_of_class(super_class_id)`
(deliberately, so the generated `@Configuration` subclass is defined by the
SAME loader as its superclass — required for CGLIB-style enhancement to
work at all) and passes it straight into `define_class_full`. For a
superclass loaded by the real Application loader, this round-trip is
`Application -> 2 -> define_class_full -> UserDefined(2)` — a DIFFERENT
`ClassLoaderId` than `Application`.

`classloading/src/access_control.rs`'s `same_runtime_package` correctly
requires `a.loader_id == b.loader_id` per JVMS 5.4.4 (a runtime package is
`(defining loader, package name)`, not the name alone). With the enhanced
subclass mistagged `UserDefined(2)` against the superclass's real
`Application` tag, every package-private `@Bean` override (`engine()`,
`car1()`, `car2()` — none have an explicit access modifier, so all are
default/package-private per JLS) failed the
`same_runtime_package`-gated `is_true_override` check in
`classloading/src/class_manager.rs`'s `build_vtable_descriptors_with_overrides`.
Each override was therefore appended as a fresh, independent vtable slot
instead of replacing the inherited slot in place — so any ordinary
`invokevirtual` dispatch to e.g. `engine()` from within another `@Bean`
method's inherited body (`Cfg.car1()`, reached via the enhancer's own
`invokespecial super.car1()`) kept resolving to the SUPERCLASS's original
slot, never seeing the override, and re-ran the real (uncached) factory
body every time.

Confirmed empirically with a battery of targeted debug traces (later
removed) at every layer: the thread-local `SimpleInstantiationStrategy
.getCurrentlyInvokedFactoryMethod()` mechanism itself was working
correctly (confirmed via a standalone diagnostic probe printing its value
at each call site); an ISOLATED direct reflective re-invocation of the
override worked correctly (ruling out a general dynamic-class dispatch
bug); the vtable-population trace showed the exact mismatch:
`class_loader_id=UserDefined(2)` for the enhanced subclass vs.
`declaring_super_loader_id=Application` for the superclass, with
`same_runtime_package=false`.

Interestingly, `allocate_loader_id` (a few dozen lines below the fix, same
file) already documents this exact reserved-id invariant ("0=Bootstrap,
1=Extension, 2=Application... starting the counter at 3 guarantees every
allocated namespace is a genuine UserDefined id") from a prior, unrelated
Hibernate bytecode-enhancement fix — but that fix only addressed
NEWLY-ALLOCATED loader ids, not this DECODE site, which could still
misinterpret an EXISTING built-in loader's id when round-tripped back
through `define_class_full`.

## Fix

Made the decode the exact, symmetric inverse of the encode:

```rust
let cl_id = match loader_id {
    0 => ClassLoaderId::Application,  // preserve the pre-existing default-sentinel convention
    1 => ClassLoaderId::Extension,
    2 => ClassLoaderId::Application,
    other => ClassLoaderId::UserDefined(other),
};
```

(`0` intentionally keeps decoding to `Application`, not `Bootstrap`, to
avoid changing behavior for any existing caller that passes literal `0` as
a "default loader" sentinel rather than a genuine round-tripped
`loader_id_of_class` value — `Bootstrap` is not otherwise reachable through
this path in practice.)

Three sibling call sites in the same file had the identical bug shape and
were fixed the same way for consistency, though none were proven to cause
a currently-observed failure (verified safe via the full
`cratonvm-classloading` + `cratonvm-vm` test suites, both clean relative
to a pre-fix baseline):
- `define_class_with_loader` (previously ALWAYS `UserDefined(loader_id)`,
  no special-casing at all)
- `class_id_by_name_and_loader` / `class_id_defined_by_loader_exact`
  (same, always `UserDefined(loader_id)`)
- `list_initiated_class_ids` (same 2-arm bug as `define_class_full` had)

## Verification

- Standalone diagnostic
  (`docs/known-issues/repros/cglib-loaderid-fix-20260727/CglibDiag.java`, prints
  `SimpleInstantiationStrategy.getCurrentlyInvokedFactoryMethod()` at each
  `@Bean` method entry): `engineCtor` now stays at 1 (was 3), both cars
  share the same `Engine` instance.
- Real fixture: `S03_ConfigProxy` scenario at
  `/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite`:
  **8/8** (was 4/8).
- Full 10-scenario Spring Boot suite: **10/10 scenarios, 95/95 checks**,
  no regressions (S01-S10 all still pass).
- `cargo test --release -p cratonvm-classloading --lib`: 637 passed, 0
  failed.
- `cargo test --release -p cratonvm-vm --lib`: 2410 passed, 18 failed --
  identical to the pre-fix baseline (confirmed by stashing the fix and
  re-running: same 18 `runtime::lock_order` tests fail both with and
  without this change, a pre-existing `--release`-vs-debug-assertion test
  config mismatch unrelated to this fix). **Closed in the residual round —
  see R4 below; the release run is now clean.**

---

## Residuals (2026-07-28)

Four things the first round left behind. All four are fixed; the sections
below record what each one actually was, since three of them were invisible
to the suite that motivated the original fix.

### R1 — the sibling-site cleanup regressed `Instrumentation.getInitiatedClasses`

The "fixed the same way for consistency" pass above gave the four sibling
sites a **strict** inverse (`0 => Bootstrap`) while `define_class_full` itself
kept `0 => Application`. That is not a cosmetic inconsistency: the `u32`
boundary carries *two* conventions at once, and `0` belongs to the second one.
`native-api`'s trait doc has always said "`loader_id == 0` means use the
application loader", and plenty of callers pass a literal `0` for exactly that
(`native-builtins`'s hidden-class defines, the JBoss-module synthetic-`main`
define, the Spring bootstrap subclass defines). One of the four re-pointed
sites had a caller that *depended* on it:

`vm/src/runtime/instrument.rs`'s `native_get_initiated_classes0`
(`Instrumentation.getInitiatedClasses(ClassLoader)`) always passes `0` and
documents, in a comment right above the call, that it wants the
application-loader classes. After the cleanup it returned the **bootstrap**
loader's classes instead — i.e. the JDK's own classes rather than the
application's. Nothing in the Spring Boot suite calls it, so nothing caught it.

### R2 — the same defect on the *caller* side, at three more define sites

The first round fixed the **decode**. The mirror-image defect is a caller that
hardcodes `loader_id = 0` where it should be passing the superclass's own
loader — which is what `cce_enhance` was already (correctly) doing, and the
only reason the decode bug was reachable at all. Three generated-subclass
sites still hardcoded it:

- `native-builtins/src/cglib_enhancer.rs`'s
  `build_factory_bean_subclass_wrapper` (concrete-`FactoryBean` wrapper);
- `native-builtins/src/spring_startup_bootstrap.rs`'s `@Lookup`
  method-override subclass;
- ...and its replaced-method override subclass.

All three now pass `loader_id_of_class(<superclass>)`. This is a no-op
whenever the bean class is app-loaded (id 2 decodes to `Application`), which
is why it never showed up — but for a fork-loaded bean class the generated
subclass landed in the application namespace while its superclass lived in the
child loader's, i.e. the exact `same_runtime_package` mismatch this whole
document is about.

**Observable, not theoretical:** with
`docs/known-issues/repros/cglib-loaderid-fix-20260727/{LookupForkDiag,Driver}.java`
(see that directory's `README.md`), running the bean classes behind a child
`URLClassLoader`:

```
CratonVM before: widgetUserClass=probe.Driver$WidgetUser$$SpringCGLIB$$LM0
                 loader=jdk.internal.loader.ClassLoaders$AppClassLoader
                 superLoader=java.net.URLClassLoader
HotSpot:         widgetUserClass=probe.Driver$WidgetUser$$SpringCGLIB$$0
                 loader=java.net.URLClassLoader
                 superLoader=java.net.URLClassLoader
```

`fork.lookupSubclassSameLoader` FAILs on the pre-fix binary and PASSes after,
with the app-loaded run (`LookupForkDiag app`) green throughout — 11/11 in both
modes, matching HotSpot exactly.

### R3 — the encode and the decode were still two hand-written tables

Six copies of the same `0/1/2/else` match, in one file, with no test asserting
they were inverses — which is precisely how they drifted in the first place.
Both directions now live on `ClassLoaderId` itself
(`types/src/class_id.rs`), next to each other and next to the reserved-id
constants:

- `to_native_id()` — the encode `loader_id_of_class` returns;
- `from_native_id()` — the strict inverse (`0 => Bootstrap`);
- `from_native_id_or_default()` — the boundary convention (`0 => Application`,
  "caller did not specify");
- `NATIVE_{BOOTSTRAP,EXTENSION,APPLICATION,FIRST_USER_DEFINED}` — the reserved
  ids, so `allocate_loader_id`'s "must start at 3" invariant no longer repeats
  a bare literal.

All six `vm_exec.rs` sites now call the codec, and six unit tests pin it,
including `application_never_decodes_to_a_user_defined_namespace` (the exact
shape of the original bug) and `default_sentinel_decode_differs_from_strict_
decode_only_at_zero`. `native-api`'s trait doc now states the encoding and
names the decoder implementations must use.

### R4 — the 18 `runtime::lock_order` `--release` failures

Not caused by this fix, but re-triaged as "pre-existing noise" on every
release test run, which is a cost this document should not keep paying.
Cause: lock-order enforcement is unconditional under `debug_assertions` and
**off by default** in release (opt-in via `CRATONVM_LOCK_ORDER_CHECK`), so
`cargo test --release` silently stopped checking and all 15
`#[should_panic(expected = "lock order violation")]` tests stopped panicking;
`enforcement_active_in_debug_builds` additionally asserted outright that the
runner *was* a debug build.

Fixed by adding `tracking::force_enable_for_testing()` (a `#[doc(hidden)]`
test hook that can only ever *enable* checking — flipping it on mid-process
can cause a violation to be missed, never invented) and calling it from the
tests that need the checker, plus rewriting the two gating tests to pin the
real contract rather than the build profile. A release run now exercises the
same code path a debug run does.

**Consequence worth knowing about:** because release runs now enforce like
debug runs, they also inherit debug's pre-existing flake in
`native::jni::tests::process_vm_publish_and_resolve`, which asserts that
dropping its own `Arc<SharedVm>` was the last strong reference. Isolated it
passes 5/5; in a full parallel run it fails intermittently. This is **not**
introduced here — it is enforcement-*timing*-sensitive, not code-sensitive:
running this same release binary with `CRATONVM_LOCK_ORDER_CHECK=1` and
`--skip runtime::lock_order` (so none of the changed tests execute) still
fails it 2 of 4 runs, and a **debug** `cargo test -p cratonvm-vm --lib --
--skip runtime::lock_order`, where enforcement has always been unconditional,
fails it 2 of 6. Release runs were simply blind to it while enforcement was
silently off — the same blind spot R4 is about.

## Residual-round verification

All on the Linux build host, worktree
`/data/data/wt-cglib-loaderid-20260727` (branch
`fix/cglib-loaderid-residuals-20260727`, from `dev` @ `d0a6c7987`), against a
same-tree pre-change baseline binary built from that exact commit:

| | baseline `d0a6c7987` | with residual fixes |
|---|---|---|
| Spring Boot 10-scenario battery | 10/10 scenarios, 95/95 checks, 0 divergences | **10/10, 95/95, 0 divergences** |
| `CglibDiag` | `engineCtor=1`, both cars share the engine | unchanged |
| `LookupForkDiag app` | 11/11 | **11/11** |
| `LookupForkDiag fork` | 10/11 (`lookupSubclassSameLoader` FAIL) | **11/11** (== HotSpot) |
| `cargo test --release -p cratonvm-types --lib` | — | **417 passed, 0 failed** |
| `cargo test --release -p cratonvm-classloading --lib` | — | **637 passed, 0 failed** |
| `cargo test --release -p cratonvm-vm --lib` | 2410 passed, 18 failed | **2428 passed, 1 failed** |

The vm-lib delta is exactly the 18 `runtime::lock_order` tests moving from
failed to passed (2410 + 18 = 2428). The one *deterministic* remaining failure,
`jit::skip_list::tests::elasticsearch_vector_diskbbq_hang_cluster_stays_
interpreted_by_default`, was **unrelated and pre-existing on `dev`**: commit
`bae30dc3c` ("remove the blanket `org/elasticsearch/` JIT ban") deleted the
ban those class names matched but left the test asserting they were still
banned. Nothing in this branch touches JIT skip-list policy; the test was
fixed independently on `dev` (renamed to
`elasticsearch_vector_diskbbq_cluster_is_jit_eligible_after_es_cluster_removal`)
and the merge of `origin/dev` @ `2cdc451fb` picked that up.

## Post-merge re-verification (`origin/dev` @ `2cdc451fb`)

Rebuilt and re-run after merging `origin/dev` into the branch:

- Spring Boot 10-scenario battery: **10/10 scenarios, 95/95 checks, 0
  divergences**.
- `LookupForkDiag`: **11/11 app, 11/11 fork**.
- `cargo test --release -p cratonvm-classloading --lib`: **637 passed, 0
  failed**.
- `cargo test --release -p cratonvm-types --lib`: **417 passed, 0 failed**
  (3/3 runs). One run *inside a combined multi-package invocation* failed
  `compact_value::tests::to_value_unchecked_degrade_increments_counter`
  (expected 3 degradations, saw 4) — a pre-existing race on that test's
  process-global counter, in code this branch does not touch.
- `cargo test --release -p cratonvm-vm --lib`: **2429 passed, 0 failed** on a
  clean run; two of three runs additionally tripped the pre-existing
  `process_vm_publish_and_resolve` flake described under R4. With
  `--skip runtime::lock_order` the suite is **2379 passed, 0 failed, 3/3**.
