---
name: springrepos-extension-hang-jit-throughput-and-deep-recursion
description: Archived handoff for the Spring Boot buildSrc SpringRepositoriesExtensionTests hang. Current dev re-verified 2026-07-01: the class passes 11/11 with the guarded ANTLR cold-path lift enabled. Remaining deep-recursion/fault-recovery work is split to docs/known-issues/jit-deep-recursion-fault-recovery.md.
metadata:
  type: internal
  area: jit, classloader, groovy, throughput
---

# SpringRepositoriesExtensionTests — full handoff

Scope: the one genuine CratonVM-only item from the Spring Boot buildSrc suite
(`org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests`).
HotSpot passes it 11/11 in ~5s. This doc captures everything learned across the
investigation, what landed, and what remains.

**Archived status (2026-07-01):** FIXED / VERIFIED. Current `dev` passes
`SpringRepositoriesExtensionTests` 11/11 with
`CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/`, so the stale hang/re-opened
status below is historical. The remaining infrastructure item is not this test:
deep JIT recursion still lacks production-grade resumable fault recovery and is
tracked in
[`docs/known-issues/jit-deep-recursion-fault-recovery.md`](../known-issues/jit-deep-recursion-fault-recovery.md).

## 2026-07-01 Retry

Built current `dev` in a fresh retry worktree and copied the release executable
to a unique binary name:

```text
C:\craton\CratonVM-codex-springrepos-jit-retry-20260701-1\cvspringretry-20260701-1.exe
```

Validation used the out-of-tree Spring Boot buildSrc runner artifacts from the
main checkout:

```powershell
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG = '1'
$env:CRATONVM_JIT_ALLOW_PACKAGES = 'groovyjarjarantlr4/'
$CP = 'runner;' + (Get-Content 'C:\craton\CratonVM\apps\spring-boot\buildSrc\test-classpath.txt' -Raw).Trim()

.\cvspringretry-20260701-1.exe --java-home 'C:\Program Files\Java\jdk-25' `
    --stack-dump-on-timeout 0 -cp $CP `
    RunJUnitV org.springframework.boot.build.groovyscripts.SpringRepositoriesExtensionTests
```

Result:

```text
JUNIT_RESULT tests=11 passed=11 failed=0 skipped=0 aborted=0
```

The narrower parse probe also passed under the same guarded ANTLR lift:

```text
WARMUP(trivial) parsed in 6654ms
SCRIPT parsed OK: SpringRepositorySupport in 43549ms
```

`GroovyNestProbe` no longer reproduced the historical native-stack crash in the
old failure window, but it is still a throughput stressor rather than a clean
fixed test: it reached `depth=20 parsed 20421ms`, threw Groovy
`CompilationFailedException` at depths 40 and 80, then remained CPU-bound at
depth 160 until manually stopped after 526 seconds. That residual belongs to the
new deep-recursion/fault-recovery tracker, not this archived SpringRepos test
handoff.

**Current status:** `dev` **passes** this test (via the peer's GC-root-snapshot
fix, commit `1d523351`). Three additional general JIT/classloader fixes landed on
`dev`. The remaining cold-path throughput work is **not a blocker**, and the one
crash it surfaces is precisely diagnosed (a native stack overflow, design for the
fix below).

> ## ⚠️ CONTRADICTED by a 2026-06-22 re-run (dev `d95a836e`) — re-opened as a HANG
> A fresh test-level execution (NOT just the commit-ancestry check the 2026-06-21
> note relied on) **does not pass**: `SpringRepositoriesExtensionTests` was run
> **P-core-pinned (`0xFFFF`), default watchdog disabled, 360s timeout** and was
> **killed at 361s with no `JUNIT_RESULT`** — i.e. it still hangs. HotSpot passes
> it 11/11 in 5s. Either (a) a regression between `0c904c04` (the "passes" basis)
> and `d95a836e`, or (b) the real 163-line script's cold ANTLR ATN simulation
> genuinely needs **>360s** even pinned (consistent with §5's "first-time
> per-decision simulation" being O(distinct decisions) and the script being
> decision-diverse). **Next step:** a 900s-timeout re-run to separate regression
> from pure throughput. The stall coincides with the cross-thread STW JIT-root WARN
> (`scan_active_jit_frames … cross_thread_jit_gap_hits=1 global_jit_depth=6`,
> Family A4 / [[fork6-fjp-multithread-jit-root-reclamation]]). See the archived
> [spring-boot-buildsrc-coldpath-hangs-2026-06-22.md](fixed-suite-bugs/springboot/spring-boot-buildsrc-coldpath-hangs-2026-06-22.md)
> suite context.

