# In-process javac rejected every `@SuppressWarnings("...")` — "duplicate element 'value'" — FIXED

**Status: ✅ FIXED 2026-07-27 (fix landed on `dev` the same day). Root-caused —
this doc's own stated hypothesis was wrong. Re-verified against the original
probes, against a 66-assertion collection-semantics differential, and against
the two Spring AOT test classes that were its loudest victims. The follow-up it
asked for (re-evaluating `SPRING-TESTCOMPILER.3`) was carried out; that ban
stays — see below.**

## Original symptom

Any source containing a single-string-argument `@SuppressWarnings("...")`
failed to compile via `ToolProvider.getSystemJavaCompiler().run(...)` under
CratonVM, on the very first in-process compilation, with JIT fully disabled:

```
error: duplicate element 'value' in annotation @SuppressWarnings.
```

## Root cause — NOT what this doc hypothesised

The doc guessed that CratonVM's own metadata for `java.lang.SuppressWarnings`
reported the sole `value` element twice (a duplicated synthetic
bridge/accessor). **That is false, and was measured to be false.**
`AnnotationElementProbe.java` enumerates the declared methods of
`SuppressWarnings`/`Deprecated`/`Retention` and of single- and multi-element
custom `@interface`s; its output is byte-identical to HotSpot JDK 25 —
`java.lang.SuppressWarnings.declaredMethods=[value/0]`, exactly one element.

The actual cause was a **collection** defect with no annotation content at all:
`java.util.LinkedHashSet.remove(Object)` deleted the element but returned
`false`. `native_linkedhashset_remove`
(`native-builtins/src/properties_sidetable.rs`, an override that only really
wants `Properties.keySet()` snapshots) sent every *ordinary* `LinkedHashSet` to
real bytecode via `invoke_virtual_bytecode_only`; real `HashSet.remove` is
`return map.remove(o) == PRESENT;`, and this VM's synthetic backing map stores
an `Int(1)` sentinel, never JDK `HashSet.PRESENT`, so that identity comparison
was always false.

javac's `com.sun.tools.javac.comp.Annotate.attributeAnnotation` collects an
annotation type's elements into a `LinkedHashSet` and reports
`duplicate element 'value' in annotation @X` precisely when
`members.remove(method)` answers `false`. Hence *every* annotation with a
`value` element became uncompilable by the in-process compiler — which is what
Spring's AOT `TestCompiler` uses.

Fixed by `try_native_hashset_remove` (new `pub fn` in `native-collections`),
called from all three bytecode-only fallbacks in `native_linkedhashset_remove`.

**Reusable lesson:** a native override that "passes through to real bytecode"
for receivers it does not own is only safe if that bytecode does not depend on
state the synthetic layout fakes. Grep for `invoke_virtual_bytecode_only` when
a collection method returns a wrong *boolean* while its side effect is correct.

## Verification (2026-07-27, worktree `fix/suppresswarnings-dupvalue-20260727`)

Real-JDK mode, `CRATONVM_JAVA_HOME=/data/jdk25-real-20260717/jdk-25.0.3+9`.

| check | before | after |
|---|---|---|
| `NarrowAnnotationProbe` — the 5 isolation cases in the original table | 5/5 FAIL | **5/5 RC=0** |
| `SingleShotAnnotationProbe` — first-ever in-process compile | FAIL | **RC=0** |
| `AnnotationElementProbe` — annotation metadata vs HotSpot | — | **identical to HotSpot** |
| `SetBooleanProbe` — 66 assertions over HashSet/LinkedHashSet/TreeSet `remove`/`removeAll`/`retainAll`/`removeIf`/`addAll`/`iterator().remove()`, plus HashMap/LinkedHashMap/Properties `keySet()` mutation | — | **identical to HotSpot** |
| `JavacConsolidationProbe`, 100 varied compilations | — | **100/100 OK** |
| `AutowiredAnnotationBeanRegistrationAotContributionTests` | — | **14/14** (HotSpot: 14/14) |
| `BeanDefinitionMethodGeneratorTests` | — | **34/34** (HotSpot: 34/34) |

