# `getAnnotation` returned null for "the VM failed"

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

**None required.** Every one of the sixteen changed call sites lives in
`native-builtins/src/lang_class.rs`, and `ladder_rung` is a private helper in
that file. No other lane's file is touched, and no shared API changed shape:
`parameter_erased_type_mirror` is a module-private `fn` with two call sites,
both in the same function.

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
* **R2 — five sites re-raise a failure as the wrong exception type.**
  `link_isolated_method_signatures` turns any `initialize_class` failure into
  `ClassNotFoundException`, which swallows the identity of an
  `ExceptionInInitializerError`; `wf_shim_synth_main_method`'s four sites turn
  any failure into `NoSuchMethodException`. Both are loud, so neither is this
  record's species — but a caller that catches on type is still being lied to.
* **R3 — this sweep covered one file.** The species is defined by a helper's
  return type, not by a subsystem, so the same two scans are worth running
  over the other large native modules. The second scan (invoke sites followed
  by an `Err`-catching arm) found nine of the sixteen fixed sites and none of
  them were visible to the first; a sweep that only greps swallow shapes will
  under-report by roughly half.

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