> ## 🔬 FULL RE-INVESTIGATION 2026-06-23 (dev `99510377`) — it is NOT a hang; it is a 3-bug cascade
> Ran the test to completion on a freshly-built current-dev binary, P-core-pinned,
> watchdog off. **The "hang" framing is wrong** — it terminates. The test is blocked
> by **three independent CratonVM bugs**, peeled one at a time:
>
> | # | Bug | Path | Status |
> |---|-----|------|--------|
> | 1 | **JIT miscompile in the ANTLR `PredictionContext` equality/hash cluster** → a null `PredictionContext` flows into interpreted `ATNConfigSet.optimizeConfigs` → `ATN.getCachedContext` **NPE** → Groovy `MultipleCompilationErrorsException: General error during parsing: NullPointerException` → 0/11 run. `--nojit` parses cleanly. **Bisected** (`CRATONVM_JIT_BISECT_SKIP` on a standalone `GroovyScriptProbe`) from ~105 compiled ANTLR methods down to **7**: `PredictionContext.{calculateHashCode,hashCode}`, `PredictionContext$IdentityEqualityComparator.hashCode`, `SingletonPredictionContext.{equals,isEmpty,size}`, `ObjectEqualityComparator.equals` (a wrong hash/equals corrupts ATN config-context dedup → null context). | **JIT only** | ✅ **FIXED** on branch `fix/springrepos-coldpath` (`978c783a`): ban `groovyjarjarantlr4/` from JIT (`vm/src/jit/skip_list.rs`), matching the BouncyCastle/ByteBuddy precedent. Verified: `GroovyScriptProbe` parses the script OK under default JIT (was NPE). Open follow-up: the exact single method / codegen archetype (for a surgical 7-method ban or a real codegen fix). NB a *partial* ANTLR ban exposes a separate conservative-root-scan-over-deep-interpreter-recursion throughput cliff; the Hibernate HQL reproducer is consolidated in §5 below. |
> | 2 | **Type-variable USE resolved only against the immediate decl** → a method bound `<S extends T>` (Gradle `RepositoryHandler.withType`/`named`) fell back to a synthetic `TypeVariable` stub (right name, but not identity-equal to the class's real `T`, bound defaulted to `Object`) → ByteBuddy `TypeVariableSource.findExpectedVariable` throws `Cannot resolve T` → **all 11 Mockito mocks fail**. | both (JIT + `--nojit`) | ✅ **FIXED** on branch `fix/springrepos-coldpath` (`f7942b09`, `native-builtins/src/generics.rs`): walk the enclosing generic scope (method → declaring class → outer). Verified: probe reports bound `[T]` matching HotSpot; "Cannot resolve T" gone. General fix — repairs Mockito-on-generics VM-wide. |
> | 3 | **Groovy invokedynamic / MethodHandle receiver mismatch** — after #2, dispatch reaches `vmplugin.v8.Selector.correctCoerce` and threw `GroovyBugError: argument array length and parameter array length should be the same`. **Root cause (found via a `java.lang.invoke` probe vs HotSpot):** CratonVM's `Lookup.unreflect`/`findVirtual` omitted the leading **receiver** from a virtual/special `MethodHandle.type()` (`int m(Object)` → `(Object)int` pcount 1 vs HotSpot `(Recv,Object)int` pcount 2), and `bindTo` didn't drop it. | both | ◐ **PARTIAL** (`fix/springrepos-coldpath` `bcbe2f23`): receiver now included in `type()` + dropped on `bindTo` (`native-builtins/src/lang_invoke.rs`), verified == HotSpot (probe + lambda/method-ref/concat smoke + 23/23 invoke tests). GroovyBugError is **gone**. **Layer 3b also fixed** (`894d718c`): CratonVM's `insertArguments`/`asCollector` set the adapter `type()` to the target's raw descriptor unchanged (lost arity); now they track the adapted MethodType (drop bound params / replace the trailing array param), verified == HotSpot via a guard-chain probe (`insertArguments` pc 2→1, `asCollector` pc=2). **Layer 3c FIXED** (`fix/springrepos-indy-3c` `5d36c432`, `native-builtins/src/lang_invoke.rs`): the `sameClasses` AIOOBE was `MethodHandles.guardWithTest` copying the target's **raw** descriptor (`mh_read_desc`), which omits the receiver for an unbound virtual target → the GUARD adapter's `type()` was one param short → `Selector.setGuards` sized the `SAME_CLASSES` collector below `classes.length`. Fix: chain off the EFFECTIVE type (`mh_type_descriptor`). **Layer 3d FIXED** (same commit): once 3c cleared, `fromCache`'s `invokeExact` underflowed because an Object-returning poly-invoke of a `void` target (`addRepositories`) returned `Ok(None)`; `auto_box_return` now maps `Ok(None)`→null for `L`/`[` returns. **Net: 0/11 (all crashed) → 3/11 (clean).** **Remaining (3e, OPEN):** the 8 non-empty cases fail "expected size N but was 0" — a Groovy indy call (`this.repositories.maven { … }`) on a **Mockito mock** records no interaction → see [[spring-boot-groovy-indy-mockito-mock-dispatch]]. Full detail: [[spring-boot-groovy-indy-runtime-argcount-3c-FIXED]]. |
>
> **Net:** the old "dev passes 11/11 via root-snapshot" claim is false on `99510377`
> (it predates this test-level run). **Bugs #1 and #2 are FIXED, and #3 is partially
> fixed** (the MethodHandle-receiver layer; one deeper indy guard-arg layer remains) —
> all on branch `fix/springrepos-coldpath`. The class is not yet green but has advanced
> through three distinct CratonVM defects, two-and-a-half now fixed. Repro binary
> `C:\craton\CratonVM-sbrepos\target\release\cvsbrepos.exe`; reflection probe
> `apps/spring-boot/buildSrc/runner/RhProbe.java`; parse repro
> `apps/spring-boot/buildSrc/runner/GroovyScriptProbe.java`.

> **CHECKED 2026-06-21 against dev `0c904c04`.** All four load-bearing commits this
> doc relies on are confirmed present on the current dev tip (git ancestry):
> `1d523351` (root-snapshot/hang fix), `43f5fe03` (pdcache), `05b9622a`
> (AssertionError preload), `2fbabc0b` (hashCode/equals-override compile). The doc's
> account is accurate and the two residuals (§5 cold-path throughput, §6–§7
> deep-recursion native stack overflow + stack-banging guard) remain **non-blockers /
> open handoff**, unchanged. A fresh test-level re-run of
> `SpringRepositoriesExtensionTests` was **not** repeated here — it uses the separate
> `apps/spring-boot/buildSrc/runner` harness (~55 s Groovy bootstrap, watchdog must be
> disabled) rather than the spring-framework KRun harness used for the other tickets
> in this batch; the commit-level verification above is the check performed.
>
> **Cross-link / second reproducer:** the Spring `GroovyScriptEvaluator` cluster in
> [[spring-bug-11]] hits this **same ANTLR ATN cold-path**. A standalone probe
> (`new GroovyScriptEvaluator().evaluate(new StaticScriptSource("return 3 * 2"))`) —
> a *trivial* script — returns instantly on HotSpot but on dev `0c904c04` **hangs >120 s
> and trips the stack-dump watchdog**, frozen in
> `GroovyParser.<clinit> → ATNDeserializer.deserialize → BitSet.get/<init>` (never
> finishing the one-time ATN deserialize). So this cold-path throughput is not
> buildSrc-specific — it gates *any* first Groovy parse, and is a more minimal repro
> than `SpringRepositoriesExtensionTests` for the §5–§6 work.

> **DEAD END (investigated 2026-06-21, do not repeat) — the compile-bail count is a
> RED HERRING; class-loading is NOT the bottleneck.** A tempting hypothesis is "almost
> nothing JIT-compiles during the Groovy/ANTLR bootstrap, so it runs interpreted and is
> slow." `CRATONVM_DBG_JITC=1` shows ~173 `compile-bail`s on the trivial-script probe,
> ~119 of them ANTLR. **But this is not a fixable signal.** Two experiments (each a full
> release build, both reverted) prove it:
> 1. **Prewarm `new`-site classes** at tier-up (load+init the classes a method's `new`
>    sites construct, so `resolve_jit_new_site` stops returning `None`) → compile-bails
>    **173 → 173**, ANTLR **119 → 119**. Zero change.
> 2. **Prewarm *all* referenced classes** (walk the holder constant pool, load+init every
>    `CONSTANT_Class` + every Field/Method/InterfaceMethod owner) → **173/119 → 174/118**
>    (i.e. noise). Zero change. Wall-clock identical on/off.
>
> Why: `backend_attempted=false` is **not** an unresolved-class miss. The still-bailing
> set includes `java/util/BitSet.get` (**native — no bytecode to compile**) and
> `java/lang/String.compareTo` (**always loaded** — bootstrap). These bail for *intrinsic*
> non-compilability (native / unsupported bytecode), and most of the 173 are legitimately
> non-compilable and always interpreted — not a bottleneck. Loading classes can never help.
>
> Also: **wall-clock is machine-contention-dominated** here — the *same* probe measured
> 33 s and 81 s on this multi-session box (on==off), so the bootstrap's true cost cannot be
> measured reliably without a quiet machine.
>
> **Prerequisite for real progress (not done): a sampling profiler over the interpreter
> dispatch loop (a frame histogram), to find the genuinely hot method(s).** The single
> watchdog stack snapshot (`deserialize → BitSet`) is one sample, not a profile — do not
> treat it, or the compile-bail list, as the bottleneck. The reverted experiments used a
> `CRATONVM_JIT_NO_PREWARM_NEW` gate; neither the gate nor the prewarm is on dev.

---

## 1. The test decomposes into THREE independent defects (earlier reports conflated them)

| # | Defect | Status |
|---|--------|--------|
| 1 | `SecureClassLoader.pdcache` left null → deterministic `computeIfAbsent on null` NPE in Groovy class-gen | ✅ FIXED on dev (`43f5fe03`) |
| 2 | CRASH-04 JIT register-invisibility heap corruption on the Groovy path (only under `GC_STRESS`) | 🔴 separate, open (shadow-stack workstream) |
| 3 | ANTLR adaptive-prediction interpreter-throughput blow-up parsing the closure-heavy script → the HANG | 🟡 mitigated on dev by root-snapshot; deeper JIT throughput work open |

Plus, the **hang itself** on `dev` was already fixed by the peer via a *fourth*,
orthogonal root cause: the **per-native-call GC root snapshot rescan**
(`update_root_snapshot` rescanned every interpreter frame on every object-returning
native call; at the Groovy-compile-under-JUnit stack depth ~46 this is
O(stack-depth) per call → >300s hang). Commit `1d523351` made `rootsnap_cache` /
`skip_redundant_native_snapshot` / `rootsnap_cache_survive_gc` default-ON. bt18
stays `68332206`. **Do not re-solve the hang — dev already passes.**

### Corrected diagnoses (prior reports were wrong)
- **No thread leak.** The watchdog "1015 / 3807 thread(s) dumped" is its **ack
  counter** (poll iterations during the 3s grace window), NOT a thread count. The
  actual registry has **2 threads** (`main` + the JDK `Cleaner`). It re-dumps the
  single `main` thread thousands of times.
- The hang is **single-threaded, CPU-bound**, pinned in ANTLR
  `ParserATNSimulator.adaptivePredict → execATN → computeReachSet → computeTargetState`
  — **NOT** `ATNDeserializer.deserialize` (one earlier theory) and **NOT**
  `MetaClassRegistryImpl.registerMethods` (an older theory).
- The DFA cache **works** (refutes the "memoization broken" theory): a scaling
  probe shows `N=1` parse = 62s but `N=8` = 6.5s — the 8-statement parse reuses the
  first decision's DFA. The cost is the *first-time* simulation of each **distinct**
  grammar decision; the diverse 163-line script has many, so the interpreted cold
  path dominates.

---

## 2. Fix 1 — `SecureClassLoader.pdcache` (LANDED, dev `43f5fe03`)

**Symptom:** `NullPointerException: Cannot invoke computeIfAbsent on null` thrown
in Groovy's `class generation` phase (re-wrapped as `GroovyBugError`).

**Real root cause** (via `CRATONVM_DBG_NPE_STACK=1` interpreter frames — the
reconstructed Java trace is unreliable here): the null receiver is
`java/security/SecureClassLoader.pdcache`, read by real-JDK bytecode
`SecureClassLoader.getProtectionDomain(CodeSource)` ← `defineClass(name, byte[], …,
CodeSource)` ← Groovy's `GroovyClassLoader$ClassCollector.createClass`.
`pdcache = new ConcurrentHashMap<>(11)` is set by `SecureClassLoader.<init>`'s
inline field initialiser, which CratonVM's simplified URLClassLoader/ClassLoader
`<init>` natives bypass — `init_classloader_common_fields`
(`native-builtins/src/classloader_real.rs`) populated every inherited field
**except** `pdcache`.

**Fix:** init `pdcache` to a real, *segment-initialised* `ConcurrentHashMap` (built
via its native `<init>()V`; a bare `alloc_concurrent_synthetic` leaves the segments
array null so `computeIfAbsent` would silently no-op), null-guarded.

**This was DETERMINISTIC, not flaky/CRASH-04.** Pre-fix: 3/3 runs throw the NPE
with **0** `inconsistent header` warnings (corruption produces warnings; their
absence + perfect reproducibility rule it out). A peer report had misfiled it as a
flaky CRASH-04 symptom; that report's own `GC_STRESS` matrix shows 0 warnings in
the no-stress row, consistent with this gap. CRASH-04 *is* real but separate (3432
warnings only under `GC_STRESS`, on both pre- and post-fix binaries).

---

## 3. Fix 2 — preload `java/lang/AssertionError` (LANDED, dev `05b9622a`)

**General JIT bug:** a method's JIT compilation is vetoed wholesale when a `new`-site
references a class the JIT's new-resolver can't resolve — `resolve_jit_new_site`
(`vm/src/runtime/interpreter.rs`) does `find_class_by_name(name)?` → `None` when the
class is unloaded → `try_compile_inner` bails the **entire** method
(`backend_attempted=false`, a pre-backend resolver miss).

`assert` compiles to `getstatic $assertionsDisabled; ifne L; … new
java/lang/AssertionError; …; athrow; L:`. With assertions **disabled** (the default)
that path is dead and `AssertionError` is **never loaded** → every assert-bearing
method fails to JIT-compile and runs interpreted forever. `assert` is pervasive;
ANTLR's `ParserATNSimulator` (`closure`, `getEpsilonTarget`, `ATNConfigSet.add`,
`PredictionContext.join`, `SingletonPredictionContext.getReturnState`, …) is
assert-saturated → none compiled → Groovy parse ~2500× slower than HotSpot.

