# CratonVM Spring Suite — Consolidated Open Bugs
**Latest Update: July 15, 2026**

## Executive Summary
Multiple major bug clusters have been successfully resolved (including the JIT SIGSEGV in Groovy, the `java.home` Locale regression, the `Semaphore` deadlock, and dozens of classloader visibility/AOT fixes).

This document tracks the **genuine remaining failures**.

---

## 1. Deep-Dive Investigations (Root-Caused, Pending Fix)

*   **Mockito `spy()` StackOverflowError** (`context.annotation.ImportSelectorTests`)
  *   **Status**: **FIXED (verified 2026-07-16, joint verification session)**. The heap-corruption
    failure mode described below (`importSelectorsWithNestedGroup`,
    `importSelectorsWithNestedGroupSameDeferredImport`) was an instance of the same cross-cutting
    young-GC exact-walk bug independently root-caused and fixed by an unrelated concurrent session's
    commit `fb15be63` ("fix(gc): GAP_FILLER_CLASS_ID not special-cased in new young-GC exact-walk
    loops") — see `CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`'s **2026-07-16 joint verification**
    addendum to the `ImportSelectorTests` section for the full rebuild-and-rerun confirmation
    (`found=9 succ=9 fail=0`, all 9 methods including both previously-crashing ones now pass, zero
    corruption-signature log lines). Not this session's fix; attributing correctly rather than
    claiming credit. Full history of the original investigation kept below for context.
  *   **2026-07-17 independent re-confirmation** (separate task, separate worktree, no code
    changes): re-verified the FIXED status above from scratch rather than trusting the prior
    session's self-report, per this codebase's own "verify merged state, not agent self-reports"
    lesson. Deliberately used a **maximally independent setup** to rule out any shared-fixture
    artifact: a brand-new `git clone` of upstream `spring-projects/spring-framework` (not a reused
    worktree -- the host's disk-pressure cleanup had deleted every prior spring-framework checkout
    on this host by the time this task started), built fresh via Gradle (`:spring-context:testClasses`,
    version `7.1.0-SNAPSHOT`, single `byte-buddy-1.18.3`/`mockito-core-5.23.0` on the classpath),
    against a from-scratch `cargo build --release` of CratonVM at `dev` tip `56728b1a` (confirmed via
    `git merge-base --is-ancestor fb15be63 origin/dev` that the fix commit is an ancestor). Ran the
    real `ImportSelectorTests` class (`MethodRun`/JUnit-Platform-launcher pattern), both individually
    per previously-crashing method and as the full 9-method class, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`,
    real JDK 25: **`RESULT started=9 succeeded=9 failed=0`, twice in a row, zero corruption-signature
    log lines, zero `StackOverflowError`.** Confirms the FIXED status is real, not an artifact of a
    stale/shared build. **Bonus finding for future sessions** (unrelated to this bug, cost real time
    to diagnose): a `NoClassDefFoundError` on `StandardBeanExpressionResolver$1` reproduced
    deterministically on the *first* Gradle-cache-restored build of `spring-context` (`FROM-CACHE`
    task outputs) even though the `.class` file was verifiably present on disk -- traced to Gradle's
    build-cache restore silently producing an incomplete `classes/java/main` tree, almost certainly
    because this host's chronic root-filesystem (`/`) 100%-full episodes corrupted an in-progress
    cache extraction earlier in the session. Forcing `--rerun-tasks --no-build-cache` on
    `:spring-context:compileJava`/`testClasses`/`jar`/`testFixturesJar` produced a correct tree and
    made the error disappear; this had nothing to do with the Mockito/GC bug and would be worth its
    own throwaway-repro note if it recurs. Also note for reproducing on this host: `GRADLE_USER_HOME`
    and `-Djava.io.tmpdir` both need to be redirected off `/` (e.g. to `/data/tmp`) -- Mockito's
    self-attach boot-jar write and Gradle's own caches both fail with `IOException: No space left on
    device` on the chronically-full root filesystem otherwise, which can otherwise be misread as a
    CratonVM bug.
  *   **2026-07-16 pre-fix status (superseded above, kept for history)**: symptom shape changed
    2026-07-16. On a fresh `dev` tip (`6c517cd9`), the
    original isolated repro (`SpyDLBFProbe.java`: `spy(new DefaultListableBeanFactory())` + one
    `registerSingleton()` call) **no longer reproduces**, and 3 of the 5 real `spy()` sub-tests
    (`importSelectors`, `importSelectorsWithGroup`, `importSelectorsSeparateWithGroup`) now **pass**
    individually. The remaining 2 (`importSelectorsWithNestedGroup`,
    `importSelectorsWithNestedGroupSameDeferredImport`) still fail, but via a **new, more severe
    failure mode**: a deterministic native heap-corruption abort (`GC: young object-start walk
    stopped at an implausible extent`, `GC-ARRAY-GUARD` trips, stale zeroed object headers) instead
    of a clean catchable `StackOverflowError`. Confirmed heap-size-independent (identical corruption
    offset at default heap and `--Xmx 512m`), so it's a deterministic correctness bug, not a GC-timing
    race. Not yet root-caused to a Rust source line or confirmed to share the same underlying cause
    as the `isOverridden` hypothesis below — full writeup, ruled-out list, and next steps in
    `CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`'s **2026-07-16** `ImportSelectorTests` section.
  *   **2026-07-15 status (superseded above, kept for history)**: 5/9 methods SOE, reconfirmed 2026-07-15 on a binary containing the mockk
    fix below — this is a **different root cause** than the mockk sibling, which is now FIXED.
  *   **2026-07-15 root-cause sharpening (supersedes the older hypotheses below)**: full recursion
    cycle captured (`KRUN_STACK=1`, log at `/data/tmp/mockk-tmp/importsel2.log` on the Azure host):
    `MockMethodAdvice.handle` (intercepting `DefaultSingletonBeanRegistry.registerSingleton` on the
    spy) -> `CallsRealMethods.answer` -> `InterceptedInvocation.callRealMethod` ->
    `SerializableRealMethodCall.invoke` -> `MockMethodAdvice.tryInvoke` ->
    `ModuleMemberAccessor`/`InstrumentationMemberAccessor.invoke` (MethodHandle-based) which
    dispatches VIRTUALLY and lands in the subclass override
    `DefaultListableBeanFactory.registerSingleton:1491` -> its `super.registerSingleton` ->
    `DefaultSingletonBeanRegistry.registerSingleton:142` -> intercepted AGAIN -> same cycle forever.
    On HotSpot the super-method frame is never intercepted: `MockMethodAdvice.enter` first checks
    `dispatcher.isOverridden(mock, origin)` (a ByteBuddy `MethodGraph` compiled over
    `instance.getClass()`) and bails to real code when the runtime class overrides `origin`. Prime
    suspect: `isOverridden` computing `false` under CratonVM (reflection/MethodGraph disagreement
    about the retransformed hierarchy) — NOT the ThreadLocal guard (CratonVM ThreadLocal semantics
    probe-verified correct, TLProbe 2026-07-15).
  *   ~~**Old hypothesis**: the `ThreadLocal`-based `MockMethodAdvice$SelfCallInfo.checkSelfCall`
    guard fails to match on CratonVM~~ — never verified; ThreadLocal itself ruled out.
  *   **Next step**: standalone probe that spies a 2-level hierarchy (subclass override calling
    `super.<same method>`), invokes the parent method through the spy, and instruments what
    `MockMethodAdvice.isOverridden` computes on CratonVM vs HotSpot.

*   **mockk `hashCode()` StackOverflowError — ~10 Kotlin reactive classes** — **FIXED (2026-07-15,
    dev `9dce07a5`)**, plus a host-environment trap that will bite again if undocumented:
  *   Affected classes (all previously 100% SOE): `WebTestClientExtensionsTests`,
    `WebClientExtensionsTests`, `ServerResponseExtensionsTests`, `ServerRequestExtensionsTests`,
    `RenderingResponseExtensionsTests`, `ClientResponseExtensionsTests`,
    `RSocketRequesterExtensionsTests`, `WebClientObservationTests`, `CoExchangeFilterFunctionTests`,
    `InvocableHandlerMethodKotlinTests`.
  *   **True root cause (NOT method dispatch)**: mockk only recurses in its **agent-less** mode.
    `JvmMockKAgentFactory$init$Initializer` picks the handler map via
    `MockHandlerMap.create(instrumentation != null)`: with instrumentation ->
    `WeakMockHandlersMap` (identity-keyed `JvmMockKWeakMap`, never calls `hashCode()`); without ->
    `SynchronizedMockHandlersMap` (plain `Collections.synchronizedMap(LinkedHashMap)`). The
    generated mock subclass intercepts `hashCode()` on BOTH VMs by design (`SubclassInstrumentation`
    uses `ElementMatchers.any()`), so in agent-less mode `handlers.get(mock)` -> `mock.hashCode()`
    -> interceptor -> `handlers.get(mock)` -> SOE. The earlier claim "CratonVM routes the mock's
    `hashCode()` through the interceptor where HotSpot dispatches to the real `Object.hashCode`"
    was WRONG — both VMs intercept it; HotSpot survives only because it normally has instrumentation
    and the identity-keyed map. (Reproduced the identical SOE **on HotSpot jdk25** by letting the
    boot jar fail.)
  *   **Why CratonVM ended up agent-less**: the Azure host's root fs (`/tmp`) flaps at ~100% full;
    mockk's `BootJarLoader` writes its dispatcher boot jar via `File.createTempFile` +
    `JarOutputStream`, the jar write hits ENOSPC, mockk logs "Can't inject boot jar." at TRACE only
    and silently downgrades. Two genuine CratonVM parity bugs then sealed the failure:
    **(1)** the `File.createTempFile` (both overloads), `Files.createTempFile` and
    `Files.createTempDirectory` natives read `std::env::temp_dir()` directly and **ignored
    `-Djava.io.tmpdir`**, so the standard redirect escape hatch was a no-op; **(2)** they swallowed
    creation errors (`let _ = std::fs::File::create(..)`) instead of throwing `IOException` (real-JDK
    contract; mockk falls back to a CWD boot jar when `createTempFile` throws). Both fixed in
    `9dce07a5` (`native-builtins/src/phases_late.rs`, helpers `jdk_temp_dir` /
    `jdk_create_temp_file`).
  *   **Host-env trap (still live)**: with `/tmp` full, HotSpot fails these classes too, just
    differently — the attach socket `/tmp/.java_pid<pid>` cannot be created, so
    `ByteBuddyAgent.install()` throws (mockk does NOT catch it) ->
    `ExceptionInInitializerError: io.mockk.impl.JvmMockKGateway`. Any suite run comparing the two
    VMs during a full-`/tmp` window is comparing two env failures. CratonVM implements
    `Instrumentation` internally (no attach socket), so post-fix CratonVM passes where host HotSpot
    currently cannot. **Harness requirement on this host: run mockk/Mockito-heavy classes with
    `-Djava.io.tmpdir=/data/tmp/<writable>`** (honored by CratonVM as of this fix).
  *   Verification (fixed binary + `-Djava.io.tmpdir=/data/tmp/mockk-tmp`, KRun):
    WebTestClient 10/10, ServerRequest 29/29, ServerResponse 13/13, ClientResponse 16/16,
    RenderingResponse 1/1, RSocketRequester 13/13, WebClientObservation 9/9,
    CoExchangeFilterFunction 1/1, InvocableHandlerMethodKotlin 40/40 — all OK. A/B control: unfixed
    binary, identical flags -> 10/10 SOE.
  *   Residual: `WebClientExtensionsTests` 20/32 (SOE gone). The 12 failures are two separate,
    pre-existing families: (a) `IllegalStateException: Unable to create proxy for sealed class
    interface org.springframework.http.HttpStatusCode, no subclasses available` — mockk resolves
    `KClass.sealedSubclasses` (kotlin-reflect metadata family); (b) mockk `verify` matcher failures
    (`... was not called`), same family as the tracked `RestClientExtensionsTests` residual.
  *   Probe kit: `/data/data/wt-mockk-dispatch-20260715/probes/` — `MkProbe.java` (agent-init +
    hashCode chain with a printing `MockKAgentLogFactory`; the init TRACE lines name the exact
    failing step), `BootProbe.java` (boot-jar append + null-loader `forName`), `TmpProbe.java`
    (`java.io.tmpdir` honoring).
*   **`@Import` attribute CCE across `@CompileWithForkedClassLoader`** (`web.service.registry.ImportHttpServiceRegistrarTests`)
  *   **Status**: **FIXED** (2026-07-16, `resolve_field_ref_loader_aware` — see the "2026-07-16 update #2" section below for full verification). The original `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;` at `ConfigurationClassParser$SourceClass.getAnnotationAttributes:1119` no longer reproduces. The class still does not reach 5/5 OK — the remaining 2/5 fail on an unrelated, pre-existing, documented host-environment gap (`java.lang.classfile.ClassFile` needs JDK24+; this host only has JDK17/21).
  *   **2026-07-16/17 addendum (independent parallel investigation, unresolved flag for follow-up)**: a separate session investigating this same bug (before discovering this fix had already landed) built and stress-tested an equivalent re-entrant fix and additionally ran a memory/RSS stress check that the verification above did not cover: a single larger, loader-heavy AOT-cluster class (`BeanDefinitionMethodGeneratorTests` alone, real in-process javac, 34 test methods) grew the VM process's RSS past 4GB and was OOM-killed by the kernel (`sudo dmesg`: "Out of memory: Killed process ... (cratonvm-fieldr) total-vm:8856216kB anon-rss:4063936kB"), and the flagship repro itself ran roughly 60-100x slower than baseline (2.2s to 135-280s across runs) with an equivalent fix applied. This happened on a host that was ALSO under severe, independent, confirmed disk-full/memory pressure at the time (dmesg shows an unrelated `rustc` process OOM-killed in the same general window; both `/dev/root` and `/data/data`'s backing volume were at or near 100% full) — so it is NOT conclusively proven this fix specifically causes unbounded memory growth outside that already-degraded environment. This session's own attempt to re-verify directly against this exact landed commit was itself blocked by the same disk-full condition recurring (`cargo build` failing with "No space left on device" mid-archive-write) before it could be settled either way. Recorded here, unresolved, as a flag for follow-up: re-run the same stress scenario (a single heavy, loader-forking AOT test class, watching peak RSS over the whole run, not just pass/fail) on a host with real headroom before treating this fix as fully hardened for broad, unattended CI use.
  *   **2026-07-15 update**: reproduces SOLO in ~1s (`/data/tmp/aotfix-runs/MethodRun.java` single-method launcher on the Azure host). The 2026-07-14 SoftReference/GC-relocation hypothesis is now DOUBTED: this failure survived eight classloader-identity fixes, and three focused probes (plain `@Import` reflection, forked-loader variant, `@Import` as meta-annotation on a repeatable annotation type — `ImportProbe2.java`) all PASS. The divergence is somewhere in the full `ConfigurationClassParser`/`MergedAnnotations` path for the repeatable `@ImportHttpServices` container under a forked loader.
  *   **2026-07-15 second update — CratonVM's native annotation layer RULED OUT, narrowed to Spring's own `AnnotationTypeMapping` caching.** The failing lookup is `collectImports` examining `@ImportHttpServices` itself as a `SourceClass`, asking "is `@ImportHttpServices` meta-annotated with `@Import`?" and requesting `Import`'s `value()` attribute with `classValuesAsString=true` (Spring's `TypeMappedAnnotation.adapt()` expects a `Class[]`→`String[]` conversion here). Two CratonVM-internal traces already present in the codebase (`CRATONVM_IAE_TRACE`, `CRATONVM_ANN_PROXY_DISPATCH_TRACE` in `native-builtins/src/lang_class.rs` / `vm/src/vm/vm_exec.rs::annotation_proxy_dispatch_impl`) were used, plus one new element-level trace (temporary, reverted) confirming the FULL chain from raw classfile annotation bytes through to the reflective proxy dispatch is correct:
      - `container_loader=SOME` for both `ListingConfig` and `ImportHttpServices` (both correctly recognized as fork-loaded, so `resolve_annotation_class_via_loader` is used, not the stale global store).
      - `annotation_element_to_java_typed`'s `Array` branch builds a genuine 1-element `Class[]` array for `@Import`'s `value` (`elem_cname=java/lang/Class`, no `TypeNotPresentException` sentinel collapse — that hypothesis, and the "insufficient loader scoping" hypothesis, are both REFUTED for this specific bug).
      - `annotation_proxy_dispatch_impl`'s element-accessor walk (the code that answers a reflective `Method.invoke()` on the annotation's `$ProxyN`) returns exactly that array back to the Java caller: `[ANN-PROXY-DISPATCH-VAL] ... type_desc=.../Import; returning cid=12 name="java/lang/Class" is_array=true array_len=1` — i.e. CratonVM hands Spring a **correct** 1-element `Class[]`.

      Since the value crossing the native/Java boundary is provably correct, the bug is NOT in CratonVM's annotation-parsing or reflection-dispatch layers — it must be inside Spring's OWN `TypeMappedAnnotation`/`AnnotationTypeMapping` Java code, specifically `getValueFromMetaAnnotation`'s `useMergedValues` branch (`this.mapping.getMappedAnnotationValue(attributeIndex, forMirrorResolution)`), which is a SEPARATE retrieval path from the raw reflective `AnnotationUtils.invokeAnnotationMethod` fallback and was NOT exercised by the traces above (those traces fire on the raw-reflection path; `getMappedAnnotationValue` may resolve the value some other way — e.g. via a cached/mirrored `Method` reference — before ever reaching a `Method.invoke()` call). ~~NEXT STEP: trace (or `javap`/read) `AnnotationTypeMapping.getMappedAnnotationValue` and its mirror-set resolution to find where a correct 1-element array could become a bare `Class` — prime suspect is `Method`-object IDENTITY comparison...~~

  *   **2026-07-16 update — ROOT-CAUSED PRECISELY via live Java-source instrumentation; the `Method`-identity hypothesis above is REFUTED; a targeted VM fix was attempted and REVERTED after it caused heap corruption. Still OPEN, but now with an exact, reproducible, one-line failure signature and a well-scoped (but not-yet-safe) fix direction.**

      **Method used**: rather than guessing further from CratonVM-side traces, patched Spring's OWN source (`TypeMappedAnnotation.java`, `AnnotationTypeMapping.java`, `MergedAnnotation.java`, `ConfigurationClassParser.java` — copies under `/data/tmp/importhttpsvc-repro/patch-src/` on the Azure host, env-var-gated `System.err.println` trace calls only, no logic changes), recompiled just those files with `javac` against the existing `combined_cp.txt` classpath (`/data/tmp/aotfix-runs/combined_cp.txt`, still valid — points at `/data/data/wt-osr-other516-20260708-2131/apps/spring-framework/*/build/classes`), and reran the existing `KRun`-based repro (`/data/tmp/hib-cv-26-krun/KRun.java`, a generic JUnit5 `Launcher` wrapper — copies at `/data/tmp/importhttpsvc-repro/`) with the patched classes prepended to the classpath. This let the trace fire from inside REAL Spring bytecode at the exact failing call, both under JIT and `--nojit` (confirmed identical — this is NOT a JIT bug).

      **`getMappedAnnotationValue` hypothesis directly refuted.** Traced every `TypeMappedAnnotation.getValue`/`.adapt`/`.getTypeForMapOptions`/`AnnotationTypeMapping.getMappedAnnotationValue` call for `Import.value()` across the whole run: every single one resolves and adapts a genuine 1-element `Class[]` correctly — `Method` identity, `getReturnType()`, `Method.invoke()` results, everything checked out. The instrumented run **still reproduces the identical CCE** even with this whole path proven clean, meaning the divergence isn't there at all.

      **Actual root cause: `MergedAnnotation.Adapt` enum-constant identity split across two class-loaders.** `SourceClass.getAnnotationAttributes` → `AnnotatedTypeMetadata.getAnnotationAttributes(name, true)` → `TypeMappedAnnotation.asMap(factory, Adapt.values(true, true))`. Traced `Adapt.CLASS_TO_STRING.isIn(adaptations)` (`MergedAnnotation.java`'s `Adapt` enum, `protected final boolean isIn(Adapt... adaptations) { for (Adapt c : adaptations) if (c == this) return true; ...}` — reference-identity comparison) directly: `Arrays.toString(adaptations)` prints `[CLASS_TO_STRING, ANNOTATION_TO_MAP]` (i.e. the array DOES logically contain `CLASS_TO_STRING`), yet `isIn()` returns **`false`**. Adding an identity/loader trace inside `isIn()` itself nailed it exactly:
      ```
      this=CLASS_TO_STRING@41429 this.getClass()=...@41436 thisLoader=jdk.internal.loader.ClassLoaders$AppClassLoader@c1
        | candidate=CLASS_TO_STRING@49136 candidate.getClass()=...@49132
          candidateLoader=CompileWithForkedClassLoaderClassLoader@7324
        candidate.name()=CLASS_TO_STRING this.name()=CLASS_TO_STRING candidate.equals(this)=false ==?false
      ```
      `this` (the `Adapt.CLASS_TO_STRING` singleton `getTypeForMapOptions`'s own `getstatic` resolves to) is the **application-loader's** copy of `MergedAnnotation$Adapt`; `candidate` (built moments earlier by `Adapt.values()`, called from `AnnotatedElementUtils`/`AnnotatedTypeMetadata` processing the SAME fork-loaded `ImportHttpServices`/`Import` classes) is the **fork-loader's** copy. Two distinct, non-identical `Class` objects for the same-named nested enum, silently mixed within one logical operation — so `Adapt.CLASS_TO_STRING.isIn(adaptations)` returns `false` even though `classValuesAsString=true` was correctly threaded all the way down. `getTypeForMapOptions` then picks `Object.class` instead of `String[].class`, `getAdaptType`'s `type==Object.class` branch resolves the target type from `attribute.getReturnType()` directly (bypassing the `Class[]`→`String[]` conversion branch entirely), and the RAW `Class[]` (well-formed, correct, 1-element) ends up stored in the attributes map under `"value"` untouched — which is what `ConfigurationClassParser$SourceClass.getAnnotationAttributes`'s `(String[]) annotationAttributes.get(attribute)` then fails to cast.

      *(Note: the exact `java.lang.Class cannot be cast to [Ljava.lang.String;` — singular `Class`, not `[Ljava.lang.Class;` — wording comes from a DIFFERENT, single-attribute annotation elsewhere in the same `collectImports` recursion also hitting this same `Adapt.isIn()` bug on a scalar-`Class`-valued attribute, not from `Import.value()` itself degrading from array to scalar; the mechanism — `Adapt.CLASS_TO_STRING.isIn()` returning a false negative due to cross-loader identity — is the same either way and was confirmed via the `getTypeForMapOptions`/`adapt` traces to be the single failure point common to both call sites.)*

      **CratonVM-side root cause, pinpointed**: `resolve_field_ref` (`vm/src/runtime/interpreter.rs`, backs `getstatic`/`getfield`/`putstatic`/`putfield`) resolves a field reference's OWNING CLASS via `lookup_loader_initiated(shared, current_class_id, &field_class_name)` (a loader-aware CACHE lookup only) and, on a miss, falls straight to the flat, loader-blind `shared.load_class_concurrent(&field_class_name)` — i.e. "whichever copy loaded first, globally, wins." This is a strictly weaker resolution than `resolve_class_loader_aware` (used for `CONSTANT_Class` references — `ldc`/`new`/`checkcast`/`instanceof` — same file), which on a `lookup_loader_initiated` miss additionally drives the REFERENCING class's own defining loader's `loadClass()` re-entrantly (`drive_defining_loader_load`) before falling back globally. Since `TypeMappedAnnotation` (executing the `getstatic Adapt.CLASS_TO_STRING`) is itself fork-loaded in this scenario but its `MergedAnnotation$Adapt` field-class resolution hits the weaker path, it silently binds to the application loader's (first-loaded, globally-cached) `Adapt` class instead of its own fork's — while `Adapt.values()` (a self-referential `getstatic` from WITHIN `Adapt`'s own bytecode, always trivially correct) resolves the fork's own `Adapt` — producing exactly the observed cross-loader mismatch.

      **Fix attempted and REVERTED (unsafe): `resolve_field_ref_loader_aware`.** Added a second entry point mirroring `resolve_class_loader_aware`'s full two-tier resolution (extracted the field-lookup tail of `resolve_field_ref` into a shared `resolve_field_in_class` helper reused by both), wired it into the `Instruction::Getstatic`/`Instruction::Putstatic` opcode handlers only (the two sites with a `&mut JvmThread` available and no other call-site impact). **Rebuilt clean, but the fixed binary corrupts heap state on the very same repro**: `RESULT ... found=0 ... status=LOADERR`, with `ClassId(0)`/`class_name=java/lang/Object`/`real_field_count=Some(0)` out-of-bounds field warnings, `Stale pointer detected in invokevirtual receiver (... all-zero header)`, and a spurious `NoSuchMethodError: java/lang/Object.lambda$executeRecursively$5()V` — all symptoms of a GC-safety violation, reproduced identically on 2 separate runs. The unmodified baseline binary (same classpath, same `KRun` harness) is unaffected. Not root-caused further this session — best-supported hypothesis: `Instruction::Getstatic`/`Putstatic`'s dispatch loop does not currently expect a nested, potentially-GC-triggering Java call (`drive_defining_loader_load`'s re-entrant `ctx.invoke_virtual(loader_obj, "loadClass", ...)`) to happen THIS early/THIS often in ordinary field access — unlike `new`/`checkcast`, which are rarer and already tolerate a nested resolve — so some GC-root-publishing or safepoint invariant that the `CONSTANT_Class` opcodes already satisfy is being skipped for the field-opcode path. Reverted cleanly (`git checkout -- vm/src/runtime/interpreter.rs` in the worktree, no commit was made); zero risk to `dev`.

      **Repro assets** (Azure host `20.83.144.174`, persisted, reusable — none of this needs to be regenerated): `/data/tmp/importhttpsvc-repro/` — `KRun.java`/`MethodRun.java`/`ImportProbe3.java` (drivers), `combined_cp.txt` (full spring-framework test-runtime classpath, `javac`/`java` argument-list-too-long on this classpath size — use `@run_args_clean.txt`-style `javac`/VM argfiles, NOT `-cp`/`CLASSPATH` directly, both blow past Linux's combined argv+envp limit), `patch-src/org/springframework/{core/annotation/{TypeMappedAnnotation,AnnotationTypeMapping,MergedAnnotation}.java,context/annotation/ConfigurationClassParser.java}` (instrumented copies, env-gated on `CV_TMA_TRACE`/`CV_CCP_TRACE`/`CV_ADAPT_TRACE`), `patch-out/` (compiled instrumented classes — prepend to classpath, ahead of `combined_cp.txt`, to reproduce the traces). Baseline (unfixed) binary: `/data/tmp/cratonvm-aotfix10-postmerge.bin`. My attempted-and-reverted fix's full diff (for reference, do not reapply as-is — it regresses): `/data/tmp/importhttpsvc-repro/adapt-fix.diff`.

      **Next step for whoever picks this up**: the diagnosis is solid and the fix DIRECTION (make `getstatic`/`putstatic`'s field-owning-class resolution as loader-faithful as `CONSTANT_Class` resolution already is) is very likely correct — what's missing is making the re-entrant `loadClass()` invocation safe from inside the field-opcode fast path. Two directions worth trying: (a) find and satisfy whatever GC-safety precondition `resolve_class_loader_aware`'s existing callers (`ldc`/`new`/`checkcast`/`instanceof` opcode handlers) already establish before calling it, and replicate it in the `Getstatic`/`Putstatic` handlers; or (b) avoid the re-entrant call entirely — mirror the EXISTING, already-safe, non-reentrant `retarget_instance_field_to_receiver` pattern (same file) for STATIC fields: resolve the field the OLD (fast, safe) way first, then — only if the referencing class is user-loader-owned — do a purely in-memory, non-reentrant re-check against `class_manager`'s already-known loader→class registrations (no `loadClass()` invoke at all) and retarget if a same-named-but-different class is found for that loader, accepting that a genuinely cold (never-yet-resolved) case might still fall through uncorrected rather than risk unsafe re-entrancy.

      **2026-07-16 update #2 — FIXED.** `resolve_field_ref_loader_aware` (mirroring `resolve_class_loader_aware`'s two-tier resolution, extracted a shared `resolve_field_in_class` tail) wired into `Instruction::Getstatic`/`Instruction::Putstatic`, exactly this doc's "Next step" direction (a). Re-verified from a fresh `origin/dev` checkout rather than reapplying the old diff blindly. The previous session's heap-corruption blocker **no longer reproduces**: current `dev` already includes `fb15be63` ("GAP_FILLER_CLASS_ID not special-cased in new young-GC exact-walk loops" — the same "mass stale-pointer/all-zero-header" symptom shape the reverted attempt hit), landed independently the same day by a different session. With that GC bug closed, the identical `resolve_field_ref_loader_aware` mechanism builds clean and runs clean: multiple solo runs plus `CRATONVM_DBG_STALE_OBJREF=1 CRATONVM_DBG_STALE_OBJREF_CYCLES=8` (the multi-cycle quarantine assertion, since a 1-cycle-late stale read is the known blind spot of the default) all show zero corruption warnings.

      `ImportHttpServiceRegistrarTests`'s `ClassCastException` is **gone** — confirmed absent across every run (baseline vs. fixed vs. post-merge, ~10 total runs). The class still does not reach 5/5: both `basicListingWithAot`/`basicScanWithAot` now fail on `java.lang.NoClassDefFoundError: java/lang/classfile/ClassFile` — the SAME pre-existing, already-documented host-environment gap as `PersistenceManagedTypesBeanRegistrationAotProcessorTests` above (JDK 24+'s Class-File API; this Azure worktree host's `java` on PATH is JDK 21.0.11, confirmed via `java -version`). Not a regression — the next-layer-down gap this fix's correctness improvement now lets the test reach.

      **Merge-time finding (unrelated, NOT caused by this fix):** post-merge with same-day `origin/dev`, `ImportHttpServiceRegistrarTests` intermittently instead shows `java.util.ServiceConfigurationError: Provider org.junit.support.testng.engine.TestNGTestEngine could not be instantiated` on the same two methods, and `cargo test -p cratonvm-vm --lib` shows one additional failure (`runtime::interpreter::tests::buffered_input_stream_real_jdk_uses_its_own_bytecode`, "BufferedInputStream.read()I must keep its real-JDK bytecode") beyond the 16 pre-existing `lock_order`/`skip_list` ones. **Both A/B-isolated via `git revert --no-commit` of this fix's own commit on top of the same merge**: both reproduce identically with this fix present OR reverted — caused by one of the OTHER commits that landed on `dev` the same day (package-private access enforcement, `f62f1772`, is the leading suspect for both — TestNG's engine and `BufferedInputStream` dispatch both cross a loader/native-dispatch boundary), not by this change. Worth a follow-up investigation by whoever owns that area; out of scope here.

      Verified regression-free: `cargo test -p cratonvm-vm --lib` 2200 passed/16 failed/111 ignored on the pre-merge branch, **byte-identical failing-test list with vs. without this fix** (`git stash`-isolated A/B). `cargo test -p cratonvm-native-builtins --lib`: 3000 passed/0 failed. Spot-checked `GroupsMetadataValueDelegateTests` and `ConfigurationClassPostProcessorAotContributionTests` (same loader-identity investigation cluster) — both are extremely slow AOT/compilation-heavy tests that don't complete within a 200s per-class ceiling on this host EITHER WAY (confirmed identical timeout behavior baseline vs. fixed, 2 runs each) — pre-existing host/AOT-overhead characteristic, not a regression.

      Repro assets at `/data/tmp/importhttpsvc-repro/` on the Azure host remain valid for any follow-up (binaries added this session: `cratonvm-baseline-20260716-192222` (pre-fix), `cratonvm-fixed-20260716-192222` (fix, pre-merge), `cratonvm-postmerge-20260716-192222`/`cratonvm-revertcheck-20260716-192222` (post-merge fix/no-fix pair used for the ServiceConfigurationError + BufferedInputStream A/B isolation above).
*   **STOMP Message Hang / premature close** (`web.socket.messaging.StompWebSocketIntegrationTests`)
  *   **Status**: **FIXED (2026-07-17)** — the premature-close/EPIPE symptom that made this class bucket as TIMEOUT is resolved. Root cause: `AsynchronousSocketChannel.write(ByteBuffer)`'s Future-based completion path (`native-io/src/async_socket.rs::deliver_future_completion`, the `FutureOutcome::Count(n)` arm) never advanced the *source* `ByteBuffer`'s `position` after a successful write, unlike the sibling read-completion arm (`FutureOutcome::Bytes`, which correctly calls `write_into_buffer_and_advance`). This violates `AsynchronousByteChannel.write`'s documented contract ("the buffer's position is updated to reflect the bytes written"). Tomcat's own client-side WS write path (`WsRemoteEndpointImplBase`/`WsRemoteEndpointImplClient`, used as the `client=Standard` parameterization against BOTH the Jetty and Tomcat servers) follows the standard `while (buffer.hasRemaining()) channel.write(buffer).get()` idiom — since `position` never advanced, the buffer looked permanently unwritten, so the client kept re-submitting and physically re-sending the SAME STOMP `CONNECT`-wrapped WebSocket frame. The server correctly rejects each redundant `CONNECT` with Spring's own (working-as-designed) `IllegalStateException: Session already exists` guard and closes — this is the "STOMP Message Hang"/premature-close symptom chased across the 2026-07-14 through 2026-07-16 sessions below.
      **Fix**: `deliver_future_completion`'s `FutureOutcome::Count(n)` arm now advances the resolved source buffer's `position` field by `n` (mirroring the read arm's existing pattern) whenever `n > 0`; `Count(n)` is otherwise only ever produced with `n == 0` (two pre-existing early-outs — an empty write buffer, and `aio_asc_read_future`'s zero-capacity destination case), so the fix is a no-op there. One-line-of-substance change plus explanatory comment, `native-io/src/async_socket.rs`.
      **Evidence chain that pinned it**: reproduced fresh (Spring Framework `main`/7.1.0-SNAPSHOT re-cloned this session after the prior checkout was lost to host disk-space churn) with two new opt-in diagnostics added to `native-io/src/socket_channel.rs::sc_read` under the existing `CRATONVM_DBG_SC_READ` env var: (1) an FNV-1a hash + hex dump of every real (`n>0`) read's bytes, and (2) a Java-side stack capture (`NativeContext::capture_stack_trace`) the first time a read's hash repeats the immediately-preceding read on the same channel id. This showed, for the Jetty parameterization, a single physical server-side read of `n=2345` bytes that was **exactly 67 back-to-back repeats of the same 35-byte masked WebSocket CONNECT frame** (`2345 = 35×67`, byte-identical repeat verified programmatically) — i.e. the duplication was real bytes-on-the-wire, not a read-side re-delivery artifact, immediately reorienting the investigation from the read/accept/close path (where the prior sessions had been looking) to the *write* path. For the Tomcat parameterization, the repeat-stack diagnostic captured the exact call site re-issuing the redundant write's eventual read on the server: `WsFrameServer.onDataAvailable` (`WsFrameServer.java:86`) → `NioSocketWrapper.fillReadBuffer`/`.read` (`NioEndpoint.java:1566`/`1452`) → `NioChannel.read` (`NioChannel.java:176`, our `sc_read`) — confirming the same 35-byte frame arriving repeatedly is genuine Tomcat-side WebSocket frame processing of genuinely-repeated wire bytes, not a Rust-side artifact. Root-caused to the write side by reading `native-io/src/async_socket.rs::aio_asc_write_future`/`deliver_future_completion` end-to-end and finding the missing buffer-advance by inspection once the write side was correctly implicated.
      **Verified**: rebuilt and reran `sendMessageToController` against both parameterizations with `CRATONVM_DBG_SC_READ=1` — each connection now shows exactly ONE 35-byte CONNECT frame read (no more repeats), and the connection close moved from within tens-of-ms-to-2s of the handshake (the original bug) to ~20s later (Tomcat's `blockingSendTimeout`), with no more EPIPE/"Broken pipe" anywhere in the log. Regression-clean: `cargo test -p cratonvm-native-io --lib --release` 349/349 passed; `cargo test -p cratonvm-vm --lib --release` 2200 passed/17 failed/111 ignored — matches this doc's own pre-existing baseline (`2200 passed/16 failed/111 ignored`, see the `@Import` attribute CCE entry above) to within one flaky test; every failure is `jit::skip_list::*`/`runtime::lock_order::*`/one `interpreter::*` test, all pre-existing release-vs-debug-build environment artifacts (the `lock_order` tests literally assert `debug_assertions` is on) unrelated to this change. Spot-checks against the fixed binary: `WebSocketConfigurationTests` 4/4 OK (matches documented baseline); `WebSocketHandshakeTests` **6/6 OK** — an *improvement* over the previously-documented 4/6 (the pre-existing "Blocking write timeout" failures there were very likely the same buffer-advance bug, now incidentally also fixed).
      **Known residual, NOT yet investigated**: `sendMessageToController` itself still FAILs (both parameterizations) — but now via `controller.latch.await(10, SECONDS)` timing out (the test's own STOMP `SEND`-to-controller latch), not the premature-close/EPIPE symptom this fix targeted. This could be a genuinely separate, deeper STOMP message-routing defect, or an artifact of this session's from-scratch classpath reconstruction (the original Spring Framework checkout + dependency jars were lost to a disk-space cleanup mid-session and were rebuilt via a fresh `git clone` + targeted Gradle build); it was not chased further this session. Whoever picks this up next should re-verify against a known-good classpath before assuming it's a new CratonVM bug.
  *   **2026-07-16 second follow-up — superseded by the 2026-07-17 fix above, kept for history**: root-caused precisely: the server dispatches the client's single STOMP `CONNECT` frame to `StompSubProtocolHandler.handleMessageFromClient` **more than once**, tripping Spring's own (correct, by-design) `IllegalStateException: Session already exists` guard, which sends a STOMP `ERROR` frame and calls `session.close(CloseStatus.PROTOCOL_ERROR)` — this, not an HTTP/1.1 keep-alive/"must-close" misfire, is what closes the just-upgraded connection. See the **2026-07-16 second follow-up** section in `CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md` for the full evidence chain (this was the correct diagnosis; the 2026-07-17 session found and fixed the exact mechanism producing the duplicate frame).
  *   **2026-07-16 update** (superseded below, kept for history): the class does not hang forever. Isolated to just the `sendMessageToController` sub-test (the other 7 `@ParameterizedWebSocketTest` methods commented out in a scratch copy) it finishes in ~30s with a normal FAIL, both parameterizations (`server=Jetty` and `server=Tomcat`, both `client=Standard`). Root cause (at the time): the client's very first write after the WS handshake -- the STOMP `CONNECT` frame -- gets a genuine OS-level EPIPE/"Broken pipe" (confirmed via `AsynchronousSocketChannel`'s real, non-synthetic Future-write path, `native-io/src/async_socket.rs::aio_asc_write_future`). Tomcat's own client-side `blockingSendTimeout` (default 20000ms) is what turns this into an assertion failure rather than a true hang -- 20s x 16 parameterizations (8 methods x 2 servers) in the full class comfortably exceeds the suite's 600s per-class ceiling, which is why it buckets as TIMEOUT. **This fully explains the TIMEOUT symptom without any VM-level concurrency bug** (the 2026-07-14 "hang" gdb snapshot below just caught the process mid-run on a live, correctly-timed wait; `LockSupport.parkNanos`/`parkUntil`'s timeout plumbing was re-audited and is correct).
      A new opt-in diagnostic, `CRATONVM_DBG_SC_CLOSE=1` (`native-io/src/socket_channel.rs::sc_close`, commit `dd1cddec`, merged to `dev`), traced every real `SocketChannel.close()` with local/peer address + timestamp and correlated it against a millisecond-stamped client-side trace: **the embedded Tomcat/Jetty server closes its just-accepted connection within ~40ms-2s of completing the WS upgrade handshake**, for BOTH server backends, before the client's first post-handshake frame write. `SocketChannel.close()` belongs exclusively to the server-side servlet-container transport (the client uses a separate `AsynchronousSocketChannel` native module), so this unambiguously implicates the **server** side closing a just-upgraded connection -- ~~most likely each container's standard HTTP/1.1 keep-alive/"must-close" determination (normally suppressed for a `101 Switching Protocols` hand-off) firing because CratonVM doesn't correctly preserve whatever signal Tomcat/Jetty rely on to recognize the connection was upgraded~~ **this hypothesis is REFUTED, see the 2026-07-16 second follow-up below.** `WebSocketConfigurationTests` (same base class, but doesn't write a message right after the handshake) is unaffected/regression-clean (4/4 OK, confirmed again this session); `WebSocketHandshakeTests` shows unrelated pre-existing `Blocking write timeout` failures (4/6) not investigated further this session and NOT a regression from any change made here (see the second follow-up's regression-check note).
      **2026-07-16 second follow-up: ROOT-CAUSED.** Added a Java-level stack capture at the moment of `sc_close` (`NativeContext::capture_stack_trace`, no `Thread` handle needed — see the diagnostic commit) and instrumented Spring's own `StompSubProtocolHandler` source directly. Finding: the client writes its `CONNECT` frame exactly once (confirmed via client-side trace), but the server's `handleMessageFromClient` is invoked **twice** (Jetty) to **4000+ times** (Tomcat, a genuine redelivery spin) for that single frame, each time independently decoding a byte-identical "CONNECT" payload. The second (and later) deliveries hit Spring's `Assert.state(prevInfo == null, "Session already exists")` in the `isConnect` branch, which is **working exactly as designed** — the actual defect is the duplicate delivery, not Spring's reaction to it. Full evidence and next steps: `CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`'s "2026-07-16 second follow-up" section. **Not fixed this session** — the duplicate-dispatch mechanism differs by backend (Jetty: two independently-constructed message objects, `sc_read` called exactly twice before either dispatch; Tomcat: thousands of redeliveries of the SAME cached message object) and pinning the exact Rust call site responsible needs more live-debugging time than was available.

---

## 2. AOT / In-Memory Javac Cluster — LARGELY RESOLVED (2026-07-15)

The 11-class AOT cluster turned out to be a FAMILY of **classloader-identity
bugs**: `@CompileWithForkedClassLoader` re-defines the whole framework in a
fork loader (and every `TestCompiler` compile uses a fresh
`DynamicClassLoader`), so any name-global lookup inside the VM could resolve
the WRONG same-named copy. Eight fixes landed on
`fix/spring-aot-cluster-20260715` (commits `7c5aa7ce`, `5833302e`):

1. Link-time Pass-3 re-verification removed (name-indexed adapter threw
   spurious `VerifyError`s; define-time Pass 3 is loader-aware and
   authoritative). Also ~2x faster on javac-heavy tests.
2. `LambdaCallSite` records the loader-resolved functional-interface
   `ClassId` at indy bootstrap; non-SAM (default) interface methods on lambda
   proxies dispatch through it (fork-side `ArgumentCodeGenerator.and()`
   chains no longer produce app-side javapoet `TypeName`s).
3. `loader_namespace_id` keyed by loader OBJECT with GC reconcile (identity
   hashes recur; fresh per-compile loaders inherited dead siblings'
   namespaces -> stale `Test__Injector` CCEs).
4. Field/method/parameter annotation TYPES resolve through the declaring
   class's loader (fork-side `@Autowired` members no longer materialize
   app-loader annotation types that fail identity comparisons).
5. Jars appended via `Instrumentation.appendToBootstrapClassLoaderSearch`
   are recorded; the synthetic-mode loadClass defer-gate serves them via
   parent delegation.
6. `defineClass1` returns the already-defined BOOTSTRAP copy for
   appended-jar classes (Mockito `MockMethodDispatcher` null-loader assert).
7. `Class.forName` on Spring's `DynamicClassLoader` reroutes to the parent
   ONLY when the parent is the forked test loader (CGLIB `$$SpringCGLIB$$`
   classes defined into a plain DCL were CNFE-invisible).
8. JVMS 5.3 chain-scoped flat-store fallback in the synthetic-mode base
   delegation.

**Full-cluster validation (2026-07-15, one VM per class, real JDK 25, JIT on,
900 s watchdogs, results `/data/tmp/aotfix-runs/V7_shard*/results.tsv`):**

| class | doc baseline (2026-07-14) | now |
|---|---|---|
| `AutowiredAnnotationBeanRegistrationAotContributionTests` | FAIL 14/1 | **OK 14/14** |
| `CommonAnnotationBeanRegistrationAotContributionTests` | FAIL 8/1 | **OK 8/8** |
| `BeanDefinitionPropertiesCodeGeneratorTests` | FAIL 47/0 | **OK 47/47** |
| `InstanceSupplierCodeGeneratorTests` | FAIL 26/0 | **OK 26/24 (2 skip)** |
| `BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | LOADERR/ABEND | **OK 44/44** |
| `DefaultBeanRegistrationCodeFragmentsTests` | (was fixed) | **OK 19/19** |
| `GroupsMetadataValueDelegateTests` (WritableContent residual) | FAIL | **OK 8/8** |
| `ScopedProxyBeanRegistrationAotProcessorTests` | FAIL (3 methods) | **OK 5/5** |
| `PersistenceManagedTypesBeanRegistrationAotProcessorTests` | FAIL | FAIL 2/0 (host lacks JDK 24+, see below — not a VM bug) |
| `TestClassScannerTests` | TIMEOUT 600 s | **completes 177 s** (7/7 or flaky 7/6) |
| `TestCompilerTests` | TIMEOUT 600 s+ | **completes 40 s**, **OK 22/22 (2026-07-16, all 4 residuals now fixed)** |
| `ApplicationContextAotGeneratorTests` | ABEND (CGLIB load) | **2026-07-16 joint verification: LOADERR FIXED** — `found=40 succ=25 fail=15`, 0 corruption-signature lines, see below |
| `BeanDefinitionMethodGeneratorTests` | FAIL 34/3 | **OK 34/34** |
| `ConfigurationClassPostProcessorAotContributionTests` | FAIL 20/8 | **OK-ish 20/15/5** (5 residual = host ClassFile gap, see below) |
| `PersistenceAnnotationBeanPostProcessorAotContributionTests` | FAIL 8/0 (NCDFE) | **2026-07-17 re-triage: FAIL 8/3/5** — the two previously-documented failure modes ((a) cold-attach, (b) ByteBuddy "Cannot resolve T") are CONFIRMED GONE on current dev tip; a third, distinct, NOT-yet-root-caused AssertJ reflection residual now blocks the remaining 5, see below |
| `TestContextAotGeneratorIntegrationTests` | FAIL 4/0 @393 s | **2026-07-16 re-triage: FAIL 4/0 @8.3 s** (was a genuine ~393 s slowdown, now fast; 4 distinct root causes, see below) |
| `BeanRegistrationsAotContributionTests` | TIMEOUT | TIMEOUT (throughput, see below) |

### Remaining OPEN residuals in the AOT cluster

*   `BeanRegistrationsAotContributionTests` — **TIMEOUT even at 1500 s**, 100%
    CPU, steadily progressing (NOT a deadlock). Stack samples put the main
    thread repeatedly in Mockito's inline-mock-maker constructor interception
    (`InlineDelegateByteBuddyMockMaker.lambda$new$2/3`) plus GC frame-root
    scanning — an interpreter-throughput problem under constructor
    instrumentation, needing perf work rather than a correctness fix.
*   ~~`ConfigurationClassPostProcessorAotContributionTests`~~ **FIXED
    (2026-07-15, commit `aca7f635`).** `BeanRegistrarTests`'
    `applyToWhenIsPackagePrivate`/`applyToWhenIsPackagePrivateAndImportAware`
    threw `IllegalArgumentException: Could not generate code for
    <com.example.TestTarget__TestCode>::applyBeanRegistrars: parameter 0 of
    type org.springframework.beans.factory.ListableBeanFactory is not
    supported` under `@CompileWithForkedClassLoader`. Root cause:
    `Vm::declaring_class` (`vm/src/vm/vm_exec.rs`, backs
    `Class.getEnclosingClass()`/`getDeclaringClass()`) resolved the outer-class
    name via a FLAT, GLOBAL, name-only lookup that never considered the
    INNER class's own defining loader — so `BeanRegistrarTests` (correctly
    fork-loaded) got back the STALE, original app-loader copy of its
    enclosing class, and everything transitively touched through it
    (`ConfigurationClassPostProcessor`, `ListableBeanFactory` itself, …)
    inherited that stale identity, producing two different
    `ListableBeanFactory` copies for Spring's javapoet-based
    `ArgumentCodeGenerator` matching to reconcile. This completely bypassed
    the `resolve_class_loader_aware` / `CRATONVM_LOADER_AWARE_RESOLUTION`
    machinery that already correctly handles ordinary bytecode-level
    `ldc`/`checkcast`/`new` class references.

    Fixed in `native_class_get_declaring_class` (`native-builtins/src/lang_class.rs`),
    which already has a mutable `NativeContext` (unlike the `Vm::declaring_class`
    trait method it calls): checks first whether the class has a registered,
    eligible defining loader, then — only when the existing global answer's
    own defining loader DIFFERS from the class's own — resolves the
    enclosing-class name by invoking that loader's OWN `loadClass()`
    (mirrors `drive_defining_loader_load`'s re-entrant-call pattern). A cheap
    same-loader short-circuit keeps the cost near-zero for the overwhelming
    common case; an earlier version without it caused a severe slowdown by
    invoking `loadClass()` on every `getEnclosingClass()` call for any
    user-defined-loader class.

    Verified: both `ListableBeanFactory` failures fixed (now hit the
    pre-existing, unrelated `java.lang.classfile.ClassFile` host JDK-version
    gap instead — confirmed via A/B testing not a regression). Full class:
    14→15 succeeded, 6→5 failed, remaining failures identical in nature
    between pre/post-fix baselines. Regression-checked
    `DefaultBeanRegistrationCodeFragmentsTests`/`ThrowawayClassLoaderTests`/
    `PersistenceManagedTypesBeanRegistrationAotProcessorTests` clean. Broader
    regression coverage was inconclusive at time of testing due to severe,
    unrelated host I/O contention (confirmed via A/B comparison that the
    slowness reproduces identically pre-fix) — worth a spot-check when the
    host is less loaded.
*   `PersistenceAnnotationBeanPostProcessorAotContributionTests` — **2026-07-16
    dedicated re-triage.** Rebuilt fresh off dev tip `6c517cd9` (already
    includes the same-day ByteBuddy repeat-redefine fix `c812b622`/`a2515075`)
    in an isolated worktree/binary (`/data/data/wt-persistannobpp-20260716-141911`,
    Azure host), reusing the official `apps/spring-suite-runner` classpath/`KRun`
    harness unmodified.

    **Step 1 finding: a NEW, unrelated GC crash was hiding the real residual.**
    6/6 clean repro attempts against the `6c517cd9` binary hit a hard failure
    *before* the class ever reached Mockito: `LOADERR`,
    `ClassCastException: java.lang.Object cannot be cast to
    org.junit.platform.engine.TestExecutionResult$Status`, preceded by a burst
    of `cratonvm::gc::guard` "out-of-bounds field read/write dropped"
    all-zero-header warnings and `Stale pointer detected in invokevirtual
    receiver ... falling back to CP class com/sun/tools/javac/...` during
    `TestCompiler`'s real in-process `javac` compile step (the FIRST of the
    six `testCompile()`-calling test methods; `@CompileWithForkedClassLoader`
    forks a brand-new `ClassLoader` and does a real javac compile PER test
    method, an unusually allocation/GC-heavy path run 6×). With
    `CRATONVM_DBG_STALE_OBJREF=1` this became a clean, reproducible hard panic
    pinpointing `native_hashmap_get_exact`/`map_keys_equal`/`unbox_wrapper`,
    reached from `com/sun/tools/javac/util/StringNameTable.fromString`. This
    is the SAME long-tail "Family-1 stale-ObjectRef" bug class tracked in
    `docs/internal/wildfly-parallel-boot-stale-objectref-residual.md`
    (specifically matching that doc's still-open "Follow-up session 6" finding
    #2 — a freshly-pinned `ObjectRef` stale on first dereference with no
    obvious missing pin) — corroborated by an independent, concurrent
    2026-07-16 re-triage of the sibling class `ApplicationContextAotGeneratorTests`
    (see below) hitting the *identical* symptom shape via the same TestCompiler
    path. **Not fixed by this session** — instead, an unrelated concurrent
    session's dev commit `fb15be63` ("fix(gc): GAP_FILLER_CLASS_ID not
    special-cased in new young-GC exact-walk loops", landed 2026-07-16 while
    this investigation was in progress) turned out to be the actual fix: its
    description ("misparsing the TLAB gap-filler sentinel broke the young-GC
    exact object-start walk early, leaving everything allocated afterward
    outside the exact set — `mark_young` silently drops those live objects and
    the non-moving sweep reclaims them as garbage, i.e. mass stale-pointer/
    all-zero-header corruption") matches this class's symptom precisely.
    Rebuilt at dev tip `fb15be63` and reran: **0/3 repro attempts hit the GC
    crash** (previously 6/6); the class now completes in ~150 s (previously
    hung ~150 s before crashing, or crashed in seconds under
    `CRATONVM_DBG_STALE_OBJREF`) and returns to the documented **8/2/6**
    baseline shape. Not this session's fix; attributing correctly rather than
    claiming credit.

    **Step 2: with the GC crash out of the way, the residual is (a) 1
    pre-existing cold-attach failure + (b) 5 ByteBuddy generics-resolution
    failures — exactly the previously-documented split, confirmed by exact
    stack trace this time.** (a) `processAheadOfTimeWhenCustomPersistenceUnitOnPublicSetter`
    (whichever forked test executes first) fails with
    `IllegalStateException: Could not initialize plugin: MockMaker` bottoming
    out at `org.mockito.internal.PremainAttachAccess.getInstrumentation` ->
    `ByteBuddyAgent.install` -> `IllegalStateException: The Byte Buddy agent is
    not initialized or unavailable`. This is the SAME cold-self-attach quirk
    already characterized via the standalone `BBProbe4.java` repro (fork #1
    fails cold, forks #2+ succeed) — confirmed by design here too: because
    `CompileWithForkedClassLoaderExtension.runTestWithModifiedClassPath`
    creates a **brand-new `ClassLoader` per test method** (not once per class)
    whose parent skips the original test loader entirely, every one of the 6
    mock-using tests re-resolves `org.mockito.internal.PremainAttachAccess`
    fresh — yet only the FIRST one to actually reach `Mockito.<clinit>` hits
    the cold-attach failure; the other 5 succeed the attach step. This means
    the "warm-up" that makes forks 2+ succeed in `BBProbe4` is **VM-process-level
    state** (the underlying native self-attach mechanism's own one-time lazy
    setup), not anything cached at the Java `PremainAttachAccess`/`ClassLoader`
    level — confirming this is exactly the same pre-existing, unrelated,
    already-characterized quirk, not something specific to this class's fork
    mechanism. Regression-checked clean against `BBProbe4` directly (fork 1
    cold-attach fails, forks 2-4 OK — unchanged).

    (b) The other 5 tests fail with `MockitoException: Mockito cannot mock
    this class: interface jakarta.persistence.EntityManagerFactory` /
    `Underlying exception: IllegalArgumentException: Cannot resolve T from
    class ...EntityManagerFactory$MockitoMock$...`, via
    `net.bytebuddy.description.TypeVariableSource$AbstractBase.findExpectedVariable`
    called from `Transformer$ForMethod$TransformedMethod$AttachmentVisitor.
    onTypeVariable` during `MethodRegistry.compile`'s bridge-type resolution.
    `EntityManagerFactory.<T> T unwrap(Class<T>)` is the trigger: a
    METHOD-scoped (not class-scoped) type parameter.

    **Root-caused (partially) and one real, independently-valuable bug fixed
    along the way, but the ByteBuddy crash itself is NOT resolved.** Found via
    a standalone reflection probe that `Method.getTypeParameters()`
    (`native_method_get_type_parameters`, `native-builtins/src/lang_class.rs`)
    built a brand-new synthetic `TypeVariable[]` on every call with no
    memoization, unlike real JDK's `Executable.getTypeParameters()` (cached
    per-Method `genericInfo`) — so `unwrap.getGenericReturnType()`'s "T" and
    `unwrap.getTypeParameters()[0]`'s "T" were two DIFFERENT objects
    (`==` false) instead of identity-equal as on HotSpot, which is exactly the
    kind of break `generics.rs`'s existing identity-preserving
    `resolve_declared_type_variable` machinery (added for an earlier, related
    "Cannot resolve T" fix, see the ByteBuddy repeat-redefine entry below) was
    designed to prevent. **Fixed** (dev commit `4cb070e5`,
    `fix(reflect): Method/Constructor.getTypeParameters() now
    identity-stable across calls`): caches the built array keyed by (VM
    instance, declaring class, method name+descriptor), kept alive/remapped
    across moving GCs via the existing `register_var_handle_root`/
    `read_var_handle_root` permanent-native-root mechanism (built for
    VarHandles, generic over any `ObjectRef`). Verified via the standalone
    probe (identity mismatch before, `==` true after);
    `cargo check -p cratonvm-native-builtins` clean pre- and post- the
    `origin/dev` merge; `cargo test -p cratonvm-native-builtins --lib`
    2999/0/6-ignored (matches baseline); `BBProbe4` regression-checked clean.

    **However, re-running the full class after this fix showed the "Cannot
    resolve T" failures UNCHANGED (still 5/5)** — this fix, while real and
    correct, is not what ByteBuddy actually consults on this path. Traced one
    level deeper via the real ByteBuddy 1.18.8 bytecode
    (`Transformer$ForMethod$TransformedMethod$AttachmentVisitor.onTypeVariable`):
    it first checks `TransformedMethod.getTypeVariables()` (the OVERRIDING
    method as ByteBuddy is building it for the mock subclass) for a
    same-named candidate, and only falls back to asking the **declaring
    TYPE** (`findExpectedVariable`, which by design only ever looks at a
    type's own declared params + its OUTER-class chain, never a method's) when
    that list is empty. On CratonVM this list comes back empty for `unwrap`,
    forcing the (structurally-guaranteed-to-fail-for-a-method-scoped-variable)
    type-level fallback; on real HotSpot it evidently does not. Confirmed via
    two more standalone probes that this is NOT a gap in CratonVM's own
    reflective Method API: both `EntityManagerFactory.class.getDeclaredMethods()`
    and `.getMethods()` correctly report `unwrap`'s own `<T>` (length 1,
    `getGenericReturnType()` identity-equal to it, post-fix) — so the gap is
    somewhere in how CratonVM's ByteBuddy-facing class/method model feeds
    `MethodRegistry`'s token-copying machinery when it builds the new
    override's OWN generic `Signature`, not in `java.lang.reflect` itself.
    **OPEN — not fixed.** Next step for whoever picks this up: instrument (or
    step through with `net.bytebuddy.dump`) exactly what
    `TypeDescription.ForLoadedType(EntityManagerFactory).getDeclaredMethods()`
    reports for `unwrap`'s `MethodDescription.getTypeVariables()` specifically
    in the context ByteBuddy's `MethodRegistry.Default.Prepared.Entry.compile`
    uses it (as opposed to a bare reflective probe, which was clean) — the
    difference is most likely in how the *token* used to build the mock's own
    override method (`MethodDescription.InDefinedShape.asTypeToken`/
    `TypeDescription.Generic.Visitor.Substitutor`) round-trips a method-scoped
    (not class-scoped) type variable, a narrower and more specific target than
    this doc's original "(b)" description.

    **2026-07-17 re-triage: (a) and (b) above are BOTH CONFIRMED RESOLVED on
    current dev tip — no longer reproduce at all.** Fresh isolated
    worktree/binary (`/data/data/wt-persistannobpp-bb2-20260717`, Azure
    host), rebuilt twice: once at dev tip `56728b1a` and again after
    fast-forwarding to `3cb39d87` (which pulled in the same-day
    `0a1ec47b` "fix(gc): close remaining unrooted-ObjectRef gaps in
    generics.rs (enum/type-var builders)" — a highly relevant-looking
    candidate given (b)'s generics/TypeVariable machinery, checked
    explicitly, see below). Both binaries gave the **same** result.

    Root-cause dead ends explored first (both refuted the "still needs a
    fix" hypothesis before the real end-to-end run was attempted): a
    standalone `EMFTypeVarProbe.java` comparing reflective
    `Method.getTypeParameters()` vs ByteBuddy `TypeDescription.ForLoadedType`
    vs ByteBuddy `TypePool` (bytecode-parsed) views of
    `EntityManagerFactory.unwrap` all agreed (1 type variable each, matching
    HotSpot exactly — the TypePool-vs-reflection divergence hypothesis from
    the "Next step" note above is REFUTED); a standalone `EMFMockProbe`/
    `EMFMockLoopProbe`/`EMFForkProbe.java` (the last replicating Spring's
    real `CompileWithForkedClassLoaderClassLoader` delegation policy exactly,
    verified line-for-line against
    `spring-core-test/src/main/java/org/springframework/core/test/tools/
    CompileWithForkedClassLoaderClassLoader.java`) ran `Mockito.mock
    (EntityManagerFactory.class)` 6× across 6 fresh fork loaders with **zero
    failures** — neither the cold-attach IllegalStateException nor the
    "Cannot resolve T" ByteBuddy crash reproduced in isolation.

    The real end-to-end confirmation: built the actual `spring-orm` test
    module (real JDK 25 at `/data/jdk25-real-20260717`, Gradle 9.6.1 via a
    shared `GRADLE_USER_HOME` at `/data/gradle-home-baseline-20260716` —
    the host's root filesystem was at 100% full/0 bytes free for most of
    this session, which breaks a bare `~/.gradle` build outright; routing
    `GRADLE_USER_HOME`/`TMPDIR` to the separate, non-full `/data` mount
    worked around it) and ran
    `PersistenceAnnotationBeanPostProcessorAotContributionTests` for real
    through `KRun` against both binaries: **`found=8 succ=3 fail=5`,
    identical on both.** None of the 5 failures are (a) or (b) — the
    cold-attach `IllegalStateException` and the ByteBuddy `Cannot resolve T`
    crash are simply **gone**; 3 methods now pass outright (previously all
    were folded into the 2/6 fail/other bucket in the pre-this-session
    baseline). Most likely explanation: one or more of the loader-identity
    and reflection fixes that landed between the last dedicated re-triage
    and now (`aca7f635` declaring-class loader-aware,
    `4cb070e5`/`b50c3d54` `getTypeParameters()` identity-stability,
    `19d7f417` getstatic/putstatic loader-aware, `d5544133` panama
    `segment_address()`, `0a1ec47b` generics.rs GC-rooting, or some
    combination) fixed this as a side effect; not independently
    re-attributed to a single commit since neither (a) nor (b) reproduce in
    isolation anymore to bisect against.

    **A third, DIFFERENT, NOT-yet-root-caused residual now blocks the
    remaining 5/8.** All 5 fail with the identical shape —
    `org.assertj.core.util.introspection.IntrospectionError: No getter for
    property '<name>'` — thrown from AssertJ's own
    `PropertyOrFieldSupport.getSimpleValue()` (`extracting("fieldName")`):
    AssertJ tries the JavaBean getter first (expected to fail — none of
    these fields have public getters, that's normal/by design), then falls
    back to direct reflective field access, and **the field fallback also
    throws**, so AssertJ re-raises the original (misleading) "No getter"
    message rather than a field-specific one. Two of the five are on
    ordinary test-fixture nested classes (`DefaultPersistenceUnitMethod`,
    `DefaultPersistenceContextField`, `SeveralPersistenceContextField`,
    `DefaultPersistenceUnitField` — private fields `emf`/`entityManager`/
    `customEntityManager`), and one is on a real, non-test-fixture
    application class (`org.springframework.orm.jpa.
    SharedEntityManagerCreator$SharedEntityManagerInvocationHandler`,
    private field `properties`) — ruling out "only synthetic test fixtures
    are affected". All 5 fire from inside the `Invoker.accept()` callback,
    i.e. **after** `TestCompiler.compile()` has done a real in-process javac
    compile of the AOT-generated bean-registration code and invoked its
    generated static `apply(RegisteredBean, Object)` method reflectively on
    the target instance — the same GC/allocation-heavy real-javac-compile
    choke point flagged elsewhere in this doc as the trigger for the
    unrelated `fb15be63` GC young-walk bug and the `0a1ec47b` generics.rs
    unrooted-ObjectRef fix.

    Three targeted standalone repro attempts to isolate this in a minimal
    probe **all failed to reproduce it** (i.e. all passed cleanly on
    CratonVM): (1) a bare private field + `Field.get()` on a nested static
    class, no forking; (2) the same field access from **inside** a
    `ForkFieldProbe`-style forked classloader (private field defined by the
    fork loader, read via real `org.assertj.core.api.Assertions.assertThat
    (...).extracting(...)` loaded by the parent/system loader — i.e. a
    genuine cross-classloader reflective field read); (3) a reflective
    `Field.set()` (simulating what AOT-generated injection code does)
    immediately followed by an `extracting()` read of the same field, single
    classloader. None reproduced, so the trigger needs the **combination**
    of forked classloader + a real dynamically-`javac`-compiled-and-loaded
    third classloader layer (`TestCompiler`'s own `DynamicClassLoader`) +
    the GC pressure from that real compile — a minimal isolated repro was
    not achieved this session. Rebuilding at `0a1ec47b` (the freshest
    landed generics/GC-rooting fix, the most obviously relevant candidate)
    made **no difference** — identical `found=8 succ=3 fail=5` with the
    same 5 property names, so this is confirmed to be a genuinely different
    bug from the ones `0a1ec47b` closed, not a partial/incomplete fix of it.

    **OPEN — not fixed, not fully root-caused.** Next step for whoever picks
    this up: reproduce with `CRATONVM_DBG_STALE_OBJREF=1` and/or
    `CRATONVM_DBG_GC_STRESS` set for a run of just this class (mirroring the
    technique that cracked `0a1ec47b`/`fb15be63`) to check whether this is
    another instance of the same stale-ObjectRef-after-real-javac-compile
    family rather than a distinct reflection/access-control gap; if that's
    negative, instrument `Class.getDeclaredField()`/`Field.setAccessible()`/
    `Field.get()` (`native-builtins/src/lang_class.rs`) specifically for
    calls made against a class loaded by a `CompileWithForkedClassLoader`-
    style loader from calling code loaded by a *different* loader in the
    same process, since the one common thread across all 5 failures is that
    exact cross-loader-reflection shape post-real-compile.
*   ~~ByteBuddy repeat-redefine `NoSuchMethodError` family~~ **FIXED
    (2026-07-16, commit `c812b622`, merged to dev as `a2515075`).**
    Standalone repro (`BBProbe4.java`,
    `/data/data/aot-fix-runs-20260715/bbprobe/`): 4 sequential independent
    `ForkLoader` instances each running `Mockito.mock()` — fork 3+
    deterministically failed with `NoSuchMethodError:
    net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType
    $ForLoadedType.size()I` from inside ByteBuddy's own
    `FilterableList$AbstractBase.filter()`.

    **Root cause:** `native-builtins/src/lib.rs`'s ByteBuddy native
    compatibility shim (e.g.
    `native_bytebuddy_method_list_type_substituting_size`, registered for
    real ByteBuddy classes like
    `net/bytebuddy/description/method/MethodList$TypeSubstituting`)
    forwards `size()`/`get()` calls to an internal backing-list field.
    `bytebuddy_field_value`/`bytebuddy_set_field_value` resolved that
    field's slot via `class_name_of_id(class_id_of_object(obj))` followed
    by a name-based `resolve_field_index()` call — a lossy
    ClassId→name→ClassId round-trip. `resolve_field_index()`'s real
    implementation (`get_loaded_class_id`) is a global, loader-blind,
    name-only lookup that correctly returns `None` whenever 2+ distinct
    loaders each define their own class under the same simple name (by
    design — the same Groovy `GroovyClassLoader$InnerLoader` ambiguity
    documented on that function). ByteBuddy classes redefined fresh under
    every fork loader in `@CompileWithForkedClassLoader`-style scenarios
    hit exactly this: once 2+ forks' copies of `MethodList$TypeSubstituting`
    coexist, `resolve_field_index()` goes ambiguous and the shim silently
    fell back to a hardcoded slot number that only happened to be correct
    when no inherited fields preceded the object's own declared fields —
    reading the wrong field (`declaringType` instead of
    `methodDescriptions`) and forwarding `.size()` to it.

    **Fix:** added `NativeContext::resolve_field_index_by_class_id`
    (`native-api/src/registry.rs` trait, `vm/src/vm/vm_exec.rs` impl using
    `resolve_field_index_in_hierarchy` directly) so the shim resolves by
    the object's own exact `ClassId`, never ambiguous regardless of how
    many loaders redefine the same-named class. Verified: `BBProbe4`
    forks 2/3/4 all `Mockito.mock -> OK` (fork 1's cold-attach
    `IllegalStateException` is a separate, pre-existing, unrelated quirk).
    Regression-clean: `cratonvm-native-api --lib` 179/0,
    `cratonvm-vm --lib` 2205/9 (9 pre-existing `lock_order` debug-only
    failures), `cratonvm-native-builtins --lib` 2997/1 (1 flaky test,
    passes in isolation, unrelated), `ConfigurationClassPostProcessorAot
    ContributionTests` 20/15/5 and `TestCompilerTests` 22/21/1 both match
    their prior recorded baselines exactly.

    *Investigation note:* an earlier hypothesis (a 128-bit hash collision
    in `NativeMethodRegistry::find()`) was investigated and even
    prototyped as a fix, but was directly disproven via a verification
    trace — the "colliding" registration turned out to be a genuine,
    intentional boot-time registration for that exact class, not a
    collision — and was reverted before the real fix above was found.
*   `PersistenceManagedTypesBeanRegistrationAotProcessorTests` — **NOT a
    CratonVM bug: host environment gap.** Both `processEntityManagerWithPackagesToScan`
    and `contributeJpaHints` hit `NoClassDefFoundError: java/lang/classfile/ClassFile`
    inside `ClassFileMetadataReader.parseClassModel` when run in REAL-JDK
    mode against whatever `java` is on `PATH`. `java.lang.classfile.ClassFile`
    is a JDK 24+ finalized API (JEP 484, preview in 22/23, absent before
    that) — the Azure worktree host's only installed JDKs are 17 and 21
    (`/usr/lib/jvm/java-{17,21}-openjdk-amd64`), so this class genuinely does
    not exist there; CratonVM real-JDK mode is behaving exactly like real
    JDK 21 would. The 2026-07-14 baseline's "OK 2/2" almost certainly ran
    with a newer real JDK (or synthetic-JDK mode) available in whatever
    environment produced it. Fix is environmental (point `--java-home` at a
    JDK 24+ install, or install one) — not a code change, and out of scope
    for this document until a suitable JDK is available on the run host.
*   `InstanceSupplierCodeGeneratorKotlinTests` — **CLOSED 2026-07-15,
    already fixed by unrelated prior work; doc entry was stale.** Re-run
    against current dev (`eb5336f2`) shows `found=4 succ=4 fail=0 status=OK`
    — the `ClassCastException:
    kotlin.reflect...protobuf.SmallSortedMap$Entry cannot be cast to
    java.lang.reflect.Field / AnnotationSpec` recorded here on 2026-07-14
    no longer reproduces. To attribute the fix, re-ran the identical test
    against a from-scratch baseline binary built at `d497cf21` (the commit
    immediately before the `declaring_class` loader-awareness fix,
    `aca7f635`, landed) — it ALSO passes 4/4, ruling out `aca7f635` as the
    fix and confirming this was already resolved by some other change
    that landed between the 2026-07-14 23:15 doc entry and `d497cf21`
    (most likely one of the several native/GC/classloader fixes merged
    into dev earlier on 2026-07-15 — kotlin-reflect's metadata parsing
    walks `Class`/`Field`/enclosing-class machinery heavily, any of which
    could have been the actual fix). No code change was needed; verified
    via two independent binary builds under identical harness conditions.
*   `TestCompilerTests` — **3 of 4 residuals FIXED 2026-07-15 (commit
    `0ee485a2`).** All three `CompilationException: Unable to compile source`
    residuals (`compiledCodeCanAccessExistingPackagePrivateClassIfAnnotated`,
    `compiledCodeCanReferenceAdditionalClassInSamePackage`,
    `compiledCodeCanReferenceAdditionalClassInDifferentPackage`) shared one
    root cause: `native_javac_file_manager_list`
    (`native-builtins/src/lib.rs`, the native override backing
    `JavaFileManager.list()` for TestCompiler's in-process javac) had a
    hardcoded short-circuit returning an EMPTY list for `CLASS_PATH`
    listings of the `com`/`com.example`/`com.example.*` packages (alongside
    the legitimate `java.*`/`javax.*` bootstrap-classpath exclusion) —
    presumably added because `com.example` is TestCompiler's own scratch
    namespace for dynamically generated, in-memory-only classes. That
    blanket rule also hid genuine, pre-compiled-to-disk fixtures in the
    same package (`com.example.PublicInterface`/`PackagePrivate` in
    spring-core-test's own test-classes directory) from javac's symbol
    resolution, surfacing as "cannot find symbol: class PublicInterface"
    even though `getJavaFileForInput` (direct by-name lookup, never
    short-circuited) always found it fine — confirmed via a standalone
    `StandardJavaFileManager` probe, plus separate probes ruling out any
    lower-level `File.listFiles()`/`Files.newDirectoryStream()` bug (both
    correctly enumerate the same directory). Removed `com`/`com.example`
    from the short-circuit; only `java.*`/`javax.*` remain fast-pathed to
    empty (JVMS-correct — those never live on the application classpath).
    Verified 22/18/4 → 22/21/1; regression-checked
    `BeanDefinitionMethodGeneratorTests` (heavy TestCompiler/`com.example`
    user) clean at 34/34.

    **Remaining 1 residual — DIFFERENT, pre-existing bug — FIXED
    2026-07-16 (commits `f62f1772`, `4deeb6ce`).**
    `compiledCodeCannotAccessExistingPackagePrivateClassIfNotAnnotated`
    expects an `IllegalAccessError` when code in a fresh `DynamicClassLoader`
    (a DIFFERENT defining loader than the one that defined the
    package-private `PackagePrivate`, same package NAME but different
    runtime package per JVMS §5.4.4) accesses it WITHOUT
    `@CompileWithForkedClassLoader` — but no exception was thrown; access
    silently succeeded. This already failed with this exact `AssertionError`
    (not `CompilationException`) BEFORE the fix above, so it was unaffected
    by it — a genuinely separate investigation (runtime access control, not
    compile-time symbol resolution).

    Root cause was NOT a broken loader comparison: `check_class_access` /
    `same_runtime_package` (`classloading/src/access_control.rs`) already
    correctly implemented JVMS §5.3/§5.4.4 runtime-package identity
    (defining loader + package name), with existing unit-test coverage
    including an H5 loader-spoofing test — it was simply never CALLED from
    the bytecode interpreter's `new` handler. Only the unrelated JPMS
    module-boundary check fired, which no-ops whenever no named modules are
    registered (the ordinary classpath case, as here). Wired
    `check_class_access` into `Instruction::New`
    (`vm/src/runtime/interpreter.rs`), plus the two JIT compile-time `new`-site
    resolvers that independently re-resolve classes for their inlined/
    single-pass allocation fast paths, so a denied site now falls back to the
    interpreter's real check instead of silently baking in the inaccessible
    allocation.

    Wiring the check in immediately surfaced a SECOND, previously-invisible
    bug: `ClassManager`'s requester-less fast-path class lookup
    (`get_loaded_class_id`) falls back to returning an arbitrary lone
    user-defined loader's copy of a name when no built-in loader
    (bootstrap/extension/application) has defined it yet — a deliberate,
    useful heuristic for names with no backing `.class` file (e.g.
    `Proxy`-generated classes), but unsound as `load_class`/
    `load_class_concurrent`'s PRIMARY answer: a name that genuinely exists on
    the real classpath must resolve to its own freshly-loaded built-in-loader
    copy, never an unrelated user-defined loader's redefinition. Concretely,
    Spring's `TestCompiler` (Application loader) instantiating `new
    Problems()` was resolving to a *different* `TestCompiler$Problems`
    defined by an earlier, unrelated `@CompileWithForkedClassLoader` test's
    own `DynamicClassLoader` — same simple name, wrong runtime class. Added
    `ClassManager::resolve_fast_path_class_id`, which prefers the
    loader-faithful `get_loaded_class_id_for_requester(name, Application)`
    (built-in chain only) and falls back to the ambiguous lone-user-loader
    answer only when a real classpath scan
    (`find_class_bytes_delegated`) confirms no built-in-loader copy could
    exist — preserving `Proxy`-class resolution while fixing the
    stray-loader bug. A third, related latent gap surfaced in the same
    investigation: `ClassManager::upgrade_synthetic_class` (stub-to-real
    upgrade) refreshed every other `Class` field from the freshly parsed
    class file but never updated `loader_id`, leaving it stuck at whatever
    the synthetic stub was minted with (often Bootstrap) even after
    upgrading to real Application-loaded bytecode. Fixed alongside.

    Verified: `TestCompilerTests` 22/21/1 → **22/22**. Regression:
    `cargo test -p cratonvm-vm --lib --release` 2200 passed / 16 failed (all
    16 pre-existing and unrelated — `runtime::lock_order` tests gated on a
    debug build or an env-opt-in not set in this invocation, and
    `jit::skip_list` JIT-tier-eligibility tests, neither touching
    classloading); `cargo test -p cratonvm-native-builtins --lib --release`
    3000 passed / 0 failed. Spot-checked `CompiledTests` 14/14,
    `DynamicJavaFileManagerTests` 11/11, `DynamicClassFileObjectTests` 3/3
    (all TestCompiler-adjacent, package-private-access-heavy) clean. A
    cross-module spot-check of `BeanDefinitionMethodGeneratorTests` via this
    same ad-hoc single-class harness hit an unrelated, non-deterministic
    harness/classpath artifact (`ClassCastException`/`AbstractMethodError`)
    that reproduces identically on an unmodified dev-tip baseline binary —
    confirmed pre-existing, not a regression from this fix; the module's
    proper Gradle-driven suite run (34/34, per the entry above) is
    unaffected since it doesn't go through this improvised classpath
    composition.
*   ~~`aot.nativex.feature.ThrowawayClassLoaderTests`~~ **FIXED (2026-07-15,
    commit `56a98cc4`)**. `native-builtins/src/classloader_real.rs`'s
    `cl_real_load_class_base` — the REAL-JDK-mode counterpart of the
    synthetic-mode function fixed earlier in this doc's round 2 — had the
    same missing JVMS 5.3 chain-scoping: `new ClassLoader(null){}.loadClass(x)`
    resolved app classes directly from the flat store even though the
    loader's real parent chain never reaches a built-in loader. Ported the
    same `scoped_user_chain` gate. Full class now 2/2 OK.
*   ~~`BeanDefinitionMethodGeneratorTests`~~ **FIXED (2026-07-15, commit
    `d017aa36`).** Bisection of the
    `generateBeanDefinitionMethodWhenHasExplicitResolvableType` residual
    (`MethodRun.java` accepts N method names to run together in one process)
    showed the failure was **COUNT-dependent, not content-dependent**: 9
    preceding `TestCompiler` compile cycles before the target passed; 10
    failed, regardless of which methods supplied the 9th/10th cycle. Root
    cause: `gc_reconcile_defining_loaders`
    (`native-builtins/src/classloader.rs`) DROPS a class's defining-loader
    registry entry once that loader is collected, and `cid_visible_mirror`
    reads a missing entry as "never restricted, visible to everyone" — the
    same answer it gives a class that was never loader-scoped. Once the Nth
    cycle's `DynamicClassLoader` was collected, its generated companion class
    silently became visible to every OTHER loader, so the next cycle's
    `DynamicClassLoader` reused the stale, wrong-scenario copy instead of
    generating its own. Fixed by tombstoning pruned class-ids in a permanent
    orphaned set, checked before the live-registry lookup, so a class whose
    defining loader died stays invisible to everyone forever (matches real
    unloading semantics). Full class now 34/34 OK. This is a DIFFERENT root
    cause than originally hypothesized here — it does NOT explain the
    `PersistenceAnnotationBeanPostProcessorAotContributionTests` ByteBuddy
    `NoSuchMethodError` family (confirmed unaffected, still 8/2/6 after this
    fix); that remains open and unrelated.

*   **`ApplicationContextAotGeneratorTests` and `TestContextAotGeneratorIntegrationTests`
    — 2026-07-16 dedicated re-triage (post loader-identity-fix wave).** These two were
    the only two of the original 9-class AOT hang cluster never individually
    re-characterized after `fix/spring-aot-cluster-20260715` landed. Fresh
    build, dev tip `6c517cd9`, worktree `/data/data/wt-aotgen-triage-20260716-141727`
    on the Azure host, real JDK 25 (`~/jdk25`), `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`,
    900s+ ceilings, `KRun` single-class launcher (module testcp).

    **`ApplicationContextAotGeneratorTests` — NEW, OPEN, genuine GC/heap-corruption
    bug. Status: `LOADERR found=0/succ=0/fail=0` at 30-95 s (never even reaches test
    discovery)**, not the "discovers+runs 40 methods" the table previously implied
    (that note was never substantiated — this re-triage shows it does not hold on
    current dev). HotSpot baseline (already on file): **40/40 OK, 155.9 s**.

    100% reproducible across every configuration tried (4/4 runs, identical
    signature every time):
    - default (JIT on, 2048 MB heap) — LOADERR at ms=42593
    - `CRATONVM_DBG_STALE_OBJREF=1` (JIT on, 2048 MB) — LOADERR at ms=30318,
      **the assertion never fires** (see below)
    - `--nojit` (interpreter-only, 2048 MB) — **identical** LOADERR at ms=30318,
      same objects, same classes
    - 8192 MB heap (JIT on) — **identical** LOADERR, just later (ms=95522, more
      allocation needed before the triggering GC cycle)

    Symptom: a burst of `cratonvm::gc::guard` "out-of-bounds field read/write
    dropped" warnings (`class_id=ClassId(0) class_name=java/lang/Object
    real_field_count=Some(0)` — the classic all-zero-header signature) followed by
    `cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual
    receiver (ptr=…, all-zero header) — falling back to CP class …`, hitting
    **several unrelated JUnit Platform / javac internals almost simultaneously**
    (sub-millisecond apart): `org/junit/platform/engine/support/hierarchical/
    ThrowableCollector`, `…HierarchicalTestExecutorService`, `org/junit/platform/
    launcher/core/OutcomeDelayingEngineExecutionListener`, `org/junit/platform/
    engine/support/store/NamespacedHierarchicalStore`, and (under `--nojit`, where
    the corrupted set is even larger) `com/sun/tools/javac/main/JavaCompiler`,
    `com/sun/tools/javac/api/JavacTaskImpl`, `java/lang/String`,
    `org/junit/jupiter/engine/execution/JupiterEngineExecutionContext`. The
    cascade always ends the same way: a `NoSuchMethodError` on
    `java/lang/Object.lambda$executeRecursively$5()V` (a zeroed object dispatched
    as bare `Object`), then the launcher itself LOADERRs with `Cannot invoke
    "EngineExecutionListener.executionStarted(TestDescriptor)" because
    "this.delegate" is null`.

    **Ruled out** (both are real, already-fixed mechanisms in this exact bug
    family, confirmed NOT the cause here):
    - The June `spring-bug-10` "GC root-undercount race" (RESOLVED 2026-06-21 via
      default-on precise JIT oop maps + pin-aware shadow reload) was specifically
      a *JIT shadow-stack register-root* bug. Ruled out because this reproduces
      **identically under `--nojit`** (pure interpreter, no JIT frames, no shadow
      stack involved at all).
    - The 2026-07-16 `stream-arraylist-gc-pressure-heap-corruption` fix (commit
      `1c4aaa06`, already an ancestor of this build) closed a *card-table
      old-to-young / terminal-worker-barrier / lazy-Stream-rooting* gap
      specific to `ArrayList`/`Stream`. Ruled out because (a) that fix is
      already in this build's history and the bug still reproduces, and (b) the
      corrupted objects here are JUnit-Platform/javac internals, not
      `ArrayList`/`Stream`.
    - Simple heap pressure: ruled out by the 8192 MB run reproducing identically
      (just later).

    `CRATONVM_DBG_STALE_OBJREF=1` **not firing** is itself a data point: the
    corruption's mechanism reaches the interpreter's invokevirtual-receiver
    fallback without tripping that assertion's checkpoint, so whatever zeroes
    these objects is either outside the paths that assertion instruments, or the
    forwarding/quarantine record is already gone by the time of the bad read
    (the same ambiguity the stream-arraylist doc's original investigation
    flagged for its own unfixed residual). Given TestCompiler's in-process real
    javac shows up in the corrupted set, and the timing (a tight several-millisecond
    burst hitting many unrelated classes at once, consistent with a whole
    region/arena being invalidated rather than one stale object), the most
    promising next-step hypothesis is a GC cycle firing during/immediately after
    in-process javac's own heavy allocation that fails to root live JUnit-engine
    objects on the calling thread — but this needs the same kind of dedicated,
    instrumented investigation (hardware watchpoints, `CRATONVM_DBG_SHADOW*`-style
    tracing) that closed `spring-bug-10`, which is out of scope for this triage
    session. **Not fixed.** Repro is 100% reliable with a single `KRun` invocation
    of this class alone, real JDK 25, any heap size — no batching or multi-class
    load needed, which should make this considerably easier to bisect than
    `spring-bug-10` was.

    **2026-07-16 joint verification addendum — FIXED, confirmed by rebuild+rerun.**
    A separate, concurrent 2026-07-16 session investigating
    `PersistenceAnnotationBeanPostProcessorAotContributionTests` (see that entry
    above) hit an identical-looking stale-ObjectRef/all-zero-header crash and
    traced it to an unrelated dev commit, `fb15be63` ("fix(gc): GAP_FILLER_CLASS_ID
    not special-cased in new young-GC exact-walk loops"), landed 2026-07-16 while
    both investigations were in progress. This task was to verify that fix also
    closes *this* class's LOADERR (the two symptom writeups above are effectively
    identical: all-zero-header stale pointers hitting JUnit-Platform/javac
    internals within milliseconds, reproducing under both JIT and `--nojit`, at
    every heap size tried). Fresh worktree `/data/data/wt-gcbug-verify-20260716`,
    `origin/dev` tip `47151b27` (has `fb15be63` as an ancestor; confirmed via
    `git merge-base --is-ancestor`), full `cargo build --release` (35m48s under
    heavy host contention — unrelated to the fix, just Azure-host load), real JDK
    25, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, same `KRun` single-class launcher and
    classpath as the original triage. Result:
    ```
    RESULT org.springframework.context.aot.ApplicationContextAotGeneratorTests found=40 succ=25 fail=15 skip=0 abort=0 ms=573026 status=FAIL
    ```
    **The LOADERR is gone.** Test discovery now finds all 40 methods (matching the
    HotSpot baseline's 40/40 shape) and the full run completes end-to-end — no
    `NoSuchMethodError`, no `EngineExecutionListener`/`this.delegate` NPE, no VM
    abort. Grepping the full run log for the corruption signature (`Stale pointer
    detected`, `GC-ARRAY-GUARD`, `implausible extent`, `LOADERR`) returns **zero
    matches**. The class is slow under CratonVM (573 s vs. HotSpot's 155.9 s —
    a throughput question, not correctness, and not investigated further here) but
    otherwise behaves like a normal JUnit run. The 15 failures that remain are
    ordinary, catchable test failures, not VM-level corruption: all 15
    `FAILCAUSE`s are either `org.springframework.core.test.tools.CompilationException:
    Unable to compile source` or `org.springframework.beans.factory.aot.AotBeanProcessingException:
    Error processing bean ... failed to generate code for bean definition`, all on
    CGLIB-proxy-configuration test methods (`processAheadOfTimeWhenHasCglibProxy*`,
    `processAheadOfTimeUsesCglibClassForFactoryMethod`,
    `processAheadOfTimeExposeUserClassForCglibProxy`) — the same ByteBuddy/CGLIB
    generics-resolution AOT-codegen residual family already tracked for the
    sibling `PersistenceAnnotationBeanPostProcessorAotContributionTests` class
    above. **Not this session's fix; attributing correctly.** No code change was
    needed or made — this entry is a verification-only confirmation. Full second
    corroborating data point (in addition to the `PersistenceAnnotationBeanPostProcessorAotContributionTests`
    0/3-crashes-after / 6/6-crashes-before result) that `fb15be63` closes this
    cross-cutting young-GC exact-walk family. See
    `CRATONVM-SPRING-TIMEOUT-CLUSTER-1500S-RERUN.md`'s **2026-07-16 joint
    verification** addendum to the `ImportSelectorTests` section for the sibling
    `context.annotation.ImportSelectorTests` result (also fully clean, `9/9` pass).

    **2026-07-17 third independent re-verification — still clean, no residual
    doubt.** Assigned this class as this session's bug (before checking the doc,
    per the standing "check already-fixed first" workflow). Rebuilt from
    scratch at `origin/dev` tip `56728b1a` (`fb15be63` ancestor confirmed) in a
    brand-new worktree/binary, plus an independently-built
    `spring-context`/`spring-beans` test classpath (own `gradlew testClasses` +
    `dumpTestCp`, not reused from any other session's possibly-stale or
    disk-full-tainted artifacts — see the `BeanDefinitionMethodGeneratorTests`
    entry above for why that mattered this session). Result: `found=40 succ=25
    fail=15 status=FAIL`, ms=445465 — **identical shape** to the 2026-07-16
    joint-verification result above (same found/succ/fail split, same 5
    CGLIB-proxy `FAILCAUSE`s sampled), zero corruption-signature lines. Ran
    `cargo test -p cratonvm-gc --lib --release` (791/791 passed, matches
    baseline) and `cargo test -p cratonvm-vm --lib --release` (2200 passed, 17
    failed — 16 match the documented pre-existing lock_order/skip_list release-
    mode baseline exactly; the 17th, `runtime::interpreter::tests::
    buffered_input_stream_real_jdk_uses_its_own_bytecode`, is a newly-observed,
    unrelated failure already present on unmodified `origin/dev` — flagged
    separately, not a GC issue, out of scope here). No code change made or
    needed; this is the third independent confirmation (after the original
    `PersistenceAnnotationBeanPostProcessorAotContributionTests` verification
    and the 2026-07-16 joint-verification addendum above) that `fb15be63`
    fully and durably closes this bug for both AOT-cluster classes.

    **`TestContextAotGeneratorIntegrationTests` — genuine improvement, still
    4/4 FAIL, 4 distinct causes, none newly fixed this session.** The doc's old
    "FAIL 4/0 @393 s" data point is stale on two counts: it now completes in
    **8.3 s** (the loader-identity fix wave clearly helped enormously — this was
    a Bucket-1 "still hangs" class as recently as 2026-07-13), and re-running with
    `KRUN_STACK=1` shows the 4 failures have 4 unrelated causes:
    1. `processAheadOfTimeWithWebTests` — `ClassCastException: java.lang.Class
       cannot be cast to [Ljava.lang.String;`, same signature (same exception
       type, same shape) as the **already-tracked, already deeply-investigated,
       still-OPEN** `web.service.registry.ImportHttpServiceRegistrarTests`
       residual documented above (§1: narrowed to Spring's own
       `AnnotationTypeMapping.getMappedAnnotationValue`, NOT CratonVM's
       annotation/reflection layer). Attribution only — not re-investigated or
       re-fixed here; see that entry for the full root-cause narrative.
    2. `processAheadOfTimeWithXmlTests` — `ExceptionInInitializerError` from
       `GroovyBeanDefinitionReader.<init>` caused by `ArrayStoreException:
       arraycopy: source element at index 1 is not assignable to destination
       component type` inside `groovy/lang/GroovySystem`'s `<clinit>`. This is
       **NOT** the already-fixed `GroovySystem.<clinit>` NPE (`docs/internal/
       spring/spring-boot-groovy-indy-mockito-mock-dispatch.md`, a `Module`
       descriptor-null issue) — different exception type, different mechanism.
       Newly observed; not investigated further (no known attribution) — needs
       its own session.
    3. `processAheadOfTimeWithBasicTests` (`BasicSpringJupiterTests$NestedTests`)
       — `org.yaml.snakeyaml.parser.ParserException: while parsing a block node …
       expected the node content, but found '<block mapping start>'` reading
       `test1.yaml` (`test1:\n  prop: yaml\n`, 20 bytes). **Ruled out raw
       resource-stream truncation**: a standalone probe
       (`YamlProbe.java`, `getResourceAsStream().readAllBytes()` on the exact
       same classpath resource) returns **byte-identical** content (`len=20`,
       identical byte array) under CratonVM and HotSpot side by side — so the
       underlying classpath-resource I/O is correct. The divergence must be
       further up Spring's own read path (`Resource.consumeContent` →
       `YamlProcessor.process` → SnakeYAML's `Yaml$1.next`/`Composer`/
       `ParserImpl`), specifically in the `@CompileWithForkedClassLoader`/AOT
       context-loading path used only by this `$NestedTests` case (the sibling
       `WebTests`/`XmlTests` are top-level classes; this is the only `$Nested`
       one of the three). Leading unconfirmed hypothesis: the forked/dynamic
       classloader's resource resolution returns a stream whose read position
       has already been advanced by an earlier partial read (e.g. an AOT
       hint-scanning pass or a `Resource.exists()`/existence probe that doesn't
       get a fresh stream) — but this is not confirmed; needs tracing which
       exact `Resource`/stream object SnakeYAML actually receives. Newly
       observed; not fixed.

       ~~**FIXED (2026-07-17, branch `fix/yaml-resource-stream-20260717`).**~~
       The leading hypothesis above (stale/shared stream position) was
       **refuted** — raw resource-stream I/O was already confirmed correct
       (`YamlProbe.java`), and this session's tracing shows the real
       divergence never touches the YAML stream at all. `KRun` repro (real
       JDK 25, single-class launcher, from-scratch `spring-test` classpath
       via `./gradlew spring-test:testClasses spring-test:processTestResources`
       plus a `sourceSets.test.runtimeClasspath` dump) hit a **different,
       earlier, fully-deterministic blocker on every run**: before any of
       the 4 test methods' own bodies execute,
       `CompileWithForkedClassLoaderExtension.runTest()`'s inner
       `LauncherFactory.create()` triggers JUnit Platform's own
       `TestEngine` `ServiceLoader` discovery, which resolves
       `org.junit.support.testng.engine.TestNGTestEngine` via CratonVM's
       synthetic `ServiceLoader` reimplementation
       (`native-builtins/src/service_loader.rs`, `load_provider_class`).
       That function tried `loader.findClass(fqn)` **before**
       `loader.loadClass(fqn)`. Real `java.util.ServiceLoader` always
       resolves providers via `Class.forName(cn, false, loader)`, which is
       spec'd to invoke `loader`'s **public** `loadClass(String)` — never
       the **protected** `findClass(String)` helper directly (`findClass`
       exists to be called *by* a loader's own `loadClass()` algorithm, not
       by external callers). `CompileWithForkedClassLoaderClassLoader`
       overrides `loadClass(String)` with a special case that delegates
       `org.junit`/`org.testng` names to a *different* loader instance
       (the real test classloader); its `findClass()` override has no such
       special case and unconditionally self-defines the class. Calling
       `findClass` first bypassed that delegation, so `TestNGTestEngine`
       got self-defined into the fork loader's own namespace
       (`UserDefined(N)`) instead of the `Application` loader. Its
       `<clinit>` then `new`s the package-private sibling
       `IsTestNGTestClass`, which resolves correctly via
       `resolve_class_loader_aware`
       (`vm/src/runtime/interpreter.rs`) → `loadClass()`'s proper
       delegation → `Application` — a genuine two-different-loaders split
       for the same-named, same-package pair, which
       `classloading/src/access_control.rs`'s `same_runtime_package`
       (loader-id-aware, correctly implemented) then correctly rejects,
       throwing `IllegalAccessError: class …TestNGTestEngine cannot access
       class …IsTestNGTestClass (not public, different package)` — deep
       inside JUnit Platform's own bootstrap, aborting that
       `@CompileWithForkedClassLoader`-intercepted test-method invocation
       before its actual AOT-processing body ever ran. Confirmed via
       targeted tracing (temporary `eprintln!`s at the `define_class`,
       `resolve_class_loader_aware`, and `check_class_access` call sites,
       env-var gated, removed before commit): `TestNGTestEngine` defined
       twice — once `loader_id=Application` (outer `KRun` launcher, clean),
       once `loader_id=UserDefined(N)` per forked-loader test method (one
       fresh `UserDefined` id per `new CompileWithForkedClassLoaderClassLoader(...)`)
       — while `IsTestNGTestClass` stayed `loader_id=Application` throughout.

       Fix (`native-builtins/src/service_loader.rs`, `load_provider_class`):
       swapped the order — try `loader.loadClass(fqn)` first, `findClass`
       only as a fallback (matches real `Class.forName(cn, false, loader)`
       semantics and lets any custom `loadClass()` delegation logic run).

       Verified: `TestContextAotGeneratorIntegrationTests` KRun repro no
       longer throws `IllegalAccessError` anywhere (grep for
       `IllegalAccessError`/`TestNGTestEngine` across the full run log: zero
       hits, down from 4/4 occurrences pre-fix — one per test method, direct
       A/B against the pre-fix binary). All 4 methods now progress into
       their real AOT-processing bodies. Specifically for
       `processAheadOfTimeWithBasicTests` (this bug's assigned target): the
       SnakeYAML `ParserException` on `test1.yaml` **no longer occurs** —
       grep for `snakeyaml`/`ParserException`/`test1.yaml`/`test2.yaml`
       across the full post-fix run log: zero hits, reproducible across
       three separate rebuild-and-rerun cycles. The method now fails later,
       for a **different, unrelated** reason:
       `NullPointerException: Cannot invoke
       "org.springframework.javapoet.LineWrapper$FlushType.ordinal()"
       because "flushType" is null`, inside JavaPoet's own
       `CodeWriter.emit`/`LineWrapper.flush` while stringifying
       AOT-generated source for `TestCompiler.with(...)`'s in-memory
       compile-and-verify step — clearly a separate, later-stage AOT
       source-generation bug, not a classloader/resource-stream issue;
       flagged here for a future session, not investigated further.
       (`processAheadOfTimeWithWebTests` and `endToEndTests` also progress
       to their own distinct, unrelated new failures post-fix — an
       `AnnotationConfigurationException` `@AliasFor` mirror-value mismatch
       for `endToEndTests`, and the same JavaPoet NPE for `WebTests`;
       `processAheadOfTimeWithXmlTests` still hits the already-documented
       `GroovySystem.<clinit>` `ArrayStoreException` from item 2 above —
       none of these are classloader-resource-stream bugs, all out of
       scope for this session.)

       Regression-checked clean: `cratonvm-native-builtins --lib --release`
       3000/0 (0 failed, 6 ignored, matches baseline exactly);
       `cratonvm-vm --lib --release` 2200 passed/17 failed — 16 match the
       documented pre-existing `lock_order`/`jit::skip_list` release-mode
       baseline exactly; the 17th,
       `runtime::interpreter::tests::buffered_input_stream_real_jdk_uses_its_own_bytecode`,
       is a separate, already-flagged, pre-existing failure (deterministic,
       unrelated to classloading — a different concurrently-running session
       was independently investigating it under the title
       "bufferedinputstream-regression" during this same window) and is not
       a regression from this fix. Spot-checked other
       `ServiceLoader`/resource-heavy classpath consumers against both the
       pre-fix and post-fix binaries: `org.springframework.core.io.support.
       SpringFactoriesLoaderTests` 31/33 (2 `AssertionError` failures,
       **identical** on both binaries — pre-existing, unrelated, confirmed
       via direct A/B); `org.springframework.beans.factory.serviceloader.
       ServiceLoaderTests` 3/3 OK (no regression); `org.springframework.
       test.context.env.YamlTestPropertySourceTests` (an ordinary,
       non-forked-loader `@YamlTestProperties` consumer) 4/4 OK (confirms
       the normal YAML-loading path was never broken and stays unaffected).
    4. ~~`endToEndTests` — `ClassCastException:
       org.springframework.test.context.hint.StandardTestRuntimeHints cannot be
       cast to org.springframework.test.context.aot.TestRuntimeHintsRegistrar`~~
       **FIXED (2026-07-16, branch `fix/aotservices-loaderid-20260716`).**

       Root cause: a **9th site** in the same loader-identity family as this
       doc's 8-fix wave, but on a path none of those 8 touch. `TestRuntimeHintsRegistrar`
       instances are discovered via Spring's own `AotServices.factories().load(…)`
       SPI (constructor of `TestContextAotGenerator`), which resolves down to
       `SpringFactoriesLoader.instantiateFactory`: `ClassUtils.forName(implementationName,
       this.classLoader)` (`this.classLoader` is the caller's thread-context
       classloader — the `@CompileWithForkedClassLoader` fork loader for this
       test) followed by `Constructor.newInstance()`. That path allocates the
       service object (`StandardTestRuntimeHints`) using the EXACT `ClassId` the
       reflective `Class.forName` resolved — already loader-correct, matching the
       `declaring_cid`-from-mirror pattern `native_constructor_new_instance`
       (`native-builtins/src/lang_class.rs`) already uses for this exact reason.

       The actual bug is on the CONSUMING side. `TestContextAotGenerator.
       processAheadOfTime`'s `this.testRuntimeHintsRegistrars.forEach(registrar ->
       …)` lambda has its `Consumer<TestRuntimeHintsRegistrar>` SAM parameter
       narrowed from the erased `Object`, so CratonVM's direct lambda-dispatch
       path (`try_lambda_dispatch`, which bypasses the JDK's synthetic
       `accept(Object)` bridge and its bytecode `checkcast`) "replays" that cast
       itself via `checkcast_lambda_instantiated_args` -->
       `lambda_arg_provably_not_instance` (`vm/src/runtime/interpreter.rs`).
       Unlike the ordinary bytecode `Instruction::Checkcast` opcode handler —
       which falls back to `loader_aware_name_assignable` (a NAME-based walk of
       the object's own superclass/interface chain, loader-identity-agnostic) —
       `lambda_arg_provably_not_instance` only tried a global `ClassId`-identity
       check (`get_loaded_class_id`, loader-blind, picks whichever same-named
       class was registered first) and an exact defining-loader-namespace lookup
       (`class_defined_by_loader_exact`, which misses here because the fork
       loader was never separately driven to resolve `TestRuntimeHintsRegistrar`
       by name on its own — it only received it transitively while defining
       `StandardTestRuntimeHints`). Neither proved the match, so the "provably
       not an instance" fallback fired and threw a false `ClassCastException`
       for a same-named, different-loader interface copy the object's own
       `interfaces` list already carried.

       Fix (`vm/src/runtime/interpreter.rs`, `lambda_arg_provably_not_instance`):
       added `loader_aware_name_assignable(shared, obj_class_id, target_cid,
       target)` as one more disjunct before concluding a mismatch — reusing the
       exact same helper the ordinary `checkcast` opcode already relies on, so
       direct lambda dispatch gets the same loader-faithful answer.

       Verified: `endToEndTests` no longer throws the `ClassCastException` —
       KRun repro (real JDK 25, `TestContextAotGeneratorIntegrationTests`
       standalone) now progresses past `TestContextAotGenerator`'s registrar
       loop into AOT generation proper, and fails later for the **already-
       documented, unrelated** `#3` SnakeYAML `ParserException` above (same
       `test1.yaml` block-node parse failure, now also observed via
       `BasicSpringJupiterSharedConfigTests` instead of `$NestedTests` — not
       fixed, not caused by this change). Regression-checked clean:
       `cratonvm-native-builtins --lib --release` 3000/0 (0 failed, 6 ignored);
       `cratonvm-vm --lib --release` 2200 passed/16 failed, and an A/B rerun of
       the identical 16 against the pre-fix binary reproduces the exact same 16
       (9 `lock_order` debug-only-panic tests + 7 `jit::skip_list` tests, both
       confirmed pre-existing and unrelated to loader resolution — not a
       regression). AOT-cluster spot checks against the fixed binary, real JDK
       25: `ConfigurationClassPostProcessorAotContributionTests` 20/20 OK (the
       doc's previously-recorded 5 residuals were the host-JDK `java.lang.
       classfile.ClassFile` gap noted elsewhere in this doc — JDK 25 has that
       class, so they now pass too), `TestCompilerTests` 22/21/1 (matches
       recorded baseline exactly, no change). `BeanDefinitionMethodGeneratorTests`
       hit a GC/heap-corruption LOADERR (`out-of-bounds field read … class_id=
       ClassId(0) class_name=java/lang/Object`, `Stale pointer detected in
       invokevirtual receiver`, ending in a `NoSuchMethodError` on
       `java/lang/Object.lambda$executeRecursively$5()V`) that reproduces
       **identically on the unmodified pre-fix baseline binary** (same objects,
       same time-adjacent pattern) — confirmed via direct A/B, not caused by
       this change. It matches the signature of this doc's already-tracked, OPEN
       `ApplicationContextAotGeneratorTests` GC-corruption bug (§2, 2026-07-16
       re-triage) and appears to now also be reachable from this class under
       heavy host load; needs its own dedicated session, out of scope here.

       **2026-07-17 dedicated re-triage — CONFIRMED FIXED, resolves the "needs
       its own session" note above.** Picked up as the assigned bug for this
       session (same class as the `fb15be63` GAP_FILLER_CLASS_ID fix's other
       corroborating data points). First checked whether a stray same-day log
       (`spot-bdmgt-2.log`, timestamped *after* `fb15be63` landed, from the
       `wt-aotservices-loaderid-20260716` session) still showing the identical
       corruption signature meant the fix was incomplete — but that build's own
       logs (`build1.log`/`build2-postmerge.log`) show it ran under `database or
       disk is full` conditions (the same host-wide root-filesystem exhaustion
       hit again during this session, see below), so it was not trustworthy
       evidence either way and needed independent reproduction. Built a
       completely fresh binary at current `origin/dev` tip `56728b1a` (has
       `fb15be63` as an ancestor) in a clean worktree
       (`/data/data/wt-appctxaot-corruption-20260716`), plus a from-scratch
       `spring-context`/`spring-beans` test-class build and `KRun` classpath
       (`/data/data/appctxaot-repro`) independent of any other session's
       possibly-corrupted artifacts. Result: `RESULT
       org.springframework.beans.factory.aot.BeanDefinitionMethodGeneratorTests
       found=34 succ=33 fail=1 skip=0 abort=0 status=FAIL` — full test discovery
       (34/34, matches the HotSpot baseline count), zero corruption-signature
       lines (`class_name=java/lang/Object`, `Stale pointer detected`, `LOADERR`
       all absent from the run log). The 1 failure is an unrelated
       `IllegalStateException: Unable to parse source file content` in
       `generateBeanDefinitionMethodWhenPackagePrivateBean` — a real, separate
       AOT-codegen gap, not VM-level corruption; not investigated further here.
       **`fb15be63` does fix this class too** — the disk-full-tainted log was a
       false alarm, not a residual. Combined with the `ApplicationContextAotGeneratorTests`
       independent re-verification below (same session, same fresh-build
       methodology, `found=40 succ=25 fail=15`, also zero corruption lines),
       this closes out the `fb15be63` cross-cutting young-GC exact-walk family
       for both AOT-cluster classes with no residual doubt. Host note: hit the
       same root-filesystem-full condition mid-session (`~/.gradle`,
       `/tmp/hsperfdata_victor` on `/`, unrelated to `/data`'s 240G+ free) —
       worked around via `GRADLE_USER_HOME`/`TMPDIR` pointed at `/data/tmp`; did
       not attempt to clean up the host-wide root-fs issue itself (out of scope,
       affects many concurrent sessions).

    None of the other 3 `TestContextAotGeneratorIntegrationTests` failures are
    fixed by this change; #1 remains attributed to an existing tracked residual,
    #2–#3 remain open exactly as characterized above.

---

## 3. Untriaged Clusters & Per-Class Details

**Recovered 2026-07-16/17.** The truncated 1013-line historical detail
(`CRATONVM-SPRING-GENUINE-BUGLIST-125.md`, deleted in `ccab25c6`) was pulled
back from git history (`git show ccab25c6~1:docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST-125.md`,
full 1013 lines, still available at that path in git history) and
cross-referenced class-by-class against the ~310 commits and the extensive
AOT (§2) and reactive-cluster (§4) investigations that landed on `dev` since
that 2026-07-14 snapshot. Two of the four named clusters turn out to be
**already fully covered/resolved elsewhere in this document**; the other two
are **still genuinely open and were NOT previously tracked anywhere** — the
placeholder's claim that all 21 HIB-CV-32-filtered classes were "tracked
separately" was only true for 12 of them. Live re-run of the still-open
classes on a fresh `dev`-tip binary was attempted this session but blocked
by a severe, host-wide disk-full crisis on the Azure build host's root
filesystem (repeatedly hit 0 bytes free, corrupting several other sessions'
scratch files and pruning ~20 concurrent git worktrees including this
investigation's own two build worktrees mid-session) which also wiped every
surviving pre-built `spring-framework` test classpath on the host — rebuilding
one from scratch (`./gradlew jar testFixturesJar testClasses`) was out of
scope for the remaining time budget. The status below is therefore the most
accurate currently-available synthesis of doc + git evidence, but the
"still open, unconfirmed" classes marked below need a live rerun as the
concrete next step, once a `spring-framework` test classpath exists on the
host again.

### WebFlux Backend-Specific Failures
(`web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests`, `RequestMappingMessageConversionIntegrationTests`)
*   **Update 2026-07-15 (reactive-cluster session, branch `fix/reactive-cluster-20260715`)**: the whole
    per-backend residual cluster (Jetty `MimeTypes$Mutable` NCDFE / Tomcat `StandardServer`
    LifecycleException / Reactor Netty "failed to create a child event loop") was root-caused to
    process-global poisoning chains and fixed: `CharBuffer.getArray` AIOOBE (missing `Buffer.address`
    seed on `asCharBuffer()` views) poisoned `jdk.internal.icu` → `java.net.IDN` → Netty's buffer
    stack; a JIT NPE in `sun.misc.Unsafe.putOrderedLong` (the native `<clinit>` shadow never populated
    `theInternalUnsafe`; the interpreter's C11/C16 null-receiver rescue papered over it but compiled
    code has no such rescue) killed `ByteBufUtil.<clinit>` at Netty's `MpmcArrayQueue(4096)` init loop.
    **`CrossOriginAnnotationIntegrationTests` is now fully OK (68/68)** on all backends.
*   ~~**Current Residual (OPEN)**: `RequestMappingMessageConversionIntegrationTests` (160 tests) runs but
    is pathologically slow — steady progress (fresh `AnnotationConfigApplicationContext` + HTTP server
    per test; watchdog stack dumps show active bean creation, no deadlock) yet does not finish within
    1800s (HotSpot: 13s). Needs a dedicated perf investigation.~~
    **PARTIALLY FIXED across two rounds (2026-07-15, commits `4290124b`/`96c8a57f`/`b7a1ed84`, branch
    `fix/xerces-perf-20260715`)**: root-caused and fixed 5 real, independently-verified bugs — a
    missing invoke-cache consult for JDK-internal bytecode, two JIT-eligibility-check memoization
    gaps, an unbounded per-native-call JIT-code-range resort, a SipHash-vs-FxHash hasher choice, and
    an O(n) free-list byte recount inside the per-allocation `needs_gc` check. Each individually
    verified (isolated repro speedups, `perf record` before/after, zero test regressions). **STILL
    NOT sufficient to close the class-level gap**: live-profiling the real class after all 5 fixes
    shows the true remaining bottleneck is CratonVM's conservative (non-precise) GC root scanning on
    the interpreter's native-call path (`is_object_address` + `update_root_snapshot`, >60% of sampled
    CPU) — an already-documented, deliberately-deferred architectural item (see `gc/src/arena.rs`'s
    `Arena::reset` doc comment), not a quick-fix bug. Full detailed writeup, all numbers, and the
    concrete next step further down in this document, section 2, same bullet.

### WebFlux EMPTY-discovery family — RESOLVED (superseded by §4's 2026-07-15 reactive sweep)

The historical doc characterized (2026-07-11, dev `9948295e`) a cluster of `web.reactive.result.method.annotation.*`
and `web.reactive.result.view.*` classes that loaded (`found=1`) but discovered **zero runnable
test methods** — a JUnit-discovery/filtering gap specific to WebFlux's reactive test style, distinct
from ordinary FAILs. Named examples: `GlobalCorsConfigIntegrationTests`, `MessageReaderArgumentResolverTests`,
`ProtobufIntegrationTests`, `FreeMarkerMacroTests`.

**§4's full 295-class reactive sweep (2026-07-15) supersedes this with a direct, explicit finding**: after
that session's 7 (then 8) VM fixes, "the only EMPTY classes are the same 4 abstract classes HotSpot
reports EMPTY" — i.e. the WebFlux-specific EMPTY-discovery anomaly is gone; every previously-EMPTY
concrete WebFlux test class now discovers and runs its real test methods, matching HotSpot's own
EMPTY set (legitimately-abstract base classes only) exactly. Treated as **FIXED/RETIRED** — no
separate tracking needed; see §4 for the full fix list and verification detail.

### HIB-CV-32-filtered 21-class list — reconciled

The historical doc filtered 21 classes out of its main "still open" count as exclusively-ABEND,
`rc=139`-SIGSEGV crashes carrying the generic `gen_heap::read_slot: corrupt Value cell` guard message
— informally shorthanded "the HIB-CV-32 family" — on the theory they were a shared, load-dependent
batch-corruption artifact rather than 21 independent bugs. Two important corrections from this
session's research:

1. **The name is a red herring.** The literal `HIB-CV-32` defect (`docs/internal/hibernate-bugs/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md`)
   — a GC-corruptor triggered by `promotion_oom_risk` diverting `--nojit` young collections into a
   corrupting non-moving sweep — was already root-caused and **fixed weeks earlier, 2026-06-23,
   commit `c9258e17`**, well before the 2026-07-09 Spring 125-class baseline even existed. What the
   Spring doc calls "the HIB-CV-32 family" is really every crash that trips the *diagnostic guard*
   that fix's defense-in-depth left behind (`read_value_checked`/`gen_heap::read_slot`'s "corrupt
   Value cell" log line) — a generic heap-integrity tripwire, not evidence of one shared root cause.
   §4's Round 3 investigation independently confirms this guard is "pre-existing, deliberately-built
   forensic instrumentation... a conservative-scan heuristic that sometimes misclassifies a live
   region as corrupt... a SAFE recovery path by design, not silent data corruption" — consistent with
   different classes in this 21-class list having entirely different underlying causes.

2. **Only 12 of the 21 are actually accounted for elsewhere in this document; 9 are not tracked
   anywhere and are genuinely still open/untriaged.**

   | # | Class | Current status |
   |---|---|---|
   | 1-9 | `web.reactive.function.client.support.WebClientProxyRegistryIntegrationTests`, `web.reactive.function.server.InvalidHttpMethodIntegrationTests`, `web.reactive.config.WebFluxConfigurationSupportTests`, `web.reactive.config.WebFluxViewResolutionIntegrationTests`, `web.reactive.result.method.annotation.GlobalCorsConfigIntegrationTests`, `web.reactive.result.method.annotation.RequestMappingDataBindingIntegrationTests`, `web.reactive.result.method.annotation.RequestMappingViewResolutionIntegrationTests`, `web.reactive.result.view.LocaleContextResolverIntegrationTests`, `web.reactive.result.view.freemarker.FreeMarkerMacroTests` | **FIXED** — covered by §4's 295-class reactive sweep (reached HotSpot parity for the whole `web.reactive.*` scope) |
   | 10 | `messaging.rsocket.RSocketBufferLeakTests` | **FIXED** — same §4 sweep (`messaging.rsocket.*` explicitly in scope) |
   | 11 | `test.web.reactive.server.samples.JsonContentTests` | **FIXED** — same §4 sweep (`test.web.reactive.*` explicitly in scope) |
   | 12 | `web.socket.WebSocketHandshakeTests` | **STILL OPEN, but no longer ABEND** — confirmed via this session's own STOMP investigation (§3 "STOMP Message Hang" note): no longer crashes, now `FAIL 4/6` on real "Blocking write timeout" failures, "not investigated further this session." Genuine progress, not closed. |
   | 13 | `cache.jcache.JCacheEhCacheAnnotationTests` | **UNKNOWN / genuinely untracked** — zero mentions anywhere in current `dev`, no dedicated fix commit found (`git log --grep`). Not reactive, not AOT — outside every investigation that has landed since 2026-07-09. Needs a live rerun. |
   | 14 | `http.client.JettyClientHttpRequestFactoryTests` | **UNKNOWN / genuinely untracked** — same reasoning; this is the blocking `http.client` module, not `http.client.reactive` (which §4 covered), so it fell outside every session's scope. Needs a live rerun. |
   | 15 | `jdbc.config.JdbcNamespaceIntegrationTests` | **UNKNOWN / genuinely untracked** — original crash signature was an out-of-bounds field read on `org/hsqldb/RangeGroup$RangeGroupEmpty` (`real_field_count=Some(0)`), a shape plausibly touched by the broad `fb15be63` GC exact-walk fix (see below) but never re-verified. Needs a live rerun. |
   | 16-21 | `test.context.groovy.AbsolutePathGroovySpringContextTests`, `DefaultScriptDetectionGroovySpringContextTests`, `GroovySpringContextTests`, `MixedXmlAndGroovySpringContextTests`, `RelativePathGroovySpringContextTests`, `test.context.web.BasicGroovyWacTests` | **LIKELY STILL OPEN** — see "Groovy scripting cluster" below; same family as the 8-9 already-characterized Groovy classes, no dedicated fix landed for any of it. |

   Net: **12/21 resolved or improved** (11 fixed outright via the reactive sweep, 1 improved from
   crash to ordinary FAIL), **9/21 remain open and, contrary to the previous placeholder text, were
   never actually "tracked separately" anywhere** — 3 are entirely unique untracked classes and 6
   belong to the Groovy cluster below.

### Groovy scripting cluster — STILL OPEN (no dedicated fix has landed)

The historical doc (2026-07-11 characterization) named 8 classes as a systemic Groovy-script-loading
gap (high per-method failure ratios, e.g. `GroovyBeanDefinitionReaderTests` 35/36 methods failing),
plus 6 more in the HIB-CV-32-filtered list that share the same root area (`test.context.groovy.*`,
`test.context.web.BasicGroovyWacTests`) — 10 distinct classes total once de-duplicated:

- `scripting.groovy.GroovyAspectTests` — ABEND `rc=139` (2026-07-09 baseline; dumped core)
- `scripting.groovy.GroovyAspectIntegrationTests` — ABEND `rc=139` (dumped core)
- `scripting.groovy.GroovyScriptFactoryTests` — ABEND `rc=139` / TIMEOUT (dumped core)
- `scripting.config.ScriptingDefaultsTests` — ABEND `rc=139` (dumped core; part of the "6 found=0 ABENDs" cluster below, same family)
- `context.groovy.GroovyBeanDefinitionReaderTests` — ABEND `rc=139` (dumped core)
- `test.context.groovy.AbsolutePathGroovySpringContextTests` / `DefaultScriptDetectionGroovySpringContextTests` / `GroovySpringContextTests` / `MixedXmlAndGroovySpringContextTests` / `RelativePathGroovySpringContextTests` — all ABEND `rc=139`
- `test.context.web.BasicGroovyWacTests` — ABEND `rc=139`

**No dedicated fix for this cluster has landed on `dev`** — `git log --oneline ccab25c6..origin/dev
-i --grep=groovy` across the ~310 commits since the historical snapshot turns up only incidental
touches (a ByteBuddy field-lookup fix, an enclosing-class loader-awareness fix), nothing that
addresses Groovy script/classloader integration directly.

**Stronger evidence it's still broken, found this session**: the AOT cluster's 2026-07-16 re-triage of
`TestContextAotGeneratorIntegrationTests` (§2) independently hit a **new, different, unfixed Groovy
defect** in the exact same area — `processAheadOfTimeWithXmlTests` throws `ExceptionInInitializerError`
from `GroovyBeanDefinitionReader.<init>`, caused by an `ArrayStoreException: arraycopy: source element
at index 1 is not assignable to destination component type` inside `groovy/lang/GroovySystem`'s
`<clinit>` — explicitly documented as **NOT** the already-fixed `GroovySystem.<clinit>` NPE
(`docs/internal/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md`, a `Module`-descriptor-null
issue referenced in this doc's Executive Summary), a different exception type and mechanism, "newly
observed; not investigated further." This directly implicates `GroovyBeanDefinitionReader` — the same
class `context.groovy.GroovyBeanDefinitionReaderTests` exercises — and is consistent with the whole
cluster still being broken, just with the crash shape likely shifted from a hard `rc=139` SIGSEGV
(2026-07-09 baseline) to a catchable `ExceptionInInitializerError`/`ArrayStoreException` now, given
the general heap-corruption-guard fixes (`fb15be63`, HIB-CV-26 `Class.forName` fix) that have landed
in between. **Not confirmed via live rerun this session** (blocked by the host disk crisis and the
loss of the `spring-framework` test classpath, see the section-3 recovery note above) — flagged as
the highest-value next step: re-run these 10 classes on a fresh binary and characterize the current
crash/fail shape before attempting a fix.

### 6 found=0 ABEND cluster — reconciled

The historical doc separately flagged 6 classes that crashed **before any test method was discovered**
(immediate crash on class load) as a distinct shape from the mid-run HIB-CV-32-guard crashes above:

| Class | 2026-07-09 baseline | Current status |
|---|---|---|
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | LOADERR — `OutOfMemoryError: Java heap space` | **FIXED** — confirmed `OK 44/44` in §2's AOT-cluster validation table (one of the 8 loader-identity fixes' beneficiaries) |
| `core.codec.ResourceRegionEncoderTests` | ABEND `rc=139` | **UNKNOWN / untracked** — not `web.reactive.*` by package name, so likely (but not confirmed) outside §4's reactive-sweep scope even though `ResourceRegionEncoder` is reactive-stack machinery; needs a live rerun to confirm either way |
| `jdbc.config.JdbcNamespaceIntegrationTests` | ABEND `rc=1` — hsqldb `RangeGroup$RangeGroupEmpty` OOB field read | **UNKNOWN / untracked** (also HIB-CV-32-list #15 above) |
| `scheduling.quartz.QuartzSupportTests` | TIMEOUT (120s) | **UNKNOWN / untracked** — zero mentions anywhere in current `dev` |
| `scripting.config.ScriptingDefaultsTests` | ABEND `rc=139` (dumped core) | **LIKELY STILL OPEN** — same Groovy/scripting family as above, no dedicated fix |
| `test.web.servlet.htmlunit.MockWebResponseBuilderTests` | Actually **FAIL**, not ABEND, in the per-class detail (`buildContent() :: AssertionFailedError`, single method) — the historical doc's own summary and per-class sections disagree on this one's shape | **UNKNOWN / untracked**, but looks like a narrow single-assertion bug (same pattern as several other now-fixed single-method FAILs in this doc) — a plausible quick-fix candidate for a future session with a working classpath |

Net: **1/6 fixed**, **5/6 unconfirmed/likely still open**, none of them tracked anywhere else in this
document prior to this recovery pass.

## 4. Reactive cluster session 2026-07-15 (branch `fix/reactive-cluster-20260715`)

Full 295-class reactive sweep (`web.reactive.*`, `messaging.rsocket.*`, `test.web.reactive.*`,
`http.server.reactive.*`, `http.client.reactive.*`, reactive tx/core classes) vs a same-day HotSpot
baseline. Baseline on the 2026-07-15 dev tip: 178 OK / 50 FAIL / 12 LOADERR / 5 ABEND / 4 TIMEOUT /
46 EMPTY. After the session's 7 VM fixes (missing `Buffer.address` on `asCharBuffer()` views;
`theInternalUnsafe` never populated by the `sun/misc/Unsafe` clinit shadow — JIT-compiled
`putOrderedLong` NPE; mutable-`ArrayList`-typed `Collections.EMPTY_LIST/MAP/SET` singletons that
kotlin-reflect's shaded protobuf mutated in place, corrupting `emptyList()` process-wide; speculative-BCE
loop-header guard missing the null-array check — freemarker `TemplateElement.setChildren` SIGSEGV; raw
pointer dereference of tagged Unsafe-arena handles in the TLS engine's direct-buffer accessors —
`SSLEngine.unwrap` SIGSEGV; `cratonvm/net/HttpBodyReplaySubscription` not declaring
`Flow$Subscription` — 40 sub-test failures on the `[2] JDK` WebClient connector; identity `finisher()`
on JOINING/COUNTING collectors when Reactor drives the raw Collector protocol) the sweep reaches
**HotSpot parity minus the residuals below** (the only EMPTY classes are the same 4 abstract classes
HotSpot reports EMPTY, and `ResourceWebHandlerTests` fails the same single
`servesResourcesFromFileSystem` test on both VMs).

An 8th fix landed during final validation: `java.net.URI`'s construction-time field writes
(`uri_store_named`) and the `getScheme`/`getRawSchemeSpecificPart` raw-string fallbacks treated ANY
first colon as a scheme delimiter, so a relative reference with a colon in its first path segment
(`/redirect:account`) parsed as `scheme="/redirect", path="account"`. Spring's view-resolution tests
derive the default view name from the request path, so `ViewResolutionResultHandlerTests.
defaultViewNameWithRedirectPrefixFails` (the FAIL this doc has tracked since the 516-class runs)
resolved the wrong view and completed instead of erroring. Scheme detection now mirrors the real JDK
parser (first stop char among `:/?#` must be `:`, ALPHA-start + alphanum/`+`/`-`/`.` name — the same
rule `uri_scheme_name_fail_index` already enforced for exceptions). The class is now 11/11 OK.

**RETRACTED 2026-07-15**: the "batch-context `<clinit>` contamination" theorized below (classes failing only in batched suite runs, passing solo) was investigated further and is **NOT a CratonVM bug**. Root cause was Azure-host environment corruption, confirmed live: (1) `/home/victor/jdk25` (the symlink itself) had vanished mid-session — every `--java-home ~/jdk25` invocation failed with `path does not exist`, producing zero test output, easily misread as a VM hang; (2) separately, `/tmp` (the directory itself, not just stale contents) had vanished — `Tomcat.initBaseDir()` threw `IllegalStateException: Unable to create the directory [/tmp]`, which looks exactly like a filesystem native bug but is the OS directory missing. After `ln -sf /data/data/jdk25-real ~/jdk25` and `mkdir -p /tmp && chmod 1777 /tmp`, the exact same 8-class batch that previously showed `SseIntegrationTests` failing 9/48 (deterministically, 3/3 reruns) now passes 42/48 with 6 aborted, byte-for-byte matching the HotSpot baseline shape, and `DefaultRenderingBuilderTests` never reproduced its `ExceptionInInitializerError` again across 3 clean reruns of the identical batch order on the identical binary. See `azure-host-disk-full-flapping-20260715.md` memory (Claude's memory system) for the full host instability catalog.

**Remaining OPEN reactive residuals:**
*   `http.client.reactive.ClientHttpConnectorTests` — **PARTIALLY FIXED 2026-07-15**
    (`fix/jdkclient-patch-hang-20260715`, commit `8e47b8a9`). Root-caused ONE genuine mechanism: the
    `java.net.http.HttpClient` native implementation (RE.5, `net_phase_e.rs`) performs its
    "async" `sendAsync()` synchronously on the calling thread via raw `TcpStream`
    connect/write/read (30s socket timeout) and a request-body `Publisher`-draining condvar wait
    (10s), neither of which was bracketed with `begin_blocking_region()`/`end_blocking_region()` —
    unlike every other blocking native I/O call in the same file. A concurrent Stop-The-World pause
    (GC/JIT takeover) then waits indefinitely on this uncooperative thread, while the SAME pause is
    what freezes the real Java thread on the other end of the socket (e.g. MockWebServer's response
    dispatcher) that this thread is blocked waiting to hear from — a genuine deadlock. Live capture
    (`CRATONVM_DBG_RE5=1`, new diagnostic) caught it in the act: a `DELETE` request's write
    succeeded, then the response read blocked the full 30s and failed with `WouldBlock`, with a
    `"STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1
    taken=0"` warning firing mid-block. Confirmed CratonVM-specific and connector-specific:
    HotSpot ran the identical 32-request sequential-`MockWebServer` probe (`ReactorNetty`/`Jetty`/
    `HttpComponents`/`Jdk` × 8 HTTP methods each, reusing one connector instance per type) 8/8 clean;
    CratonVM hit the hang on ~2/13 attempts, always on the `Jdk` connector, never the other 3 (which
    don't route through this raw-socket path). Fixed both blocking sites; verified 15/15 clean on the
    same probe post-fix, `cargo test -p cratonvm-native-builtins --lib` 2996/0.
    ~~**Residual investigated + partially fixed 2026-07-15**~~ (`fix/httpconn-residual-20260715`,
    commits `91cb806c` + `3d1449a7`). Bisected the "which of the other 3 connectors" question with
    a `FullMatrixProbe`-style stress harness plus a from-scratch JUnit launcher driving the real
    `ClientHttpConnectorTests` class (49 sub-tests) directly under CratonVM.

    **Round 1 (`91cb806c`) — found and fixed two genuine missing-`begin_blocking_region` bugs**,
    but in the *shared* `java.net.Socket`/`java.net.ServerSocket` implementation
    (`native-builtins/src/plain_socket.rs`, JDK13+'s `NioSocketImpl` backing both classes) rather
    than in any one connector's own code — `java.net.ServerSocket.accept()` is what MockWebServer
    itself uses to accept every connection for all 4 connector cases (`socket_accept()`'s
    `listener.accept()` was never bracketed; `socket_connect()` additionally ran its blocking
    `connect()` *inside* the global socket-registry write-lock closure). Also applied defensively
    to `native-io/src/socket_channel.rs`'s blocking-mode `SocketChannel` paths. Direct gdb evidence
    showed this was NOT what caused the residual hangs on its own (no thread ever caught parked in
    an unwrapped blocking syscall) — real bugs, correctly fixed, but not sufficient alone.

    **Round 2 (`3d1449a7`) — found and fixed the actual dominant mechanism.** Live `perf record -g
    --call-graph dwarf` + `sudo gdb -p <pid> --batch -ex 'thread apply all bt'` sampling (multiple
    independent captures, both from the ad hoc probe and the real `ClientHttpConnectorTests` class)
    consistently caught a worker thread pegged at ~90-100% CPU for 20-25+ seconds straight (verified
    via `top -H` per-thread CPU-time deltas across samples, not just a single snapshot), its top
    frame cycling through `force_native_over_real_jdk_bytecode` — a ~55-branch/~1400-line
    sequential string-comparison special-case dispatcher, reached via
    `intercept_force_registered_native` → `execute_invokevirtual_cached`/`execute_invokestatic_cached`
    — while the caller executed a tight Java-level spin/poll loop typical of Reactor/Netty/Jetty's
    lock-free scheduling. This is the SAME function already flagged as consuming ~51% of all
    executed instructions on method-call-heavy workloads in
    `docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md` (2026-07-13) — that
    session measured call *frequency* but not per-call cost, and judged a fast-reject allowlist too
    risky to hand-write (a first attempt missed a case hiding in a nested helper). This fix instead
    memoizes the check exactly (no approximation, so no risk of silently changing behaviour):
    `CachedBytecodeMethod` gains a `force_native_cache: OnceLock<bool>` field, populated once per
    invoke-cache entry and read thereafter, cutting the recheck from every cached dispatch hit to
    once per unique callsite. (A direct earlier hypothesis — that a `CodeAttribute` deep-clone in
    `interpreter.rs`'s `execute()` was the multi-second cost — was investigated with hard timing
    instrumentation and REFUTED: individual clones measured consistently sub-millisecond, even
    during a 30s+ hung run; that was a genuine but much smaller waste, not the driver, and was
    reverted rather than landed as an unverified guess.)

    **Verification**: `cargo test -p cratonvm-native-builtins --lib` 2997/0 and
    `cargo test -p cratonvm-vm --lib` 2218/0, both unchanged (no regression). Live re-capture
    post-fix confirms `force_native_over_real_jdk_bytecode` no longer appears in hot-thread
    snapshots. Clean (single test run at a time, no concurrent host load) 15-run stress comparison
    of `ClientHttpConnectorTests` end-to-end, 30s hang timeout:
      - pre-fix (dev tip before this fix): **9/15 hangs (60%)**
      - post-fix (this commit): **4/15 hangs (27%)**
    A real, substantial, cleanly-measured improvement (~2.2x hang-rate reduction) — but **not a
    full fix**.

    **Follow-up investigation (same day): checked whether `try_lambda_dispatch`'s
    `shared.lambda_proxies.read()` is a second concentrated bottleneck of the same shape —
    it is NOT, ruled out with quantified evidence.** The initial post-fix hot-thread capture that
    first surfaced `lambda_proxies.read()` was one sample; before proposing the same
    per-callsite-memoization fix again, gathered 41+ independent live captures across many
    freshly-reproduced hangs (`sudo gdb -p <pid> --batch -ex bt`, `top -H` to identify the
    genuinely CPU-bound thread first, catching each hang within seconds of it starting) and
    tallied the leaf/near-leaf frames. Result: `lambda_proxies` appears in only **1 of 41+**
    samples — statistically indistinguishable from noise, not a concentration. The actual
    breakdown (28-sample batch): `hashbrown` hashmap probing 5, `parking_lot` lock/unlock spread
    across *many different* locks (invoke-cache, resolution-cache, JIT profile counters, GC arena)
    4, `mimalloc` allocator internals (`mi_page_malloc_zero`/`mi_block_set_next`) 3,
    `Arc`/`Weak` drop (refcount decrement + dealloc) 3, `epoll_wait` (legitimate NIO selector
    parks, not a bug — the thread is blocked waiting for I/O, not spinning) 2, `is_stale`
    (invoke-cache redefine-generation check) 1, `SipHash finish()` 1. No single function
    concentrates anywhere near the ~100%-of-samples signature `force_native_over_real_jdk_bytecode`
    showed before its fix. This is a genuinely diffuse, distributed cost — general hashmap/hashing
    overhead, allocator throughput, and many small independent lock acquisitions under a workload
    that allocates and dispatches heavily (Reactor's operator chains create many short-lived
    lambda/wrapper/Subscription objects; 76+ live threads by sub-test 6 means many threads paying
    this simultaneously) — not a second missing-cache bug of the same shape as the two already
    fixed. Did not implement a speculative `lambda_proxies` cache: the evidence does not support it
    as worth the risk/complexity of touching more interpreter dispatch code for an expected marginal
    (not measurable-with-confidence) return.

    ~~**Current status**: `ClientHttpConnectorTests` is measurably, substantially more reliable after
    the two rounds of fixes above (9/15 → 4/15 clean hang rate) but not fully closed. The remaining
    ~27% is consistent with the same "accumulated per-call interpreter dispatch/allocation overhead
    compounding on method-call-heavy code" conclusion the 2026-07-13
    `silent-hang-no-signature-cluster` investigation reached independently on a different test
    class — this looks like the same underlying interpreter-throughput ceiling, not a
    `ClientHttpConnectorTests`-specific bug. Closing it further needs a genuine interpreter/
    allocator throughput initiative (e.g. profiling `mimalloc` allocation-path cost under this
    object-churn pattern, or auditing the several distinct locks that showed up for reducible
    contention individually), not another single-function fix — out of scope for a "residual"
    investigation.~~ **2026-07-16 update (see section 5.3)**: the 27% hang rate is now resolved,
    independently reconfirmed at **0/60** across three fresh stress batches (including a
    deliberate 2x-concurrent-contention batch that the original 27% measurement never tested
    against). A direct causal A/B experiment (reverting only the `Thread.getId()` fix on an
    otherwise-current tip) confirms the diffuse contention/throughput cost this section describes
    is real and `getId()`'s collapse is a genuine, measurable contributor to it (mean wall time
    +10-25%, occasional runs crossing the historical 30s hang bound) — but `getId()` alone does not
    reproduce anything close to the historical 27% rate; the resolution is the combined effect of
    this session's several fixes (blocking-region gaps, `force_native_over_real_jdk_bytecode`
    memoization, `Thread.getId()`, the executor-`submit()`/Netty-`Future` fix in 5.2), not any one
    of them alone. The diffuse hashbrown/parking_lot/mimalloc/`Arc`/`Weak` cost pattern itself is
    still visible in hot-thread sampling and remains a legitimate, still-open *performance*
    characteristic — just no longer severe enough to cause an observed hang.
    Also unfixed: the T19.H1 watchdog stack-dump itself SIGSEGVs when JIT frames are on the stack
    (separate small bug; `--nojit` dumps work).
    **2026-07-16 investigation**: root-caused the *reliability* half of this note but could
    **not** reproduce a live SIGSEGV after extensive targeted testing on dev tip (single
    tier-up-compiled JIT calls, OSR-adjacent long single-invocation loops, deep
    JIT<->interpreter interface-dispatch recursion, and multi-threaded runs with one thread
    parked deep inside a JIT-compiled method while another thread acks normally) — the
    watchdog consistently either dumped correctly or fell back to its documented "0 java
    threads responded" path, never crashed. What the testing DID confirm as a genuine,
    reproducible gap: `SharedVm::dump_current_thread_frames` (`vm/src/vm/vm_init.rs`) walks
    only `thread.frames`, the interpreter's own logical frame stack — a method dispatched
    straight to already-JIT-compiled machine code
    (`execute_invokestatic_cached`/`execute_jit_call` in `runtime/interpreter.rs`) never gets
    a `Frame` pushed there at all, so that call level is silently invisible to the dump
    (either the whole thread shows 0 acks, or the frame count is misleadingly shallow) —
    never a fabricated/garbage frame in this revision, but a real diagnostic blind spot for
    a debug-tooling feature whose whole job is showing what a thread is doing. Landed two
    low-risk hardening changes on `fix/watchdog-jit-sigsegv-20260716` (both in
    `vm/src/vm/vm_init.rs`, `dump_current_thread_frames` and `set_wait_site_snapshot`): (1)
    each rendered frame line now goes through `catch_unwind` so a panic while formatting one
    (e.g. future regression hitting a malformed frame) can't prevent the watchdog from
    reaching its own `process::abort()` — that failure mode would otherwise turn an
    intended, informative crash-with-dump into a silent hang instead; (2) when
    `conservative_roots::current_thread_jit_depth()` is nonzero at dump time the output now
    appends an explicit note that one or more call levels are JIT-compiled and not shown,
    pointing at `--nojit` as a workaround, instead of leaving a shallow dump to be misread as
    a shallow call stack. Verified: `cargo test -p cratonvm-vm --lib` (82/0 in the touched
    `vm_init` module, no regressions) plus live re-runs of every repro scenario above on the
    rebuilt binary — identical dump/abort behaviour to pre-fix, no crashes, notes render
    correctly when the JIT-depth condition is met. Left as **UNFIXED** (not renamed
    `-FIXED`): the originally-reported SIGSEGV itself was never reproduced or root-caused,
    only hardened against; if it recurs, capture a core dump (`ulimit -c unlimited` +
    `/proc/sys/kernel/core_pattern`) or run directly under `gdb -q --args cratonvm
    --stack-dump-on-timeout=N ...` so the exact faulting frame is available next time,
    ideally under the `release`/`profsym` profile (this session's repro attempts used the
    `dev-full` profile — no LTO/opt-level=3 — because the shared build host repeatedly
    OOM-killed the full `profsym` release+LTO link under concurrent multi-session load;
    timing-sensitive interpreter/JIT-boundary races are plausible under release codegen that
    a `dev-full` binary's much slower interpreter dispatch may simply not expose).
*   ~~`web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` —
    pathological slowness (>1800s vs HotSpot's 13s, 160 tests).~~ **2026-07-15 update**: confirmed
    genuine forward progress, not a hang (frame counts change across successive
    `--stack-dump-on-timeout` watchdog dumps — the watchdog fires repeatedly during a single run,
    which doubles as a free sampling profiler: 10,970 dumps captured over ~40s). Aggregating the
    innermost non-framework frame across all dumps found the hot path: **`ConfigurationClassParser.
    parse`** (6004 samples), **`AbstractHttpHandlerIntegrationTests.startServer`** (5970), and —
    disproportionately — raw **Xerces XML parsing** (`XML11Configuration.parse` 4638 samples,
    `XMLDTDValidator.emptyElement` 1986, full SAX/DTD-scanning call chain beneath it) — roughly
    **42% of all sampled CPU time** inside Xerces, for a workload (annotation-`@Configuration`
    Spring context + embedded Tomcat/Reactor/Jetty bootstrap, 160x) that should barely touch XML
    parsing at all.

    **ROOT-CAUSED AND FIXED (2026-07-15, commits `4290124b` + `96c8a57f`, branch
    `fix/xerces-perf-20260715`, merged to `dev`)**. This turned out to be a GENERAL bug, not
    Xerces-specific — confirmed via a standalone `SAXParserFactory` parse-loop repro
    (`XercesRepro.java`, no Spring/Tomcat/networking involved) that isolated the exact same
    disproportionate cost outside any Spring context. Root cause, found by instrumenting the
    invoke-dispatch path with a one-off counter (`CRATONVM_DBG_INVOKESTATS`, kept in the tree as a
    permanent gated diagnostic): `execute_frame`'s raw-byte-peek fast dispatch loop — which includes
    the monomorphic `InvokeCache` inline cache used by `invokevirtual`/`invokespecial`/
    `invokeinterface` — is gated off for `is_jdk_class` frames (`vm/src/runtime/interpreter.rs`
    around line 6813; a deliberate, correct gate — some of that loop's *other* opcode fusions use
    truly-unchecked stack pops unsafe for real-JDK bytecode shapes). But the general
    `Instruction::decode` → `execute_instruction` path those frames fall back to called
    `execute_invoke`/`execute_invoke_kind` **unconditionally** for every invoke instruction — it
    never consulted the cache at all. Since Xerces (`com.sun.org.apache.xerces.internal.*`) ships
    inside `java.xml`, every single method call it makes — and Xerces's SAX/DTD state-machine scanner
    makes an enormous number of very small calls per parse — paid full method resolution (native-
    registry hash lookup, hierarchy walk, `class_manager` RwLock reads, several hardcoded special-case
    string comparisons) instead of the O(1) lock-free cache hit non-JDK bytecode already got.
    Measured with `CRATONVM_DBG_INVOKESTATS` on the repro: ~1,546 cache hits vs ~1,800,000 full-slow-
    path calls over 20 parses (a ~0.08% hit rate) before the fix. Fix: the `Instruction::Invokevirtual`
    / `Invokespecial` / `Invokeinterface` arms in `execute_instruction`
    (`vm/src/runtime/interpreter.rs`, "Method invocation (slow path)") now call
    `execute_invokevirtual_cached` first and only fall through to the existing slow path on a genuine
    cache miss — safe to reuse unconditionally because that function's arg decode already goes through
    `pop_arg_for_descriptor_checked` (descriptor-aware, checked), not the unchecked pops the
    `is_jdk_class` gate exists to avoid. A companion investigation (commit `96c8a57f`) found and fixed
    two related JIT-eligibility-check memoization gaps in the same hot path (a full O(bytecode-size)
    `jit_method_calls_native_shadowed` re-scan that never got cached via the existing
    `mark_jit_bail_listed`/`jit_skip_set` mechanisms, so it re-ran every 64 invocations forever for any
    method that calls a native-shadowed target).

    **Verification — isolated repro** (`XercesRepro.java`, 500 SAX parses of an ~800-line
    web.xml-shaped document, `dtds/web-app_2_3.dtd` for the validating case):

    | Mode | Before | After | HotSpot | Speedup |
    |---|--:|--:|--:|--:|
    | non-validating | 155.5 ms/parse | 57.2 ms/parse | 0.46 ms/parse | 2.72x |
    | DTD-validating | 536.8 ms/parse | 156.3 ms/parse | 1.18 ms/parse | 3.44x |

    `perf record` self-time for `NativeMethodRegistry::find` dropped 11.61%→4.86% and
    `jit_method_calls_native_shadowed` dropped 5.18%→0.58% across the two fixes.
    `cargo test -p cratonvm-vm --lib`: 2203 passed / 9 failed (all 9 are pre-existing
    `runtime::lock_order` tests that explicitly require a debug build — "test runner is expected to
    be a debug build" — an artifact of running `cargo test --release`, unrelated to this change) /
    111 ignored.

    **Full-class wall-clock impact after round 1: NOT confirmed** — the round-1 A/B comparison
    above was inconclusive (host contention), but its qualitative observation was right: per-test
    wall time (~40-55s) was far larger than the Xerces-repro cost could explain, meaning something
    else dominated. Investigated further same-day, round 2 (2026-07-15, commit `b7a1ed84`, same
    branch):

    **Repro correction first**: the original standalone bootstrap repro (`BootstrapRepro.java`) had
    no package declaration, so its `@ComponentScan` scanned the entire classpath root recursively —
    an unrealistic worst case. The real test's `WebConfig` is properly packaged
    (`org.springframework.web.reactive.result.method.annotation`), scoping its scan to one package.
    A corrected, properly-packaged repro (`BootstrapRepro2.java`, package
    `com.example.bootstraprepro`) dropped HotSpot's steady-state `ctx_ms` from ~90-140ms to
    ~20-32ms — the original repro was measuring an artificial worst case, not the real per-test cost.
    This mistake cost real investigation time; flagging the lesson (fair repro scope matters as much
    as the fix) for future sessions chasing this class of bug.

    **Three further, independent, verified fixes** found via `perf record` (both on the isolated
    repro and, critically, `perf record -p <pid>` attached LIVE to an in-progress real-class run —
    the isolated repro's profile shape and the real class's profile shape turned out to be quite
    different, so both were needed) plus live gdb stack sampling:

    1. `jit/src/lib.rs` (`snapshot_code_ranges_into`): the per-native-call JIT-code-range snapshot
       used by `native_stack_has_jit_frame` (`vm/src/jit/conservative_roots.rs`) was rebuilt (full
       lock + Vec copy + `sort_unstable()` over every registered range) on EVERY call despite being
       cached in a thread-local buffer — a write-only "cache". Since registered ranges only grow
       over a process's life (compiled code is retained), and round 1's own fix made more JDK-class
       code tier up to JIT, this was an O(n log n) cost per native call that grew across a session —
       directly visible as `ctx_ms` climbing 14.8s→26.2s across BootstrapRepro iterations sharing one
       process, and `perf record` showing 7.19% self-time in a deeply-recursive `quicksort`. Fixed
       with a generation counter, bumped only on actual add/remove, gating the resnapshot/resort.
    2. `vm/src/runtime/local_liveness.rs` (`live_locals_mask`): used `std::collections::HashMap`'s
       default SipHash hasher instead of this codebase's usual `FxHashMap` for its (also
       per-native-call) code-blob cache and per-pc liveness table. Pure hasher swap.
    3. `gc/src/arena.rs` (`Arena::free_list_bytes`): summed both free-list tiers from scratch on
       every call; its only caller, `GenerationalHeap::needs_gc`, runs on every allocation attempt.
       Live-profiling an ACTUAL `RequestMappingMessageConversionIntegrationTests` run in progress
       found `needs_gc` alone at **24.5% of all sampled CPU time**. Fixed with the same
       generation-counter-cache shape as #1. Verified with a second live-attached `perf record`:
       `needs_gc` dropped 24.5%→7.2% of sampled CPU with no other symbol regressing.

    All three: `cargo test -p cratonvm-gc --lib` 787/0 and `cargo test -p cratonvm-vm --lib`
    2203/9(pre-existing debug-only)/111 unchanged (no regressions).

    **Class-level result: still NOT resolved.** A live-attached `perf record` on the real class
    AFTER all three round-2 fixes shows the true remaining bottleneck clearly: with `needs_gc` fixed,
    `GenerationalHeap::is_object_address` (28.7% direct + 10.1% via its `VmHeap` wrapper ≈ **38.7%**)
    and `update_root_snapshot` (22.2%) now dominate — together over 60% of sampled CPU, i.e. the SAME
    conservative-GC-root-scanning family as the fixes above, but the piece of it that ISN'T a bug: on
    every object-returning native call, CratonVM's conservative (non-precise) collector validates
    every 8-byte-aligned candidate word in the scanned native-stack/frame regions against
    `is_object_address` (alignment + region-bounds + header-tag-plausibility checks) because it has
    no precise stack map for that call site. This is explicitly documented, pre-existing, deliberately
    deferred architecture — see the doc comment on `Arena::reset` in `gc/src/arena.rs`: *"the
    audit-flagged 'perf bug' is a real cost, but the correctness hazard outweighs it... until
    `is_object_address` is tightened to honour the cursor bound"* / *"when the GC switches to precise
    stack maps"* — not a quick-fix bug. Confirmed empirically: a full-class run with all 3 round-2
    fixes reached the same ~29 Tomcat-backend tests in a 1200s bound as the pre-round-2 binary — a
    real, individually-verified reduction in one contributor (`needs_gc`) did not move the class-level
    wall clock outside measurement noise, because the now-larger `is_object_address` /
    `update_root_snapshot` contributor was untouched.

    **Honest summary across both rounds**: 5 real, independently-verified CPU-dispatch and
    GC-bookkeeping bugs found and fixed (commits `4290124b`, `96c8a57f`, `b7a1ed84`), each with clean
    before/after measurements and no regressions. `RequestMappingMessageConversionIntegrationTests`
    still does not complete in a reasonable multiple of HotSpot's 13s. The remaining, now
    clearly-identified bottleneck is architectural: CratonVM's conservative (stack-map-free) GC root
    scanning on the interpreter's native-call path. Closing that gap requires precise stack maps for
    that specific scan path (the project has PARTIAL precise-map coverage already — see the
    precise-jit-maps roadmap items — but apparently not for this native-call conservative-scan site),
    which is a substantially larger effort than a bug-fix session: expect a dedicated investigation,
    not a quick follow-up.

    **Round 3 (2026-07-16): heap-corruption false alarm investigated and closed — `b7a1ed84`
    exonerated with direct A/B evidence; two SEPARATE real bugs found instead.** An independent
    verification pass reported the class failing to complete within 1200s with continuous
    "implausible object size" / corrupted-header non-moving-sweep warnings, and named `b7a1ed84`'s
    `Arena::free_list_bytes()` epoch-cache (round 2, fix #3 above) as the prime suspect. Investigated
    with full rigor on a fresh worktree + fresh build at the then-current `dev` tip (`1f8e398e`):

    - **Code audit of the epoch-cache found no plausible corruption mechanism.** Every mutation site
      to the arena's free lists (`push_block_routed`; the `alloc()` free-list-hit branch; all 3
      list-clearing paths) bumps the generation counter — re-verified line-by-line against the exact
      committed diff. `Arena` is exclusively accessed through a `Mutex` in every backend that uses it
      (`gen_heap.rs`, `zgc.rs` — both have explicit doc comments stating "all allocation/collection
      serialize through that mutex"); no unsafe `Sync`/raw-pointer bypass exists, so the `Cell`-based
      cache cannot race across threads. `free_list_bytes()`'s only callers (`needs_gc`,
      `live_bytes_estimate`, `remaining`) use it purely as a heuristic/estimate — never for unsafe
      pointer arithmetic — and `Arena::alloc()`'s actual allocation logic always does its own
      independent, authoritative bounds-checked work regardless of what the cache reports.
    - **Direct reproduction hit an UNRELATED, real, pre-existing bug first**: every run — with or
      without the round-2 fixes — crashed within seconds with `Error in thread "main" internal
      error: Class.forName: class initialization failed: class initialization raised an exception`,
      never reaching real test execution. Root cause: `native-builtins/src/lang_class.rs`'s
      `Class.forName(name, true, loader)` implementation (the "HIB-CV-26" code path, committed
      2026-06-23 — weeks before either investigation) wraps ANY nested class-initialization failure
      (here, `MemorySegment.<clinit>` throwing `IllegalCallerException` because native access is
      disabled by default post-security-audit, commit `8e9e85c8`) as an unrecoverable
      `VmError::Internal` instead of a normal, catchable Java `ExceptionInInitializerError` — killing
      the whole VM instead of letting JUnit report a failed test. Passing `--enable-native-access`
      (documented, existing CLI flag) avoids triggering the nested failure and unblocks the run. This
      is a genuine, separate, worth-fixing bug — filed for follow-up, NOT fixed in this session
      (out of scope: unrelated code, would need its own investigation into every `initialize_class`
      call site that wraps errors this way).
    - **With that unblocked, the ACTUAL corruption warnings were reproduced** — confirming the
      independent verification's observation was real, not a fluke. But a direct A/B (same build
      host, same classpath, same JDK, `--enable-native-access` on both sides) proves `b7a1ed84` is
      NOT the cause:

      | Build | Corruption warnings | Completes in 1200s? | Result |
      |---|--:|---|---|
      | `96c8a57f` (round 1 only, BEFORE the round-2 arena fix) | 679,454 | **NO** (killed at timeout) | — |
      | `dev` tip / `b7a1ed84`+, trial 1 (isolated) | 1,074 | Yes, 589s | 159/160 |
      | `dev` tip / `b7a1ed84`+, trial 2 (isolated) | 786 | Yes, 556s | 160/160 |
      | `dev` tip / `b7a1ed84`+, trial 3 (isolated) | 731 | Yes, 527s | 160/160 |

      **Extended validation (2026-07-16, same session): 8 MORE trials — 11 total — to get real
      statistical confidence, not just 3 lucky runs**, per a direct follow-up request after the
      initial 3-trial result above. All 8 run under SELF-INDUCED CPU contention (deliberately, to
      probe whether the corruption-heuristic's firing rate is load/timing-sensitive): two batches of
      3 concurrent processes each, one batch of 2:

      | Trial | Condition | Corruption warnings | Wall time | Result |
      |---|---|--:|--:|---|
      | 4 | 3-way parallel (batch A) | 1,477 | 678s | 159/160 |
      | 5 | 3-way parallel (batch A) | 1,411 | 610s | 159/160 |
      | 6 | 3-way parallel (batch A) | 1,093 | 716s | 160/160 |
      | 7 | 3-way parallel (batch B) | 1,129 | 694s | 160/160 |
      | 8 | 3-way parallel (batch B) | 1,962 | 708s | 160/160 |
      | 9 | 3-way parallel (batch B) | 1,860 | 668s | 160/160 |
      | 10 | 2-way parallel (batch C) | 884 | 623s | 160/160 |
      | 11 | 2-way parallel (batch C) | 776 | 625s | 159/160 |

      **Combined result across all 11 trials (3 initial isolated + 8 extended parallel): 11/11
      (100%) completed within the 1200s bound.** Wall-clock time ranged 527s-716s (mean ≈636s) — no
      trial came anywhere near the 1200s ceiling, let alone failed to finish. Corruption-warning
      count ranged 731-1,962 (mean ≈1,199) — every single trial stayed 346x-929x BELOW the pre-fix
      catastrophic case (679,454) and zero trials showed runaway/unbounded growth. Test-pass rate:
      7/11 trials fully clean (160/160); 4/11 trials had exactly one unrelated failure (never more
      than one, and NOT correlated with warning count: trial 6 (1,093 warnings) passed 160/160 clean
      while trial 8 (1,962 warnings — the highest of all 11 trials) also passed 160/160 clean, yet
      trial 4 (1,477 warnings, mid-range) lost one test) — this is separate, low-priority,
      pre-existing test flakiness (a `ClassCastException` on
      `RequestMappingHandlerMapping$AnnotationDescriptor` bean creation in at least 2 of the 4 cases;
      the other 2 showed a different failure not yet identified), not chased further as clearly
      out of scope for this investigation.

      **The load-sensitivity hypothesis is confirmed, but bounded**: the 3 isolated trials (1-3)
      averaged ~864 warnings vs. ~1,324 for the 8 trials run under self-induced 2-3-way parallel
      contention (4-11) — about 53% higher, consistent with the corruption-detection heuristic firing
      somewhat more under scheduling contention, as hypothesized. But even under that adverse,
      artificially-induced condition (worse than a typical solo CI run — 2-3 full test-class runs
      competing for the same cores simultaneously), every trial still completed comfortably inside
      1200s with no sign of approaching the pre-fix non-completion regime. No trial across all 11
      showed any early-warning signal (a mid-run spike, a stall, an accelerating rate) that would
      suggest an occasional tip into the pre-fix catastrophic mode is lurking — the distribution looks
      like ordinary variance around a stable, load-correlated mean, not a bimodal "usually fine,
      occasionally catastrophic" pattern.

      **On the discrepancy with the independent verification agent's 1200s-timeout run**: the
      standard `spring-suite-runner`'s `run-suite.sh` does NOT pass `--enable-native-access` by
      default either (confirmed by reading its source — the flag would have to be added via the
      opt-in `EXTRA_VM_ARGS` env var, which a typical invocation would not set). If their harness
      also omitted it, their run should have hit the SAME near-instant `Class.forName` crash this
      investigation hit first, not run for 14.5 minutes with progress before showing corruption —
      meaning either their invocation differed in a way not yet identified (a different harness, a
      module-system flag, a different JDK build), or that crash is itself non-deterministic in a way
      neither investigation has fully characterized. This is flagged as an open question for whoever
      owns the independent-verification harness to check, rather than resolved here.

      The corruption-detection-and-resync mechanism itself is OLD, pre-existing, deliberately-built
      forensic instrumentation in the non-moving sweep (`gc/src/gen_heap.rs`, the
      `[quiesce] FIRST corruption` diagnostic and its `resync_to_next_free_block`/`skip_free_blocks`
      recovery path — confirmed via `git log` to predate this entire investigation by weeks; none of
      the 41 commits between `b7a1ed84` and the verification tip touch this code, and it fires
      identically on the commit BEFORE the arena fix). It is a conservative-scan heuristic that
      sometimes misclassifies a live region as corrupt (the code's own comments call this "anomaly
      evidence") and safely resyncs to the next known-good free-block anchor rather than risk
      double-freeing — i.e. it is a (noisy but) SAFE recovery path by design, not silent data
      corruption. What actually differs is severity: **628x fewer** warnings and **reliable
      completion** with the round-2 fixes applied, vs. runaway warnings and non-completion without
      them. This is because the round-2 fixes made the whole class ~2x faster (`~589s` vs. previously
      unable to finish in 1200s), which gives the sweep's occasional heuristic misfire far less total
      GC-cycle volume to compound across. **The round-2 arena fix mitigates this pre-existing issue's
      practical impact; it does not cause it.**

    `cargo test -p cratonvm-gc --lib`: 790 passed / 0 failed on the verification tip (both before and
    after the arena fix). `cargo test -p cratonvm-vm --lib`: the same 9 pre-existing
    debug-build-only `lock_order` failures, plus a variable number (7-8, non-deterministic across
    reruns — 16 then 17 failures on two consecutive runs of the identical binary) of `jit::skip_list`
    test failures that are parallel-test-execution races over shared global state in an unrelated
    subsystem (`vm/src/jit/skip_list.rs`, nothing touched by any commit in this investigation) — flagged
    as a separate, pre-existing test-suite flakiness item, not a real regression (not chased further;
    out of scope for this investigation).

    **Conclusion: `b7a1ed84` is NOT reverted — it is confirmed safe and net-positive.** The
    non-moving-sweep corruption-warning phenomenon is real but pre-existing, already has a safe
    (if noisy) recovery path, and is a separate open item from the CPU-dispatch/GC-bookkeeping work
    in rounds 1-2. Two new, genuine follow-up items filed by this investigation: (1) the
    `Class.forName`/`HIB-CV-26` unrecoverable-internal-error-on-nested-clinit-failure bug in
    `native-builtins/src/lang_class.rs` (real, reproducible, deserves its own fix), and (2) the
    non-moving sweep's occasional false-positive corruption detection under heavy allocation churn
    (real, pre-existing, already has a safe recovery path but the false-positive rate itself — and
    whatever per-resync cost compounds into non-completion on a slow/loaded run — is unexplained and
    worth its own dedicated investigation).

    ~~`Class.forName`/`HIB-CV-26` unrecoverable-internal-error-on-nested-clinit-failure~~
    **FIXED (2026-07-16), commit `43e130fa` (merged to `dev` at `ba51b880`).** Root cause was
    TWO stacked defects, both in the class-initialization-failure-wrapping machinery, not just
    the `Class.forName` call site:
    1. `NativeContext::initialize_class` (`native-api/src/registry.rs`) had a lossy
       `Result<(), String>` signature. Its one real implementation
       (`vm/src/vm/vm_exec.rs`) called the interpreter's `ensure_class_initialized_shared`
       (which already correctly wraps a `<clinit>` exception as a catchable
       `MethodCallFailed::ExceptionThrown(ExceptionInInitializerError)` per JVMS §5.5) and then
       *discarded* that distinction, flattening both `ExceptionThrown` and `InternalError` into
       a bare string. All 5 native-builtins call sites (`Class.forName` in `lang_class.rs`,
       `Constructor.newInstance` also in `lang_class.rs`, and two independent
       `Lookup.ensureInitialized` registrations in `lang_invoke.rs` / `classloader.rs`) then
       re-wrapped that string as an unrecoverable `VmError::Internal` — turning an ordinary,
       catchable `<clinit>` exception into a VM abort every time. Fix: changed the trait method
       to return `Result<(), MethodCallFailed>` (mirroring the already-correct sibling
       `ensure_class_initialized_with_class_id`) and pass the interpreter's result straight
       through; all 5 call sites now propagate via `?` instead of hand-rolling a
       `VmError::Internal`.
    2. Verifying fix #1 surfaced a second, closely related bug in
       `ensure_class_initialized_shared` itself (`vm/src/vm/vm_util.rs`, two occurrences): when
       a class is re-triggered for initialization after already being marked
       `ClassState::InitializationError` (JVMS §5.5's "already failed to initialize" case —
       e.g. a caller that `catch`es the first `ExceptionInInitializerError` and retries), the
       function returned `MethodCallFailed::InternalError(VmError::Linkage(NoClassDefFoundError))`
       — also uncatchable, so a caught-and-retried `Class.forName` crashed the VM on the
       *second* call. Fixed by routing both occurrences through the existing
       `raise_no_class_def_found` helper (`vm/src/runtime/exceptions.rs`), which constructs the
       real, catchable `NoClassDefFoundError` object. One of the two occurrences is reached
       while still holding the `class_manager` write-lock guard (`cm`) that the local `class:
       &mut Class` borrow is tied to; `raise_no_class_def_found` itself needs to read/write that
       same `RwLock` to allocate the exception object, so an explicit `drop(cm)` was added
       immediately before the call to avoid a self-deadlock (the borrow's last use is the
       preceding `class.name.to_string()`, so NLL allows the drop).

    Verified with a standalone repro (`Class.forName("java.lang.foreign.MemorySegment")`,
    `--java-home <jdk25>`, no `--enable-native-access`, so `MemorySegment.<clinit>` throws
    `IllegalCallerException` exactly as in the original finding): uncaught now prints a normal
    `Exception in thread "main" java/lang/ExceptionInInitializerError` trace and exits 1 (was: VM
    abort); wrapped in `try/catch (ExceptionInInitializerError)` it recovers cleanly and control
    continues; a second `Class.forName` call on the now-poisoned class throws (and catches as)
    `NoClassDefFoundError` instead of crashing. `cargo test -p cratonvm-native-builtins --lib
    --release`: 2999 passed / 0 failed. `cargo test -p cratonvm-vm --lib --release`: 2199 passed
    / 16 failed — the same pre-existing `lock_order` debug-build-only + `jit::skip_list`
    parallel-race flakiness documented above, zero new failures.

    Also re-ran `RequestMappingMessageConversionIntegrationTests` (`spring-webflux`) without
    `--enable-native-access` as an end-to-end check: it no longer dies instantly at
    `MemorySegment.<clinit>` — it now runs for several minutes and gets through ~26 Tomcat-backed
    test methods (consistent with the "~29 Tomcat-backend tests" ceiling already documented
    above for this class) before hitting a `SIGSEGV` in `gc/src/gen_heap.rs`'s non-moving-sweep
    path, preceded by 2000+ "implausible object size" / corruption-resync warnings — i.e. it now
    runs into follow-up item (2) above (the pre-existing, already-filed, separate non-moving-sweep
    false-positive-corruption issue), not a HIB-CV-26 regression. This host was also running
    several other sessions' builds/tests concurrently at the time (including at least one other
    unrelated binary segfaulting minutes earlier), so heavy contention is a plausible contributor;
    not re-tested in isolation due to time. Flagged here so whoever picks up item (2) has this
    additional data point: fixing HIB-CV-26 means real full-class runs now reach far enough to
    actually exercise the non-moving-sweep bottleneck instead of being masked by the earlier,
    more-severe `Class.forName` abort.

    **Round 4 (2026-07-16): dedicated feasibility investigation into the `is_object_address` /
    `update_root_snapshot` conservative-GC-root-scanning bottleneck itself (the item Round 2
    identified as "a substantially larger effort than a bug-fix session"). Result: a genuine,
    well-evidenced, provably-safe redundancy WAS found and implemented, but it measured as a
    PERFORMANCE REGRESSION rather than a win on the best available repro — NOT landed to `dev`.**
    Branch `fix/precise-native-scan-20260716` (pushed, unmerged, commit `faedea48`) preserves the
    full implementation and data for a future session with working profiler access.

    Investigation: `update_root_snapshot`'s interpreter-frame operand-stack scan
    (`ValueStack::scan_object_refs`, `vm/src/runtime/value_stack.rs`) already distinguishes
    genuine object references from long-bit-pattern false positives via a per-slot `kinds`
    side-array (commit `6161dc1b`, 2026-05-29 — "marks are only ever written at genuine
    long/double producers, never over-marked"). Despite that, FOUR hot call sites
    (`update_root_snapshot`'s two internal paths in `interpreter.rs`, `collect_roots` in
    `vm/src/memory/roots.rs`, and the blocked-thread deposit path in `vm/src/vm/vm_exec.rs`) all
    additionally re-validate EVERY entry `scan_object_refs` appends against the expensive, strict
    `heap.is_object_address` header probe — a defense that predates the `kinds` mechanism
    (introduced in commit `56a73aee`, 2026-05-16, thirteen days earlier) and, once `kinds` exists,
    can never actually reject anything from the kind-verified branch: `git log` confirms the
    ordering (boundary filter added first, kinds added later), and the mirrored fix already
    landed for LOCALS (`Frame::scan_local_objects`, commit `333b24b5`, "root young/mid-init
    objects held in frame locals") explicitly switched off the equivalent strict probe for the
    same reason — it can incorrectly DROP a genuine root whose header a moving collector's
    young/mid-init state hasn't fully validated yet. A dedicated investigative sub-agent
    additionally confirmed the generational allocator (`gc/src/gen_heap.rs::alloc_object` and
    siblings) writes the full object header synchronously, under the arena lock, before ever
    returning the pointer to any caller (commit `74dc80b8d`, 2026-07-03) — closing the one
    remaining question about whether a kind-verified operand-stack root could ever observe an
    unpublished header. G1 and ZGC were NOT independently audited for the same allocator
    invariant (their allocators release the region/arena lock before the header write), so the
    implementation scoped the change to `VmHeap::is_generational()` only, leaving G1/ZGC on the
    unconditional pre-existing validation.

    Implementation: `ValueStack::scan_object_refs_split` appends roots in the SAME single pass as
    `scan_object_refs` (a first two-pass draft was measurably slower — see Performance below —
    and was replaced), additionally recording the (normally empty) list of offsets that came from
    the separate, unrelated "loose JNI-long-smuggle" candidate path (untagged `Long`/`Double`
    slots, still validated exactly as before). `memory::roots::scan_stack_roots_boundary` (new,
    shared by all four call sites) skips the `is_object_address` re-probe entirely for the
    trusted majority, gated on `VmHeap::is_generational()` (new) and a default-on opt-out flag
    `CRATONVM_TRUST_TAGGED_STACK_ROOTS` (env_cache.rs), plus a `CRATONVM_DBG_VERIFY_TRUSTED_ROOTS`
    diagnostic that probes the skipped entries anyway and logs (without dropping) any mismatch —
    the empirical falsification test for the whole argument.

    **Correctness: extensively verified, zero issues found.** `cargo test -p cratonvm-gc --lib`:
    790/0 (matches the established baseline exactly). `cargo test -p cratonvm-vm --lib`: 2199
    passed / 16 failed, and the failing set is EXACTLY the pre-existing, already-documented
    9 debug-build-only `lock_order` + 7 parallel-race `jit::skip_list` tests (see Round-3's
    "16 then 17 failures" note above) — zero new failures. The `binarytrees` checksum oracle
    (`docs/internal/repros/gc-stress-bintrees-main-args/binarytrees.java`, the heaviest
    allocation/GC-stress repro in the tree) was run at all three documented depths with
    `CRATONVM_DBG_VERIFY_TRUSTED_ROOTS=1`: bt14 (checksum `3222190`), bt16 (`14985902`), bt18
    (`68332206`, `-Xmx8g`) — all three matched the documented golden checksums exactly, and ZERO
    trusted-root mismatches were logged across any of them (bt18 alone triggers a very large
    number of collections). Note bt14/16/18 are pure-bytecode benchmarks with NO native calls,
    so they exercise `collect_roots` (the STW mark path) heavily but not the native-call-triggered
    `update_root_snapshot` path — both were still covered since `collect_roots` is one of the four
    modified call sites.

    **Performance: does NOT deliver the hoped-for win — a measured regression, not landed.**
    `RequestMappingMessageConversionIntegrationTests` itself was not re-run end-to-end (it needs
    ~530-720s per the Round-3 data and this investigation's time budget did not allow a full
    before/after suite comparison); instead the isolated, properly-packaged Spring-bootstrap repro
    from the Round-2 investigation (`BootstrapRepro2.java`, `com.example.bootstraprepro` package,
    `AnnotationConfigApplicationContext` + embedded Tomcat, native-call-heavy — the same shape as
    the profiled bottleneck) was run under `CRATONVM_DBG_ROOTSNAP` (the in-tree diagnostic built
    specifically to measure this exact question), 30 iterations per run to average out host-load
    noise on the shared build host (confirmed heavily loaded — 15-20+ concurrent `cargo`/`rustc`
    processes from other sessions throughout this investigation):

    | Build | `update_root_snapshot` avg cost/call |
    |---|--:|
    | baseline (dev `1f8e398e`, doc-only-commits ancestor of this branch's base) | ~1.9–2.0 us |
    | this branch, optimization ON (`CRATONVM_TRUST_TAGGED_STACK_ROOTS=1`, default) | ~2.5–2.6 us |
    | this branch, optimization OFF (`=0`, same binary, old behavior) | ~2.2 us |

    Reproduced across TWO independent implementations (the original two-pass split, and the
    single-pass redesign written specifically to rule out double-iteration overhead as the cause)
    — both showed the same regression shape, so this is not attributed to the two-pass draft
    alone. The flag-OFF control run on the same v2 binary is ALSO slower than the true baseline
    (~2.2us vs ~1.9-2.0us), which isolates that at least part of the regression comes from
    refactoring `scan_object_refs` into two small `#[inline]` helper methods
    (`push_trusted_object_ref` / `push_smuggled_long_ref`) shared with the new split function —
    an extraction that looked behavior-preserving and low-risk but apparently changed
    inlining/codegen even on the code path that should be byte-identical to before. The remaining
    gap (flag ON vs flag OFF on the same binary) is the actual cost of the new
    split/boundary-filter machinery, separate from the extraction regression.

    **`perf record` was unavailable to root-cause this precisely**: `perf_event_paranoid=4` on
    the build host blocks unprivileged profiling, and `sudo perf record` (which has passwordless
    sudo on this host) ran past its own `timeout 120` wrapper without producing usable output —
    killed manually after several minutes with no progress. Without instruction-level attribution,
    it was not possible in this session to distinguish "LLVM declined to inline
    `scan_stack_roots_boundary` across the module boundary" from "the thread-local
    `RefCell`-guarded scratch buffer for untrusted-offsets has real per-call TLS/borrow-check
    cost" from some other codegen effect of the refactor.

    **Conclusion and recommendation**: the CORRECTNESS argument for this optimization is sound
    and thoroughly verified — the redundancy is real, provable from the `kinds` side-array's own
    invariants, and empirically confirmed to never misfire across the checksum oracle. The
    PERFORMANCE argument, which is the entire reason to make this change, is NOT confirmed — it
    is contradicted by direct, repeated measurement. Per this investigation's own mandate ("if you
    have any doubt about correctness [or value], don't land it; document the attempt and doubt
    instead"), this is NOT merged into `dev`. A future session with working `perf` (or `gdb`-based
    sampling, or a from-scratch `objdump`/`cargo asm` inspection of the generated code) should
    either (a) determine why the seemingly-inert `scan_object_refs` extraction regressed the
    flag-OFF control and fix the codegen issue, then re-measure the flag-ON case in isolation, or
    (b) if the fundamental costs really are TLS/call-boundary/branch overhead that eats the
    `is_object_address` savings for typical (shallow) operand stacks, conclude this specific
    avenue is not profitable and that the actual remaining path to closing
    `RequestMappingMessageConversionIntegrationTests`'s perf gap is the full precise-stack-maps
    project Round 2 already scoped (new stack-map generation for interpreter frames at
    native-call boundaries, reusing the verifier's existing type-inference machinery if one
    exists — NOT investigated in this round beyond confirming the JIT's existing
    `PreciseFrameInfo`/`scan_one_frame_precise` machinery in `vm/src/jit/conservative_roots.rs`
    is structurally scoped to JIT-compiled frames only, at GC-safepoint/JIT-frame-scan sites, and
    does not extend naturally to interpreter frames at native-call boundaries — a genuinely
    separate, larger piece of infrastructure).

    **Round 5 (2026-07-16, same-day follow-up): perf tooling fixed, live-profiled the real test
    class directly, root-caused PART of the regression — still not a confirmed win, still not
    landed.** A follow-up request specifically asked to (1) try fixing `perf_event_paranoid`
    rather than accepting it as a dead end, and (2) fall back to disassembly/size comparison if
    perf still didn't cooperate. Both were tried, in that order, with real findings from each.

    **Perf tooling**: `sudo sysctl kernel.perf_event_paranoid=-1` (authorized, tried first)
    immediately unblocked `perf record` on this host — the earlier `sudo perf record` hang in
    Round 4 was specifically because the kernel was still refusing the profiling syscalls
    underneath `sudo`, not a `sudo`/`timeout` interaction bug. With that fixed, live-attached
    `perf record -p <pid> -g --call-graph fp` was run against the ACTUAL
    `RequestMappingMessageConversionIntegrationTests` process (launched via a from-scratch JUnit
    launcher, `KRun.java`, against the real `spring-webflux` test classpath and
    `--enable-native-access` to route around the unrelated HIB-CV-26 abort documented above),
    attached ~65s into the run (past classloading/bootstrap, into steady-state Tomcat
    start/stop/test cycling) for a 35s capture window — the same "attach live, mid-run" technique
    the original Round-2 investigation used, this time reproduced directly rather than inferred.
    (The earlier Round-4 measurements, by contrast, used an isolated `BootstrapRepro2.java`
    micro-benchmark that turned out to be a poor proxy: profiled on its own, `is_object_address`
    was only ~0.6% of its CPU and `update_root_snapshot` ~1%, nowhere near the real class's
    profile shape — the noisy/contradictory BootstrapRepro2 numbers in Round 4 were mostly
    measuring something else entirely. Live-attaching to the real class is the correct technique;
    isolated micro-repros of this specific bottleneck should be treated with suspicion going
    forward unless their own profile independently confirms `is_object_address` dominance.)

    Live-attached baseline (`dev` `1f8e398e`) profile: `is_object_address` 9.82% self-time
    (`GenerationalHeap` 7.22% + `VmHeap` wrapper 2.60%), `update_root_snapshot` 6.82% — both
    meaningfully present, though not at the ~60% combined level the original live-profiling
    session reported (plausibly a different point in the run, a longer/differently-shaped
    capture, or JIT warm-up state; not fully reconciled).

    The Round-4 implementation was rebuilt fresh in a new worktree (the original was deleted
    mid-investigation) and re-profiled the same way: `is_object_address` INCREASED to 11.30%,
    `update_root_snapshot` to 12.69% — confirming Round 4's regression finding was real, not an
    artifact of the isolated repro, and reproducible via the correct live-attach technique too.

    **Redesign attempt**: hypothesized the regression was the extra cross-module
    `scan_object_refs_split` + `memory::roots::scan_stack_roots_boundary` call boundary (a
    thread-local `RefCell` scratch buffer plus a closure) increasing `update_root_snapshot`'s
    perceived cost and defeating an unrelated inlining decision. Redesigned to fold the entire
    trusted/untrusted validation logic directly into `ValueStack::scan_object_refs` itself — no
    new function, no cross-module call, no extra data structure. (Also simplified the
    JNI-long-smuggle branch: since `is_object_address` is a strict superset check of
    `is_heap_addr` — same alignment+region checks plus more — calling it alone reproduces the OLD
    "is_heap_addr then external is_object_address boundary filter" net behavior exactly, in one
    call instead of two.) Re-verified correctness with full rigor again: `cargo test -p
    cratonvm-gc --lib` 790/0; `cargo test -p cratonvm-vm --lib` 2199/16 (same pre-existing flaky
    set, zero new failures); `binarytrees` bt14/bt16/bt18 checksum oracle correct with zero
    `CRATONVM_DBG_VERIFY_TRUSTED_ROOTS` mismatches on all three.

    **Re-profiled live against the real class: the regression got WORSE, not better** —
    `is_object_address` rose further to 14.60%, though `update_root_snapshot` improved somewhat
    (12.69% → 10.98%). The redesign did not fix the core problem.

    **Root cause, PARTIALLY found via `nm --size-sort` disassembly comparison** (the
    coordinator's suggested fallback, tried after the redesign still didn't help): between
    baseline and the modified binary, `Frame::scan_local_objects_inner` — a function untouched by
    ANY of this investigation's code changes — grew from 761 bytes to 9,921 bytes (13x). Cross-
    checking against the live profiles confirms why: `local_liveness::live_locals_mask`, a
    separate symbol at 6.16% self-time in the baseline profile, is ABSENT ENTIRELY from the
    modified binary's profile — it has been fully inlined into `scan_local_objects_inner`. This
    is a real LLVM inlining-cascade side effect of fat-LTO + `codegen-units = 1` (this project's
    release profile): changing `scan_object_refs` — called immediately after `scan_local_objects`
    in every one of the four hot root-scanning call sites — altered the whole-program inliner's
    cost/benefit calculus for an entirely unrelated neighboring call site. This part of the
    "regression" looks mostly like a profile ATTRIBUTION change rather than a real slowdown: the
    combined cost is comparable before and after (0.21% + 6.16% = 6.37% baseline vs. 5.92% after,
    if anything slightly lower), and a rough throughput proxy — counting completed
    Tomcat-server start/stop cycles (one per sub-test) in a matched ~101-second window across all
    three binaries (baseline / first redesign / this redesign) — showed comparable progress (10
    vs. 9 vs. 9 cycles), consistent with no large real end-to-end regression, though this is too
    small a sample (9-10 data points) to confirm a genuine win either way.

    **What remains UNEXPLAINED**: `is_object_address` itself — a distinct function, not merged
    with anything else by the inlining cascade above — shows a real, consistent, and in fact
    GROWING self-time percentage across every attempt (9.82% → 11.30% → 14.60%), which is the
    opposite of what a change specifically designed to eliminate most calls to it should produce.
    The backend was confirmed to genuinely be `GenerationalHeap` for this run (the profiled symbol
    is literally `GenerationalHeap::is_object_address`, which cannot appear at all if a different
    `VmHeap` backend were active), so the new fast path is provably engaging, not silently falling
    through to the unchanged branch. Ran out of investigation time budget to pin this down further
    — the next step would be a call-COUNT diagnostic (not just perf's time-based sampling) to
    directly confirm whether the trusted-branch skip is reducing `is_object_address` call volume
    by the expected amount, or whether some other call site (the JNI-smuggle branch's now-direct
    `is_object_address` call, `scan_locals_conservative`, `scan_active_jit_frames`, or something
    else entirely) is calling it more often than before for a reason unrelated to this change.

    **Conclusion, updated**: perf access was successfully restored (a one-line `sysctl`, safe on
    this dev host) and used for direct, live profiling of the real target test class — the
    strongest evidence-gathering this investigation has had. A genuine partial root cause was
    found and is well-supported (the LTO inlining-cascade attribution shift), but the central,
    original question — does `is_object_address`'s actual cost go down as intended — is NOT
    resolved, and the live data currently shows the opposite for that specific function. Per this
    investigation's explicit mandate to attempt real root-causing but not over-invest indefinitely,
    and given the stakes of touching GC-root-scanning code, this is STILL NOT merged into `dev`.
    The redesigned, still-correctness-verified implementation (commit `145c13b4`, superseding the
    original `faedea48`) remains on branch `fix/precise-native-scan-20260716` (pushed, unmerged)
    for a future session with more time budget for IR/assembly-level LLVM inlining investigation,
    or for a call-count-based diagnostic to isolate exactly which call site is responsible for
    `is_object_address`'s unexplained growth.
*   ~~`web.reactive.result.view.script.JRubyScriptTemplateTests`~~ **FIXED (2026-07-15) --
    all 6 chained bugs closed, test class PASSES.** JRuby's own
    bootstrap (`rubygems/specification.rb` / `rubygems/version.rb`) turned out to hit a CHAIN of
    independent CratonVM bugs, each masking the next -- fixing one just exposes the next further
    into the same bootstrap. Root-caused and fixed so far:

    1. **FIXED, commit `d8ae2b96`.** `org.jruby.runtime.BlockCallback` in JRuby 10.x (the ACTUAL
       test classpath is `jruby-base`/`jruby-stdlib` 10.0.2.0 -- an earlier pass in this
       investigation misdiagnosed against a stale `jruby-complete-9.1.17.0.jar` decompile and
       chased an unrelated GC-safety gap first) declares one abstract SAM
       `call(ThreadContext, IRubyObject[], Block)` plus five same-named DEFAULT overloads,
       including `call(ThreadContext, IRubyObject, Block)` (scalar) which should wrap its
       argument into a 1-element array and re-invoke the real SAM.
       `interpreter::lambda_args_sam_compatible` (`vm/src/runtime/interpreter.rs`)
       unconditionally skipped array-typed SAM parameters (`!pd.starts_with('L')` is true for
       `[...`, so the "generic/erased — never second-guess" catch-all swallowed them), so it
       could never tell the scalar DEFAULT `call` apart from the array-taking abstract SAM.
       `try_lambda_dispatch` then fed the raw scalar (a `RubySymbol`, e.g. `:foo` from
       `Enumerable#partition`'s per-element block callback) straight into the array-typed
       lambda body, so `RubyEnumerable.packEnumValues(ThreadContext, IRubyObject[])` executed
       `arraylength` against a bare `RubySymbol` — the `[GC-ARRAY-GUARD]` hit — silently
       returning 0 instead of throwing, which produced the empty `"@#{key} = nil"` →
       `"@ = nil"` corruption that a nested `eval()` rejected as a `SyntaxError`. Fix: when a
       SAM parameter descriptor is array-typed, require the actual argument to be null,
       missing, or a genuine array. A related, narrower GC-safety hardening (pin
       `obj_ref`/`call_args` across `lambda_args_sam_compatible`'s class-loading-capable
       helpers, mirroring the existing `invoke_virtual` fix in `d64fab85`) landed alongside it
       in commit `6d652338` — legitimate but, on its own, insufficient for this bug.

    2. **FIXED, commit `3af9ab62`.** Fixing (1) let bootstrap progress past the `SyntaxError`
       and immediately into a `NullPointerException` in JRuby's own indy-based
       `org.jruby.ir.targets.indy.IsTrueSite.init` (`rubygems/version.rb`'s
       `canonical_segments`, `@canonical_segments ||= ...`'s truthiness test).
       `MethodHandles.filterReturnValue` (`native-builtins/src/lang_invoke.rs`) was a no-op
       stub ("simplified: return the target MH unchanged", silently dropping the filter
       handle). JRuby's `VariableSite.ivar` ivar-getter call-site targets rely on
       `filterReturnValue` to substitute the runtime's `nil` singleton for the raw Java `null`
       that `IRubyObject.getInstanceVariable` genuinely returns for an unset ivar (normal at
       that raw layer). With the filter dropped, `mh.invoke()` returned the raw `null`
       straight through, and `IsTrueSite.init` crashed calling `.getRuntime()` on it. Fix:
       added `MH_KIND_RETURN_FILTER` + `mh_dispatch_return_filter`, mirroring the existing
       `MH_KIND_FILTER`/`MH_KIND_CATCH` adapter pattern — invoke target, pass its result
       through the filter handle, return the filter's result.

    **Evidence for (1)+(2)**: minimal `ScriptEngineManager` repro
    (`[:foo,:bar].map {|key| "@#{key} = nil"}.join`, and separately `require 'erb'; require
    'ostruct'` for (2)) now produces correct output / no longer NPEs; the `[GC-ARRAY-GUARD]`
    warning count for a full `JRubyScriptTemplateTests` run dropped from 6 to 0;
    `cargo test -p cratonvm-vm --lib --release` unchanged at 2203 passed / 9 pre-existing
    `--release`-only `lock_order` failures; `cargo test -p cratonvm-native-builtins --lib
    --release` unchanged at 2997 passed / 0 failed.

    **OPEN — bug 3, current blocker, NOT fixed (updated 2026-07-15, round 2).** Past (1)+(2),
    the same minimal repro (and the full test class) still hits `ArgumentError: wrong number of
    arguments (given 0, expected 1..2)` at `rubygems/version.rb:413` (`canonical_segments`'s
    `@version.sub(regex, "")` call), raised from deep inside REAL gem-dependency resolution
    (`Gem::Dependency#to_spec` → `#to_specs` → `#matching_specs` → `Specification.find_all_by_name`
    → `Requirement#satisfied_by?` → `RubyComparable#>=` → `Version#<=>` → `#canonical_segments`) —
    reached only once (1) and (2) let bootstrap progress far enough to resolve real gem versions.

    **Round 1** traced via `CRATONVM_DBG_INDY_GENERIC=1` to
    `org.jruby.ir.targets.simple.NormalInvokeSite.bootstrap`'s
    `invoke:sub(ThreadContext, IRubyObject, IRubyObject, IRubyObject, IRubyObject)` call site: the
    operand stack legitimately holds exactly 5 values when `bootstrap_generic`
    (`vm/src/runtime/invokedynamic.rs`) pops them (no stack-depth mismatch), but the VALUES are
    wrong: the receiver slot holds the `Gem::Version` instance itself (`self`) instead of
    `@version`'s string value, and a stray `Regexp` literal lands in the replacement-string/block
    argument slots instead of the frozen `""` string.

    **Round 1 fix (genuine but did not close this)**: `MethodHandles.dropArguments`
    (`native-builtins/src/lang_invoke.rs`, `MH_KIND_DROP`) computed how many arguments to discard
    as `extra_args.len() - inner_expected`, re-deriving `inner_expected` at DISPATCH time by
    re-parsing the wrapped inner handle's reported descriptor — fragile for nested/chained
    `dropArguments` (JRuby's `InvokeSite` composes SIX `dropArguments` calls per call site, several
    nested). Fixed to store the exact drop count explicitly at construction time (`"pos:count"`
    encoded in `MH_CLASS`) instead of re-deriving it — commit `6a77cd93`, with a new regression
    test (`drop_arguments_dispatch_keeps_correct_slot_not_adjacent_ones`, previously zero
    MethodHandle-combinator-dispatch test coverage existed in this file). Verified as a genuine,
    independent correctness fix (`cargo test -p cratonvm-native-builtins --lib --release`: 2998
    passed / 0 failed) — but the minimal repro still hits the IDENTICAL `ArgumentError` afterward:
    `dropArguments` is not even exercised on this specific call's path.

    **Round 2 — full combinator-chain tracing (added `MH_DISPATCH_ARGS`, alongside the existing
    `CRATONVM_DBG_MH_DISPATCH`, to print every dispatch step's actual argument VALUES, not just
    argc).** Walked the ENTIRE chain for the specific `ivarGet:@version` call that produces the
    wrong receiver:
    `guardWithTest(test=insertArguments(testRealClass, classId), target=filterReturnValue(target=
    RubyObject5.var0-getter, filter=insertArguments(Helpers.nullToNil, nilSingleton)),
    fallback=...)`, dispatched with `[self]`. EVERY step computes the mathematically correct
    value: `testRealClass(classId, self)` → true; `var0(self)` → `@version`'s real `FString`
    value; `nullToNil(FString, nil)` → the same `FString` (non-null passthrough); the WHOLE chain's
    logged `"indy-generic] target MH invoke result"` is correctly that `FString`. `guardWithTest`,
    `insertArguments` (both occurrences), and `filterReturnValue` (this investigation's own
    round-1 fix, commit `3af9ab62`) all forward/receive exactly the args JDK semantics require —
    no combinator in this chain is at fault.

    Despite the computation being correct, the FOLLOWING `invoke:sub` call's popped operand-stack
    values show BOTH `self` (the ORIGINAL, pre-`ivarGet` value) AND the correctly-computed `FString`
    result present as separate stack slots (`self` at the position `FString` should occupy, and
    every value after it shifted one slot right) — i.e. the `ivarGet` `invokedynamic` instruction's
    result push did not correctly REPLACE `self` on the operand stack; `self` and the result both
    ended up present. `bootstrap_generic`'s own pop/push mechanics were re-read line-by-line and are
    correct (`ValueStack::pop_compact` is a genuine decrement-then-read, not a peek). This rules
    OUT every MethodHandle combinator AND `bootstrap_generic`'s own stack bookkeeping as the fault;
    the extra `self` most likely comes from an ORDINARY (non-`invokedynamic`) bytecode instruction
    — a `dup`/extra `aload` — in JRuby's own dynamically-IR-compiled snippet for this call
    instruction, which CratonVM's core opcode interpreter (not the MethodHandle/indy subsystem)
    executes. This snippet is synthesized at runtime by JRuby's own IR-to-bytecode compiler (not
    present as a static `.class` file `javap` can decompile), so confirming the exact instruction
    responsible would need bytecode-level dumping/disassembly of runtime-generated method bodies —
    a different, larger investigation than MethodHandle-combinator auditing (which this round
    conclusively ruled out via direct argument-value tracing, not by assumption).

    **Evidence for round 2**: `cargo test -p cratonvm-native-builtins --lib --release` unchanged at
    2998 passed (2997 baseline + the round-1 regression test) / 0 failed; `cargo test -p
    cratonvm-vm --lib --release` unchanged at 2205 passed / 9 pre-existing `--release`-only
    `lock_order` failures; minimal repro (`require 'erb'; require 'ostruct'`) and the full
    `JRubyScriptTemplateTests` class both still fail with the identical `ArgumentError` at
    `rubygems/version.rb:413` after every fix landed so far. `JRubyScriptTemplateTests` remains
    FAILING end-to-end — flagged for a dedicated follow-up with bytecode-level tracing of the
    runtime-generated `canonical_segments` snippet's actual dup/aload/astore instruction sequence.

    3. **FIXED, commit `9a6e945a`.** Round 3 followed the coordinator's instruction to dump the
       ACTUAL runtime-generated bytecode (new `CRATONVM_DBG_BYTECODE_DUMP` diagnostic in
       `push_frame_and_fire_entry`, `vm/src/runtime/interpreter.rs`) for `canonical_segments`'s
       `.sub()` call site. The disassembly DISPROVED round 2's "extra `self` from a stray
       `dup`/`aload`" hypothesis outright: the `aload_2` (self) at the call site is a
       deliberate, compiler-emitted instruction (JRuby's own call-site convention pushes the
       CALLER's `self` as a visibility-check argument, separate from the receiver) -- not an
       accidental leftover. Re-decompiling the real `jruby-base-10.0.2.0.jar`'s
       `org.jruby.ir.targets.indy.InvokeSite`/`NormalInvokeSite` classes via `javap -p` showed
       BOTH real `invoke(...)` overloads take a trailing plain `IRubyObject[] args` array
       parameter (`(ThreadContext, IRubyObject, IRubyObject, IRubyObject[], Block)` and
       `(ThreadContext, IRubyObject, IRubyObject[], Block)`) -- NEITHER is Java `ACC_VARARGS`
       (ordinary `T[]`, not `T...`). Live `CRATONVM_DBG_MH_DISPATCH` tracing against the FULL
       `JRubyScriptTemplateTests` run (not just the minimal repro) confirmed the terminal
       `NormalInvokeSite.invoke` dispatch: its OWN recorded descriptor (now also traced --
       added `desc={desc:?}` to the dispatch log) correctly reports the 5-param array-typed
       signature, but it was invoked with 6 FLAT values
       `[ctx, self, receiver, Regexp, Block, replacement-FString]` -- the Block landed in the
       array's slot, pushing the true last argument out past it. Root cause:
       `collect_trailing_varargs` (`native-builtins/src/lang_invoke.rs`) -- the function
       responsible for packing excess trailing MethodHandle-dispatch arguments into a target's
       trailing array parameter -- only did so when the target was Java `ACC_VARARGS`. JRuby's
       `invokebinder`-built call chain supplies these trailing values via a sequence of
       correctly-implemented `MethodHandles.insertArguments` splices (traced and verified
       directly -- not the bug) and never calls `asCollector` (confirmed absent from the entire
       dispatch chain via the same trace), so by dispatch time an arity mismatch against an
       array-typed last param is the only signal available, and the old ACC_VARARGS-only gate
       missed it for this (and any other) plain-`T[]`, non-varargs target. Fix: also collect
       when `params.len() > declared_param_count` and the last declared param is an array type,
       independent of ACC_VARARGS. `cargo test -p cratonvm-native-builtins --lib --release`:
       2999 passed / 0 failed; `cargo test -p cratonvm-vm --lib --release`: 2205 passed / 9
       pre-existing `--release`-only `lock_order` failures, unrelated.

    4. **FIXED, commit `fc9c3a85`.** Live tracing (with bug 3's fix applied) showed the
       `RubyComparable.op_ge` -> `JavaMethod$JavaMethodOneOrTwoBlock` `ArgumentError` (given 0,
       expected 1..2) was NOT a different dispatch mechanism after all -- it was the SAME
       `collect_trailing_varargs` (`native-builtins/src/lang_invoke.rs`) that bug 3 touched,
       with two more precise gaps in the SAME function, both hit by the exact same
       `canonical_segments` call chain immediately after the `.sub()` call bug 3 fixed:
       (a) it assumed the array-typed parameter is always the descriptor's LAST parameter
       (`ptypes.last()`), but JRuby's Ruby-call convention routinely appends a trailing `Block`
       parameter AFTER the args array (`InvokeSite#invoke(ThreadContext, IRubyObject,
       IRubyObject, IRubyObject[], Block)`), so the array sits at `len - 2`, not `len - 1`, and
       the old code bailed out before even checking arity -- worse, because CratonVM's
       `MethodHandles.insertArguments` chain never collapses the array until this function runs,
       the `Block` value is frequently spliced into the MIDDLE of what should become the array's
       contents (traced: `[..., Regexp, Block, replacement]`), so a naive "last N positions"
       split can't recover the right grouping even once the array is found; fixed by locating
       the array by scanning (not assuming last) and recovering the split by matching each
       trailing declared type (e.g. `Block`) against its RUNTIME class within the tail region,
       pulling matches out wherever they actually sit. (b) the `arity_excess` trigger
       (`params.len() > declared_param_count`) only caught cases with MORE flat args than
       declared params; a single data value destined for a 1-element array arrives with
       `params.len() == declared_param_count` exactly (no excess) but the value at the array's
       position is a bare scalar -- traced on `org.jruby.ir.targets.indy.SelfInvokeSite.invoke`
       (the very next call in the same chain, right after `.sub()`): arrived with exactly 4 flat
       args matching declared arity, 3rd (array-typed) slot holding one bare `IRubyObject`,
       surfacing as `ArgumentError: wrong number of arguments (given 0, expected 1)` inside the
       interpreted Ruby method it called; fixed by also triggering collection when the exact
       arity matches but the array-position value isn't already array-shaped (nor null).
       `cargo test -p cratonvm-native-builtins --lib --release -- --test-threads=1`: 2999
       passed / 0 failed (the default multi-threaded run showed 1 unrelated pre-existing
       test-ordering flake, `lang_system::checkexec_security_tests::
       denying_sm_blocks_runtime_exec_before_spawn`, confirmed passing alone and under
       `--test-threads=1`); `cargo test -p cratonvm-vm --lib --release`: 2205 passed / 9
       pre-existing `--release`-only `lock_order` failures, unrelated.

       **Shared root cause found** (per the coordinator's request to check across bugs 1/3/4):
       bugs 3 and 4 turned out to share ONE root cause -- both were gaps in the SAME
       `collect_trailing_varargs` function, now closed together in this one commit. Bug 1
       (SAM/lambda array-vs-scalar dispatch, `lambda_args_sam_compatible` in `interpreter.rs`)
       remains a genuinely separate code path -- already independently confirmed in the round-1
       investigation, no further common root found there.

    **Evidence for bug 3+4 combined**: with both fixes applied, the minimal repro (and the full
    `JRubyScriptTemplateTests` class) progress completely PAST `rubygems/version.rb` -- both the
    `op_ge` arity bug and the `SelfInvokeSite` scalar-wrap bug are gone, `Gem::Version` comparison
    now works end-to-end -- into `require 'ostruct'`, hitting a new, unrelated failure.

    5. **FIXED, commit `d250607f`.** Root cause: `bootstrap_generic`
       (`vm/src/runtime/invokedynamic.rs` -- the generic, uncached fallback used to link any
       indy call site not specially handled) builds the BOOTSTRAP METHOD's own argument list
       FLAT (`[lookup, name, methodType, static_arg_1, ..., static_arg_N]`), correct for the
       common bootstrap shapes seen elsewhere (`(Lookup,String,MethodType)`, or fixed-arity
       extra args) but wrong for a bootstrap method whose LAST formal parameter is `Object[]`,
       collecting ALL extra constant-pool bootstrap arguments (JVMS-legal; mirrors a Java
       varargs method) -- exactly JRuby 10.x's string-interpolation bootstrap,
       `BuildDynamicStringSite.buildDString(Lookup, String, MethodType, Object[])` (confirmed
       via `javap`). Without packing, the declared `Object[] bsmArgs` parameter received
       `bsm_args[3]` -- the FIRST static bootstrap arg, a scalar, not an array;
       `BuildDynamicStringSite`'s own constructor then computes `bsmArgs.length - 6` as a
       metadata offset, CratonVM's `arraylength`-of-non-array guard silently returns 0 for the
       scalar, the offset goes negative, and the next `aaload` throws
       `ArrayIndexOutOfBoundsException` at `<init>` -- reached via `JRubyScriptTemplateTests`'s
       `require 'ostruct'` (`ostruct.rb:477`, inside `OpenStruct`'s class body). Fixed by adding
       `descriptor_param_count_and_last_is_object_array` (deliberately narrower than
       `native-builtins`'s `collect_trailing_varargs` -- per JVMS this bootstrap-method
       collecting parameter is always both syntactically LAST and always exactly `Object[]`, so
       none of that function's Block-after-array ordering complication applies here) and, when
       the bootstrap descriptor's last param is `[Ljava/lang/Object;` and more static args were
       supplied than declared params, packing the excess into a real `Object[]` (boxing
       primitive `Value`s via `Integer/Long/Float/Double.valueOf` -- `Object[]` elements must be
       references) before invoking. Verified: `<init>` no longer throws
       `ArrayIndexOutOfBoundsException` in any of 9 repeated `JRubyScriptTemplateTests` runs.
       `cargo test -p cratonvm-native-builtins --lib --release -- --test-threads=1`: 2999
       passed / 0 failed; `cargo test -p cratonvm-vm --lib --release`: 2205 passed / 9
       pre-existing `--release`-only `lock_order` failures, unrelated.

    **Evidence for bug 5**: with the fix applied, `BuildDynamicStringSite` construction itself
    (the `<init>` crash) is reliably gone across repeated runs -- but a DIFFERENT, downstream bug
    in the SAME class's runtime dispatch then surfaces (see bug 6 below), so
    `JRubyScriptTemplateTests` still does not pass reliably yet. One isolated run DID pass
    end-to-end (found=1 succ=1 fail=0 status=OK) before bug 6 was characterized, confirming the
    remaining gap is narrow, but 8 of 9 repeated runs since have hit bug 6's `ClassCastException`
    -- NOT claiming the class passes; treating the single pass as most likely a timing-dependent
    window rather than a reliable state.

    6. **FIXED, commit `78198922`.** Initial round-5 hypothesis (repeated below for the trail,
       then corrected): after bug 5's fix, `BuildDynamicStringSite` construction succeeds, but its
       RUNTIME string-building dispatch threw `java.lang.ClassCastException:
       org.jruby.runtime.ThreadContext cannot be cast to org.jruby.runtime.builtin.IRubyObject`
       inside `RubyString.append` <- `appendAsStringOrAny` <- `BuildDynamicStringSite.buildString`.
       Live `CRATONVM_DBG_MH_DISPATCH` tracing showed the final `buildString` dispatch (5-param
       overload `(ThreadContext, IRubyObject, ByteListAndCodeRange, Encoding, int)`) receiving 6
       flat args `[ThreadContext, ThreadContext, ByteListAndCodeRange, Encoding, Integer,
       RubySymbol]` -- a duplicated `ThreadContext` sitting where the dynamic interpolated value
       (`RubySymbol`) should be, with that value pushed to the very end instead. Traced upstream to
       a `MethodHandles.permuteArguments` (`MH_KIND_PERMUTE`) step whose reorder `int[]` reads
       `[0, 0, 1]`. **Initial suspicion that this reorder array itself was wrong was DISPROVED** by
       `javap`-decompiling the real, unmodified `com.headius.invokebinder-1.14.jar`'s `Binder`
       class plus `BuildDynamicStringSite`'s own constructor bytecode: the `[0, 0, 1, ...]` shape is
       computed by genuine, correct invokebinder bytecode with a deliberate "one `(ThreadContext,
       value)` pair per interpolated segment" stride pattern, and `MH_KIND_PERMUTE`'s dispatch was
       independently re-verified against the real JDK `permuteArguments` semantics and found
       correct. The REAL root cause, one level further downstream: right after the permute,
       JRuby's bytecode (via `Binder.collect(index, count, type, filterMH)`, itself calling
       `MethodHandles.collectArguments(target, pos, filter)`) is meant to consume each
       `(ThreadContext, value)` pair through a `to_s`-guard filter handle, REPLACING the pair with
       one converted `IRubyObject`. `collectArguments` (`native-builtins/src/lang_invoke.rs`) was a
       complete no-op stub -- `Ok(Some(args.first().copied()...))`, returning the target unchanged
       and silently dropping `pos`/`filter` entirely (the same failure shape as the
       `filterReturnValue` no-op bug fixed in round 1, commit `3af9ab62`). Without it, the
       duplicated `ThreadContext` was never reduced away and survived unchanged into
       `buildString`'s `IRubyObject` slot. Fixed by adding `MH_KIND_COLLECT_ARGS` +
       `make_collect_args_adapter` + `mh_dispatch_collect_args`, modeled on the existing
       `MH_KIND_FOLD`/`foldArguments` machinery (same wrapper shape) but with REPLACE semantics
       (the consumed range is replaced by the filter's result) instead of fold's
       splice-in-addition-to-the-full-list semantics. `cargo test -p cratonvm-native-builtins --lib
       --release -- --test-threads=1`: 2999 passed / 0 failed. `cargo test -p cratonvm-vm --lib
       --release -- --test-threads=1`: 2197 passed / 17 failed -- 9 are the documented pre-existing
       `lock_order` failures; the other 8 (`jit::skip_list::tests::*`) were confirmed, via `git
       stash` (this fix backed out, rebuilt, retested: identical 17/17 failures), to be ALREADY
       present on `dev` independent of this change -- a separate, pre-existing regression from
       elsewhere on the shared branch, not this investigation's concern.

    **Final verification**: `JRubyScriptTemplateTests` now PASSES reliably -- confirmed across 19
    repeated runs total (`found=1 succ=1 fail=0 skip=0 abort=0 status=OK`), spanning multiple
    fresh release rebuilds (including at the final `dev`-rebased tip immediately before landing).
    This closes the entire chained-bug investigation: 6 independent, individually real and
    verified CratonVM correctness bugs, spanning 4 genuinely distinct subsystems -- SAM/lambda
    array-vs-scalar dispatch (bug 1), a `filterReturnValue` no-op stub (bug 2), MethodHandle
    trailing-array-collection gaps in `collect_trailing_varargs` shared by `dropArguments`'s
    position bookkeeping and two array/arity shapes (bugs 3 and 4), an invokedynamic
    bootstrap-method `Object[]`-varargs-collection gap (bug 5), and a `collectArguments` no-op
    stub (bug 6) -- each masking the next until fixed. `MethodHandles.collectArguments` and
    `MethodHandles.permuteArguments` are the SECOND and THIRD distinct MethodHandle combinators
    (after `dropArguments`) found to have real implementation gaps in this file, each only
    surfacing under complex, deeply-nested real-world composition shapes like JRuby's own
    `invokebinder`-built call sites -- worth treating `native-builtins/src/lang_invoke.rs`'s other
    combinators (`foldArguments`, `guardWithTest`, `filterArguments`, `insertArguments`) as
    similarly under-exercised by existing test coverage, and worth a dedicated pass adding direct
    nested/chained-composition unit tests for them before the next multi-hour bug hunt finds
    another one this way.
*   ~~Batch-context `<clinit>` contamination~~ — RETRACTED, see above (host environment issue: missing /tmp + missing ~/jdk25 symlink, not CratonVM).

## 5. Combined verification (2026-07-15, post-merge sign-off)

Independent, from-scratch verification of `dev` tip `22dfc55e` after all three parallel
2026-07-15 fix sessions landed together (`ClientHttpConnectorTests` 2 fixes `91cb806c`/`3d1449a7`,
`RequestMappingMessageConversionIntegrationTests` 5 fixes `4290124b`/`96c8a57f`/`b7a1ed84`,
`JRubyScriptTemplateTests` 6 fixes `d8ae2b96`/`3af9ab62`/`9a6e945a`/`fc9c3a85`/`74745ee4`/`78198922`).
Fresh worktree (`verify/final-reactive-cluster-20260715`), fresh release binary
(`~/vmfix-finalverify-20260715`), independent of any fixing agent's own build/binary.

**Good news first:**
*   `cargo test -p cratonvm-native-builtins --lib --release`: **2999 passed / 0 failed** (top of
    expected 2997-2999 range).
*   `cargo test -p cratonvm-gc --lib --release`: **787 passed / 0 failed** (exact baseline match).
*   `cargo test -p cratonvm-native-io --lib --release`: **349 passed / 0 failed** (exact baseline
    match).
*   `cargo test -p cratonvm-vm --lib --release`: **2197 passed / 18 failed** — all 18 are
    pre-existing and already documented, not new: 9 are `jit::skip_list::tests::*` (unrelated
    concurrent Hibernate-longtail session, confirmed via `git log -S` on `jit/skip_list.rs` —
    doc's own JRuby-fix section already noted 8 of these before that session added a 9th) and 9
    are `runtime::lock_order::tests::*`, all of which fail under plain `--release` simply because
    lock-order enforcement is gated off by default outside debug builds (`vm/src/runtime/
    lock_order.rs`, opt back in with `CRATONVM_LOCK_ORDER_CHECK=1`); re-ran with that env var set
    and 8 of the 9 immediately pass, the 9th (`enforcement_active_in_debug_builds`) asserts
    `cfg!(debug_assertions)` itself and is *designed* to fail under `--release` — not a functional
    regression, a test/harness nuance.
*   `JRubyScriptTemplateTests`: **12/12 clean runs** (`found=1 succ=1 fail=0`), fresh independent
    binary — corroborates the "FULLY FIXED" claim.
*   `RequestMappingMessageConversionIntegrationTests`: **completes in 563s** (well under the
    documented 1200s bound) with **158/160 passing**. This is *better* than the doc's "still does
    NOT complete" residual note — worth a doc update in section 2/4, not investigated further here
    for time. The 2 failures are both HTTP 500s on the `[3] Reactor Netty` and `[4] Tomcat` server
    backends (`org.springframework.web.client.HttpServerErrorException$InternalServerError`), not
    yet root-caused.

**Real finding — `cargo test --workspace --release` does not compile.** `cratonvm-jit`'s own
`#[cfg(test)]` code fails with 11× `error[E0063]: missing field `force_native_cache` in
initializer of `cratonvm_jit_api::CachedBytecodeMethod`` at `jit/src/lib.rs:7963,8109,8179,8633,
8772,8883,8998,9055,9115,10319,10333`. Root cause confirmed via `git log -S'force_native_cache'`:
commit `3d1449a7` ("perf(interp): memoize force_native_over_real_jdk_bytecode per invoke-cache
entry", the round-2 `ClientHttpConnectorTests` fix) added the `force_native_cache: OnceLock<bool>`
field to `CachedBytecodeMethod` in `jit-api/src/lib.rs` and correctly updated every *production*
construction site (`vm/src/runtime/interpreter.rs`, `vm/src/runtime/vtable.rs`,
`vm/src/jit/helpers.rs`, `vm/src/runtime/lockfree_resolve.rs`) but never touched
`jit/src/lib.rs`, whose own unit-test fixtures build `CachedBytecodeMethod` literals directly.
Reproduce: `cd <dev-tip-checkout> && cargo test -p cratonvm-jit --lib --release`. This means the
`jit` crate's own unit test suite has been uncompilable on `dev` since `3d1449a7` landed, and
nobody has been able to run it standalone since — worth a one-line follow-up fix (add
`force_native_cache: std::sync::OnceLock::new()` at the 11 sites) but per this task's scope, not
applied here.

**Real finding — `ClientHttpConnectorTests` hangs 21/21 times (100%), not ~27%.** Ran the class
15× under moderate host load (matching the doc's own stress-test setup) and, after confirming
that wasn't a load artifact, 6 more times completely isolated (host load average 1.09, zero other
CratonVM/cargo processes) — **every single one of the 21 runs timed out at 180s**, versus the
doc's own clean 15-run measurement of 4/15 (27%) immediately after `3d1449a7` landed. Every one of
the 21 hangs shows the identical signature on the `MockWebServer` connection-handling thread,
seconds into the run:
```
ERROR [mockwebserver3.MockWebServer] MockWebServer{port=...} connection from 127.0.0.1/127.0.0.1 crashed
java.lang.ClassCastException: java.lang.Object cannot be cast to okio.Segment
	at okio.SegmentPool.take(SegmentPool.kt:81)
	at okio.Buffer.writableSegment$okio(Buffer.kt:1440)
	at okio.internal.DefaultSocket$SocketSource.read(DefaultSocket.kt:120)
	...
	at java.util.concurrent.ThreadPoolExecutor.runWorker(ThreadPoolExecutor.java:1090)
```
i.e. a pooled-object slot that should hold an `okio.Segment` yields a plain `Object` instead —
classic type-confusion in a shared pool/cache, not present in the isolated `3d1449a7` verification
(that investigation's own writeup describes a pure CPU-bound spin in
`force_native_over_real_jdk_bytecode`/interpreter dispatch, no exception of any kind). This reads
as a genuine "concurrent fix + concurrent fix" interaction: `3d1449a7` itself is a caching change
to `CachedBytecodeMethod`/invoke-cache entries, landing alongside `b7a1ed84`'s own JIT
code-range/free-list caching changes (`gc/src/arena.rs`, `jit/src/lib.rs`) from the
`RequestMappingMessageConversionIntegrationTests` session — plausibly corrupting shared pooled-
object bookkeeping under concurrent load, though the exact mechanism has not been root-caused here
(out of scope — verification only). Repro: build `dev` tip `22dfc55e` release,
`cratonvm --java-home <jdk25> --enable-native-access=ALL-UNNAMED KRun
org.springframework.http.client.reactive.ClientHttpConnectorTests`, repeat a handful of times.

**Second, related finding — `ClassCastException: java.util.concurrent.CompletableFuture cannot be
cast to io.netty.util.concurrent.Future`.** Surfaced consistently in a 36-class representative
slice of the reactive cluster (see below): every failing sub-test of
`ReactorClientHttpConnectorTests` (3/5 failing, previously 5/5 OK in the 02:xx same-day baseline)
and 5 sub-tests of `RSocketClientToServerIntegrationTests` (previously 1/12 failing, now
12/12 — all failing) show this exact cast failure, e.g.:
```
RSocketClientToServerIntegrationTests :: echo() :: java.lang.AssertionError: expectation
"expectNext(Hello 1)" failed (expected: onNext(Hello 1); actual: onError(
java.lang.ClassCastException: java.util.concurrent.CompletableFuture cannot be cast to
io.netty.util.concurrent.Future))
```
Both affected classes are Reactor-Netty-backed. Given the JRuby fix cluster's changes are all in
MethodHandle/invokedynamic dispatch (`native-builtins/src/lang_invoke.rs`,
`vm/src/runtime/invokedynamic.rs` — Reactor Netty's own `CompletableFuture`⇄Netty-`Future` bridging
is lambda/MethodHandle-heavy), this is a plausible second instance of the same "combined fix
interaction" class as the `okio.Segment` one above, but likewise not root-caused here.

**Broader 36-class reactive sample** (representative slice of the 295-class sweep in section 4,
not the same day-of exhaustive run — direct comparison confounded by the fact **most of the
02:xx same-day baseline's EMPTY/ABEND/LOADERR/TIMEOUT statuses turned out to be caused by the
suite driver never passing `--enable-native-access`, not real bugs** — re-running the identical
36-class list with the flag on flips the large majority of those categories to OK, e.g.
`DefaultWebClientTests` EMPTY→OK 25/25, `RequestMappingIntegrationTests` FAIL 20/15/5→OK 20/20,
`WebClientIntegrationTests` TIMEOUT→completes in 12.8s with 126/170 passing,
`RSocketServiceMethodTests` ABEND→OK 4/4). Net effect is strongly positive, but four classes look
worse than the (imperfectly comparable) same-day baseline and are flagged for follow-up rather
than treated as confirmed regressions given the confound above:
*   `ReactorClientHttpConnectorTests` — was OK 5/5/0, now FAIL 5/2/3 (see `CompletableFuture`/
    `Future` CCE above).
*   `RSocketClientToServerIntegrationTests` — was FAIL 12/0/1, now FAIL 12/0/12 (same CCE family).
*   `JythonScriptTemplateTests` — was OK 1/1/0, now FAIL 1/0/1,
    `java.lang.ExceptionInInitializerError: null` (not investigated further).
*   `WebSocketIntegrationTests` — was FAIL 72/48/24, now FAIL 72/24/48 (pass/fail ratio inverted;
    not investigated further).

**Bottom line**: the three fix sessions' own individually-claimed numbers hold up (native-builtins,
gc, native-io unit suites clean; `JRubyScriptTemplateTests` genuinely solid;
`RequestMappingMessageConversionIntegrationTests` now actually completes, better than documented).
But the combined tip has at least two real, previously-undocumented defects not visible to any
single fixing session's own isolated verification: (1) `cratonvm-jit`'s test target does not
compile (trivial one-line-per-site fix, `3d1449a7`'s responsibility), and (2) `ClientHttpConnectorTests`
and Reactor-Netty-backed classes generally are now hanging/failing dramatically more than the
27% figure `3d1449a7` was verified against in isolation, with two consistent `ClassCastException`
signatures (`okio.Segment`, `io.netty.util.concurrent.Future`) that were never observed during that
session's own investigation. This is exactly the "concurrent fix + concurrent fix ≠ correct"
failure mode this verification pass was commissioned to check for. **Recommend NOT treating the
`ClientHttpConnectorTests`/reactive-connector residual as merely "diffuse throughput, 27% hang,
accepted" going forward — re-open it as a correctness regression, not a perf residual.**


### 5.1 Follow-up (2026-07-16): the "combined-fix regression" was refuted; real independent bug found + fixed for one of the two signatures

Investigated both real findings from section 5 above with the same rigor as the original
`3d1449a7` session. Bottom line: **neither `ClassCastException` was caused by any of the three
2026-07-15 concurrent fix sessions.** Both are pre-existing CratonVM bugs, invisible until this
verification pass because no prior test harness (this investigation's own, nor any of the three
fixing sessions' own verification) had ever passed `--enable-native-access=ALL-UNNAMED` to the
launcher — so `java.lang.foreign.MemorySegment`'s clinit always failed early with
`IllegalCallerException`, and depending on exactly where that failure was first triggered,
either got silently tolerated (the common case, logged as a `<clinit> failed` warning and
ignored) or occasionally produced a hard `Class.forName` failure that aborted the whole run
before any real test executed — masking whatever happened downstream, including both bugs below.

**`okio.Segment` `ClassCastException` — root-caused and FIXED.** Reverted exactly `3d1449a7`'s
diff from `22dfc55e` (clean `git revert`, nothing else touched), rebuilt, reran with
`--enable-native-access=ALL-UNNAMED`: **the identical crash + 100% hang still reproduced**,
proving `3d1449a7` innocent. Went one step further and reproduced the identical crash on
`b5c8f43f` — the commit immediately *before* any of the three 2026-07-15 sessions touched
anything. Root cause: `java/lang/Thread.getId()` (`native-builtins/src/lib.rs`) was hardcoded to
return the constant `1` for every thread in the process. Okio's `SegmentPool` (okio-jvm 3.x,
used by MockWebServer/OkHttp) shards its lock-free segment free-list across
`HASH_BUCKET_COUNT = highestOneBit(availableProcessors()*2-1)` (16 on this host)
`AtomicReference<Segment>` buckets via `Thread.currentThread().getId() & (HASH_BUCKET_COUNT-1)`.
With `getId()` always `1`, every thread collapsed onto the identical bucket, concentrating the
entire process's segment-pool churn onto one shared `AtomicReference` and exposing a race under
that artificially extreme contention. `Thread.threadId()` (the JDK 19+ replacement, registered
separately in `phases_late.rs`) already did this correctly via `ctx.thread_id()`; fixed the
legacy `getId()` to match. Verified via a standalone probe: threads now get distinct real IDs.
**Result: 100% → 0% hangs** (10/10 clean completions with `--enable-native-access=ALL-UNNAMED`,
vs. the 21/21 hangs section 5 documented). Landed: commit `19a5025f` (also fixes the
`cratonvm-jit` test-fixture compile break from section 5's first finding — 11 sites in
`jit/src/lib.rs` never got `3d1449a7`'s `force_native_cache` field because that commit's sweep
was driven by `cargo build --release` errors, which don't compile `#[cfg(test)]` code; verified
`cargo test -p cratonvm-jit --lib`: 905/0, was: compile error).

**`CompletableFuture`/`io.netty.util.concurrent.Future` `ClassCastException` — confirmed real,
independent, NOT resolved.** With the hang eliminated, `ClientHttpConnectorTests` now completes
cleanly every time but with `33/49` passing (was `44/49` on a lucky non-hung pre-fix run) — the
16 failures split into 5 already-known/classpath-related (1 `NoClassDefFoundError: ByteBuddy`,
3 `NoClassDefFoundError: AssertJ Assumptions`, 1 unrelated `IOException`) plus **11 genuinely new
failures**, all `ClassCastException: java.util.concurrent.CompletableFuture cannot be cast to
io.netty.util.concurrent.Future`, all on Reactor Netty sub-tests. Checked whether this also
pre-exists: 3 clean (non-hung) runs of the true pre-everything baseline (`b5c8f43f`, also with
`--enable-native-access=ALL-UNNAMED`) show **zero** occurrences of this failure — only the same 5
known ones. Unlike the `okio.Segment` bug, this one does NOT clearly reproduce on the pre-session
baseline in the samples gathered, so it cannot yet be ruled either "definitely pre-existing but
previously masked by the hang" or "a genuine interaction surfaced by the `getId()` fix restoring
real thread identity" — the sample size (3 clean baseline runs) is too small to be confident
either way, and this was NOT root-caused (searched `native-builtins/src/*.rs` for any
Netty-`Future` bridging code and found none — this looks like a bytecode/lambda-dispatch-level
type-confusion, not a missing native override, plausibly connected to the JRuby session's
MethodHandle/invokedynamic dispatch changes as section 5 already speculated, but not verified).
**Flagged for dedicated follow-up, not force-fixed here** — same standard this investigation has
applied throughout: land what's verified, document what isn't.

**Current true state of `ClientHttpConnectorTests`**: 0% hangs (was 100%), 33/49 (67%) passing
per run, consistently reproducible. A real, large improvement over the section-5 regression
report, not yet a full fix — 11 sub-tests fail deterministically on the `CompletableFuture`/
Netty-`Future` cast bug above.

Verified (dev tip after `19a5025f`): `cargo test -p cratonvm-native-builtins --lib` 2999/0,
`cargo test -p cratonvm-jit --lib` 905/0, `cargo test -p cratonvm-vm --lib` 2212 passed / 9
failed (all 9 are the pre-existing `jit::skip_list::tests::*` failures section 5 already
attributed to an unrelated concurrent Hibernate-longtail session — confirmed unchanged, not
caused by this fix).


### 5.2 CompletableFuture/Netty-Future `ClassCastException` — root-caused and FIXED (2026-07-16)

Closes the "flagged for dedicated follow-up" item from 5.1 above.

**Reproduction.** Rather than rely on JRun's own stack-trace capture (Reactor's
`StepVerifier`/`onError` signal path does not preserve the original exception's real stack trace
as a Java `cause` chain, only a message string — this genuinely blocked the initial investigation),
added a temporary diagnostic directly in the interpreter's `checkcast` handler
(`vm/src/runtime/interpreter.rs`, gated behind a new `CRATONVM_DBG_CCE_TRACE` env var, reverted
before landing the real fix) that dumps the live call-frame stack at the exact moment a checkcast
to a `netty`-named class fails. This immediately surfaced the real throw site:

```
[cce-trace] checkcast FAILED: obj=java.util.concurrent.CompletableFuture target=io.netty.util.concurrent.Future
[cce-trace]   frame[108] class=io/netty/util/concurrent/AbstractEventExecutor method=submit(Ljava/lang/Runnable;)Lio/netty/util/concurrent/Future;
[cce-trace]   frame[107] class=reactor/netty/resources/ColocatedEventLoopGroup method=<init>(Lio/netty/channel/EventLoopGroup;)V
```

`javap -c -p` on `io.netty.util.concurrent.AbstractEventExecutor` confirmed its `submit(Runnable)`
override does `invokespecial AbstractExecutorService.submit(Runnable)` (to get the default
implementation) then `checkcast io/netty/util/concurrent/Future` on the result. The real JDK's
`AbstractExecutorService.submit()` internally calls `newTaskFor(runnable)` — a method
`AbstractEventExecutor` overrides to hand back a Netty `PromiseTask` (which implements Netty's
`Future`) instead of a plain JDK `FutureTask`.

**Root cause.** `native_es_submit_runnable`/`native_es_submit_callable` (`native-builtins/src/lib.rs`)
are registered on `ExecutorService`, `AbstractExecutorService`, and `ThreadPoolExecutor` alike, and
unconditionally ran CratonVM's synthetic single-threaded-immediate-execution model, handing back a
plain `CompletableFuture` via `completed_executor_future()` — regardless of whether the receiver
was one of CratonVM's own synthetic placeholder executors or a genuinely-real bytecode object. Per
this VM's established rule that `invokespecial` always prefers a registered native over real
bytecode, `AbstractEventExecutor`'s `invokespecial AbstractExecutorService.submit(...)` landed on
the synthetic native instead of the real `AbstractExecutorService.submit()` bytecode, silently
bypassing the `newTaskFor()` override and producing a `CompletableFuture` where the caller
immediately `checkcast`s to `io.netty.util.concurrent.Future`.

This is exactly the real-vs-synthetic ambiguity `execute()`/`shutdown()` already guard against
(`executor_has_real_workers()` + `invoke_special_bytecode_only` redispatch to real bytecode) — in
fact `invoke_special_bytecode_only`'s own doc comment explicitly lists `submit` as part of "the
canonical example" of natives needing this guard, but the guard was never actually applied to
`submit()` itself, only to `execute`/`shutdown`.

`executor_has_real_workers()` could not simply be reused: it disambiguates only the
`ThreadPoolExecutor` class-name collision (CratonVM's own synthetic placeholder is deliberately
stamped with the real `ThreadPoolExecutor` class name so field writes/native dispatch line up) via
a per-instance `workers` field probe. Netty's `AbstractEventExecutor`/`SingleThreadEventExecutor`
are not `ThreadPoolExecutor`s at all and have no `workers` field, so the existing check would
misreport them as synthetic.

**Fix.** Added a generalized `executor_is_real(ctx, exec)` helper: resolve the receiver's actual
runtime class name (unwrapping the `Executors$DelegatedExecutorService`-style `e` field indirection
first, same as before); for the ambiguous `ThreadPoolExecutor` case keep the existing `workers`
field probe; for the bare interface markers `ExecutorService`/`ScheduledExecutorService`/`Executor`
(used only by CratonVM's own wrapper placeholders in `phases_late.rs` — no real object's runtime
class can ever literally be an interface) always report synthetic; any other concrete class name
reaching this code path (Netty's classes, a real `ForkJoinPool`, a user subclass, ...) is
necessarily real, since CratonVM never fabricates a synthetic executor under any other class name
— confirmed by grepping every `alloc_concurrent_synthetic(ctx, "java/util/concurrent/...", ...)`
call site in the tree. `native_es_submit_runnable`/`native_es_submit_callable` now check
`executor_is_real()` first and, for a real receiver, redispatch via
`invoke_special_bytecode_only("java/util/concurrent/AbstractExecutorService", "submit", ..., args)`
— matching the `invokespecial` call site and letting the receiver's real `newTaskFor()` override
run, exactly mirroring the established `execute()`/`shutdown()` pattern.

**Verification.**
- `ClientHttpConnectorTests`: 0 `CompletableFuture`/`Netty-Future` `ClassCastException`s across 20
  clean runs (was 11 deterministic failures every run), 0 hangs. Pass rate 42-44/49 per run (was
  33/49); the residual 5-7 failures per run are pre-existing and unrelated to this bug: missing
  `bytebuddy`/`assertj` on this ad hoc harness's classpath (4-5 failures, a harness gap not a VM
  bug), an occasional Jetty `EofException` connection-flake, an unrelated enum `valueOf()`
  `ClassCastException` (a known separate synthetic-enum gap), and one flaky `StepVerifier`
  exception-identity assertion.
- `ReactorClientHttpConnectorTests` (the class 5.1 specifically named for a definitive pre/post
  check): 5/5 across 5 clean runs, both pre- and post-rebase-to-dev-tip.
- `RSocketClientToServerIntegrationTests`: 12/12 across 3 clean runs, both pre- and
  post-rebase-to-dev-tip (was 1/12 in section 5's original report, then regressed further to 0/12
  under the same `CompletableFuture`/Netty-`Future` signature — RSocket's TCP transport also
  routes through Netty's `AbstractEventExecutor.submit()`).
- `cargo test -p cratonvm-native-builtins --lib`: 2999/0 (unchanged).
- `cargo test -p cratonvm-jit --lib`: 905/0 (unchanged).
- `cargo test -p cratonvm-vm --lib`: 2197 passed / 18 failed. 9 are the pre-existing
  `jit::skip_list::tests::*` failures already attributed (section 5) to an unrelated concurrent
  Hibernate-longtail session. The other 9 are new `runtime::lock_order::tests::*` failures,
  confirmed unrelated to this fix: that module (added by an unrelated concurrent commit,
  `3dd488a9`, "env-gated runtime opt-in for lock-order enforcement in release builds") gates its
  enforcement behind `cfg!(debug_assertions)`, and this verification pass ran `cargo test
  --release` (debug assertions off) rather than a debug build — a pre-existing test/build-profile
  mismatch in that unrelated module, not caused by or related to executor/native dispatch.

Landed: commit `9850617b` on `dev` (rebased cleanly onto dev tip `436ec59b` before push, re-verified
against the post-rebase binary).

**Current true state of `ClientHttpConnectorTests`**: 0% hangs, 0 `CompletableFuture`/Netty-`Future`
cast failures, 42-44/49 (86-90%) passing per run — up from 33/49 (67%) in 5.1 and the original
21/21-hang regression in section 5. The remaining ~5-7 failures per run are all pre-existing,
independently-tracked gaps (harness classpath completeness, an unrelated enum dispatch bug, and
test-infra flakiness), none of them the `AbstractExecutorService`/Netty-`Future` bug this section
closes out.


### 5.3 Follow-up (2026-07-16): does the `getId()` fix explain the historical "diffuse throughput, 27% hang" residual? — real contributor, confirmed causally, but not the dominant cause

Directly investigates whether the "27% hang rate" / diffuse hashbrown-parking_lot-mimalloc-Arc/Weak
CPU-bound-spin residual documented under section 4 (`ClientHttpConnectorTests`, round 2, commit
`3d1449a7`) — at the time judged a systemic "interpreter/allocator throughput ceiling," not a
discrete bug — was actually substantially caused by the since-fixed `Thread.getId()` bug
(`19a5025f`, section 5.1), given the mechanism (`Okio SegmentPool` collapsing every thread onto one
shared `AtomicReference<Segment>` bucket, forcing extra CAS retries/allocations/lock traffic) looks
exactly like the kind of diffuse cost the 27% investigation observed.

**Method.** Built three binaries from a fresh `git fetch origin dev` at tip `5e13631a` (worktree
`wt-diffuse-throughput-20260716`, `cargo build --release`, independent of any prior session's own
binary) plus one deliberately-regressed control binary, and ran the real
`ClientHttpConnectorTests` class (not a synthetic probe) through it directly, matching this
effort's established methodology throughout section 5. Measured *test-completion* time (the
`RESULT ...` line the harness prints), not process-exit time — confirmed separately that the
process legitimately never exits on its own after the JUnit run completes (`[cratonvm] main()
returned; VM held alive by 32 non-daemon thread(s) (JVM-spec behaviour)`), which would otherwise
make every single run misreport as a "hang" under a naive wall-clock-to-process-exit measurement.

**Current true state, reconfirmed independently: 0/60 hangs.**
*   20 runs against the existing verified `dev`-tip binary (`wt-final-verify-20260716`, confirmed
    functionally identical to `5e13631a` via `git diff --stat` — the only commits between its build
    point and current tip are docs-only): **0/20 hangs**, wall times 9.5-26.1s, 44-46/49 passing
    (one run found only 45 tests, a discovery flake, not a hang).
*   20 runs against a from-scratch independent build (`~/vmfix-diffusethroughput-20260716`):
    **0/20 hangs**, wall times 9.0-19.1s, 42-46/49 passing (one outlier run found only 17/49 tests
    — a one-off test-discovery flake under this ad hoc harness, not reproduced elsewhere and not a
    VM hang).
*   10 rounds (20 process launches) of **two simultaneous instances** of the full 49-sub-test class
    — deliberate added contention beyond any single prior session's own stress conditions, since the
    original diffuse-cost investigation's own methodology explicitly ran "under moderate host load":
    **0/20 hangs**, wall times 8.0-34.4s (only under this doubled contention does wall time approach
    the historical 30s hang bound — never observed in any single-instance run).
*   **Total: 60/60 clean completions across three independent stress batches, 0% hang rate** — a
    real, fully-confirmed resolution of the 27% figure section 4 documented, corroborating (and
    independently reproducing, with a fresh build) section 5.2's own 0/20 finding.

**Hot-thread sampling: the diffuse *flavor* is still present, just no longer pathological.**
24 live `sudo gdb -p <pid> --batch -ex 'thread apply all bt'` captures across 4 independent launches
(identifying the CPU-hottest thread via `top -H` first, same technique as the original 41-sample
investigation) during normal (non-hung, completing-within-10-25s) runs found leaf frames spread
across: SIMD memcpy/memcmp/memset intrinsics, `parking_lot` lock/unlock (multiple distinct call
sites), interpreter dispatch (`execute_invokevirtual_cached`, `nth_param_tag_byte`), `Arc` drop,
`mimalloc` allocation (`mi_page_malloc_zero`), a classloading B-tree range lookup
(`find_in_multi_release_archive`), and blocking syscalls (legitimate socket I/O, not spinning) —
the same general *shape* (many small, unrelated costs, no dominant single site) as the historical
41-sample breakdown, but every capture comes from a thread that is doing bounded, real work in a
run that reliably finishes in seconds, not an indefinite spin. This is consistent with "diffuse
interpreter/allocator throughput cost" remaining a genuine, still-open architectural characteristic
of this VM — it just no longer manifests as an unbounded hang now that the discrete bugs that used
to push individual runs over the edge are fixed.

**Direct causal A/B experiment (not just correlation).** Built a fourth, deliberately-regressed
control binary: same `dev` tip `5e13631a`, with *only* the `native-builtins/src/lib.rs`
`Thread.getId()` registration reverted back to the pre-`19a5025f` hardcoded `Ok(Some(Value::Long(1)))`
(everything else — the executor-`submit()` fix `9850617b`, the SATB-buffer fix `5fa116fd`, both
blocking-region fixes, etc. — left intact). Ran the identical 20-run single-instance stress
protocol:

| Binary | n | mean wall | max wall | runs > 30s |
|---|---|---|---|---|
| Fixed (existing verified binary) | 20 | 15.9s | 26.1s | 0 |
| Fixed (fresh independent build) | 20 | 13.1s | 19.1s | 0 |
| **Control (`getId()` reverted only)** | 20 | **17.5s** | **33.4s** | **2 (10%)** |

Reinstating *only* the `getId()` bug on an otherwise-fully-fixed tip measurably slows the class down
(mean +10-25%, worst case +7-14s) and is the only one of the three batches to ever cross the 30s
mark the original investigation used as its hang threshold — direct, reproducible, causal evidence
that the mechanism `19a5025f`'s commit message describes (SegmentPool bucket collapse → CAS
retry/allocation storm) is real and does contribute measurable diffuse cost, not a hypothesis.
**However, it does not come close to reproducing the historical 27% hang rate on its own**: 0/20
runs failed to complete within the 60s bound (vs. an expected ~5/20 if `getId()` alone explained the
original 27% figure), and critically, **the `okio.Segment ClassCastException` from section 5.1 did
not reproduce at all** in this control batch (`grep` for the signature across all 20 logs: zero
matches) — even though this is the exact bug section 5.1 attributed it to. The difference: this
control binary already has the executor-`submit()`/Netty-`Future` dispatch fix (`9850617b`)
applied, which section 5.1's own repro conditions (tip `22dfc55e`) did not. This indicates the
historical 100% CCE-crash regression (section 5) needed `getId()`'s collapse *compounding with*
something else — most plausibly the missing `--enable-native-access` flag masking/reshaping which
code paths executed at all, as section 5.1 itself already found for the crash's *visibility* — not
`getId()` in isolation.

**Conclusion.** `Thread.getId()`'s collapse was a real, now causally-confirmed contributor to this
class's diffuse per-call contention/throughput cost — but it was never, by itself, the dominant
cause of either the original 27% hang rate (section 4) or the later 100% CCE-crash regression
(section 5). Both of those needed the `getId()` bug compounding with other factors: the original
27% was measured *after* `3d1449a7`'s `force_native_over_real_jdk_bytecode` memoization already cut
a 60% hang rate to 27% and *before* the two blocking-region fixes and the executor-`submit()` fix
existed at all; the 100% CCE regression needed the missing `--enable-native-access` flag and the
still-unfixed executor-`submit()` gap alongside it. The combined effect of *all* of this session's
fixes — not `getId()` alone — is what took `ClientHttpConnectorTests` from a documented 27% hang
rate down to a measured, reproducible, causally-stress-tested **0% (0/60)** on the current `dev`
tip. The "diffuse hashbrown/parking_lot/mimalloc/`Arc`/`Weak` per-call cost" the section 4
investigation catalogued is **not eliminated** — it remains visible in hot-thread sampling with the
same general shape — but it no longer pushes any observed run past the point of actually failing to
complete, even under deliberate 2x concurrent contention. Recommend downgrading section 4's framing
from "closing it further needs a genuine interpreter/allocator throughput initiative" (implying an
open reliability problem) to "a legitimate, still-open *performance* characteristic with no
currently-observed reliability impact" — the hang symptom itself is resolved and independently
reconfirmed, not merely reasoned about.

No code change lands from this section — it is a verification/root-cause-attribution pass. The
`control/getid-reverted-20260716` branch/worktree used for the A/B experiment is a throwaway
(deliberately regresses a fixed bug) and is not merged.
                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        

### 5.4 `TestTemplateInvocationContext` `ClassCastException` — dynamic-test discovery undercounting root-caused; genuine but PARTIAL fix landed (2026-07-16)

Investigated the low-frequency (roughly 1/15-1/45 across this session's own
sampling) `ClassCastException: java.lang.Object cannot be cast to
org.junit.jupiter.api.extension.TestTemplateInvocationContext` hitting
`http.client.reactive.ClientHttpConnectorTests`'s `@ParameterizedTest`
methods (`basic(ClientHttpConnector, HttpMethod)` and others, e.g.
`partitionedCookieSupport(ClientHttpConnector)`). When it fires, JUnit 5's
dynamic-test discovery for that class truncates partway through (the
class-wide total drops from 49, e.g. to 17 when it hits early in `basic`'s
49-way connector x HTTP-method cross product) — the same symptom this
effort had earlier observed independently as an uncharacterized
"intermittent JUnit dynamic-test discovery undercounting" issue, now with a
concrete exception signature.

**Reproduction.** Reused the `finalverify0716-runner` harness (`KRun.class`,
`af/web.txt` classpath, `KRUN_STACK=1` for full stack traces) in a fresh
isolated worktree, looping `ClientHttpConnectorTests` with `timeout 40`
per attempt (the class's own non-daemon threads never let the process exit
on its own — killing it after the `RESULT` line is expected, not a hang;
see section 5's stress-harness note). Caught the first live occurrence
after ~24 attempts (~1/24 that run); a second independent stress run
confirmed the base rate at roughly 1/45 (1 hit in 45 clean-binary runs).

Full stack trace (previously only a one-line message had been captured):

```
java.lang.ClassCastException: java.lang.Object cannot be cast to org.junit.jupiter.api.extension.TestTemplateInvocationContext
	at org.junit.jupiter.engine.descriptor.TestTemplateTestDescriptor$TestTemplateExecutor.createInvocationTestDescriptor(TestTemplateTestDescriptor.java:116)
	at org.junit.jupiter.engine.descriptor.TemplateExecutor.createInvocationTestDescriptor(TemplateExecutor.java:89)
	at org.junit.jupiter.engine.descriptor.TemplateExecutor.lambda$executeForProvider$0(TemplateExecutor.java:57)
	at org.junit.jupiter.engine.descriptor.TemplateExecutor.executeForProvider(TemplateExecutor.java:57)
	at org.junit.jupiter.engine.descriptor.TemplateExecutor.execute(TemplateExecutor.java:46)
	at org.junit.jupiter.engine.descriptor.TestTemplateTestDescriptor.execute(TestTemplateTestDescriptor.java:112)
	... (JUnit Platform hierarchical-executor frames) ...
	at KRun.runOne(KRun.java:54)
```

`javap` on `junit-jupiter-engine-6.1.1.jar` confirmed `TestTemplateTestDescriptor.java:116`
is a compiler-generated *bridge method* — `TestTemplateExecutor extends
TemplateExecutor<TestTemplateInvocationContextProvider,
TestTemplateInvocationContext>`'s erasure-mandated
`createInvocationTestDescriptor(UniqueId, Object, int)` override, which
`checkcast`s the `Object` argument to `TestTemplateInvocationContext` before
delegating to the real typed method — completely standard `javac`-generated
generics-erasure bytecode, unconditionally correct on a real JVM.

**Root cause.** Live `CRATONVM_DBG_CCE=1` capture (an existing, already-wired
diagnostic — `crate::runtime::env_cache::cce_dbg()`,
`vm/src/runtime/interpreter.rs`'s `Checkcast` handler) on the failing
checkcast showed `obj_cid=0 obj_class=java/lang/Object` — the object being
cast genuinely has class-id 0 at the moment of the check, i.e. it is not a
real, wrongly-typed object; it is a *bare, reused* `java.lang.Object`
allocation. This is the exact signature this codebase's own checkcast
handler already has a documented comment for ("S-trinity #1: when the
runtime class is a bare `Object` / cid=0 (synthetic alloc that lost
class_id)...") and matches the already-tracked, currently-OPEN
"register-invisible root" / cross-thread GC-root-visibility race family
documented in
[[wildfly-standalone-boot-attributeaccess-cce-register-invisible-root]]
(`docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`)
— this is a **fifth independent occurrence** of that family (after the
`AttributeAccess`/`AttributeDefinition` WildFly-boot occurrences, the
`invoke_virtual` lambda-SAM-compat stale-locals site, and the CHM
stale-at-store windows), now in a structurally different call shape:
single-thread JUnit 5 dynamic-test dispatch racing against long-lived
background Reactor Netty / MockWebServer threads' own GC-triggering
allocation, rather than WildFly's ~30-40-thread `parallel-extension-add`
boot storm.

**A concrete, previously-unidentified contributing mechanism was found and
fixed for this call shape.** `ClientHttpConnectorTests`'s
`@ParameterizedTest` methods all funnel through JUnit's
`ParameterizedInvocationContextProvider.provideInvocationContexts` (decompiled
via `javap`), which builds
`sources.stream().map(...).map(...).map(...).flatMap(...).map(createInvocationContext)`
— a real `java.util.stream.Stream` pipeline that CratonVM implements with its
own native lazy-stream machinery (`native-collections/src/lib.rs`), not real
bytecode. Two of that machinery's functions —
`native_stream_for_each` (the materialize-then-consume path a `.forEach()`
with an upstream chain takes) and `stream_pull_internal` (the generic
chain-driving pull used to materialize a stream's elements, including a
`flatMap` stage's inner stream) — each pin a **whole batch** of `Stream`
elements up front via `pin_value_slice`, then drive **long, re-entrant**
per-element Java execution (JUnit's own dynamic-test dispatch machinery,
which for this class means a full HTTP round trip per invocation) that can
tier up into JIT.

JIT-compiled code has **no periodic cooperative safepoint poll** of its own
(see `jit_safepoint_flush_satb`'s doc comment in `vm/src/jit/helpers.rs`:
"JIT-running threads have no such poll — they only return through one of
the runtime helpers below" — i.e. only when a JIT-emitted allocation
happens to fail its fast path and falls through to a GC-triggering runtime
helper). This thread's *deposited* root snapshot
(`thread.root_snapshot`, refreshed by `update_root_snapshot`/
`deposit_root_snapshot`) is the **only** view a peer-initiated
stop-the-world collection has of `native_pin_roots` for a thread that gets
forcibly frozen via the STW takeover mechanism while executing JIT code:
the takeover's own conservative register/stack scan
(`vm/src/jit/xt_root_scan.rs`) has no knowledge of `native_pin_roots` (a
plain Rust-heap `Vec`, not something conservative register/stack scanning
would discover). Confirmed directly from the existing GC-safety comment on
`NativeContextImpl::pin_native_root` (`vm/src/vm/vm_exec.rs`): "a pin pushed
while this thread's `in_blocked_region` flag is raised is invisible to
**both** the STW root scan (which reads the deposit-time snapshot) and the
blocked-thread fold... the object dies or moves and the pin is never
remapped." The specific gap closed here is the "actively running" sibling
of that documented "blocked" case: `native_stream_for_each`/
`stream_pull_internal` pin a batch of objects, then run for a long,
JIT-heavy stretch **without ever blocking or self-initiating GC**, so
nothing refreshes the deposit between "pins pushed" and "peer's takeover
freeze" — a live pin can be invisible to a peer's mark phase and get
reclaimed under Generational's non-moving frozen-peer sweep; the next
(correctly pin-revalidated) read observes the reused memory as a bare
`java.lang.Object`.

**Fix.** Added `NativeContext::refresh_root_snapshot()` (default no-op;
`native-api/src/registry.rs`), overridden on `NativeContextImpl`
(`vm/src/vm/vm_exec.rs`) to call the pre-existing
`deposit_root_snapshot()` — the identical mechanism already used before
blocking natives, just invoked without actually blocking. Called it right
after `pin_value_slice` establishes each element batch, and again at the
top of every per-element loop iteration, in both `native_stream_for_each`
and `stream_pull_internal`.

**Verification — genuine improvement, NOT a full fix.**

- `cargo test -p cratonvm-native-collections -p cratonvm-vm -p
  cratonvm-native-api --lib` (post-rebase onto fresh `dev`): 179/179
  (`native-api`), 73/73 (`native-collections`), 2198/2215 (`cratonvm-vm`; 17
  "failures" — 16 are the doc's own already-documented pre-existing baseline
  [`jit::skip_list::tests::*` — an unrelated concurrent session's flakes —
  plus `runtime::lock_order::tests::*`, gated behind `cfg!(debug_assertions)`
  and expected to fail under `--release`], the 17th,
  `jit::helpers::tests::jit_getfield_never_tears_against_concurrent_jit_putfield_int`,
  passed cleanly in isolation — a host-contention flake under this session's
  parallel test run, not a regression: this branch never touches
  `jit/helpers.rs`).
- Picked up and fixed, separately, an unrelated pre-existing test-compile
  break on `dev`: `c812b622` ("fix(bytebuddy): resolve native compat shim
  field lookup by ClassId, not name") added
  `NativeContext::resolve_field_index_by_class_id` with no default impl but
  never updated `native-collections/src/lib.rs`'s own inline `MockCtx` test
  fixture, so `cargo test -p cratonvm-native-collections --lib` did not
  compile on `dev` before this session's rebase. Landed as a separate,
  trivial, obviously-correct commit (`None` stub — the mock has no field
  layout model) in the same branch/PR, not squashed into the GC fix.
- Stress-tested `ClientHttpConnectorTests` **254 runs total post-fix**
  across three build iterations (84 runs with an earlier single-site version
  of the fix covering only `native_stream_for_each`; 170 runs with both
  sites patched): **4 recurrences of the exact same
  `ClassCastException`/`obj_cid=0` signature** (~1/64 aggregate), on `basic`
  once and `partitionedCookieSupport` three times. This is somewhat lower
  than this session's own pre-fix measurement (1/45) and well below the
  originally-documented ~1/15-1/20 range, but the sample sizes on both sides
  are small enough that this should be read as **suggestive, not proven,
  improvement** — a two-proportion comparison of 1/45 pre-fix vs. 4/254
  post-fix is not clearly significant. Adding the second call site
  (`stream_pull_internal`) did not measurably change the recurrence rate
  versus the single-site version (1/84 vs. 3/170), consistent with the
  residual living in a **different, uncovered window this fix's granularity
  cannot close**: the refresh happens once before a per-element loop and
  once between iterations, but `accept()`'s own execution for a single
  element (an entire HTTP round trip, potentially spanning multiple peer GC
  cycles on its own) is not itself covered by any further refresh — true
  elimination needs a genuine cooperative safepoint poll inside JIT-compiled
  code, which is exactly the precise-oop-map / shadow-stack infrastructure
  this whole bug family's roadmap doc
  (`docs/feature-designs/precise-jit-maps-default.md`) already names as the
  real fix.

**Bottom line — matches this bug family's established pattern exactly.**
This is a real, verified, zero-regression, low-risk improvement: a genuine
gap in the deposited-root-snapshot mechanism, precisely identified from
source and live diagnostic capture, safely closed using an existing,
already-proven mechanism (`deposit_root_snapshot`) applied to a previously
uncovered call shape. It is **not** a complete fix — the `ClassCastException`
still reproduces post-fix, at a lower but not conclusively-proven-lower
rate — consistent with every other narrow contributing-site fix found for
this same open "register-invisible root" family to date (the
`AttributeAccess` doc's own `invoke_virtual` lambda-SAM-compat fix and CHM
pin fixes both reduced but did not eliminate their respective residuals).
Landed per this project's established policy for this family: land the
verified, safe, narrow improvement; document the residual honestly; do not
force a deeper fix into the cross-thread GC synchronization protocol under
time pressure without the ability to fully verify it.

Landed: branch `fix/testtemplate-cce-20260716`, commits `8a5c2274`
(the GC fix) and a follow-up MockCtx compile-break fix, rebased onto `dev`
tip `6c517cd9` before push.

### 5.5 `RequestMappingMessageConversionIntegrationTests` 2/160 HTTP 500 residual (section 5) — investigated, does NOT reproduce on current `dev`; treating as already-resolved (2026-07-16)

Follow-up on section 5's own "not yet root-caused" note: **2 HTTP 500s
(`HttpServerErrorException$InternalServerError`) on the `[3] Reactor Netty`
and `[4] Tomcat` server backends**. Investigated from scratch, dedicated
session, fresh worktree (`/data/data/wt-reqmapping-http500-20260716`, branch
`fix/reqmapping-http500-20260716`), fresh release binary
(`vmfix-reqmapping-http500-20260716`) off `dev` tip `6178c36f`
(`--enable-native-access=ALL-UNNAMED`, real JDK 25).

**Could not reproduce, at all, after exhaustive per-test-invocation
verification.** A full-class `KRun` pass completed in 560s with `fail=0`
(`status=OK`), but with a lower `found` count than expected (found=83 of a
theoretical 160 test-template invocations) — not trusted at face value, so
built a custom `VerboseRun.java` JUnit-Platform-Launcher harness
(`DiscoverySelectors.selectMethod(class, name, HttpServer.class.getName())`
+ a `TestExecutionListener` printing `TSTART`/`TEND` for every leaf
`test-template-invocation`) and ran **every one of the 40
`@ParameterizedHttpServerTest` methods individually across all 4 backends**
(small batches of 2-5 methods per fresh JVM, to sidestep an unrelated,
already-documented, low-frequency GC race — see below). Result: **160/160
individual method×backend combinations `SUCCESSFUL`, zero `FAILED`, zero
`HttpServerErrorException`**, across 9 separate JVM invocations including
both suspect-looking methods (`personResponseBodyWithCompletableFuture`,
`personTransformWithCompletableFuture` — CompletableFuture-based bodies,
the most plausible executor-identity-dispatch suspects) and the
threading/timing-sensitive ones (`personTransformWithFluxDelayed`, the
XML-marshalling `*Xml` variants, `resource`).

**Conclusion: this residual is already fixed on current `dev`, most likely
as a side effect of one or both of two unrelated fix sessions that landed
*after* section 5's original report** (dev tip `22dfc55e`, 2026-07-15) **and
before this investigation's tip** (`6178c36f`/`704aedd3`, 2026-07-16):
commit `19a5025f` (section 5.1, `Thread.getId()` hardcoded-to-`1` fix) and
commit `9850617b` (section 5.2, `AbstractExecutorService.submit()`
real-vs-synthetic-executor redispatch fix for Netty's
`AbstractEventExecutor`). Both land squarely on the real-thread /
executor-identity mechanics that a backend-specific (Reactor Netty and
Tomcat are this class's only two backends with real, JDK-executor-backed
thread pools; Jetty/Jetty Core are not) failure in a
CompletableFuture-touching message-conversion test would plausibly hit;
neither fix was targeted at this class, so the resolution was not
independently re-verified end-to-end before now. Not re-attempted as a
target-the-old-tip bisection (would need a second ~30 min release build
under this session's severe host contention) given the 160/160
current-tip pass rate is already strong, direct evidence.

**One unrelated, already-known, already-partially-fixed instability
surfaced during this sweep and cost real time before being correctly
attributed — noted here so the next session doesn't re-chase it.** Two of
the batched multi-method runs crashed mid-run with
`java.lang.ArrayIndexOutOfBoundsException` in
`org/junit/platform/commons/util/ExceptionUtils.<clinit>`, cascading into
`NoSuchMethodError`/`AbstractMethodError` on JUnit Platform's own
hierarchical-executor lambda dispatch. This is **not** a new bug: it is the
same "register-invisible root" family documented in section 5.4
(`TestTemplateInvocationContext` CCE, `obj_cid=0` bare-`Object` signature)
and in `docs/internal/springboot/testengine-getid-abstractmethoderror-
young-gc-forwarding-gap-FIXED.md` (confirmed present on `origin/dev` as of
this session's final `git fetch`, tip `704aedd3`) — a rare (~1/45-1/64),
partially-fixed GC-root-visibility race unrelated to message conversion.
Worked around by keeping JVM batches small (2-5 methods, 8-20 test
invocations) rather than chasing it; every crash recovered cleanly on retry
with zero real test failures.

**Regression suites** (dev tip `6178c36f`, same binary):
`cargo test -p cratonvm-native-builtins --lib --release`: **3000 passed, 0
failed, 6 ignored** (clean baseline match). `cargo test -p cratonvm-vm --lib
--release`: **2199 passed, 17 failed** — first attempt was OOM-killed by
this severely overloaded shared host (`load average` 140-230,
<code>/</code> at 100%, only ~2 GB RAM free with 2000+ concurrent users) and
retried clean; the 17 failures exactly match this doc's own
already-documented pre-existing baseline (7 `jit::skip_list::tests::*` +
9 `runtime::lock_order::tests::*`, both already attributed to unrelated
sessions in section 5/5.4, plus one host-contention flake,
`jit_getfield_never_tears_against_concurrent_jit_putfield_int`) — no new
regressions.

**No code change landed** — nothing to fix; this entry exists to close the
loop on section 5's "not yet root-caused" note with evidence, and to
prevent a future session from re-opening a hunt for a bug that no longer
reproduces. If it resurfaces, re-check first whether `19a5025f`/`9850617b`
are still present on whatever tip is being tested before assuming a
regression.


### 5.6 Three named `ClientHttpConnectorTests` minor residuals (2026-07-16): StepVerifier identity FIXED, EofException confirmed CratonVM-specific but not fixed, enum CCE not reproduced

Follow-up to 5.2's residual list: "an occasional Jetty `EofException` connection-flake,
an unrelated enum `valueOf()` `ClassCastException` (a known separate synthetic-enum
gap), and one flaky `StepVerifier` exception-identity assertion." Branch
`fix/httpconn-minor-residuals-20260716`.

**Harness housekeeping (two false-positive noise sources ruled out first)**: a large
fraction of this session's early stress-run failures were NOT new bugs:
1. Omitting `--enable-native-access=ALL-UNNAMED` (required per section 5.1) lets
   `MemorySegment` clinit failures corrupt JUnit's ServiceLoader-based engine discovery
   in confusing ways (`ClassCastException: Object cannot be cast to TestEngine`-family
   symptoms, `AbstractMethodError ... has no Code attribute`).
2. `--stack-dump-on-timeout 0` disables CratonVM's watchdog entirely (confirmed from
   `vm-cli/src/main.rs`'s `resolve_watchdog_timeout`: `Some(0)` unconditionally maps to
   `None`, regardless of any `CRATONVM_DEFAULT_WATCHDOG_SEC` env default). A
   `timeout(1)`-wrapped run that already printed its `RESULT` line and is only being
   reaped for lingering non-daemon Reactor-Netty threads (`"main() returned; VM held
   alive by N non-daemon thread(s)"`) is NOT a hang — `run-suite.sh`'s own
   `flush_batch` already records the `RESULT` regardless of the wrapper's exit code; an
   ad hoc harness that checks the exit code first will misreport clean runs as timeouts.

With both handled, the dominant remaining noise this session hit was a **severe,
transient flare-up of the already-known, already-partially-fixed section-5.4
"register-invisible-root" family** — `ClassCastException: Object cannot be cast to X`
for whatever `X` a reused `cid=0` bare-Object checkcast happens to hit
(`TestExecutionResult$Status`, `String`, `IllegalArgumentException: Could not create
type` on `@ParameterizedTest` argument construction — never an enum type in any
capture this session took), plus outright process crashes (`AbstractMethodError:
TestEngine.getId() has no Code attribute`, SIGSEGV). This was root-caused (same day,
independently, by a different concurrent session) to a genuinely NEW regression from
same-day GC work — a missing `skip_free_blocks` call in the young-GC
`young_object_starts` pre-forwarding walk (`gc/src/gen_heap.rs`) — and fixed via
`fix/wildfly-cce0079-close-20260716`, merged to `dev` mid-session (see
`docs/internal/springboot/testengine-getid-abstractmethoderror-young-gc-forwarding-gap-FIXED.md`).
Recorded here because it dominated this session's raw failure counts and could
otherwise be mistaken for one of the three items below by a future reader of raw logs.
The shared build host was also independently in a severe resource crisis for large
parts of this session (`/data/data`, where all worktrees live, hit **0 bytes free**
at least twice, once aborting `cargo build`'s LLVM output stage outright and once
blocking `git commit`; load average peaked over 160 on a 16-core box with 2000+
concurrent sessions) — flagged only as context for why sample sizes below are smaller
than planned, not as a CratonVM bug.

**(c) StepVerifier exception-identity failure — ROOT-CAUSED AND FIXED.**
`ClientHttpConnectorTests.errorInRequestBody(ClientHttpConnector)`:
```java
Exception error = new RuntimeException();
Flux<DataBuffer> body = Flux.concat(stringBuffer("foo"), Mono.error(error));
...
StepVerifier.create(futureResponse)
    .expectErrorSatisfies(throwable -> assertThat(throwable).isSameAs(error))
    .verify();
```
failed deterministically on the `Jdk` connector parameterization (looks "flaky" only
at the whole-class level, since which of the 4 parameterized connectors gets exercised
varies run to run, and the section-5.4 noise above obscured many runs entirely):
```
java.lang.AssertionError: expectation "expectErrorSatisfies" failed (assertion failed
on exception <java.io.IOException: HttpRequest body publisher failed:
java.lang.RuntimeException>: ... to refer to the same object)
```
Confirmed CratonVM-specific via a direct HotSpot A/B (same classpath/harness, JDK 25):
**8/8 clean HotSpot runs**, `found=49 succ=47 fail=0 abort=2` every time — zero
occurrences of this failure on real HotSpot.

Root cause: `JdkClientHttpConnector`'s `HttpClient.sendAsync()` is backed by
CratonVM's own native Rust reimplementation (`native-builtins/src/net_phase_e.rs`'s
RE.5 family — see section 4's original `sendAsync()` finding), not real JDK bytecode.
`re5_body_collector_on_error` (the `Flow.Subscriber.onError` native callback that
observes the request-body publisher's failure) converted the delivered `Throwable` to
a plain Rust `String` (`re5_throwable_text`) and discarded the object; the waiting
side, `re5_collect_publisher_body`, then synthesized a brand-new `java.io.IOException`
from that string. This is a structural identity loss, not a timing race — every
request whose body publisher fails and is routed through this path was guaranteed to
fail an `isSameAs()` check on the propagated exception.

Fix (commit `39f152bb`, merged to `dev` at `83418b1d`): `Re5PublisherBodyState` gains
`error_obj_root: Option<usize>`, a **global** GC root (required because `on_error`
fires on the Reactor scheduler thread, a different Java thread than the one waiting in
`re5_collect_publisher_body`) for the original `Throwable`, captured alongside the
existing text in `re5_body_collector_on_error`. `re5_collect_publisher_body` now
resolves and rethrows the ORIGINAL object (`MethodCallFailed::ExceptionThrown(orig)`)
instead of synthesizing an `IOException` wrapper, whenever the root resolves
successfully — falling back to the old text-only `IOException` only if resolution
somehow fails, so no failure mode is worse than before.

**Verification**: `cargo test -p cratonvm-native-builtins --lib --release`: 3000
passed / 0 failed / 6 ignored (clean, unchanged). `cargo test -p cratonvm-vm --lib
--release`: 2200 passed / 16 failed — all 16 are the pre-existing
`jit::skip_list::tests::*` (7, parallel-test-execution races over shared global state)
+ `runtime::lock_order::tests::*` (9, `cfg!(debug_assertions)`-gated, expected under
`--release`) buckets this doc already attributes to unrelated sessions in 5.1/5.2/5.4
— no new regressions, confirmed by full failure-list diff. Stress verification: across
post-fix `ClientHttpConnectorTests` runs that reached real test execution (host
instability capped this at a handful of clean samples — see housekeeping note above),
**zero recurrences** of the `errorInRequestBody(Jdk)` identity failure, versus
appearing in roughly half of pre-fix runs that reached the `Jdk` connector case (4/8 in
one clean batch). Small post-fix sample size is an honest limitation, but the fix is
an unconditional architectural correction (the old code path could never have passed
this assertion; the new one always propagates the real object when available), not a
probabilistic mitigation, so the mechanism-level confidence is high independent of
sample count.

**(a) Jetty `EofException` connection-flake — confirmed CratonVM-specific, NOT
fixed.** Reproduced twice independently on the `basic(ClientHttpConnector, HttpMethod)`
parameterized test (the connector×method cross product that makes up most of the
class's 49 sub-tests), Jetty connector, two different HTTP methods (`PATCH` at index
`[13]`, `TRACE` at index `[16]`):
```
java.lang.AssertionError: expectation "assertNext" failed (expected: onNext();
actual: onError(org.eclipse.jetty.io.EofException: write(gathering): channel not
connected))
```
Repro rate this session: 2 of roughly 35 CratonVM runs that reached real test
execution (~6%). HotSpot A/B: 0 occurrences across 8 clean HotSpot runs (`fail=0`
every time) — confirmed CratonVM-specific, not pre-existing Jetty/MockWebServer test
flakiness (satisfies the task's suggested HotSpot-baseline check directly).

Not root-caused this session — flagged as OPEN with a specific next-step hypothesis
rather than force-fixed: Jetty's `HttpClient` pools/reuses connections across the
class's ~49 sequential sub-test invocations on one shared connector instance (see the
`autoCloseArguments = false` / "shared between parameterized test invocations" comment
on `basic()`'s `@ParameterizedTest` annotation). "write(gathering): channel not
connected" is Jetty's own error for attempting to write to a channel it already
considers open but the OS/peer has actually closed — consistent with a **stale
pooled-connection reuse race**: MockWebServer closes an idle keep-alive connection
between two of the ~49 requests, and CratonVM's socket/channel readiness signalling
(`native-io/src/socket_channel.rs`, `native-io/src/nio_selector.rs` — both touched by
unrelated fixes earlier the same day per the `git merge` pulled into this branch, so
re-verify against a fresh build before assuming this hypothesis still holds) does not
correctly surface a half-closed peer to Jetty's pool-health check before the next
reuse attempt, unlike real JDK's underlying socket implementation. This is a
structurally different code path from the already-fixed `ServerSocket.accept()`/
`socket_connect()` blocking-region gaps in section 4 (those are accept/connect-time;
this would be a write-after-reuse-time gap on the client pooling side) — plausible but
NOT confirmed via live capture (gdb/strace on a caught-in-the-act repro), which the
severely resource-constrained shared host made impractical to obtain reliably this
session. Recommend: reproduce with `CRATONVM_DBG_SC_CLOSE=1` (an existing diagnostic,
confirmed present via `git log`, that traces `SocketChannel.close()` call sites) on a
tight repeat-loop of just `basic[Jetty]` sub-tests, once host load allows a clean
multi-hour capture session.

**(b) Enum `valueOf()` `ClassCastException` — NOT REPRODUCED this session.** Searched
`docs/known-issues/` and `git log` for prior synthetic-enum bugs before starting empirical
repro, per the task's instructions. Candidates considered and ruled out as different
call shapes: `ce3544ce` "Fix-enum-constant-subclass-isEnum" (anonymous enum-constant
subclass `isEnum()`/`getEnumConstants()`, unrelated); `e9ed6693` "Round 73: ...
Enum.<init>/name/ordinal use canonical Enum slot (subclass shadowing)" (field-slot
resolution, unrelated); `ea96497e` (same-day) "`ImportHttpServiceRegistrarTests` CCE
root-caused to cross-loader `Adapt` enum identity; VM fix attempted+reverted (heap
corruption)" — closest analog in *mechanism* (cross-loader/cross-context enum
identity) but a structurally different call site (`MergedAnnotation.Adapt.isIn()`, not
`Enum.valueOf()`), and that session's attempted fix was reverted for causing heap
corruption, so there is no landed pattern to reuse even if the mechanisms turn out to
be related.

Across roughly 35 CratonVM `ClientHttpConnectorTests` runs that reached real test
execution this session (spanning both the noisy pre-GC-fix and cleaner post-GC-fix
periods), **zero occurrences** of any `ClassCastException` naming an enum type or
involving `Enum.valueOf` were captured. Every CCE actually observed was either the
register-invisible-root family (5.4 — confirmed generic, never targets an enum type in
any capture) or the now-fixed StepVerifier identity issue above (not itself a CCE).
Status: could not reproduce with the repro budget available this session (further
inflated by the concurrent host disk/load crisis capping usable sample throughput).
Two honest possibilities, not distinguished by this session's evidence: (1) already
resolved by one of the several enum- and reflection-adjacent fixes that landed on
`dev` between whenever this residual was first observed and this session's tip
(`db047d38` unrooted-ObjectRef reflection-native GC-safety fixes, `e9ed6693` Enum slot
canonicalization, `0c0ee830` "root enum and hashmap lookups across GC" are all
plausible candidates, none individually confirmed as the fix); or (2) rare enough
(comfortably under 3%, going by a zero-in-35 sample) that this session's budget simply
wasn't enough to catch it. Recommend: if it resurfaces, capture with `KRUN_STACK=1`
for a full stack trace immediately (this doc had none for it before this session, and
still has none) rather than assuming it is the same family as (a) or (c) above.

**Current true state of `ClientHttpConnectorTests`**: one of three named residuals
(StepVerifier identity) fixed and landed; one (Jetty `EofException`) confirmed
CratonVM-specific and precisely characterized but not fixed; one (enum `valueOf()`
CCE) not reproduced and left as an open question rather than a confirmed-open bug.