The residual sweep this bug's shape invites — other natives returning a wrong
boolean while mutating correctly — found nothing: `SetBooleanProbe` is clean.

## Recommendation #2, answered: `SPRING-TESTCOMPILER.3` is NOT this bug

The doc suspected that `SPRING-TESTCOMPILER.3`'s documented symptom (a)
(`@SuppressWarnings("deprecation")` present in generated source but `-Werror`
failing anyway) might have been this defect rather than a `ClassFinder.complete`
JIT miscompile, and that the ban's justification should be re-evaluated once
this was fixed. **It was re-evaluated. The answer is no — that ban remains
necessary.**

A/B on one binary carrying an env-gated ban-lift hook (temporary, reverted,
never committed), real JDK, all other bans left active:

| configuration | AutowiredAnn…AotContributionTests | BeanDefinitionMethodGeneratorTests |
|---|---|---|
| all bans active | 14/14 | 34/34 |
| `ClassFinder.complete` un-banned | **9/14** — all 5 `DeprecationTests` fail | **32/34** |

The 5 failures are symptom (a) verbatim: `warnings found and -Werror specified`
against generated source that does carry `@SuppressWarnings("deprecation")`.

`DeprecationSuppressionProbe.java` (new) is a Spring-free standalone
reproducer — it reproduces symptom (a) at iteration ~21 in ~2 minutes.
**The shape matters:** the deprecated type must live in a separate,
already-compiled `.class` file on the classpath. A single-file version
(deprecated member and its suppressed user in one compilation unit) never
reproduces, because javac does not warn at all when
`s.outermostClass() == other.outermostClass()` — such a probe tests nothing.

Narrowing, via `AnnotationEffectProbe2`/`AnnotationEffectProbe3`, which run
extra oracles in the same process the moment the suppression oracle flips.
Measured, post-trip, in the same JVM:

| oracle | expected | observed after the trip |
|---|---|---|
| `@FunctionalInterface` on a 2-abstract-method interface | error | error ✓ |
| `@Override` on a non-overriding method | error | error ✓ |
| `@SafeVarargs` on a non-varargs method | error | error ✓ |
| plain class, no annotations | ok | ok ✓ |
| command-line `-Xlint:-deprecation -Werror` on the failing source | ok | ok ✓ |
| `@SuppressWarnings("deprecation")`, `-Xlint:deprecation`, **no** `-Werror` | ok, no warning printed | ok, no warning printed ✓ |
| `@SuppressWarnings("rawtypes")`, `-Xlint:rawtypes -Werror` | ok | **fails** ✗ |
| unsuppressed cross-unit deprecation, `-Werror` | error | error ✓ |

Three things follow.

1. Annotation **attribution is intact** — every compile-time annotation check
   still fires — so the "the `Annotate` block counter leaks and attribution
   stops" theory is refuted.
2. The loss is **not deprecation-specific**: `@SuppressWarnings("rawtypes")`
   fails the same way, so this is annotation-derived `Lint` generally, not one
   category's warning path.
3. The one oracle that still suppresses correctly differs from the failing
   ones only in **not passing `-Werror`**, reproducibly (3/3 runs, and
   order-independent — `AnnotationEffectProbe3` runs the set both ways). With
   the root cause below in hand this is a consequence, not a cause: whether a
   given compilation trips depends on how many `CompletionFailure`s pass
   through `ClassFinder.complete` before the counter is stuck, and the
   `-Werror` runs drive more of them.