**Fix:** load `java/lang/AssertionError` once at VM bootstrap (next to
`java/lang/Object` in `vm/src/vm/vm_init.rs`). Verified: `closure` + cluster now in
`[JIT_COMPILED]` with no NEW-miss bail; ~2× faster parse; LibraryTests 4/4.

**Pinning tool added:** `CRATONVM_DBG_JITBAIL=<method-substring>` in
`jit/src/lib.rs::try_compile_inner` prints which pre-backend resolver returns `None`
(`closure` → `NEW miss cp=123` = AssertionError). Also: `CRATONVM_DBG_DUMP_JIT=LIST`
(list compiled methods), `CRATONVM_DBG_JITC=1` (compile-bails).

---

## 4. Fix 3 — `hashCode`/`equals` override JIT compilation (LANDED, dev `2fbabc0b`)

Two coupled JIT throughput bugs (`vm/src/runtime/interpreter.rs`):

**3a — over-broad native-shadow refusal.** `try_jit_compile_callee_slow` refused to
compile a method if **any ancestor** had a Rust native of the same signature.
`Object.{hashCode,equals,toString,clone}` are native → **every** bytecode override
of them was refused JIT compilation, even though the override shadows the ancestor
native and is what actually runs. `PredictionContext.hashCode` (3 bytecodes,
`getfield cachedHashCode; ireturn`) was ~32% of the parse profile, interpreted.
**Fix:** check the native shadow at the **resolved declaring class** (+ the
receiver's own class), not the whole ancestor chain — preserving the
`ForkJoinTask.fork()` inherited-native case.

**3b — recompile storm.** `try_jit_compile_callee` looks up the JIT cache by
**receiver** class, but `callee_slow` stored under the **declaring** class. For an
inherited method (`SingletonPredictionContext.hashCode → final
PredictionContext.hashCode`) the keys differ → cache never hits → the dispatch
helper recompiled it on **every** polymorphic call (**42,676** recompiles in one
parse). **Fix:** store under the receiver class to match the lookup.

Verified (no miscompile): bt18 = `68332206`; Mockito/ByteBuddy
`InteractiveUpgradeResolverTests` 1/1; `DependencyVersionUpgradeTests` 63/63;
`ArtifactVersionDependencyVersionTests` 20/20; `LibraryTests` 4/4.

**2026-07-01 update — precise native-shadow guard landed for the interpreter
paths.** The first-call path and `try_jit_upgrade_with_gate` no longer reject every
bytecode override merely because an ancestor has a native identity method. They
now use the intended `jit_method_calls_native_shadowed(declaring_id, code)` guard:
compile an override **unless its bytecode internally invokes a method that resolves
to a native**. A leaf like `PredictionContext.hashCode` (no inner invoke) can
compile; `ATNConfig.hashCode` (calls the *override* `PredictionContext.hashCode`,
not native) can compile; `LazyProjection.equals` (calls `Object.equals`) is still
refused. This keeps the ByteBuddy safety case while removing the cold interpreter
path's parent-walk over-refusal.

---

## 5. The cold path (residual throughput) — what it really is

After fixes 2+3, the hot `hashCode` methods compile, yet a fresh interpreter
leaf-frame profile still shows much of the ATN simulation interpreted. The
watchdog samples **only interpreter frames**, and the reason they show is: the
**first-time recursive simulation of each new grammar decision runs interpreted**.
The interpreter *does* dispatch interpreted call-sites to already-compiled callees
(the `CachedInvokeTarget::Bytecode` fast-path checks the JIT cache every call).
The 2026-07-01 native-shadow narrowing removes one leaf-method blocker in the
interpreter's own compile path, but the broad throughput bug remains: larger
ATN-simulation bodies still exceed the single-pass backend's supported shape, and
deeper recursive compiled paths still need robust native-stack handling (§6-§7).

dev already passes the test (root-snapshot), so this is a **pure throughput
follow-up**, not a blocker.

### 2026-07-01 cold-path validation guard

The coarse ANTLR package ban can now be lifted for validation with
`CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/` without re-enabling the known
parse-corrupting `PredictionContext` equality/hash cluster. The skip list keeps
these seven methods interpreted even under that package lift:
`PredictionContext.{calculateHashCode,hashCode}`,
`PredictionContext$IdentityEqualityComparator.hashCode`,
`SingletonPredictionContext.{equals,isEmpty,size}`, and
`ObjectEqualityComparator.equals`.

This turns the old all-or-nothing cold-path experiment into a narrower validation
mode: the ATN simulator leaves can be retried after the recursive direct-call
routing fix, while the already-bisected correctness defect remains contained.
Pinned by `antlr_coldpath_validation_lifts_non_bad_atn_methods` and
`antlr_prediction_context_cluster_stays_interpreted_under_validation_lift`.

### Hibernate HQL reproducer (same cold ANTLR prediction bug)

The Hibernate census H4 timeout (`function.json.JsonArrayUnnestTest`) is the same
underlying throughput defect, surfaced through Hibernate's HQL parser instead of
Groovy. `em.createQuery(hql)` with a multi-item select list spends tens of seconds
in ANTLR cold full-context prediction on CratonVM while HotSpot finishes in
milliseconds. The parse happens before semantic resolution, so a trivial
`SessionFactory` plus an HQL string that references even non-existent entities is
enough to reproduce.

Measured on the 2026-06-20/2026-07-01 `dev` lineage:

| HQL shape | CratonVM | HotSpot |
|---|---:|---:|
| `select e.id from Book e` (1 select item) | 12.7 s | ~ms |
| `select e.id, e.name from Book e` (2 items) | 52-58 s | ~ms |
| `select e.id, index(p), p.name from Book e ...` (3 items) | >600 s timeout | ~ms |

This is not an infinite loop, GC pressure, or a broken ANTLR cache: the 2-item
case completes, `-Xmx8g` is unchanged, and a warm parse of the same grammar shape
drops from 55,836 ms to about 649 ms. `--nojit` is essentially identical to
JIT-on for the cold parse (51.9 s vs 55.8 s), which confirms the hot
`ParserATNSimulator.adaptivePredict -> closure/closureCheckingStopState/
computeReachSet` path is still running interpreted.

`CRATONVM_DBG_JITC` showed the relevant ATN-simulation methods crossing the
invocation threshold but being declined by the single-pass backend:
`ParserATNSimulator.closureCheckingStopState` (~3925 compile attempts),
`ParserATNSimulator.closure_` (~3922), `ATNConfigSet.add` (~1311),
`ParserATNSimulator.ruleTransition` (~1003), and
`PredictionContext.mergeArrays` (~316). These are object- and exception-heavy
methods (`closure_` includes `athrow`, repeated `checkcast`/`instanceof`, and
`invokeinterface` sites), so this remains a backend-coverage project rather than
a one-line VM fix. Running the Hibernate suite in one shared JVM is the current
mitigation because it amortizes ANTLR DFA warmup across classes.

---

## 6. Cold-path fix ATTEMPT → uncovered a NATIVE STACK OVERFLOW (the real next bug)

The earlier broad cold-path experiment that applied the precise §4 inner-invoke
guard and forced more ATN-sim compilation through the interpreted simulation path
**passed** bt18 (`68332206`) and ByteBuddy/Mockito 1/1 — **but the real parse
SIGSEGV'd ~5.5 min in**, deep in `ParserATNSimulator.closure` recursion. That
broad experiment was reverted. The narrower 2026-07-01 change keeps the precise
native-shadow guard for first-call/upgrade paths, but this full parse path still
needs separate validation after the stack-overflow work.

**The crash is a NATIVE STACK OVERFLOW, not a value-miscompile** (so NOT a codegen
bisect). VEH dump signature:
- `EXCEPTION_ACCESS_VIOLATION` reading a **page-aligned guard address** (`read at
  0x409C0000`, `R10 = 0x184F0000`);
- `ShadowStack` `top` (`0x3C9DB468`) grown **past** its `end` limit (`0x3CA41C58`);
- faulting frame spill slots literally spell **`"operand stack overflow"`**;
- stack = `closure → closure → …` 110+ frames.

**Mechanism:** the cold-path experiment makes the ATN-sim **leaf** methods compile, so each
`closure()` level stays in JIT'd native code (native stack) instead of bailing to
the interpreter (VM frame stack). The deep ANTLR `closure()` recursion then overruns
the **native** stack, and the VM's overflow path (GC / shadow-stack scan or
`StackOverflowError` construction) faults instead of throwing cleanly.

