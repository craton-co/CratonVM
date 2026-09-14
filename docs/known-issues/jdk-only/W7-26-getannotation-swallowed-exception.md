# `getAnnotation` returned null for "the VM failed"

> **UPDATED 2026-08-12 — R1/R2/R3 partially discharged; see "R3 discharged"
> near the end.** The census is widened from one file to the workspace (21
> `loadClass` delegations in **12** files; R1's "twelve" and R2's "five" were
> both one-file counts), four more sites are narrowed in source, and the
> thirteen that remain are named with two exact out-of-file patches. Nothing
> in that section was built or run either.

**Status: FIXED in source 2026-08-11, NOT BUILT.** Every measurement below
came out of the already-built `dev` binary at `C:/craton/CratonVM`
(`target/release/cratonvm.exe`, mtime 19:41). No claim is made about the code
this record changes — nothing here was compiled or run after the edit.

This closes residual **R1** of `W7-12-strict-annotation-proxy.md`, which
diagnosed the strict-mode refusal of the annotation carrier and observed, in
passing, that one cause was wearing two faces. The refusal itself is fixed
(commit `5266bf8c7`, "route the annotation carrier through the VM-internal
door"). This record is about the *second* face: the reason a caller could not
see the first one.

## Why this is the campaign's signature species, not a detail of it

`getAnnotation` returning `null` is the fabricated-success shape at its
purest, because **`null` is a legitimate answer** — it is how the JDK says
"this annotation is not present". A caller has no way to distinguish

* "you asked for `@Tag` and this class does not have one", from
* "the VM could not build the proxy and gave up".

Every other instance of this species in the campaign at least degraded a
value the caller might have squinted at (`Vec::new()` in `W7-20`, a
zero-length read in `W7-8`). This one produces the *correct-looking* answer
for the *most common* case. `isAnnotationPresent` makes it worse rather than
better: it answers from the parsed class-file bytes and never touches the
builder, so the pair could disagree —
`isAnnotationPresent(Tag.class) == true` while
`getAnnotation(Tag.class) == null` — which is a state no conforming JVM can
produce and no library checks for.

## Reproduction: one cause, two faces, one binary

Three vectors, same command shape, same pre-fix binary:

```sh
./target/release/cratonvm.exe --jdk-only \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  -cp regression-suite/build <VECTOR>
```

| vector | `--jdk-only` | `--real-jdk` |
|---|---|---|
| `RJdkJmx` | `NoClassDefFoundError: java/lang/annotation/AnnotationProxy` | — |
| `RReflect` | `AssertionError: getAnnotation present` at `RReflect.java:35` | `PASS RReflect (40 checks)` |
| `RJdkReflect` | `AssertionError: runtime annotation on the type` at `RJdkReflect.java:267` | `PASS RJdkReflect (60 checks)` |

That table **is** the diagnosis. The same failure, in the same binary, on the
same class, reached one caller naming the class it could not mint and reached
the other two as a bare assertion three frames away. The only difference is
which entry point ran: `Introspector.descriptorForElement` calls
`getAnnotations()` (array-valued, propagates with `?`), while `RReflect` and
`RJdkReflect` call `getAnnotation(X)` (single-valued, swallowed).

This is `W7-20`'s observation reproduced exactly one wave later: *the same
refusal was loud on some rows and silent on others, and the only difference
was the helper it landed in.*

Note on staging: the binary above predates commit `5266bf8c7` by 45 minutes,
so what it shows is the **pre-carrier-fix** world. That does not weaken the
evidence for this defect — it strengthens it, because it is the only state in
which both faces of the one cause are visible at once. With the carrier fix
landed, this particular refusal no longer occurs and these two vectors are
expected to pass; the swallow it exposed remains, and would have hidden the
next refusal just as thoroughly.

## The discrimination line

Taken verbatim from `W7-20`, because the shape is identical:

> **`Err` propagates; `Ok`-with-nothing-usable stays empty.**

`cached_annotation_proxy_resolving` and `create_annotation_proxy` both return
`Result<Option<ObjectRef>, MethodCallFailed>`, and the two channels mean
different things. `Ok(None)` is "this annotation type would not resolve, and
that is a normal outcome" — it still falls through to `null`, unchanged.
`Err` is a failure, and `MethodCallFailed::ExceptionThrown` is worse than a
failure: it means an exception is **already pending in the VM**, and the old
spelling both hid it and kept calling back into the VM underneath it. HotSpot
bails at the first `CHECK_` for exactly this reason.

The file already agreed with this rule everywhere else.
`native_method_get_annotation` — `Method.getAnnotation`, the direct sibling of
the four — is spelled `Ok(Some(Value::Object(Some(proxy?))))`. It was never
broken. The four sites were the outliers, not the convention.

## The five single-annotation sites

All in `native-builtins/src/lang_class.rs`. Anchor on the function names; line
numbers rot.

| # | site | was | now | caller-visible observable |
|---|---|---|---|---|
| 1 | `native_class_get_declared_annotation` | `if let Ok(Some(proxy)) = cached_annotation_proxy_resolving(…)` | `if let Some(proxy) = …?` | `Class.getDeclaredAnnotation(X)` throws the builder's exception instead of answering `null` |
| 2 | `native_class_get_annotation`, own-class scan | same | same | `Class.getAnnotation(X)` throws instead of answering `null`; this is the site `RReflect.java:35` and `RJdkReflect.java:267` ran through |
| 3 | `native_class_get_annotation`, `@Inherited` superclass walk | same, inside a `while` | same | as above, **and** the walk stops instead of continuing up the chain calling `class_annotations` with an exception pending |
| 4 | `native_field_get_annotation` | `if let Ok(Some(proxy)) = create_annotation_proxy(…)` | `if let Some(proxy) = …?` | `Field.getAnnotation(X)` throws instead of agreeing with `Field.isAnnotationPresent(X) == true` and answering `null` |
| 5 | `native_annotated_type_get_annotation` | `if let Ok(Some(Value::Object(Some(tm)))) = ctx.invoke_virtual(proxy, "annotationType", …)` | `ladder_rung` on the call, match on the value | `AnnotatedType.getAnnotation(X)` throws instead of reading as "no such type-use annotation" when the proxy's invocation handler throws |

Site 5 takes `ladder_rung` (below) rather than a bare `?` because it is a
**scan**, not a build: unlike sites 1–4 there is a next element to try, so a
stashed object with no `annotationType` at all is still skipped exactly as it
was. Only a real pending exception aborts the scan.

Site 5 was **not** in W7-12's list of four. The sweep found it: it is the same
species, reached through a different helper, and it is the only one outside
the `Class`/`Field` pair.

W7-12 also mis-attributed the four — it placed three in
`native_class_get_annotation` and one in `native_field_get_annotation`. The
third is actually in `native_class_get_declared_annotation`, a different
public method with a different contract (no `@Inherited` walk). The count was
right and the fix is the same, but the census was not, which is worth noting
for anyone re-deriving the set from that record rather than from the file.

## The sweep

`lang_class.rs` is 25,906 lines. Two passes were run over it: one keyed on the
swallow *shapes* (`if let Ok(`, `.ok()`, `unwrap_or*`, `let _ =`, `is_ok()`,
`Err(_) =>`, `matches!(`) intersected with the file's **233** `Result`-
returning functions plus the fallible `NativeContext` surface, and a second
keyed on `invoke_virtual`/`invoke` call sites followed by an `Err`-catching
arm — which is what found sites 5 and the whole `AnnotatedType` cluster that
the first pass missed.

The raw shape grep returns **501** lines. That number is worthless and the
campaign README is right that grep-derived counts run an order of magnitude
high: **~390 of them are `unwrap_or*`/`ok_or` on an `Option`**, which has no
error channel to drop, and 11 more are in `#[cfg(test)]` code. Resolving call
sites instead gives **63**, every one of which was read.

**63 of 63 resolved. 16 changed.**

### Changed (16)

| site | what it swallowed | caller can carry? | action |
|---|---|---|---|
| the five single-annotation sites above | the proxy builder's `Err`, incl. `ExceptionThrown` | yes — all are `MethodCallResult` | `?` |
| `native_method_get_annotated_return_type` ×2 (`getGenericReturnType`, `getReturnType`) | an exception from the type resolution ladder | yes | `ladder_rung` + `?` |
| `native_field_get_annotated_type` ×2 (`getGenericType`, `getType`) | same | yes | `ladder_rung` + `?` |
| `native_executable_get_annotated_parameter_types` ×2 (`getParameterTypes`, `getGenericParameterTypes`) | same | yes | `ladder_rung` + `?` |
| `native_parameter_get_annotated_type` (`getGenericParameterTypes`) | same | yes | `ladder_rung` + `?` |
| `parameter_erased_type_mirror` (`getType`) | same | **no — that was the bug** | return type changed to `Result<ObjectRef, MethodCallFailed>` |
| `native_annotated_type_get_annotated_owner_type` ×2 (`getOwnerType`, `getDeclaringClass`) | same | yes | `ladder_rung` + `?` |
| `native_annotated_parameterized_type_get_annotated_actual_type_arguments` (`getActualTypeArguments`) | same | yes | `ladder_rung` + `?` |

The `AnnotatedType` builders are written as **ladders** — try
`getGenericReturnType()`, else the erased `getReturnType()`, else a
`ClassId(0)` mirror — and every rung was spelled `_ =>`, which catches `Err`
alongside the "this rung had no answer" cases. A thrown exception therefore
became an `AnnotatedType` wrapping the *wrong* type. The caller-visible
observables, in order: a reflective reader (ByteBuddy's `JavaDispatcher`, used
by Mockito, per the existing doc comment on that function) sees the
`ClassId(0)` mirror instead of the declared return/field/parameter type; sees
`getAnnotatedParameterTypes()` **shortened to zero elements**, because the
erased vector is also the length reference for the generic array; sees a
**null owner**, which is the legitimate answer for a top-level type; and sees
an `AnnotatedParameterizedType` reporting **no type arguments**, which is a
contradiction in terms.

`ladder_rung` is the one place the discrimination is written down.
`MethodCallFailed` has exactly two variants and the ladders only ever meant
one of them:

* `ExceptionThrown` — a real pending Java exception (a malformed `Signature`
  attribute raising `GenericSignatureFormatError`, a `TypeNotPresentException`
  from a class-valued member). HotSpot propagates these out of
  `getAnnotatedReturnType` too. **Re-raised.**
* `InternalError` — the receiver has no such method at all, which is exactly
  the "unavailable, use the next rung" case these fallbacks were written for
  (a CratonVM-built `Method`, a synthetic-JDK receiver). **Mapped to
  `Ok(None)`** so the existing fallback runs unchanged.

This is why the ladders take `ladder_rung` and the five annotation sites take
a plain `?`: the annotation sites have no next rung, so the alternative to
propagating is a lie, whereas the ladders have a documented fallback that must
survive.

### Resolved, no change (47)

Grouped by why, with the count in each group.

| group | n | what it "swallows" | caller can carry? | why it stays |
|---|---|---|---|---|
| `let _ = ctx.load_class(<name>)` immediately followed by `ctx.class_id_by_name(<name>)` | 6 | nothing | n/a | **Not a swallow.** This is the warm-then-requery idiom: the outcome is read back on the very next line through a different channel, so the `Err` carries no information that is lost. |
| loader-resolution ladder rungs (`resolve_annotation_class_via_loader` → `Err(_) => None`, `class_id_by_name_via_referencing_class(…).ok()`, `loadClass` rungs in `native_class_get_nest_host`, `declaring_class_loader_aware`, `native_class_get_declared_classes`, `descriptor_to_class_mirror_via_loader`) | 12 | a `ClassNotFoundException` from a user loader | partly | **Intentional filter, and the design already says so.** `resolve_annotation_class_via_loader` returns `Result<ObjectRef, Option<ObjectRef>>` — *not* `MethodCallFailed` — precisely so it hands the caught exception object to the caller rather than leaving one pending. Propagating here would turn "optional dependency absent" into a throw and break every `@ConditionalOnClass`, which the 15-line comment above the `TypeNotPresentException` sentinel documents at length. See residual R1 below. |
| annotation-element conversion fallbacks (`Enum.valueOf`, `load_class`, `annotation_component_class_id`, `resolve_annotation_type_via_container_loader`) | 6 | a resolution failure | partly | Same family as above; each ends in a **defined** sentinel (a synthetic enum instance, the shared `TypeNotPresentException`), not in a plausible-looking value. |
| failures re-raised as a *different* exception (`link_isolated_method_signatures` `.is_ok()` → `ClassNotFoundException`; `wf_shim_synth_main_method` ×4 → `NoSuchMethodException`) | 5 | the original exception's identity | yes | **Loud, not silent** — the caller gets an exception either way. Wrong *type*, not fabricated success. Residual R2. |
| documented best-effort constructor runs (`build_empty_permissions`, `wrap_as_invocation_target_exception`, `native_constructor_new_instance` ×2) | 4 | a `<init>` failure | no meaningfully | The object is non-null and that is the contract callers rely on; in the ITE case, propagating would *replace* the exception being wrapped. |
| spec-mandated null (`native_class_for_name_module`) | 1 | a load failure | yes | The JDK overload is specified to return `null`, not throw, on a miss. |
| `Package`/module construction fallbacks (`package_version_info`, `ensure_class_initialized("…$VersionInfo")` ×4, `getModule` → `canonical_unnamed_module`, `load_class`) | 8 | a class-init failure | yes | Each falls back to a canonical object the surrounding code documents as mandatory ("leaving `module` unset is never acceptable"); the fallbacks themselves use `?`. |
| predicates and diagnostics (`native_method_invoke`'s `matches!(native_class_is_instance(…))`, `check_deep_reflection_access(…).is_ok()`, the `[CLASS-RES]` `available()` debug print, `native_class_get_class_loader` in the two resource lookups) | 5 | a predicate's error | mixed | The two resource lookups fall back to the static `-cp` scan, which is the pre-delegation behaviour; the debug print is `eprintln!`; `native_method_invoke` throws `IllegalArgumentException` on a false answer, so a failure is loud. |
| `.ok()` on `Option`-shaped accessors already checked downstream (`get_annotated_superclass`, `get_annotated_bounds`, `get_protection_domain0`) | — | nothing | n/a | Counted in the groups above where fallible; `native_type_variable_get_annotated_bounds` already uses `?` on its `invoke_virtual` and is the in-file precedent the ladder fix follows. |

## Which mode moves

**`Compatible` is byte-for-byte unchanged on every non-throwing path**, and
the reason is structural rather than a claim about coverage:

* At the five annotation sites, `Ok(Some(proxy))` and `Ok(None)` both do
  exactly what they did — return the proxy, or fall through to
  `Ok(Some(Value::Object(None)))`. Only the `Err` arm is rewritten, and `Err`
  is by definition not a non-throwing path.
* At the eleven ladder rungs, `ladder_rung` maps `Ok(v) => Ok(v)` and
  `InternalError => Ok(None)`, and `Ok(None)` is already one of the values the
  old `_ =>` arm matched. So every rung that previously fell through still
  falls through, for every non-`ExceptionThrown` reason. Only
  `ExceptionThrown` is diverted.

The intended change, stated precisely: **an exception that was previously
discarded now reaches the caller.** In `Compatible` that means a
`getAnnotation`/`getAnnotatedType` call that used to answer `null`, an empty
array, or a `ClassId(0)` mirror while an exception sat pending now throws that
exception. In `--jdk-only` it additionally means a strict-mode refusal names
the class it refused instead of arriving as an assertion three frames away —
which is the whole point.

Nothing here was rebuilt. Land it against `RReflect`, `RJdkReflect`,
`RJdkJmx`, `RJdkProxy` and an annotation-heavy Spring workload in **both**
modes before believing either half — `Compatible` is the mode at risk and the
mode that is green today.

## Out-of-file patch (not applied)

**None required *for the sixteen sites above*.** Every one of them lives in
`native-builtins/src/lang_class.rs`, and `ladder_rung` is a private helper in
that file. No other lane's file is touched, and no shared API changed shape:
`parameter_erased_type_mirror` is a module-private `fn` with two call sites,
both in the same function.

**Thirteen more were found by the widened census on 2026-08-12 and DO need
out-of-file patches** — see "R3 discharged" below, which carries the exact
replacement text for the two whose argument is settled.

## Residuals

* **R1 — the loader ladders discriminate on nothing.** The twelve rungs in
  the "intentional filter" group catch *any* exception from a user loader's
  `loadClass`, not just `ClassNotFoundException`. A loader that raises a
  `LinkageError`, or that OOMs, is filtered as if the class were merely
  absent. The correct shape is to test the caught throwable's class and re-
  raise anything that is not `ClassNotFoundException`/`NoClassDefFoundError`.
  `resolve_annotation_class_via_loader` already hands the exception **object**
  back to its callers (`Err(Some(exc))`), so the information is in hand at
  every one of those sites and only the test is missing. Not attempted here:
  it changes behaviour on the `@ConditionalOnClass` path, which is the
  highest-traffic annotation path in the Spring suites, and this lane could
  not build.
  **PARTIALLY DISCHARGED 2026-08-12** — the census is widened (21 `loadClass`
  delegations in 12 files, not 12 in one), the policy helper exists
  (`absorb_class_absent`, two absorbed roots, `ClassId`-hierarchy test), and
  four sites are narrowed with a scheduled assertion. Thirteen remain, every
  one in another lane's file; two of them carry exact replacement text. See
  "R3 discharged" below. The "twelve" in this bullet was a one-file count and
  should not be quoted as a population.
* **R2 — five sites re-raise a failure as the wrong exception type.**
  `link_isolated_method_signatures` turns any `initialize_class` failure into
  `ClassNotFoundException`, which swallows the identity of an
  `ExceptionInInitializerError`; `wf_shim_synth_main_method`'s four sites turn
  any failure into `NoSuchMethodException`. Both are loud, so neither is this
  record's species — but a caller that catches on type is still being lied to.
  **The count of five was also a one-file count.** The widened census found
  two more, both on the class-loading path and both worse than the five
  because `ClassNotFoundException` is what a loader's caller *expects*, so the
  lie is invisible: `ucl_real_find_class` (`classloader_real.rs`) reported
  every `ctx.load_class` failure as "not found", and `lang_class.rs:10022`
  re-mints every isolated-loader `loadClass` failure as
  `isolated_loader_class_not_found`. The first is **FIXED 2026-08-12**; the
  second carries exact replacement text under "R3 discharged". The original
  five are untouched.
* **R3 — this sweep covered one file.** The species is defined by a helper's
  return type, not by a subsystem, so the same two scans are worth running
  over the other large native modules. The second scan (invoke sites followed
  by an `Err`-catching arm) found nine of the sixteen fixed sites and none of
  them were visible to the first; a sweep that only greps swallow shapes will
  under-report by roughly half.

## R3 discharged — the widened census, 2026-08-12

**Nothing below was built or run.** Written by a lane that owns
`classloading/**` and three `native-builtins` files and could not compile.

R3 was right that the one-file scan under-reports, and wrong about by how
much. Re-run **workspace-wide** — `native-builtins`, `native-io`,
`native-collections`, `vm` and `classloading` — with the second scan
re-keyed on the two helpers that define the loader-ladder species rather than
on `invoke_virtual` in general:

```sh
# the loader-ladder population, by delegation site rather than by shape
grep -rn --include=*.rs '"loadClass"' native-builtins/src native-io/src \
    native-collections/src vm/src classloading/src
grep -rn --include=*.rs 'class_id_by_name_via_referencing_class'
# then READ each `ctx.invoke*(` / `ctx.class_id_by_name_via_referencing_class(`
# that names one, and classify the arm that receives its `Err`
```

That is the pattern to extend; the raw shape grep is not. Over the ten files
this lane owns the shape grep returns **~367** lines and **zero** of them
would have found a single site fixed below — the two in `class_manager.rs` are
`.ok()` and `filter_map(… .ok())` inside a supertype loop, which the shape
grep does hit and drowns among 117 other hits in that one file, and the two in
`classloader_real.rs` are `_ =>` and `if let Ok(…)` arms indistinguishable
from forty correct ones.

### The `loadClass` delegation population: 21 sites in 12 files

R1's "twelve" was a count of one file. The real population, with the
disposition of the arm that receives the `Err`:

| disposition | n | sites |
|---|---|---|
| propagates | 4 | `classloader.rs:2239` (wrapped in `Some`), `classloader.rs:2830`, `classloader_real.rs:931`, `lang_system.rs:4601` |
| **discriminates on the failure** | 2 | `lang_class.rs:13621` (`resolve_annotation_class_via_loader` — hands the exception OBJECT back), `lang_class.rs:2820` |
| documented best-effort preload, returns `()` | 1 | `lang_system.rs:4555` |
| **swallows ANY failure** | **14** | below |

The fourteen: `cglib_enhancer.rs:4219`, `classloader.rs:3260`,
`classloader_real.rs:1348`, `generics.rs:312`, `jboss_module_loader.rs:2271`,
`lang_class.rs:4380`, `:10022`, `:19469`, `:19796`, `:19928`,
`service_loader.rs:420`, `:648`, `spring_startup_bootstrap.rs:3596`,
`vm/src/runtime/interpreter/constants.rs:1289`.

**One of the fourteen is in a file this lane owns and is fixed; thirteen are
not.** The one that matters most among the thirteen is
`classloader.rs:3260` — it is the *synthetic-mode twin* of the fixed site,
the same `_ => {}` arm on the same parent delegation, and the out-of-file
patch for it is written out below.

### The `class_id_by_name_via_referencing_class` population: 10 sites

| disposition | n | sites |
|---|---|---|
| propagates with `?` | 1 | `apps_h2.rs:2049` |
| **swallows** | 9 | `cglib_enhancer.rs:3945`, `generics.rs:260`, `lang_class.rs:13707`, `:14358`, `:14497`, `:14808`, `lookup_define.rs:248`, `:256`, `test_frameworks.rs:1288` |

Two of the nine (`lookup_define.rs`) are fixed below.

### Fixed, 2026-08-12 — four functions, six arms

| site | was | now | what now escapes |
|---|---|---|---|
| `classloader_real.rs` · `cl_real_load_class_base_rooted`, step 0 | `_ => { parent_user_defined_authoritative_miss = true }` over `Ok(_)` **and** `Err(_)` alike | `Ok(_)` keeps the flag; `Err` goes through `absorb_class_absent` | a parent loader's `LinkageError` / `ExceptionInInitializerError` / `RuntimeException` leaves `loadClass` instead of arriving as `ClassNotFoundException` |
| `classloader_real.rs` · `ucl_real_find_class` | `if let Ok(Some(mirror)) = ctx.load_class(…)` then fall through to `ClassNotFoundException` | `match`, with `absorb_class_absent` on the `Err` | R2's species: a class that IS found and cannot be linked (`ClassFormatError`, `VerifyError`, `UnsupportedClassVersionError`) stops being reported as "not found" |
| `lookup_define.rs` · `resolve_lookup_supertypes` ×2 | `Err(_) => return (None, None)` | returns `Result`, `absorb_class_absent` then the same fall-through | `Lookup.defineClass`/`defineHiddenClass` deliver the supertype's real `LinkageError` rather than defining the generated class against a name-only supertype |
| `classloading/src/class_manager.rs` · `upgrade_synthetic_class` ×2 | `.ok()` on the superclass, `filter_map(… .ok())` on the interfaces | explicit `match`es that `return Err(e)` after releasing `loading_guard` | an upgrade whose supertype cannot be resolved fails instead of installing a layout with `first_field_index = 0` and a short interface list |

The last row is the sharpest of the four and it is **not** a lost-diagnostic
defect — it is the slot-index species
(`docs/architecture/natives-over-real-jdk-classes.md` §5). `superclass_id`
feeds `compute_field_layout` directly, so `None` for a class whose own class
file declares a superclass collapses every inherited field slot. The sibling
that defines the same class from the same bytes,
`define_class_with_options`, already propagates both; this function was the
outlier, exactly as `native_method_get_annotation` was the convention the
original five sites departed from.

### The policy helper

`absorb_class_absent` (`native-builtins/src/classloader_real.rs`,
`pub(crate)`) is `native-api/src/delegated_close.rs`'s `absorb_thrown` shape
with two absorbed roots instead of one, because R1 names two:
`ClassNotFoundException` **and** `NoClassDefFoundError`. The type test is by
`ClassId` hierarchy, never by name. It is not in `delegated_close.rs` because
that module is `native-api`'s and this lane does not own it; if a later lane
moves it there, the two roots and the `InternalError` residual must move with
it.

**The `InternalError` residual, stated rather than hidden.**
`absorb_class_absent` absorbs `MethodCallFailed::InternalError` as well.
`delegated_close.rs` argues it should not — an internal error is not a Java
throwable and no JDK `catch` can name it. It is kept absorbed because it is
*also* the shape `resolve_class_loader_aware` returns for a plain "not on any
classpath entry" miss: its terminal arm is `Err(MethodCallFailed::from(e))`
over a `VmError`, which is the single most common reason a rung legitimately
falls through. Narrowing it before the resolver stops reporting absence as an
internal error would refuse every ordinary miss. That is a real residual and
the next lane should take it from the resolver's end, not from here.

### Blast radius of the one behaviour change that reaches applications

`cl_real_load_class_base_rooted` step 0 fires only when the receiver's
**parent** is a user-defined loader. What newly escapes is any throwable from
that parent's own `loadClass` that is not a `ClassNotFoundException` or a
`NoClassDefFoundError`. Who is likely to catch it:

* **Spring Boot** — `ModifiedClassPathClassLoader` / `ResourcesClassLoader`
  (`@ClassPathExclusions`, `@WithPackageResources`) are the loaders this
  branch was written for and they raise `ClassNotFoundException`, which is
  still absorbed. `ClassUtils.isPresent` catches `Throwable` and answers
  `false`, so `@ConditionalOnClass` is unaffected either way.
* **Quarkus** — `RunnerClassLoader` calls `getParent().loadClass(name)` inside
  `try { } catch (ClassNotFoundException)`. Still absorbed.
* **Tomcat** — `WebappClassLoaderBase` catches `ClassNotFoundException` around
  its parent delegation and rethrows everything else. Same as HotSpot.
* **The at-risk shape** is a loader that raises a `RuntimeException` from
  `loadClass` and relies on a *caller* further out catching only
  `ClassNotFoundException`. On HotSpot that caller does not catch it either,
  so a red here is a divergence being removed, not one being introduced —
  but it will look like a new failure.

`ucl_real_find_class` widens nothing in practice: `ctx.load_class` reports an
absent class as `InternalError`, which is still absorbed.

### Coverage

`regression-suite/src/RLoaderChurnDefine.java` ·
`aParentsFailureIsNotAMiss()` — in `CORE_CLASSES`, so it runs on a default
`run.sh` invocation and in both modes, not only under `SUITE=all`.

The parent is a `ClassLoader` subclass overriding
`loadClass(String,boolean)` — the canonical override point, and the one form
**both** VMs reach (HotSpot's `loadClass` calls `parent.loadClass(name,
false)`; CratonVM's native calls the one-argument form, which routes into the
override through `receiver_overrides_load_class_resolve`). The child is a
**bare `java.net.URLClassLoader`**, per this record's own rule about the shape
a real application builds — Spring Boot's
`PropertiesLauncher.wrapWithCustomClassLoader` is this exact topology and is
named in the delegation native's own comment.

Three checks, and only the first fails on the old behaviour:

1. a parent raising `IllegalStateException` must deliver it; **old behaviour
   delivered `ClassNotFoundException`** and the check names that explicitly.
2. a parent raising `ClassNotFoundException` must still be absorbed and the
   child must still define its own copy from its own URLs — the
   over-correction guard.
3. a parent that resolves normally is still the answer.

The parent counts its own invocations, so a run in which delegation never
happened cannot read green.

**Not covered by any scheduled assertion, and why:** the `ucl_real_find_class`
and `resolve_lookup_supertypes` narrowings both need a class that is *present
and malformed*, which means writing a bad `.class` to disk — this vector
writes nothing and stays deterministic. `upgrade_synthetic_class` needs a
synthetic stub whose real bytes appear later with an unresolvable supertype,
which no fixture in the suite constructs. All three are source-only.

## Out-of-file patches (not applied) — the thirteen this lane could not reach

Owned by other lanes this wave. Listed with the exact replacement text for the
two where the argument is settled; the rest are named with their disposition
so the next lane does not have to re-derive the census.

### 1. `native-builtins/src/classloader.rs:3260` — the synthetic-mode twin

This is the same defect as the fixed site, in the same shape, one file over.
`cl_load_class_base_delegation_inner`'s parent rung:

```rust
            match delegated {
                Ok(Some(Value::Object(Some(mirror)))) => {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
                _ => {}
            }
```

becomes

```rust
            match delegated {
                Ok(Some(Value::Object(Some(mirror)))) => {
                    return Ok(Some(Value::Object(Some(mirror))));
                }
                // W7-26 R1 — the synthetic-mode twin of the narrowing applied
                // to `classloader_real.rs`'s step 0. JDK 25
                // `ClassLoader.loadClass` catches `ClassNotFoundException`
                // around its parent delegation and nothing else; the bare
                // `_ =>` also caught a `LinkageError` and every
                // `RuntimeException` a loader raised, and reported the class
                // as merely absent.
                Ok(_) => {}
                Err(failed) => {
                    crate::classloader_real::absorb_class_absent(&*ctx, failed)?;
                }
            }
```

`absorb_class_absent` is `pub(crate)` in the same crate. The enclosing
function returns `MethodCallResult`, so the `?` type-checks. **Check the
GC-refresh lines immediately below the call before applying** — the
`read_native_pin` refreshes must stay above the `match`, as they already are.

### 2. `native-builtins/src/lang_class.rs:10022` — R2's shape, a wrong-type re-raise

```rust
        let mirror = match loaded {
            Ok(Some(Value::Object(Some(mirror)))) => mirror,
            _ => return Err(isolated_loader_class_not_found(ctx, name)?),
        };
```

Every failure of the isolated loader's `loadClass` — including a
`LinkageError` it raised itself — is re-minted as a `ClassNotFoundException`
naming the class. Replacement:

```rust
        let mirror = match loaded {
            Ok(Some(Value::Object(Some(mirror)))) => mirror,
            Ok(_) => return Err(isolated_loader_class_not_found(ctx, name)?),
            Err(failed) => {
                // W7-26 R2 — a failure is not a miss. Only the
                // class-absent shapes may be re-minted as this loader's
                // `ClassNotFoundException`; anything else the loader raised
                // is the answer and a caller's `catch
                // (ClassNotFoundException)` deliberately does not match it.
                crate::classloader_real::absorb_class_absent(&*ctx, failed)?;
                return Err(isolated_loader_class_not_found(ctx, name)?);
            }
        };
```

### 3–13. The remaining eleven, with dispositions rather than patches

| site | shape | disposition |
|---|---|---|
| `lang_class.rs:4380` (`descriptor_to_class_mirror_via_loader`) | `if let Ok(Some(…))` → `descriptor_to_class_mirror` | R1's named site. Same narrowing; the fallback is documented and must survive an absorbed CNFE. |
| `lang_class.rs:19469`, `:19796`, `:19928` | `_ => None` / `if let Ok(…)` | The `nest_host` / `declared_classes` / `declaring_class_loader_aware` rungs R1 names. Same narrowing. |
| `lang_class.rs:13707`, `:14358`, `:14497`, `:14808` | `.ok()` on `class_id_by_name_via_referencing_class` | Annotation-element resolution. Each ends in a defined sentinel, so the fall-through must survive; only the non-absent throwables should escape. |
| `generics.rs:260`, `:312` | `.ok()` / `if let Ok(…)` | Generic-signature resolution, global fallback below. Same narrowing. |
| `service_loader.rs:420`, `:648` | `if let Ok(…)` | `ServiceLoader` provider resolution; the next rung is a jar scan / `findClass`. Same narrowing. |
| `jboss_module_loader.rs:2271`, `spring_startup_bootstrap.rs:3596`, `cglib_enhancer.rs:3945`, `:4219`, `test_frameworks.rs:1288` | `if let Ok(…)` / `.ok()?` / `_ => false` | Third-party shims. `cglib_enhancer.rs:4219` is a **predicate** (`_ => false`) and needs the caller widened before it can carry anything, so it is the one of the thirteen where the narrowing is not local. |
| `vm/src/runtime/interpreter/constants.rs:1289` | `_ =>` → `impl_jars_load_class` | The Elasticsearch `EmbeddedImplClassLoader` fallback, documented in place. Same narrowing; the fallback must survive an absorbed CNFE. |

### False positives the shape grep produced in this lane's files — do not "fix" these

* `classloading/src/class_manager.rs`, the two
  `let _ = self.loader_constraints.pin/impose(…)` beside the JVMS §5.3.4
  supertype check. **Not a swallow.** Both return
  `Option<LoaderConstraintViolation>`, not a `Result`; the violation is already
  recorded in the table, and `classloading/src/loader_constraints.rs`'s module
  doc states the deferral as a decision ("Fail-closed, but not fail-loud yet
  … turning a violation into a thrown `LinkageError` is a separate, flagged
  step"). This is W7-57's lesson restated: `let _ =` is not the discard.
* `classloading/src/class_path.rs`, ~165 raw hits and **zero** in species.
  Every non-test one is an `Err(_) => continue` over a classpath **entry**,
  which is `URLClassPath$Loader`'s own `catch (Exception e) { return null; }`
  reproduced faithfully, or filesystem/zip I/O. Roughly forty are
  `let _ = fs::remove_dir_all` in `#[cfg(test)]` teardown.
* `classloader_real.rs`'s `init_classloader_common_fields` /
  `init_urlclassloader_fields` — three `if let Ok(…) = ctx.new_object(…)` and
  two `let _ = ctx.invoke(… "<init>" …)`. The documented best-effort
  constructor group: both functions return `()`, and the contract the callers
  rely on is that the field is non-null, which holds on every arm.
* `classloading/src/{loaders,class_origin,resolution,builtin_loaders,loader_constraints}.rs`
  and `native-builtins/src/classloader_value_sidetable.rs` — **0 in species
  between them.** The sidetable's one `invoke_virtual` propagates with `?`;
  the rest are env-var reads, `Option`-shaped cache lookups and `matches!`
  over enums.

## How to re-take this

```sh
# the three vectors, both modes
./target/release/cratonvm.exe --jdk-only --java-home <jdk-25> -cp regression-suite/build RReflect
./target/release/cratonvm.exe --real-jdk --java-home <jdk-25> -cp regression-suite/build RReflect
# the contrast that proves the two faces are one cause
./target/release/cratonvm.exe --jdk-only --java-home <jdk-25> -cp regression-suite/build RJdkJmx
```

`regression-suite/run.sh` supplies `--java-home` for every invocation; a hand
run that omits it measures the host's default JDK and has inverted a per-mode
verdict before (`W7-11`).

---

## Verification pass 2026-08-12 (lane A16) — the two out-of-file patches LANDED

**Nothing was built or run in this pass.** Source read against today's
worktree, with today's anchors.

### 1. The sixteen in-file sites are intact

| claim | today | verdict |
|---|---|---|
| site 1 `native_class_get_declared_annotation` | `lang_class.rs:15423`, `?` on `cached_annotation_proxy_resolving` | present |
| site 2 `native_class_get_annotation`, own-class scan | `:15465` | present |
| site 3 the `@Inherited` superclass walk | `:15487`, inside the `while` | present |
| site 4 `native_field_get_annotation` | `:15971`, `?` on `create_annotation_proxy` | present |
| site 5 `native_annotated_type_get_annotation` | `:21187`, `ladder_rung` on the `annotationType()` dispatch, then a match on the VALUE | present, and the scan-vs-build reasoning is written in place at `:21171`–`:21186` |
| `ladder_rung` itself | `lang_class.rs:20851` | present |
| `absorb_class_absent` | `classloader_real.rs:1146`, `pub(crate)` | present |

**Nothing now depends on the swallow.** The three `Class` sites and the `Field`
site all still fall through to `null` on `Ok(None)` — the "annotation type would
not resolve" case — so the intentional filter the `@ConditionalOnClass` path
needs is untouched; only the `Err` arm moves, exactly as "Which mode moves"
argues.

### 2. Both out-of-file patches under "the thirteen this lane could not reach" are APPLIED

Applied by other lanes, verbatim in shape:

| patch | applied at | note |
|---|---|---|
| #1 `classloader.rs:3260` — the synthetic-mode twin | `native-builtins/src/classloader.rs:3280`–`:3290` | `Ok(_) => {}` + `Err(failed) => absorb_class_absent(&*ctx, failed)?` |
| #2 `lang_class.rs:10022` — R2's wrong-type re-raise | `native-builtins/src/lang_class.rs:10121`–`:10144` | same shape |

**Correction this record owes, and the source itself flags it.** The comment at
`lang_class.rs:10137`–`:10141` says, in as many words, that this record's patch
text names the re-mint as a `ClassNotFoundException` and that it is in fact a
**`NoClassDefFoundError`** — `isolated_loader_class_not_found`
(`lang_class.rs:10174`) allocates `java/lang/NoClassDefFoundError`. **The
argument is unaffected** (a failure is still not a miss, and a caller's `catch`
still deliberately does not match), but "the lie is invisible because
`ClassNotFoundException` is what a loader's caller expects" in R2 should read
`NoClassDefFoundError`. Both are absorbed roots of `absorb_class_absent`, so the
patch text was correct as code and wrong as prose.

### 3. The remaining eleven: measured as still open, not assumed

`absorb_class_absent` has exactly **six** call sites in the whole tree —
`classloader_real.rs:1461`, `:1739`; `lookup_define.rs:272`, `:283`;
`classloader.rs:3290`; `lang_class.rs:10142`. Those are precisely the four
functions "Fixed, 2026-08-12" names plus the two patches above. **So none of the
eleven remaining sites in "3–13" has been narrowed by anyone**, and that is a
count over the policy helper rather than over line numbers, which have rotted.
Their dispositions in that table stand unrevised; re-derive the line numbers
before quoting them.

### 4. What today's probe screen does and does not say about this record

A 33-probe reachability screen was run on the current binary (HotSpot 25 oracle
33/33; `--jdk-only` 28/33).

* **`MXBean`/`ThreadMXBean` PASS under `--jdk-only`.** That corroborates this
  record's own staging note: the `RJdkJmx` row in the reproduction table is
  **pre-`5266bf8c7`** and the carrier refusal it shows is gone. The table is a
  historical measurement of one binary, not a live claim, and should not be
  re-filed as an open defect. **The swallow it exposed is a separate matter and
  is fixed in source, unrun.**
* **`ServiceLoader` iteration PASSES.** This does **not** clear
  `service_loader.rs:420` / `:648`, two of the eleven. Those swallow a
  *non-absent* throwable from a user loader's `loadClass`; a green
  `ServiceLoader` probe over ordinary providers never raises one. Reachability
  is not the defect.
* **`Proxy.newProxyInstance` PASSES**, which is relevant to site 5 only in that
  the JDK dynamic proxy representation the site now reads through the public
  `Annotation` contract is working — it is corroboration, not coverage.

**No claim in this record is contradicted by the screen.** The one claim it
*revises* is the staging of the `RJdkJmx` row, which the record already stated
itself.

### 5. Coverage, restated

`RLoaderChurnDefine.aParentsFailureIsNotAMiss()` is the only scheduled
assertion for any of this, it covers `cl_real_load_class_base_rooted` step 0
only, and the two patches that landed above (`classloader.rs`,
`lang_class.rs:10142`) have **no** scheduled assertion — the synthetic-mode twin
needs synthetic mode and the isolated-loader site needs a present-and-malformed
class on disk. The five annotation sites likewise have no vector that makes the
proxy builder throw; `RReflect` / `RJdkReflect` only prove the non-throwing path
still answers. That is the same gap this record opened with and it is unchanged.