**ROOT-CAUSED 2026-07-28.** Symptom (a) is a JIT-compiled `finally` that
never runs. `[PUTFIELD-WATCH]` on `Annotate.blockCount`
(`CRATONVM_DBG_FIELD_WATCH=Annotate.blockCount`) shows healthy compilations
pairing `blockAnnotations()` with `unblockAnnotationsNoFlush()` and returning
to 0, while the first failing compilation runs five `blockAnnotations()` with
no matching unblock and ends at 4. `ClassFinder.complete` wraps its body in
`try { annotate.blockAnnotations(); … } finally {
annotate.unblockAnnotationsNoFlush(); dependencies.pop(); }`, so a skipped
`finally` leaves annotations blocked and `Annotate.flush()` a no-op for the
rest of that compilation. javac throws `CompletionFailure` through `complete`
constantly while resolving cross-compilation-unit references — which is
exactly why the reproducer needs the deprecated type in a separate `.class`
file.

Reduced to a 3-second, javac-free witness: `try { n++; thrower(); } finally
{ n--; }` in a loop where `thrower` throws every 7th call leaks one count per
throw under JIT and zero under `--nojit` / HotSpot. Only a catch-all
(`catch_type == 0`) leaks; typed `catch`, `catch (Throwable)` and
catch-and-rethrow are unaffected, because a catch-all has nothing but the pc
range to match on. Witnesses committed alongside the others:
`FinallyBalanceProbe`, `FinallyShapeProbe`, `FinallyThrowSiteProbe`,
`CallPathProbe`.

Three independent escape routes; **two fixed 2026-07-28**:

1. **Foreign throw pc.** `JitSignals::athrow_bci` still held the bci the
   *callee's* compiled `athrow` lowering stashed, and the interpreter
   range-checked that foreign pc against the *caller's* exception table.
   Fixed by stamping the invoke's own bci in the post-invoke exception-check
   stub (`JitRuntimeHelpers::set_throw_bci`).
2. **Whole-method re-execution.** When a compiled caller invoked a compiled
   callee that threw, the runtime re-ran the callee from its entry,
   duplicating every side effect the compiled attempt had already performed.
   Fixed by resuming the callee at its handler
   (`interpreter::run_jit_callee_handler`).
3. **Lambda / method-reference dispatch — still open.** A compiled method
   reached via `try_lambda_dispatch` escapes without any drain consulting its
   exception table. `CallPathProbe` isolates it: with both fixes in place
   `STATIC-direct`, `IFACE-class-inline` and `IFACE-class-delegating` are
   clean while `LAMBDA-methodref` and `LAMBDA-body` still leak.
   `ClassFinder`'s completer is `this::complete`, a method reference, so
   javac takes exactly that route — which is why `SPRING-TESTCOMPILER.3`
   stays banned for now.

Also measured, contradicting a common assumption: `ClassFinder.complete` **is**
JIT-compiled despite carrying a non-empty exception table (confirmed with
`CRATONVM_DBG_DUMP_JIT=LIST`), so the "handler-bearing methods are never
admitted" rule of thumb does not hold for it. Of the eight javac-family bans,
only `Types.erasure`, `ClassReader.readAttrs`, `ClassReader.readInnerClasses`
and `ClassFinder.complete` were observed compiling at all in these workloads;
`ClassReader.readInnerClasses` un-banned kills the VM outright on the
consolidation probe.

## Recommendation #3, answered: yes, the impact was as broad as feared

`@SuppressWarnings` is pervasive, and this defect broke *every* annotation with
a `value` element — `@Retention`, any custom `@interface` — not just
`@SuppressWarnings`. It is a plausible contributor to the historical
`CompilationException: Unable to compile source` cluster in the Spring AOT
suites, independent of the JIT bans that were blamed at the time.

## Probes

All at `docs/known-issues/repros/jitban-remaining-20260726/`:
`NarrowAnnotationProbe`, `SingleShotAnnotationProbe` (pre-existing);
`AnnotationElementProbe`, `SetBooleanProbe`, `DeprecationSuppressionProbe`,
`AnnotationEffectProbe2`, `AnnotationEffectProbe3` (added 2026-07-27).
`JavacConsolidationProbe` gained a fifth source kind
(`@SuppressWarnings("deprecation")` under `-Xlint:deprecation -Werror`), which
it previously had to route around because of this bug.