**Confirmed cold-path-specific:** the clean dev binary parses the same
deeply-nested-closure script (`runner/GroovyNestProbe.java`) **without crashing**,
only *slowly* — its `closure()` leaf calls bail to the interpreter and never
deep-recurse in native code.

### Why the existing guard doesn't catch it
`enter_jit_dispatch` (`vm/src/jit/helpers.rs`) maintains a thread-local depth counter
and throws a *catchable* `StackOverflowError` (via `raise_jit_stack_overflow` →
`i64::MIN` sentinel) — **but only inside the two dispatch helpers**
(`jit_invoke_dispatch`, `jit_invoke_virtual_mic` = the inline-cache MISS path). Every
**direct** JIT→JIT call bypasses it:
- **invokestatic self-recursion** — `jit/src/x64.rs` ~17469 (`self_call_patches`);
- **PIC inline-cache HIT** — `jit/src/x64.rs` ~18968–19477 (`MOV R11,[R10+8]; CALL
  R11` — what warm monomorphic `closure()` recursion uses);
- regular **direct_calls** — ~15804.

Also the depth counter uses a **fixed per-level budget**
(`NATIVE_STACK_BYTES_PER_JIT_DISPATCH_LEVEL`) that under-counts when frames grow
(exactly what compiling the leaf calls does), so even the guarded path can overrun.

