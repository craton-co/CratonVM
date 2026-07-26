# Hibernate `ProxyClassReuseTest.testNoReuse` / Spring Groovy residual cluster - FIXED 2026-07-08

| | |
|---|---|
| **Status** | FIXED / RETIRED 2026-07-08. The original Hibernate `ProxyClassReuseTest` loader-identity bug still passes, and the remaining Spring/Groovy and BeanShell residuals that kept this note active are now closed by the 2026-07-08 residual fixes (`MethodHandle` direct primitive boxing, `guardWithTest` truthiness/arity, `dropArguments` effective-type widening, and synthetic system-module-reader `list()` returning an empty stream instead of throwing). |
| **Area** | VM core — real-JDK-mode class-loader identity + `CONSTANT_Class` resolution (the flat global class store conflated loader namespaces). |
| **Symptom** | `org.hibernate.orm.test.proxy.ProxyClassReuseTest.testNoReuse` fails: `MappingException: Could not instantiate persister … MyEntity`, caused by `IncompatibleClassChangeError: class …MyEntity$HibernateProxy already defined by application loader`. |
| **Severity** | medium (CratonVM-only; pre-existing — fails identically at baseline `b0aab8f9`). Same class as SBR-14 / SC-custom-classloader isolation residuals. |
| **Discovered** | 2026-06-24, triaging the Hibernate suite residuals after the collection-delegation stack-overflow fix (`7b224d8a`). |
| **See also** | [hib-bytecode-enhancement-loader-faithful-linking-FIXED.md](hib-bytecode-enhancement-loader-faithful-linking-FIXED.md) - describes the linking/dispatch layer built on top of this doc's three-layer `CONSTANT_Class`/`defineClass`-namespace/`findLoadedClass` fix. [jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md](../jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md) tracks the separate JIT invokedynamic regression that previously masked this area. |

> **RETIRED 2026-07-08:** current `dev` plus this residual-fix branch closes the
> active tails. Final Azure binary:
> `/data/data/cratonvm-probe-bins/cvproxresid-20260708-143410-final-9983ff83`.
> `ProxyClassReuseTest` reports `found=3 started=3 ok=3 failed=0`.
> `GroovyBeanDefinitionReaderTests` reports `found=36 succ=36 fail=0 skip=0`.
> `BshScriptFactoryTests` reports `found=18 succ=18 fail=0 skip=0`.
> The formerly failing `contextComponentScanSpringTag` method passes in
> isolation, `classpath*:org/springframework/context/groovy/**/*.class`
> resource scanning returns 21 resources, and the synthetic system module
> reader no longer throws while Spring scans module-path resources. The related
> Hibernate `InPredicateTest` run also passes under default JIT
> (`ok=1`, 34.760s) with no `DomainParameterXref.removeEldestEntry` NSME.

## 2026-07-08 retirement - residuals closed

Validation used the unique Azure binary
`/data/data/cratonvm-probe-bins/cvproxresid-20260708-143410-final-9983ff83`
built from the final residual branch head (`9983ff83`) in a separate detached
build worktree.

- Hibernate `ProxyClassReuseTest`: 3/3 passed
  (`/tmp/proxresid-20260708-143410-final-9983ff83/proxy.log`).
- Spring `GroovyBeanDefinitionReaderTests`: 36/36 passed in the shared Linux
  fixture
  (`/tmp/proxresid-20260708-143410-final-9983ff83/groovy-full.log`).
- Spring `BshScriptFactoryTests`: 18/18 passed
  (`/tmp/proxresid-20260708-143410-final-9983ff83/bsh-full.log`).
- Focused Spring namespace/component-scan probe:
  `contextComponentScanSpringTag` passed, and direct resource scanning for
  `classpath*:org/springframework/context/groovy/**/*.class` returned 21
  resources
  (`spring-method-contextComponentScanSpringTag.log`,
  `resourcescan-cv.log`).
- Reduced Groovy/MethodHandle probes passed: `Map.containsKey` indy returns
  boxed `Boolean` values (`true`/`false`), and namespace markup emits the
  expected `<context:component-scan .../>` tag (`mapcontains.log`,
  `groovyns.log`).
- Related Hibernate `InPredicateTest` pass: default-JIT run completed
  `found=1 started=1 ok=1 failed=0` in 34.760s with no
  `DomainParameterXref.removeEldestEntry` NSME
  (`/tmp/proxresid-20260708-143410-final-9983ff83/inpred.log`).

The live root causes were not new loader-blind class resolution defects:
`MethodHandle` adapters were leaking raw primitive direct-call results into
object-return chains, `guardWithTest` treated boxed `Boolean.FALSE` as truthy
and forwarded too many arguments to prefix guard handles, `dropArguments`
computed widening from the raw target descriptor instead of the effective
handle type, and Spring's resource scan hit CratonVM's synthetic
`SystemModuleReader.list()` implementation, which threw before classpath
resources could be consulted. Those fixes retire the residuals that kept this
document in `../../../known-issues`.

> **RETRY 2026-07-01:** Re-ran the focused builtin-loader reverse-pollution coverage on
> current `dev`: `cargo test -p cratonvm-native-builtins test_builtin_find_loaded_class -- --nocapture`
> passed both cases (`hides_user_namespace_hit`, `keeps_application_namespace_hit`). A broader
> `cratonvm-vm` custom-loader test build did not reach execution because MSVC link failed with
> insufficient disk space while producing debug test binaries. The gitignored Hibernate app
> fixture is not present in this worktree, so `ProxyClassReuseTest` / BeanShell app-level reruns
> were not retried here.

## Hibernate app-gauntlet soak (2026-07-04) — the validation this doc called for

