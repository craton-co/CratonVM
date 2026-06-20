# BUG-06-FAM6 — annotation synthesis (`@AliasFor`/`MergedAnnotation`/`MirrorSets`) value mismatches + a ~2 GB OOM

> **UPDATE 2026-06-20 (branch `fix/bug06-fam6-repeatable-merge`, off dev `be787d86`, rebased onto `5ed6d942`):**
> Three root causes found and FIXED (interpreter, verified == HotSpot JDK 25). `AnnotationUtilsTests` **67→72/72**, `MergedAnnotationsTests` **165→172/178**, `RepeatableContainersTests` 17/17, `AnnotatedElementUtilsTests` 82/82 (no regression).
> 1. **`@Repeatable` (the dominant value-mismatch family, NOT #2 as written below):** `native_class_get_annotations_by_type` only unwrapped the container `if matching.is_empty()` AND never followed `@Inherited`. So `@Foo("A") @FooContainer({@Foo("B"),@Foo("C")})` → `[A]` not `[A,B,C]`, and inherited repeatables → `[]`. Rewrote as JDK `getDirectlyAndIndirectlyPresent` (direct+container merge, declaration order) + `@Inherited` superclass walk; split a no-walk `getDeclaredAnnotationsByType` variant.
> 2. **annotation `hashCode`:** native proxy hashed `Class`/`Enum` members by NAME; JDK uses IDENTITY hash (neither overrides `Object.hashCode`). Disagreed with Spring's synthesized JDK-proxy → equal-but-different-hashCode. → identity. Fixes `hashCodeForSynthesizedAnnotations`.
> 3. **annotation `equals` asymmetry:** native `annotation_proxy_equals` rejected any non-`AnnotationProxy` arg, so `nativeProxy.equals(springSynth)`=false while reverse=true. Added symmetric delegation to `other.equals(proxy)` for a foreign instance of the same type. Fixes `equalsForSynthesizedAnnotations`.
>
> **The "~2 GB OOM" is NOT an allocation bug.** Under JIT, `AnnotationUtilsTests` infinite-recurses `java.util.stream.ReferencePipeline.toArray()` → itself (`invokevirtual toArray(IntFunction)` at pc6 mis-dispatches to no-arg `toArray()`), driven by a GC-guard'd OOB stream slot-probe. JIT-ONLY; interpreter is clean. This is the SAME JIT codegen bug as [[spring-bug-06-mergedannotations-hang]]. Separate from synthesis; OPEN. `CRATONVM_REAL_ANNOTATIONS=1` is NOT a workaround — it breaks JUnit discovery (found=0).
> **Remaining `MergedAnnotationsTests` fails (6):** all in the hierarchy/search-traversal subsystem (bridge method, interface-on-method, enclosing-class predicates, custom annotation filter) — the enclosing-class reflection primitives themselves are conformant — plus `synthesizedAnnotationShouldReuseJdkProxyClass` (needs `getAnnotation` to return a real `$ProxyN`, i.e. real-annotations mode default-on; architectural).

**Severity:** Medium — CV-unique assertion mismatches in Spring's annotation subsystem (`core.annotation` 23 + `context.annotation` 20); one class aborts with an OOM.
**Status:** 🟡 PARTIAL — synthesis value-mismatch + equals/hashCode root causes FIXED (see UPDATE above); residual = JIT toArray recursion (== spring-bug-06) + hierarchy-search traversal. Handoff.
**Mode:** Interpreter (JIT-off).
**HotSpot (JDK 25):** all pass (`AnnotationUtilsTests` 72/72).
**Origin:** family 6 of the bug-06 census; the long-standing annotation cluster `[[spring-bug-01]]`.

## Symptom

Two shapes:

1. **Value mismatches** — `core.annotation` (23) + `context.annotation` (20) assertion
   failures: synthesized-annotation / `@AliasFor` / meta-annotation attribute values differ
   from HotSpot.
2. **OOM abort** — `AnnotationUtilsTests` (HotSpot 72/72) does not assert-fail but **aborts**
   with a fixed allocation:
   ```
   memory allocation of 2127788032 bytes failed   (~2.0 GB)
   ```

## Root cause (narrowed)

**Raw annotation reading is JVMS-conformant** — sub-bugs #0/#1 (raw `@Retention`/element
parsing) are already fixed. The residual mismatches are in **Spring's own synthesis layer**,
i.e. `MergedAnnotation` / `MirrorSets` / `AnnotationTypeMapping` building synthesized proxies on
top of the (correct) raw values:

- **#2** — `@Repeatable` container accessor returns the wrong/duplicated set.
- **#3** — `@AliasFor` mirror sees **empty `{}` defaults** where HotSpot sees the real default
  (so aliased attributes resolve to the wrong value).
- **#4** — annotation-array **rank ClassCast** when an attribute is an array of annotations.

These are CV behaviours that only diverge once Spring's multi-layer meta-annotation merge runs;
each needs ~1 h of in-stack tracing against the specific failing assertion.

### The ~2 GB OOM is a *separate, pre-existing* runaway — NOT a family-3 side effect

The `2127788032`-byte allocation in `AnnotationUtilsTests` reproduces **identically on the
pre-family-3 binary** (i.e. before the `findLoadedClass` fix `4b923e86`), so it is not caused by
any of the recent classloading/extern changes. It is a fixed-size (~2 GB) allocation, which
points at a **bounded-but-wrong size computation** in annotation synthesis (e.g. an attribute
count / array length read from the wrong slot driving an allocation), rather than an unbounded
loop. Distinct from the full-disk incremental-build OOM that produced broken binaries elsewhere
in this session (that one was a build artifact; this one is deterministic VM behaviour).

## Next step

1. **OOM first** (most tractable): run `AnnotationUtilsTests` under allocation tracing / a small
   `-Xmx` so the 2 GB alloc fails fast with a stack, and find which annotation-synthesis call
   computes the ~2 GB size. A wrong length/count read is likely localized and fixable.
2. **Value mismatches**: pick one failing assertion each for #2/#3/#4, trace
   `MergedAnnotation`/`MirrorSets` resolution in-stack vs HotSpot, fix the synthesis step.

## Related

- `[[spring-bug-01]]` — the umbrella annotation cluster this family belongs to.
- Family 5 (`getDeclaredMethod on null`) — sibling reflection-surface bug
  ([bug06-fam5-reflection-getdeclaredmethod-null.md](bug06-fam5-reflection-getdeclaredmethod-null.md)).
- The boxed-primitive-annotation-array bug (kotlin-metadata, `lang_class.rs`) was a *different*
  annotation-array defect, already fixed — check it is not masking #4.