---

## 7. Deep-recursion stack guard — design + what was tried

### Attempt 1 (prototyped, reverted): explicit prologue check → deopt stub
Inline `gs:[0x30]` → `[+0x1478]` (`TEB.DeallocationStack` = reserved stack bottom)
RSP check in the prologue (after params homed, using free scratch R10/R11; gated on
`needs_heap` so the deopt stub can load `vm_ptr` from `heap_local_offset`). On
underflow, jump to the **existing deopt-stub machinery** (`emit_deopt_stubs` →
`jit_uncommon_trap`, reason 6, bci 0): the method re-executes in the interpreter
(recursion continues on the VM frame stack; the `DeoptimizationController` escalates a
repeatedly-tripping method to **not-entrant** → interpreter-only — exactly the
desired terminal state). Exact bytes:
```
65 4C 8B 14 25 30 00 00 00   ; MOV R10, qword gs:[0x30]            (TEB)
4D 8B 9A 78 14 00 00         ; MOV R11, qword [R10 + 0x1478]       (DeallocationStack)
49 81 C3 <imm32>             ; ADD R11, HEADROOM (256 KiB)
4C 39 DC                     ; CMP RSP, R11
0F 82 <rel32>                ; JB  -> deopt stub
```
**Result:** codegen **correct** (bt16 `14985902`, bt18 `68332206` golden, inert for
normal code). bt18 *appeared* ~2× slower, **but that magnitude was contaminated by
concurrent machine load** (a clean no-guard binary measured the same ~53s while the
peer session was rebuilding/running `dev`). True per-call cost **unconfirmed**.
Reverted anyway: an explicit per-prologue check (two TEB loads, one to the cold
`DeallocationStack` field) is inherently non-free on hot recursive methods, and the
guard is **inert on dev** (the deep-recursion scenario only arises with the reverted
cold-path fix).