The gate flipped default-on 2026-07-03 (see the `context.groovy` section below)
explicitly on a narrow slice (3 target classes + a handful of `scripting.bsh`/
`scripting.groovy` classes), flagging that Hibernate/Tomcat/WildFly
custom-loader-heavy suites still needed a broader soak before full confidence.
That soak: full Hibernate ORM 8.0 suite, 4548 classes, real-JDK JIT-on, gate
on, TIMEOUT=600s, Linux (Azure host, dev `81a31c08`+).

- `ProxyClassReuseTest` (this doc's original bug): **3/3 PASS**.
- Full suite: **PASS 4293/4548 (94.4%)**, FAIL 133, CRASH 17, HANG 8, ABORTED 3.
- Diffed against the known non-passed baseline and filtered for
  already-documented pre-existing clusters (`bytecode.enhancement`/`lazytoone`,
  jar-scanning, temporal-GC, the OSR-vtable-dispatch family): the residual
  ~62 classes are the same pre-existing bugs independently root-caused
  elsewhere this session — **no evidence of the gate turning any
  previously-passing class into a failure**.
- The two classes flagged as gate-sensitive in earlier same-session testing
  (`bytecode.enhancement.basic.{InheritedTest,MappedSuperclassTest}`) were
  never passing gate-off either; gate-on changes their failure mode from a
  hard native CRASH (rc=139) to ABORTED — a safety improvement, not a new
  regression, though still not a clean pass (tracked under the
  `fix/lazy-enhancement-gate` line of work, not this doc).

**Conclusion:** default-on holds under the Hibernate custom-loader-heavy
suite. See `vm/src/runtime/env_cache.rs::loader_aware_resolution` for the
updated doc comment recording this validation.

## Manifestation — Spring `context.groovy` (2026-07-03)

Three CratonVM-unique failures in `spring-context`'s `context.groovy` package
(`GroovyApplicationContextTests`, `GroovyApplicationContextDynamicBeanPropertyTests`,
`GroovyBeanDefinitionReaderTests`), all failing with e.g.
`NoSuchBeanDefinitionException: No bean named 'framework' available` even
though the Groovy config script visibly declares that bean and no exception
is thrown while loading it.

### Primary root cause (fixed) — `MethodHandles` invoker adapters were inert stubs

Not the loader-blind-resolution bug this doc otherwise describes. Spring's
`GroovyBeanDefinitionReader` uses Groovy's `BeanBuilder` DSL
(`beans { framework String, 'Grails' }`), which Groovy dispatches dynamically
(`methodMissing`/`invokeMethod`) through `invokedynamic` call sites bootstrapped
via `org.codehaus.groovy.vmplugin.v8.IndyInterface`. `IndyInterface.<clinit>`
builds exactly one `MethodHandles.exactInvoker(methodType(Object.class,
Object[].class))` handle (`CACHED_INVOKER`), and **every** generic
(non-special-cased) Groovy indy call site dispatches through it. CratonVM's
`exactInvoker`/`invoker`/`spreadInvoker` natives returned a `MethodHandle`
with no `MH_KIND` set — an inert stub — so `mh_dispatch` silently no-opped on
invocation. Every Groovy dynamic-DSL closure body therefore never actually
ran: `beans { ... }` "loaded" with no error and registered zero beans.

**Fix:** added `MH_KIND_INVOKER` and its dispatch semantics (invoke the first
argument as the real target, forwarding the rest — spreading the tail into an
array for `spreadInvoker`) in `../../../../native-builtins/src/lang_invoke.rs`. Shipped
alongside two independent classloader-hygiene fixes found investigating the
same cluster (both unconditional, not gated):

- `../../../../native-builtins/src/classloader.rs` `get_or_assign_loader_id`: in real-JDK
  mode CratonVM's synthetic `CL_LOADER_ID` slot index aliases the REAL
  `java.lang.ClassLoader.classes` field (confirmed via `javap` against JDK 25)
  — writing a loader id there clobbered that field with a bare int. Now
  delegates to the existing mode-aware `loader_namespace_id` helper instead.
- `../../../../classloading/src/class_manager.rs` `get_loaded_class_id`/`find_class_by_name`:
  the custom-loader fallback returned the *first* same-named class found
  across an unordered loader set, unsound when 2+ user-defined loaders each
  have their own distinct class under an identical name (the common case for
  Apache Groovy, which names closure literals positionally per script —
  `$_run_closure1`, `$_run_closure2`, … — so two different scripts routinely
  define two different classes sharing a name). Now returns `None` (ambiguous
  is a miss, not a guess) instead of guessing, when 2+ loaders' copies are
  found. Only changes behavior when a real name collision across loaders
  exists; the common single-custom-loader case (Tomcat/Hibernate/WildFly) is
  unaffected.

### Residual B FIXED (2026-07-06) — `invokestatic` self-calls re-resolved their own class by name

Re-verified on current `dev` tip (2026-07-06): `GroovyApplicationContextTests`
was ALREADY fully green (4/4, matching HotSpot) before any new fix — better
than this doc's 2026-07-03 snapshot ("3/4 methods fail even in isolation"),
apparently improved by unrelated loader-identity work that landed since. That
snapshot is now stale; `GroovyApplicationContextTests` is not part of the open
residual anymore.

`GroovyBeanDefinitionReaderTests` was still broken. Root-caused with a minimal
reproducer (`KRunMethod` harness running exactly two real Spring test methods,
`simpleBean` then `beanWithFactoryBean`, in one JVM — each a fresh
`GroovyShell`/`GroovyClassLoader` producing its own `beans$_run_closure1`):
the SECOND method's closure threw `NoSuchMethodError` naming a synthetic
accessor (`$get$$class$...`) that only the FIRST method's closure class
declares — order-independent, whichever script runs second fails, always
referencing the earlier script's accessor name.

**This is a different bug from the `CONSTANT_Class`/`defineClass`/
`findLoadedClass` triage this doc otherwise covers.** The Groovy-generated
synthetic class-literal-cache accessor (`$get$$class$Foo()`, a private
static helper) is called via a plain `invokestatic` SELF-call — the
closure's own `doCall` calling a method on its own class. `execute_invokestatic`
(`../../../../vm/src/runtime/interpreter.rs`) resolved the call's owner class purely by
the constant-pool NAME string, via the same loader-blind `get_loaded_class_id`
this doc's "Known remaining limitation" section already flags — even though
the executing frame's `ClassId` (`current_class_id`) is already known and IS
the correct answer for a self-call. `try_stackless_invoke` already had a
`dispatch_class_override: Option<ClassId>` parameter built for exactly this
class of bug (used by the `invokevirtual`/`invokespecial` paths — receiver
identity, and `lookup_loader_initiated`'s initiating-loader semantics,
respectively) but `execute_invokestatic` was the one caller that always
passed `None`, so a same-named self-call fell all the way through to the
global, loader-blind name lookup — which silently returns whichever
same-named class the flat store happens to hold when only ONE of the two
colliding classes has been probed/registered so far (not an ambiguous-miss
case, since both real classes are genuinely loaded — just the wrong one for
this frame).

**Fix** (`a7790a91`, `vm/src/runtime/interpreter.rs::execute_invokestatic`):
when the invokestatic's constant-pool owner name textually matches the
current frame's own class name, resolve straight to `current_class_id`
(definitionally already loaded/linked/initialized) and thread that `ClassId`
through as `try_stackless_invoke`'s existing `dispatch_class_override`
parameter, plus route the `invoke_or_native` recursive-fallback path through
`invoke_on_class_shared(current_class_id, ...)` instead of its internal
name-based lookup. No other invokestatic call site is touched — a real call
to a genuinely different class keeps the exact existing behavior.

**Verification (initial, against pre-existing `dev` tip `9f1db39d` before this
session's other concurrent gate-default change described below landed):**
- Two-method repro (`simpleBean` + `beanWithFactoryBean`, one JVM): FAIL → PASS.
- All 30 `GroovyBeanDefinitionReaderTests` methods that don't hit the
  separate `component-scan` hang (see below), run together in ONE JVM
  (matching real Spring/JUnit usage — the scenario this doc's numbers are
  measured against): **23/30 PASS** (up from ~5-7/36 in prior snapshots of
  this doc). The 7 that still fail show two DIFFERENT, deeper failure modes
  not touched by this fix (Groovy-compiler-internal state reuse across
  scripts — see "Follow-up: Groovy compiler-state reuse" below).
- `GroovyApplicationContextTests`: still 4/4, no regression.
- `cratonvm-vm` unit suite: 2129 passed (9 pre-existing `lock_order` failures
  are a `--release`-vs-`--debug` test-harness expectation mismatch, unrelated,
  reproduce identically on pre-fix code).
- Per-method isolation (one JVM per test, the shape that can't reproduce the
  cross-script collision at all): baseline 29/36 pass excluding the 6 hangs,
  fix 30/36 — the fix also incidentally corrected `beanWithParentRef`, which
  hit the same self-call pattern within a single script.

**IMPORTANT — re-verified against the ACTUAL current `dev` tip after rebasing
(2026-07-06, later same session):** while this fix was in progress, a
DIFFERENT concurrent session independently found and merged (`30e82560`/
`7367ac9c`, "Fix CRATONVM_LOADER_AWARE_RESOLUTION gate lock-step drift
disabling Hibernate enhancement fixes") the EXACT SAME stale-mirror-gate bug
described in the "Residual A" section below — flipping both `native-builtins`
and `classloading`'s copies of the gate to default-on, validated against the
Hibernate bytecode-enhancement suite (PASS 40→86/131). That fix is now
`dev`'s default behavior. Rebasing this doc's `invokestatic` fix onto that
tip and re-running the SAME 30-method `GroovyBeanDefinitionReaderTests`
batch shows a MATERIALLY WORSE starting point than the pre-gate-fix numbers
above: **without** this doc's `invokestatic` fix, current `dev` tip
(`d14d2ff3`, gate-mirrors-on) crashes outright partway through the batch
(`class file error: class not found: beans$_run_closure1` — a HARD failure,
not a soft per-test failure) — i.e. the OTHER session's gate fix, though
correctly validated against Hibernate, introduces a live regression against
this Groovy scenario that was not caught by that session's own (Hibernate-
only) validation. **With** this doc's `invokestatic` fix layered on top of
that same tip, the batch completes (no crash) at **15/30 PASS** — a real,
measurable improvement over "crashes before printing a single result," but
markedly worse than the 23/30 this fix achieved against the OLDER
pre-gate-fix baseline. The 15 failures on the combined tip show THREE
different symptoms: the `startup failed`/`Should never happen`/
`beans$_run_closure1` "class not found" patterns already described above,
PLUS a new one, `Class.isAssignableFrom: argument is null`, not previously
seen and not investigated further (out of scope / time for this session,
but worth flagging: it appeared only once the OTHER session's gate-mirror
fix was combined with this one, so it is plausibly yet another facet of the
same loader-identity-vs-Groovy-compiler-state interaction, not necessarily
a new independent bug).

**Net effect of shipping this fix on top of current `dev`:** turns a hard
crash into a partial pass (0 useful results → 15/30), a genuine improvement,
but `GroovyBeanDefinitionReaderTests` is NOT close to fully fixed on current
`dev` tip and needs more work than originally estimated from the
pre-gate-fix numbers above. Given the now-confirmed regression from the
OTHER session's already-merged gate-mirror fix, whoever picks up the
"Follow-up" items below should re-baseline against current `dev` tip first,
not against the older numbers earlier in this section.

### Follow-up: Groovy compiler-state reuse across scripts (open, deeper than Residual B)

Running all 30 non-hanging `GroovyBeanDefinitionReaderTests` methods together
still leaves 7 failing, with error text that is NOT a CratonVM
`NoSuchMethodError`/`ClassCastException` pattern but Groovy's OWN compiler
diagnostics, e.g.:

```
beans: 12: The current parameter list already contains a parameter of the name bean
 @ line 12, column 28.
```

and a bare `Should never happen` (an internal Groovy AST/compiler assertion
message) for other methods. This means some of Groovy's OWN compiler-internal
state (AST parameter-list tracking, or similar) is being incorrectly shared
or not fully reset **between separate `GroovyShell.parseClass` invocations**
— a different bug from both this doc's classloader-identity mechanism and
the `invokestatic` self-call fix above. Not investigated further in this
session (would need tracing Groovy's own compiler pipeline, e.g.
`org.codehaus.groovy.control.CompilationUnit`/`SourceUnit` state, and
whatever CratonVM native shims or reflection paths those depend on — no
CratonVM code was identified as the culprit before time ran out on this
follow-up). Flagged here as the next concrete step for closing out
`GroovyBeanDefinitionReaderTests` fully.

### Separate pre-existing hang: `component-scan` / XML-namespace Groovy DSL (open, unrelated)

6 of the 36 `GroovyBeanDefinitionReaderTests` methods —
`contextComponentScanSpringTag`, `useSpringNamespaceAsMethod`,
`useTwoSpringNamespaces`, `springAopSupport`, `springScopedProxyBean`,
`springNamespaceBean` — hang indefinitely (confirmed via per-method
`KRunMethod` isolation with a 15-30s timeout) **identically before and after**
the `invokestatic` fix above, and identically on the un-fixed baseline. All
six use the Groovy DSL's `xmlns context:"…"` / `context.'component-scan'(...)`
namespace-tag mechanism (`GroovyDynamicElementReader`), which likely performs
a real classpath/directory scan. Not triaged further — flagged as a separate,
pre-existing bug outside this doc's scope (not loader-identity-related as far
as this session went).

**2026-07-08 interim recheck, superseded later the same day:** the same cluster no longer presents as a hard hang in
the shared Linux Spring fixture, but it was not yet fixed. HotSpot passes the full
`GroovyBeanDefinitionReaderTests` class (`found=36 succ=36 fail=0`). CratonVM
with `--nojit` and the same classpath completes in about 112s but reports
`found=36 succ=30 fail=6`, with the namespace-tag methods still failing:
`useSpringNamespaceAsMethod`, `useTwoSpringNamespaces`, `springAopSupport`,
`contextComponentScanSpringTag`, `springScopedProxyBean`, and
`springNamespaceBean`. The logged signatures
are `Namespace prefix: aop is not bound to a URI` for the AOP namespace cases
and `ArrayIndexOutOfBoundsException` in Groovy's indy selector path for the
component-scan/scoped-proxy path:

```
org.codehaus.groovy.vmplugin.v8.Selector$InitSelector.getMetaClass(Selector.java:406)
org.codehaus.groovy.vmplugin.v8.Selector$MethodSelector.setCallSiteTarget(Selector.java:1055)
org.codehaus.groovy.vmplugin.v8.IndyInterface.fallback(IndyInterface.java:401)
groovy.xml.StreamingMarkupBuilder$_bind_closure7.doCall(StreamingMarkupBuilder.groovy:249)
org.springframework.beans.factory.groovy.GroovyDynamicElementReader.invokeMethod(GroovyDynamicElementReader.java:111)
```

The same CratonVM run emitted guarded heap-integrity diagnostics before the
per-test failures:

```
gen_heap::get_field: out-of-bounds field read dropped ... class_name=java/lang/Object
gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) ... Heap reference-integrity defect (see HIB-CV-32).
```

So this was no longer best described as only a timeout, but this interim run
was not yet fixed. The later 2026-07-08 retirement run at the top of this
archive supersedes this snapshot: the full class passed 36/36, the focused
component-scan method passed, and the document moved out of
`../../../known-issues`.

### 2026-07-03 snapshot (historical — superseded by the above)

Original text describing the pre-fix state, kept for history:

Even with the fixes above, `GroovyApplicationContextDynamicBeanPropertyTests`
is now fully green (2/2, byte-for-byte HotSpot match), but
`GroovyApplicationContextTests` still fails 3/4 methods **even run in total
isolation** (fresh JVM, single class), and `GroovyBeanDefinitionReaderTests`
improved from complete failure to 6/36.

Root cause: this is the SAME loader-blind-resolution family this doc
documents, one level removed. Two Groovy scripts (or two test *methods*
within one class/JVM, since JUnit's method execution order is unspecified)
each compile a positionally-named closure via their own fresh
`GroovyClassLoader` instance. The global flat class store can resolve a
later script/method's `new`/`checkcast` reference for its own closure to an
EARLIER script's same-named closure class — the closure silently runs the
wrong compiled body, registering the wrong (or zero) beans, with no
exception. Confirmed via a minimal repro (two `GroovyShell.evaluate(script,
"beans")` calls, same script `name`, different closure bodies): with
class-constant resolution loader-blind, `inner1.getClass() ==
inner2.getClass()` was `true` and the second closure's `call()` ran the
first's body.

**A `GroovyClassLoader`-type-scoped attempt was tried and reverted** — gating
`resolve_class_loader_aware`'s enhanced path on `loader_aware_resolution() ||
is_groovy_class_loader(defining_loader)` (a real subtype check against
`groovy.lang.GroovyClassLoader`, not a name-pattern match) *does* fix class
identity in isolation (`c1.getClass() != c2.getClass()` becomes correctly
`false`... i.e. distinct, matching HotSpot), confirmed via a standalone
`ClosureNameCheck.java` repro. But applied to the actual test suite it made
results WORSE (`GroovyApplicationContextTests` 1/4, `DynamicBeanPropertyTests`
1/2, `GroovyBeanDefinitionReaderTests` 3/36 — down from 1/4, 2/2, 6/36),
because a SEPARATE, deeper bug surfaced: even with two closures now correctly
distinct objects, `Closure.call()` → `getMetaClass().invokeMethod(this,
"doCall", args)` — real Groovy bytecode/reflection machinery
(`MetaClassImpl`, `ClassInfo`, `CachedClass`) — still routed the call to the
WRONG compiled body. Fixing class identity without also fixing this
MetaClass/dispatch-layer bug made the wrong-body-dispatch bug fire in MORE
cases than the accidental identity-collapse had been coincidentally masking.
This attempt was reverted; `../../../../vm/src/runtime/interpreter.rs` is unchanged by
the shipped fix.

**Flipping this doc's `CRATONVM_LOADER_AWARE_RESOLUTION` global default was
considered and explicitly rejected** for this bug cluster — this doc's own
bar ("Full app gauntlet … must be soaked with the gate ON … until then the
gate stays default-off") was not met (only the 3 target classes plus a
handful of `scripting.bsh`/`scripting.groovy` classes were checked, nowhere
near Tomcat/Hibernate/WildFly), and the checked slice already showed a
regression from flipping it (`GroovyBeanDefinitionReaderTests` 6/36 → 5/36,
a NEW `NoSuchMethodError` on Groovy's synthetic `$get$$class$...`
class-literal-caching accessor). `../../../../vm/src/runtime/env_cache.rs`'s default is
unchanged by the shipped fix.

**Follow-up scope** (not attempted here): the Groovy `MetaClass`/`ClassInfo`/
`CachedClass` dispatch-layer bug is a separate investigation from this doc's
classloading-resolution mechanism — likely another instance of "resolves by
name instead of by identity," one level up in Groovy's own runtime reflection
layer rather than CratonVM's class store. Fixing it, plus doing the actual
full app-gauntlet soak this doc calls for, would likely close out the
residual failures in both `GroovyApplicationContextTests` and
`GroovyBeanDefinitionReaderTests`.

## Fix (2026-06-24) — gated, three interacting layers

Triage (via `.scratch-loader/probe/IsoProbe.java` + `IsoProbe2.java` + `LoadProbe.java`)
found the loader-blindness is **not one bug** but three interacting real-JDK-mode
defects. All three are corrected only when `CRATONVM_LOADER_AWARE_RESOLUTION` is set
(default off → byte-identical legacy behavior):

1. **`CONSTANT_Class` resolution was loader-blind** (the originally-reported layer).
   `ldc X.class` / `new` / `checkcast` / `instanceof` / `anewarray` (and field-owner
   resolution) called `SharedVm::load_class_concurrent(name)` — a flat global lookup
   that ignores the referencing class's defining loader. **Fix:** new
   `resolve_class_loader_aware` (vm `interpreter.rs`) — for a reference reached from a
   class defined by a *user-defined* loader, resolve through that loader as the JVMS
   §5.4.3 *initiating* loader by invoking its `loadClass` (re-entrant, with an
   in-flight guard + a per-`(loader,name)` `initiating_resolution_cache` on `SharedVm`).
   Built-in-loader / JDK-namespace / array references keep the global fast path.

2. **First user-loader define landed in the Application namespace.** `defineClass1` /
   `defineClass0` (`../../../../native-builtins/src/lang_system.rs`) only gave a user loader its
   own namespace *when the class name was already loaded* (the "override-first
   redefinition" heuristic). So the **first** definer of any name always got
   `loader_id = 0` (Application) — two isolating loaders that each define `MyEntity`
   could not both be distinct from the app copy. **Fix:** when the gate is on, *every*
   user-loader define gets its own stable namespace (`loader_namespace_id`), not just
   on collision.

3. **`findLoadedClass` fell back to the global store.** `find_loaded_class_for_loader`
   (`../../../../native-builtins/src/classloader.rs`) probed the loader's own namespace via
   `class_id_by_name_and_loader`, which **delegates to `find_class_by_name` (which scans
   every user loader) on a miss** — so a fresh loader's `findLoadedClass` returned
   *another* loader's class, short-circuiting its `findClass`. **Fix:** gate-on uses a
   new exact `(loader,name)` probe (`ClassManager::class_defined_by_loader_exact` →
   `NativeContext::class_id_defined_by_loader_exact`) with no global fallback.

Acceptance (HotSpot-matched): `IsoProbe` (ldc) and `IsoProbe2` (new/instanceof/
checkcast) both **PASS** gate-on, **FAIL** gate-off (legacy), `NormProbe` (a normal
*delegating* custom loader) **PASS** both ways. classloading (527) + native-builtins
(2707) unit tests green.

### Known remaining limitation

`LoadProbe` still fails its `t1 != tApp` check: `Class.forName("X")` from the
*application* context returns a *child* loader's `X` because the global
`ClassManager::get_loaded_class_id` scans `user_loaders` on a builtin-loader miss
(reverse-direction pollution). Not required for `ProxyClassReuseTest` /
`IsoProbe`; fixing it means making the central global lookup reject cross-loader
hits, which has gauntlet-wide blast radius — deferred.

### Bar to flip the default on / merge confidently

Full app gauntlet (Tomcat / Spring / Hibernate / WildFly — all heavy custom-loader
users) must be soaked with the gate ON, since layers 2 & 3 change loader-identity
attribution for *every* user-defined loader. Until then the gate stays default-off
and `dev` is unaffected.

## Symptom

`testNoReuse` creates two **isolated** class loaders (`IsolatingClassLoader`, which
isolates `org.hibernate.orm.test.proxy.*`), builds a `SessionFactory` under each,
and asserts the two ByteBuddy entity proxies are **distinct** classes living on
their respective loaders:

```java
ClassLoader cl1 = new IsolatingClassLoader( isolatedClasses );
ClassLoader cl2 = new IsolatingClassLoader( isolatedClasses );
Class<?> proxyClass1 = withFactory( proxyGetter, null, cl1 );
Class<?> proxyClass2 = withFactory( proxyGetter, null, cl2 );
assertNotSame( proxyClass1, proxyClass2 );
```

On CratonVM the second proxy define collides:

```
Caused by: IllegalArgumentException: Lookup.defineClass:
  Linkage(IncompatibleClassChangeError {
    message: "class …ProxyClassReuseTest$MyEntity$HibernateProxy already defined by application loader" })
```

The sibling `testReuse` / `testReuseWithDifferentFactories` (which use the app
loader and *expect* reuse) pass. Only the isolated-loader case fails.

## Root cause — NOT loadClass-override; it is class-constant resolution

`loadClass`-override isolation itself **works** on CratonVM. Two minimal probes
confirm two isolated loaders produce distinct, correctly-attributed classes both
via a direct `loadClass` and via `Class.forName(name, false, cl)`.

The real defect is one level deeper: a `CONSTANT_Class` reference inside bytecode
loaded by a custom loader (`ldc MyEntity.class`, `new`, `checkcast`, method/field
resolution, …) is resolved through the **flat global / application class store**,
not through the **defining loader of the class that holds the reference**. So even
when a class is correctly isolated, the class *constants inside it* resolve to the
application-namespace copy.

Minimal reproducer (kept in `.scratch-hhsf/IsoProbe3.java`): two isolated loaders
each load a `Holder` whose method returns `Target.class`.

```java
Holder@c1.get()  →  Target.class   // resolved through Holder's loader
Holder@c2.get()  →  Target.class
```

| | `Holder@c1.get()` | `Holder@c2.get()` | `t1 == t2` |
|---|---|---|---|
| **HotSpot** | `Target@c1` | `Target@c2` | `false` |
| **CratonVM** | App-loader `Target` | App-loader `Target` | **`true`** |

In `testNoReuse` this means `cl1`'s `MyEntity` isolates correctly (instrumentation:
a user-defined namespace), but `cl2`'s `MyEntity.class` constant collapses to the
**application** `MyEntity` (the copy already loaded by the `testReuse` methods).
ByteBuddy then defines `MyEntity$HibernateProxy` under the application namespace
for *both* isolated factories → the second define hits the duplicate-define guard
(`class_manager.rs`, keyed by `(loader_id, name)`).

## Why the targeted `Lookup.defineClass` fix did not help

`Lookup.defineClass([B)` already inherits the lookup class's loader namespace
(`lookup_define.rs::lk_define_class_b` → `inherit_lookup_loader`). But because the
lookup class itself (`MyEntity` as seen by `cl2`) has already resolved to the
application namespace, the proxy correctly inherits *that* (application) namespace
and still collides. A first attempt to derive the namespace in
`classloader.rs::lk_define_class` was doubly ineffective — that function is a
**dead path** (overridden at registry layer by `lookup_define.rs`, which is
registered last) **and** it cannot help while class constants resolve loader-blind.
It was committed (`1deffb7d`) and then reverted (`b1e5e6c7`).

> Pitfall recorded here: there are **two** `MethodHandles.Lookup.defineClass([B)`
> implementations — `native-builtins/src/classloader.rs::lk_define_class` (dead)
> and `native-builtins/src/lookup_define.rs::lk_define_class_b` (live, registered
> last). Only edit the live one.

## Manifestation — Spring `scripting.bsh.BshScriptFactoryTests` (2026-07-01)

Same family; a concrete instance of the **"Known remaining limitation"** above
(builtin-loader `findLoadedClass` / `get_loaded_class_id` scanning `user_loaders`).
3 methods fail (`staticPrototypeScript`, `resourceScriptFromTag`,
`nonStaticPrototypeScript`) with `ClassCastException: MyMessenger cannot be cast to
org.springframework.scripting.ConfigurableMessenger` (or `$Proxy29 …`).

Two BeanShell scripts declare the **same class name `MyMessenger`**:
`MessengerInstance.bsh` (`implements Messenger`) and `MessengerImpl.bsh`
(`implements TestBeanAwareMessenger`). `ScriptFactoryPostProcessor` eagerly evals
*all* script beans (even lazy ones) to predict types, so both run. On HotSpot each
bsh interpreter defines its `MyMessenger` into a distinct per-interpreter
`BshClassLoader`; on CratonVM the **first** define lands in the Application namespace
and the **second** interpreter's `findLoadedClass("MyMessenger")` on the app loader
**reuses** it (`find_loaded_class_for_loader`'s guard only skips generated `$Proxy`
names, not bsh classes), so the second class — with the correct interface — is never
generated. Only one `MyMessenger` is ever defined.

Confirmed the **gate alone is insufficient here**: `CRATONVM_LOADER_AWARE_RESOLUTION=1`
isolates the first define to `UserDefined(N)` but the bug persists, because
`find_loaded_class_for_loader` for a *builtin* requesting loader still returns the
user-loader's class via the global walk — the reverse-direction pollution flagged in
"Known remaining limitation". Complete fix = per-loader-namespaced resolution **plus**
a `find_loaded_class_for_loader` correction (broadening its `is_generated_proxy_name`
guard without hiding ByteBuddy/Hibernate/CGLIB/Mockito app-namespace classes — the
same conflict this doc is about). Standalone seconds-fast repro:
`ClassPathXmlApplicationContext("bshContext.xml")` run directly on `cratonvm.exe`
(HotSpot baseline needs `--add-opens java.base/java.lang=ALL-UNNAMED` for bsh's
reflective `defineClass`).

2026-07-01 update: implemented and unit-tested the builtin-loader reverse-pollution
half in `../../../../native-builtins/src/classloader.rs`. Builtin loaders now reject actual
user-loader namespace hits (`loader_id > 2`) returned by the flat global lookup,
while preserving Application-namespace classes that merely record a user-defined
defining loader. Focused verification:
`cargo test -p cratonvm-native-builtins test_builtin_find_loaded_class -- --nocapture`
passes the new "hide user namespace" and "keep application namespace" cases (re-run
2026-07-01). Full Spring BeanShell / Hibernate app repros were not rerun in this
session, so this document remains in `../../../known-issues`.

**2026-07-06 re-verification, historical: then still failed identically.** Re-ran
`BshScriptFactoryTests` on current `dev` tip (`9f1db39d`+): **15/18 pass**, the
SAME 3 methods fail with the SAME `ClassCastException` the 2026-07-01 entry
describes (`staticPrototypeScript`, `resourceScriptFromTag`,
`nonStaticPrototypeScript`). The doc's own suspicion that current `dev`'s more
general `find_loaded_class_for_loader` (rejecting ANY `loader_id_of_class(cid)
> 2` hit for a built-in-loader query, not just generated-proxy names) might
have already fixed this incidentally was checked directly via `--verbose`
debug tracing and confirmed NOT the case — `findLoadedClass` itself now behaves
correctly (every subsequent `BshClassLoader`'s `findLoadedClass("MyMessenger")`
correctly misses, as it should), but the SECOND interpreter's `MyMessenger`
still never gets generated.

**2026-07-08 fixture caveat, superseded later the same day:** the shared Spring fixture available on the
Azure host for this recheck is not HotSpot-clean for this class
(`BshScriptFactoryTests` reports `found=18 succ=5 fail=13` on HotSpot, with
BeanCreationException failures in the same general context). Do not use that
run to update Residual A's pass/fail count or to archive this document. The
last HotSpot-clean CratonVM-specific Residual A evidence remains the 2026-07-06
15/18 CratonVM run above.

The final 2026-07-08 retirement run used the corrected fixture and the unique
residual binary named at the top of this file; `BshScriptFactoryTests` then
reported `found=18 succ=18 fail=0 skip=0`, closing Residual A.

Traced the actual failure precisely this session: BeanShell's
`ClassManagerImpl.plainClassForName` calls the STATIC 1-arg
`Class.forName(name)` (not `Class.forName(name, false, loader)`), which
CratonVM's `native_class_for_name` (`../../../../native-builtins/src/lang_class.rs`)
routes to the bootstrap-style global `ctx.ensure_class_initialized`
(→ `load_class_concurrent` → `ClassManager::get_loaded_class_id`) whenever no
explicit loader argument is present — the SAME loader-blind global lookup this
doc's "Known remaining limitation" section already names. At the moment the
SECOND `BshClassLoader` calls this, only ONE `MyMessenger` (the first
interpreter's) is registered yet, so `get_loaded_class_id`'s ambiguity check
(which only fires when 2+ DIFFERENT loaders already have their OWN copy) sees
a single, unambiguous match and returns it — `plainClassForName` "succeeds"
with the wrong class, so the second interpreter's own `MyMessenger` generation
is never triggered at all (no exception, silently wrong, exactly as the
"Known remaining limitation" section predicts). This is a genuine instance of
that already-documented, deliberately-deferred gap, not a new bug.

**Separately found (2026-07-06):** while investigating, found that
`CRATONVM_LOADER_AWARE_RESOLUTION`'s default has THREE independent copies —
`../../../../vm/src/runtime/env_cache.rs` (flipped to default-ON 2026-07-03, per this
doc's Status line), and two "mirror" copies, `native-builtins/src/
classloader.rs::loader_aware_resolution` and `classloading/src/
class_manager.rs::loader_aware_resolution`, each with a doc comment claiming
to "stay in lock-step" with the `vm` crate's copy. Neither mirror was ever
updated when the `vm` crate's copy flipped default-on — both silently stayed
default-OFF, so the `native-builtins`/`classloading` halves of loader-faithful
resolution (per-loader `defineClass` namespace assignment, loader-faithful
supertype linking) ran the OLD gate-OFF behavior in every default-config
`dev` run since 2026-07-03, invisibly out-of-lock-step with the interpreter
half. A local fix (flip both mirrors' defaults to match) was drafted and
found to fix this doc's `BshScriptFactoryTests` namespace-assignment symptom
in isolation but NOT the actual `BshScriptFactoryTests` failure (the
`Class.forName` reverse-pollution above is a separate step in the same
chain), and to regress a minimal Groovy self-call repro (`ClassNotFoundException`
where none existed before) — so it was reverted rather than shipped from this
branch, pending the full app-gauntlet soak this doc has always required for
gate-default changes.

**Superseded 2026-07-06 (same day, different concurrent session):** another
session independently found and fixed the identical stale-mirror bug
(`30e82560`/`7367ac9c`, "Fix CRATONVM_LOADER_AWARE_RESOLUTION gate lock-step
drift disabling Hibernate enhancement fixes"), flipped both mirrors'
defaults to match, validated against the Hibernate bytecode-enhancement
suite (PASS 40→86/131 on a 131-class gated subset), and merged it into
`dev` (now `dev` tip as of this doc's update, `d14d2ff3`+). That soak did
NOT include a Groovy check. Re-verifying THIS doc's own repro against the
now-current `dev` tip confirms the regression this branch's abandoned
attempt already flagged: `d14d2ff3` alone (gate-mirrors-on, no other
changes) crashes `GroovyBeanDefinitionReaderTests` outright partway through
a 30-method batch (`class file error: class not found:
beans$_run_closure1` — not a soft per-test failure, a hard VM error). This
doc's `invokestatic` self-call fix (Residual B, above) mitigates but does
not fully resolve this on the combined tip — see the "IMPORTANT — re-verified
against the ACTUAL current `dev` tip" note in the Residual B section above
for exact numbers (15/30 pass on combined tip vs. 23/30 against the
older pre-gate-fix baseline). The stale-mirror bug itself is therefore
FIXED on `dev` (Hibernate-validated), but its Groovy-suite side effect is a
newly-confirmed, still-open regression, only partially compensated by this
branch's `invokestatic` fix.

**RESOLVED 2026-07-06 (same session, follow-up):** root-caused and fixed the
regression precisely. Traced the "class not found: beans$_run_closure1" crash
(gate-mirror fix alone, no invokestatic fix) and the `Class.isAssignableFrom:
argument is null` NPE (both fixes combined) to a SINGLE additional root
cause: `Class.getEnclosingClass()` (`native_class_get_enclosing_class`,
`../../../../native-builtins/src/lang_reflect.rs`) resolves a class's `EnclosingMethod`/
`InnerClasses`-attribute enclosing/outer class NAME via the same global,
loader-blind `class_id_by_name`/`ensure_class_initialized` this doc's "Known
remaining limitation" section names. Every Groovy script compiles under the
identical top-level name (`"beans"` for Spring's `GroovyBeanDefinitionReader`),
so `beans$_run_closure1`'s `EnclosingMethod` attribute always names its
enclosing class `"beans"` — and two sequential test methods produce two
DISTINCT `"beans"` classes sharing that name. Before the gate-mirror fix, the
global lookup silently guessed SOME `"beans"` class; after it, the lookup
correctly refuses to guess (returns ambiguous/`None`) — so `getEnclosingClass()`
started returning `null`, surfacing downstream in real Groovy bytecode
(`Closure.getThisType()`'s `GeneratedClosure.class.isAssignableFrom(this
.getClass().getEnclosingClass())` loop) as the NPE. **Fix** (`f6662334`):
probe the SAME defining loader as the receiver class first (via
`loader_id_of_class` + `class_id_defined_by_loader_exact`, the same
mechanism `findLoadedClass` already uses faithfully) before falling through
to the pre-existing global lookup; built-in-loader classes are unaffected.

**Verification (both suites, per the standard this doc has always required
for gate-adjacent changes):**
- `GroovyBeanDefinitionReaderTests`, 30 non-hanging methods in one JVM
  (`--nojit` — see the separate, unrelated JIT regression noted below):
  **30/30 PASS** (up from 23/30 with only the invokestatic fix on the
  pre-gate-fix baseline, and up from a hard crash on the actual current
  `dev` tip without this fix).
- `GroovyApplicationContextTests`: still 4/4.
- `BshScriptFactoryTests` (Residual A): unchanged, 15/18, confirming this
  fix is orthogonal to Residual A's remaining bug.
- Hibernate bytecode-enhancement `gated_subset.txt` (131 classes, the exact
  suite `30e82560` validated against, re-run with a Linux-adapted
  `run_gated.sh` against the fixed binary): **PASS 100/131** — up from the
  86/131 `30e82560` itself reported, not a regression. Residual 31 failures
  match the already-documented, separately-tracked `lazy.*`/`detached.*`/
  `mapping.lazytoone.*` cluster (see `hib-bytecode-enhancement-loader-
  faithful-linking.md`) plus the two pre-existing gate-sensitive ABORTED
  cases (`InheritedTest`, `MappedSuperclassTest`) `30e82560`'s own commit
  message already flagged as not a regression.

**Separate, unrelated finding surfaced during this verification, now its own
doc**: a different, newly-landed commit on `dev` (`fb4a333d`, "Fix silent
data corruption: precise resume for JIT invokedynamic uncommon trap") broke
ALL Groovy execution when the JIT is enabled — see
[jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md](../jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression-FIXED.md)
for the full root-cause/fix/tradeoff writeup (root-caused and a targeted fix
landed 2026-07-07, though with a documented tradeoff — that doc's own status
line has the details). Unrelated to loader-identity/gate work; this doc's
own fixes are unaffected by it.

With this fix, the two concurrently-discovered halves of the same underlying
issue (the invokestatic self-call fix and this `getEnclosingClass` fix) are
BOTH now landed, and the stale-mirror gate fix's Hibernate improvement is
preserved (in fact further improved, 86→100/131) with no remaining Groovy
regression under `--nojit`. Residual B (the Groovy MetaClass/dispatch bug
this doc originally opened) is now considered FIXED, modulo the separate
JIT regression noted above (which masks it under default settings until
that unrelated bug is fixed) and the separate pre-existing `component-scan`
hang (6/36 methods, then still unresolved and unrelated).

This closing sentence was superseded by the final 2026-07-08 retirement run:
the separate JIT regression is fixed, the component-scan path is fixed, and
the full Spring Groovy and BeanShell classes now pass on the final residual
binary.

## Impact

- `ProxyClassReuseTest.testNoReuse` (1 of 3 methods).
- Spring `BshScriptFactoryTests` (3 methods) — see manifestation above.
- The same loader-blind resolution underlies other custom-loader-isolation
  residuals (cf. **SBR-14** `URLClassLoader(parent=null)` bypass and the
  `SC-custom-classloader` family). Any application that relies on the same class
  name meaning *different things* in different loaders (test isolation, OSGi-style
  plugin loaders, ShrinkWrap) is affected.

## Recommended fix (scoped project, not a one-liner)

Make class resolution loader-aware: resolve `CONSTANT_Class` (and the implicit
class references in `new`/`checkcast`/`instanceof`/method+field resolution)
through the **defining loader of the class whose constant pool is being read**,
with a per-loader resolution cache, instead of the flat global store. This is a
core change to CratonVM's class store with broad regression risk; it should land
on its own branch behind a gate, with `IsoProbe3` as the acceptance test and the
full app gauntlet as the bar. The same work likely clears SBR-14 and the
`SC-custom-classloader` residuals.
