# In-process javac miscompiles any source containing `@SuppressWarnings("...")` — "duplicate element 'value'" — real, standalone, single-shot bug

**Status: new, real, not yet root-caused, NOT part of the SPRING-TESTCOMPILER/HIB-STOREDPROC-JIT/TYPES-ERASURE javac-JIT family (confirmed unrelated — see below). Filed for a future session.**

## What happens

Any source file containing a single-string-argument `@SuppressWarnings("...")`
annotation anywhere — even completely alone, with no other annotation, no
loop, no repeated invocation — fails to compile via
`ToolProvider.getSystemJavaCompiler().run(...)` under CratonVM:

```
error: duplicate element 'value' in annotation @SuppressWarnings.
class OnlySuppress { @SuppressWarnings("deprecation") public int x() { return 1; } }
                                       ^
```

The identical source compiles cleanly under real HotSpot JDK 25 with the
same classpath/args.

## Confirmed NOT part of the existing javac-JIT-family bans

This session was investigating whether `TYPES-ERASURE.1` (a single ban on
`com.sun.tools.javac.code.Types.erasure`) subsumes the 7 other
`SPRING-TESTCOMPILER.1-4`/`HIB-STOREDPROC-JIT.1`-family bans (see
[[types-erasure-javac-jit-family-fix-20260726]] and the "NOT yet verified"
consolidation note in `vm/src/jit/skip_list.rs` next to `TYPES-ERASURE.1`).
While building a probe to test that hypothesis with realistic annotated
source content, hit this bug instead — and confirmed it is a **completely
separate, unrelated defect**:

- Reproduces on the **very first ever** in-process `javac` compilation in
  the process (`SingleShotAnnotationProbe.java`) — not after N repeated
  compilations, ruling out the "repeated-compilation JIT-warmup state
  corruption" mechanism common to the whole existing family.
- Reproduces identically with `CRATONVM_DISABLE_JIT=1` (full interpreter
  mode) — **not a JIT miscompile at all**.
- `@Deprecated` is irrelevant to triggering it — `OnlySuppress` (just
  `@SuppressWarnings("deprecation")`, no `@Deprecated` anywhere in the
  file) fails identically. Only `@SuppressWarnings` with a string-literal
  argument matters; a bare `@Deprecated`-only source
  (`OnlyDeprecated.java`) compiles fine.
- Placement doesn't matter — reproduces whether the annotation is on a
  method or on the class itself (`SuppressOnClass`).

## Isolation (all via `NarrowAnnotationProbe.java` / `SingleShotAnnotationProbe.java`, committed at `docs/known-issues/repros/jitban-remaining-20260726/`)

| Source | Result |
|---|---|
| `@SuppressWarnings("deprecation")` alone, no `@Deprecated` | **FAILS** — duplicate element 'value' |
| `@Deprecated` alone, no `@SuppressWarnings` | OK |
| Both, on separate methods | **FAILS** |
| Both, `@SuppressWarnings` method calls the `@Deprecated` one | **FAILS** |
| `@SuppressWarnings` on the class, `@Deprecated` on a method | **FAILS** |

The common factor across every failing case is simply: the source being
compiled contains `@SuppressWarnings("<any string>")` anywhere at all.

## Hypothesis, not yet confirmed

`@SuppressWarnings` is a single-element annotation type
(`String[] value()`), and Java's shorthand syntax `@SuppressWarnings("x")`
is sugar for `@SuppressWarnings(value = "x")`. javac's own `Attr`/`Check`
phases, when validating an annotation application, reflectively inspect
the annotation TYPE's declared elements (`SuppressWarnings.class`'s own
`value()` method) to resolve the shorthand into an explicit
`value = "x"` binding. If CratonVM's own class/method metadata for
`java.lang.SuppressWarnings` (or annotation-element resolution generally)
somehow reports the `value` element twice — e.g. via a native
registration path that duplicates a synthetic bridge/accessor method, or
double-counts the annotation's sole element during shorthand expansion —
javac's `Check.validateAnnotation` would see two `value` bindings and
report exactly this error. This has NOT been confirmed against actual
CratonVM class metadata for `java.lang.SuppressWarnings`; it is a
plausible mechanism only.

## Why this matters

`@SuppressWarnings` is used pervasively across nearly every real Java
codebase. Any tool or test suite that compiles Java source in-process
(Spring's `TestCompiler`/AOT processing, annotation processors, IDE
build tooling, this VM's own javac-family test suites) and whose source
under test contains even one `@SuppressWarnings("...")` annotation will
hit this — independent of, and probably contributing symptoms
indistinguishable from, the existing SPRING-TESTCOMPILER family (in fact
SPRING-TESTCOMPILER.3's own documented symptom (a) is literally
"`-Werror` ... not honored" for a `@SuppressWarnings("deprecation")`
annotation — worth re-examining whether that specific historical symptom
was actually THIS bug rather than a `ClassFinder.complete` JIT miscompile).

## Recommendation for whoever picks this up

1. Root-cause CratonVM's own representation of `java.lang.SuppressWarnings`
   (and annotation elements/shorthand-expansion generally) — check
   whether the annotation-element resolution path (wherever
   `native_class_get_declared_methods`-equivalent logic runs for annotation
   interfaces, or wherever javac's own `Symbol.Completer`/`Attr` reads
   annotation element defaults) double-registers the sole `value` element.
2. Re-examine `SPRING-TESTCOMPILER.3`'s historical symptom (a) — the
   `-Werror`/suppression-not-honored failure in
   `AutowiredAnnotationBeanRegistrationAotContributionTests` — against
   this bug specifically; it may be the same root cause, meaning that
   whole ban's continued justification should be re-evaluated once this
   is fixed.
3. Given `@SuppressWarnings`'s ubiquity, this is plausibly a
   higher-real-world-impact bug than most of the individually-named
   javac-family bans, despite being found incidentally.