### Attempt 2 (recommended): stack banging + a stack-overflow-aware fault handler
HotSpot-style, near-zero cost. Emit a single prologue **bang** — `mov eax, [rsp -
BANG_OFFSET]` — which merely *touches* committed stack on the normal path (hot cache
hit, ~free) and only faults near the guard page. Move the work to the VEH handler:
recognise `EXCEPTION_STACK_OVERFLOW` (`0xC00000FD`) / a fault at the bang address as a
recoverable deep-JIT-recursion overflow and **deopt the current JIT frame to the
interpreter** (or throw a catchable `StackOverflowError`). Hard part: the handler runs
with almost no stack left (use a reserved guard region / alternate stack) and must
unwind/deopt the JIT'd frame. This removes the per-call cost entirely.

### 2026-07-01 partial containment: recursive-edge dispatch routing

Production JIT metadata now routes recursive call sites through guarded dispatch
helpers instead of raw direct compiled edges, except for static tail-recursive
self-calls that x64 lowers to a jump back to the method body. Non-tail
`invokestatic` self-calls get `JitInvokeInfo`, same-method `invokespecial` sites
avoid direct callee compilation, and recursive `invokevirtual` / `invokeinterface`
sites keep dispatch metadata but do not allocate MIC/PIC slots, so inline-cache
hits cannot bypass `enter_jit_dispatch`.

This is a general containment for same-method recursive edges and should prevent
the known deep ANTLR-style recursion from overrunning the native stack once the
cold-path leaf-compilation experiment is retried.

### 2026-07-01 follow-up: compile-cycle direct-call routing

The direct-call metadata path now also tracks the current per-thread JIT compile
stack. If compiling `A` recursively compiles `B`, and `B` resolves an invoke back
to any outer method on that stack, the whole cycle path is marked as requiring
guarded dispatch for future direct-call attempts. Parent compilers consult that
marker after callee compilation returns, so the original `A -> B` site does not
bake a raw machine `CALL` once `B -> A` has exposed the cycle. The OSR
`direct_calls2` path uses the same marker before emitting eager invokestatic
direct calls. This closes the previously-open mutually-recursive direct-call gap
for compile-time-discovered cycles; pinned by
`recursive_compile_cycle_routes_parent_direct_call_through_dispatch`.

This is still not the full stack-banging / fault-recovery design above, and the
cold-path throughput experiment still needs separate validation before this doc
can be archived.

### 2026-07-01 follow-up: x64 stack-bang containment

The x64 single-pass backend now emits stack-bang probes in every normal compiled
method prologue. Before `sub rsp, frame_size`, it touches each 4 KiB page crossed
by the frame allocation and bails compilation if an extreme frame would need more
than 512 inline probes. After the subtract, it touches one additional page below
the final RSP so a missed direct-recursion edge trips at the method prologue
instead of later corrupting shadow-stack / operand-stack metadata. This is
default-on and can be disabled for A/B runs with `CRATONVM_JIT_STACK_BANG=0` or
`CRATONVM_JIT_NO_STACK_BANG=1`.

The Windows VEH crash report also names the faulting JIT method for the faulting
RIP when `CRATONVM_DBG_JIT_NAMES=1`, including the `EXCEPTION_STACK_OVERFLOW`
case where the handler intentionally skips stack walking.

This is still containment, not full Java-level recovery: the handler does not yet
rewrite the trapped JIT frame into a resumable deopt or stash a catchable
`StackOverflowError` from the fault context. The remaining production-grade item
is the fault-recovery half of the design.

---

## 8. Repros & tooling (all under `apps/spring-boot/buildSrc/runner/`)
- `GroovyParseProbe.java` / `GroovyNpeProbe.java` — trivial-class parse (pdcache NPE).
- `GroovyThreadProbe.java` — prints live thread count (= 1; debunks the "explosion").
- `GroovyScriptProbe.java` — parses the real `SpringRepositorySupport.groovy` after a
  warmup parse (isolates bootstrap from the script parse).
- `GroovyScaleProbe.java` — parse time vs N closure statements (proves the DFA cache
  works: cost is first-time per-decision simulation).
- `GroovyNestProbe.java` — deeply-nested closures; reproduces deep `closure()`
  recursion (clean dev: slow, no crash; cold-path binary: native stack overflow).
- Run harness: `buildSrc/runner/run-sbcrash.sh` (one class per process). JDK home
  `C:/Program Files/Java/jdk-25`. Classpath `runner;$(cat test-classpath.txt)`.
- Always set `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` for long parses (the default 120s
  watchdog aborts otherwise). The Groovy bootstrap (one-time ATN deserialize +
  metaclass) is ~55s.

## 9. Gotchas
- **Concurrent-session contamination is real and severe here.** A peer session works
  the same suite/branch: it `taskkill`s `cratonvm*` processes (silent rc=1/127 early
  deaths — use a binary copy whose name does NOT start with `cratonvm`), commits/moves
  branches under you, runs the dev binary (file-locking cargo's link step), and edits
  the same bug-report docs. Verify timing on a quiet machine; cross-check bt18 vs
  HotSpot (`68332206`).
- bt18 golden = `68332206` (HotSpot-confirmed); bt16 = `14985902`. Always cross-check
  JIT changes against these. Run via `bench/BenchSuite bintrees18`, `-Xmx8g`.
- Build: `build-cpu.bat` (PowerShell); ~4–12 min depending on contention. Transient
  "failed to remove cratonvm.exe" link error = a process (often a peer) holds the exe;
  retry when free.

## 10. Bottom line
- 3 general fixes landed on `dev` (pdcache, AssertionError preload,
  hashCode/equals-override compile + cache-key) — all regression-validated.
- `dev` **passes** `SpringRepositoriesExtensionTests` via the root-snapshot fix.
- Remaining: (a) run the real Groovy/HQL cold-path validation with
  `CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/` now that the known-bad
  PredictionContext cluster remains interpreted and recursive compile cycles route
  through guarded dispatch, and (b) finish the **§7 deep-recursion stack guard
  (stack banging + fault recovery)** before making that lift a production default.
  The stack-bang probes are now in place; the remaining part is resumable
  fault recovery / catchable `StackOverflowError` routing from the fault
  context. Neither is a blocker.
