> **2026-07-16 rollup (read this first):** the vast majority of this doc's
> findings are now **FIXED and merged to `dev`**, mostly via a large,
> separate 2026-07-15/07-16 investigation
> ([`CRATONVM-SPRING-GENUINE-BUGLIST.md`](CRATONVM-SPRING-GENUINE-BUGLIST.md),
> branch `fix/spring-aot-cluster-20260715` + `fix/reactive-cluster-20260715`)
> that root-caused the whole 9-class "AOT hang cluster" below to a FAMILY of
> classloader-identity bugs under `@CompileWithForkedClassLoader`/
> `DynamicClassLoader`, not the workload-volume hypothesis this doc's
> 2026-07-13 section originally landed on. That doc is now the
> up-to-date, actively-maintained source of truth for this whole area —
> consult it for the authoritative current per-class table. This doc is kept
> for its historical investigation narrative (ruled-out hypotheses, repro
> techniques) but its **status line and per-class tables below are stale**;
> see the accurate current-status list immediately below instead of trusting
> "Bucket 1"/"Bucket 2"/"still hangs" language further down.
>
> **Confirmed FIXED and merged to `dev`** (do not re-investigate): the
> `java.home`/`Locale` bootstrap regression (`f62d2073`); the
> `CopyOnWriteArrayList` "this.lock is null" NPE (`68c44f62`); the
> `Semaphore.release()` STW-barrier deadlock (`b6fffebf`/`9ca83d62`); the
> `Objects.toString(Object[, String])` identity-vs-virtual-dispatch bug
> (`7b1d6ff3`+); the `GroovyScriptFactoryTests` JIT-codegen SIGSEGV
> (`2724ea5b`); `ServletAnnotationControllerHandlerMethodTests` (`cd90774e`,
> `72a9ad40`, now 241/241); the `Files.walkFileTree`
> zero-field-`BasicFileAttributes` bug (`7ae137e4`); the whole AOT
> classloader-identity family (8+ fixes, `7c5aa7ce`..`d972fd43`, see the
> other doc) — `AutowiredAnnotationBeanRegistrationAotContributionTests`,
> `CommonAnnotationBeanRegistrationAotContributionTests`,
> `BeanDefinitionPropertiesCodeGeneratorTests`,
> `InstanceSupplierCodeGeneratorTests`, `BeanDefinitionMethodGeneratorTests`,
> `GroupsMetadataValueDelegateTests` (the `WritableContent` residual is also
> now fixed, 8/8 OK), `ScopedProxyBeanRegistrationAotProcessorTests`,
> `ThrowawayClassLoaderTests`, and (via a different, `d8092acb`-family fix)
> the missing-`ApiVersionStrategy`-bean + backend-specific WebFlux failures
> under `CrossOriginAnnotationIntegrationTests` (now 68/68 on all 4
> backends).
>
> **Confirmed FIXED 2026-07-16 (later the same day):**
> `web.service.registry.ImportHttpServiceRegistrarTests`'s `ClassCastException`
> (cross-loader `MergedAnnotation$Adapt` enum-identity split) — the
> `resolve_field_ref`/getstatic fix that was attempted and reverted earlier
> the same day (see immediately below) was re-attempted from a fresh
> `origin/dev` checkout and now builds and runs clean: the GC bug that caused
> the earlier heap corruption (`fb15be63`, landed independently the same day)
> is already fixed on `dev`. The class still doesn't reach 5/5 — the
> remaining 2/5 fail on the same pre-existing `java.lang.classfile.ClassFile`
> JDK24+ host gap noted below for the two "environmental, not a CratonVM bug"
> items. Full verification detail in `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s
> dedicated entry. Also now **FIXED**: `context.annotation.ImportSelectorTests`
> (Mockito `spy()` heap-corruption abort on its 2 "nested group" sub-tests) —
> same `fb15be63` GC fix closed it too, NOT the `MockMethodAdvice
> .isOverridden` hypothesis originally suspected (that hypothesis was never
> confirmed and turned out not to be needed). Verified twice independently:
> once in the 2026-07-16 joint-verification session below, and again
> 2026-07-17 from a completely fresh `spring-framework` clone + fresh
> CratonVM build — both report `9/9` passing with zero corruption-signature
> log lines. See `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s dedicated entry and
> the "2026-07-16 joint verification addendum" in this doc's own
> `ImportSelectorTests` section for full detail.
>
> **Still genuinely OPEN** (tracked as active tasks in this session,
> 2026-07-16):
> `web.socket.messaging
> .StompWebSocketIntegrationTests` (STOMP message never arrives — functional
> gap, not investigated); `orm.jpa.support
> .PersistenceAnnotationBeanPostProcessorAotContributionTests` (2026-07-16
> dedicated re-triage: back to its documented 8/2/6 shape after an unrelated
> GC crash — since fixed by `fb15be63` — was briefly hiding it; 1 pre-existing
> Mockito cold-attach failure + 5 ByteBuddy method-type-variable-resolution
> failures, the latter narrowed further but still open; one genuine,
> independently-useful `Method.getTypeParameters()` identity-stability fix
> landed, `4cb070e5`, but did not resolve the ByteBuddy residual — see
> `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s entry for the full trace);
> `beans.factory.aot.BeanRegistrationsAotContributionTests`
> (confirmed genuinely perf-bound — steady progress, 100% CPU, not a
> deadlock — needs interpreter-throughput work, not a discrete fix);
> `RequestMappingMessageConversionIntegrationTests` (partially fixed, 5
> bugs landed, but still doesn't finish — real remaining bottleneck is
> conservative/non-precise GC root scanning on the interpreter's native-call
> path, an architectural item, not a quick fix). Two items are environmental,
> not CratonVM bugs: `ConfigurationClassPostProcessorAotContributionTests`'
> and `PersistenceManagedTypesBeanRegistrationAotProcessorTests`' residual
> failures both need a JDK 24+ (`java.lang.classfile.ClassFile`, JEP 484)
> that isn't installed on the investigation host.
>
> **2026-07-16 update**: `context.aot.ApplicationContextAotGeneratorTests` and
> `test.context.aot.TestContextAotGeneratorIntegrationTests` — the two AOT-cluster
> classes that had never gotten a dedicated post-loader-identity-fix triage — are
> now re-characterized (dedicated session, dev tip `6c517cd9`, full numbers and
> stack traces in [`CRATONVM-SPRING-GENUINE-BUGLIST.md`](CRATONVM-SPRING-GENUINE-BUGLIST.md)
> §2's "2026-07-16 dedicated re-triage" bullet — that doc is the authoritative
> source, this is a pointer/summary). `TestContextAotGeneratorIntegrationTests` no
> longer hangs (393 s → 8.3 s) but still FAILs 4/4, each a distinct cause: one is
> the already-tracked `ImportHttpServiceRegistrarTests`-family `ClassCastException`
> (attribution only), the other three (a `GroovySystem.<clinit>` `ArrayStoreException`,
> a SnakeYAML parse failure on a `$Nested` test class whose raw resource bytes are
> confirmed byte-identical to HotSpot, and a NEW loader-identity `ClassCastException`
> site in Spring's own `AotServices` SPI loader) are newly characterized but not
> fixed. `ApplicationContextAotGeneratorTests` is worse than its last (unsubstantiated)
> status implied: it 100%-reproducibly `LOADERR`s with `found=0` before discovering
> any test method, from a GC heap-corruption cascade (all-zero-header stale pointers
> hitting several unrelated JUnit-Platform/javac-internal classes within
> milliseconds of each other) that reproduces identically under JIT and `--nojit`
> and at 2 GB and 8 GB heap — ruling out the two most similar already-fixed bugs
> in this codebase (`spring-bug-10`'s JIT shadow-stack race, RESOLVED 2026-06-21;
> the `stream-arraylist-gc-pressure` fix, landed 2026-07-16 and already in this
> build) as the cause. This is a new, open, 100%-reproducible GC bug — no VM code
> was changed for either class this session; both are documented, not fixed.

# Spring TIMEOUT cluster — 1500s diagnostic rerun (hung vs. slow)

| | |
|---|---|
| **Status** | OPEN (12 genuinely hung, 10 slow-but-failing, 0 crash — 1 FIXED; 2 non-residual items removed). **2026-07-13 update**: 8 Bucket-1 + 3 Bucket-2 classes reconfirmed locally — one narrower bug fixed (`7ae137e4`), the hang itself still OPEN; see the 2026-07-13 section below. **2026-07-13 update #2**: `context.annotation.ImportSelectorTests`'s `StackOverflowError` root-caused — it is a Mockito `spy()` cross-hierarchy recursion, **unrelated to Spring's `ImportSelector` mechanism** (the original hypothesis below was wrong); still OPEN, see its own section. **2026-07-13 update #3**: both `web.service.registry.*` residuals (`ImportHttpServiceRegistrarTests`, `GroupsMetadataValueDelegateTests`) root-caused to `@CompileWithForkedClassLoader`'s custom-ClassLoader machinery interacting with Spring's AOT/test-compiler pipeline — two distinct defects, neither fixed; still OPEN, see dedicated section. **2026-07-13 update #4**: the 4 non-AOT, non-`ImportSelectorTests` Bucket-1 classes (`cache.jcache.JCacheEhCacheAnnotationTests`, `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests`, `context.annotation.InitDestroyMethodLifecycleTests`, `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests`) **no longer hang** — reconfirmed clean on 2 independent runs each against a freshly-built `origin/dev` tip; see the dedicated section below. No new code was needed — all 4 were incidental beneficiaries of other unrelated fixes already on `dev`. **2026-07-13 update #5**: the "missing `ApiVersionStrategy` bean" `BeanCreationException` (2 classes: `CrossOriginAnnotationIntegrationTests`, `RequestMappingMessageConversionIntegrationTests`) **no longer reproduces** — confirmed fixed (likely a side effect of earlier JSpecify/reflection work), but both classes now fail a different way instead: a genuine **deadlock in `Semaphore.release()`'s internal monitor**, confirmed via a live `gdb` thread dump. Still OPEN, new root cause, see dedicated section. **2026-07-13 update #6**: Bucket 3's `scripting.groovy.GroovyScriptFactoryTests` SIGSEGV **FIXED** (`2724ea5b`, pushed to `dev`) — root cause was a JIT codegen bug (stale deferred patch-list offsets surviving a rewound speculative-inline attempt, corrupting a later safepoint-id store in a hot, frequently-recompiled method); see dedicated section below. **2026-07-14 update**: the update #5 `Semaphore.release()` deadlock is now **FIXED** (`b6fffebf`/`9ca83d62`, pushed) — both classes run to completion instead of TIMEOUT. A second, unrelated bug underneath it (`Objects.toString(Object[, String])` never virtually dispatching, causing a malformed HTTP `Host` header via Apache HttpComponents5) is root-caused and fixed locally (`7b1d6ff3`, not yet pushed). Full end-to-end reverification of both classes is currently blocked by a **third, severe, unrelated regression** bisected with certainty to commit `d8092acb` (`InternalError: null property: java.home` from any early `Locale` use in real-JDK mode) — a dedicated follow-up task has been filed given its severity. See the 2026-07-14 section below. **2026-07-14 update #2 (urgent)**: the `java.home` regression is now **root-caused precisely and FIXED** (`f62d2073`, pushed) — a whole-function category-tagging bug in `register_properties_sidetable` (`java.util.Properties`' native bridges silently dropped in real-JDK mode). A second instance of the identical bug family (`CopyOnWriteArrayList`'s mutators, causing `"this.lock is null"` NPEs) was found and **also fixed** (`68c44f62`, pushed). Both target classes still do not fully pass — clearing these two blockers revealed (at least) two further, distinct, unrelated, NOT-yet-investigated issues (one per remaining backend: Jetty `NoClassDefFoundError`, Tomcat `LifecycleException`). See the new 2026-07-14 follow-up #2 section below. **2026-07-14 update #3**: `GroupsMetadataValueDelegateTests`'s hard, uncatchable VM abort (`class file error: class not found: .../GroupsMetadata__TestCode`) is now **FIXED** (`9bca11f5`, pushed) — a reflective `Method.invoke()` on a static method was re-resolving its declaring class by name instead of using its already-resolved `ClassId`, which broke under multiple same-named classes across different forked loaders. Confirmed via the real suite runner: `found` went from `0` (ABEND) to `8` (all discovered, no crash) — but all 8 now fail on a different, new, not-yet-investigated `IllegalStateException: WritableContent did not append any content`; `ImportHttpServiceRegistrarTests` (the other class in this cluster) is unchanged, still 3/5, same `ClassCastException`. See the dedicated section for both. **2026-07-14 update #4**: `web.socket.messaging.StompWebSocketIntegrationTests`'s original "no `MessageHandler` bean" startup failure no longer reproduces (fixed elsewhere, same pattern as the `ApiVersionStrategy` bean); a real GC-safety bug was found and FIXED along the way (`0bb89ebf`, pushed) — blocking `SocketChannel`/`AsynchronousSocketChannel` I/O had no `begin_blocking_region`/`end_blocking_region` bracket, so a concurrent STW pause could wait forever on a thread genuinely parked in a blocking socket read/write/accept — but the class itself still TIMEOUTs, now on a separate, confirmed-distinct functional gap (a STOMP message that never arrives, not a VM concurrency bug); see its dedicated section. **Session rollup (2026-07-14)**: of the 23 original residual items, 9 are now fully resolved and pushed to `dev` (4 standalone hangs, the Groovy SIGSEGV crash, `ServletAnnotationControllerHandlerMethodTests` at 241/241, and the `ApiVersionStrategy`/`Semaphore`/`java.home`/`CopyOnWriteArrayList` chain), plus one narrow bug each landed for `web.service.registry`'s `GroupsMetadataValueDelegateTests` and for `StompWebSocketIntegrationTests`. All fixes were independently verified against a freshly-built `origin/dev` tip before merging. The remaining ~13 items are precisely root-caused but still open: the AOT/javac hang cluster (needs a symbol-capable profiler this environment lacks), `ImportHttpServiceRegistrarTests` (SoftReference/GC-relocation hypothesis, unconfirmed), `ImportSelectorTests` (a Mockito/ByteBuddy internals bug, not Spring's), and several newly-exposed per-backend issues underneath the `ApiVersionStrategy` cluster's WebFlux tests (Jetty `NoClassDefFoundError`, Tomcat `LifecycleException`, Reactor Netty's child-event-loop failure) and the `WritableContent` issue underneath `GroupsMetadataValueDelegateTests`. See each dedicated section for precise state and next steps. |
| **Discovered** | 2026-07-11, following up on the 25 classes that hit TIMEOUT in the
125-class scoped rerun (dev `9948295e`, standard 120s timeout — see
[`CRATONVM-SPRING-GENUINE-BUGLIST-125.md`](CRATONVM-SPRING-GENUINE-BUGLIST-125.md)). |

## Why this doc exists

A 120s timeout can't distinguish "genuinely hung forever" from "just slow."
All 25 TIMEOUT classes from the `-125` rerun were rerun individually
(`BATCH=1`, one class per process, isolated) on the same binary
(`cratonvm-rerun4-20260711.bin`, dev `9948295e`) with the timeout raised to
1500s. Azure host `20.83.144.174`, worktree
`/data/data/wt-osr-other516-20260708-2131`, 8-way sharded, `suite-run.sh`.

**Caveat on elapsed times:** `suite-run.sh`'s crash-recovery logic retries any
batch that times out as an individual `run_one` call with its own fresh
timeout — with `BATCH=1` this means a genuinely hung class silently burns
**two consecutive 1500s windows** (~50 min) before being recorded as
`TIMEOUT`, not one. This was confirmed by process-elapsed-time inspection
mid-run (six shards' first classes reappeared as fresh processes at ~279s
after apparently running for the full 1500s). The `1500000` ms figure
recorded for hung classes is the single retry window's duration, not the
cumulative wall-clock.

## Resolved during this investigation

- `test.context.aot.TestClassScannerTests` was already clean in the 1500s
  rerun (7/7) and is not an active issue.
- `web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` now
  passes (0 failures). The fix makes `Class.forName` invoked through Spring's
  `DynamicClassLoader` resolve generated classes through its parent loader,
  preserving the class identity expected by the test compiler and registry.

## 2026-07-13 local investigation — AOT bean-registration hang cluster + in-memory-javac `CompilationException` cluster confirmed to share one root cause (still OPEN)

Reproduced entirely locally (Azure host unreachable), worktree
`cratonvm-wt-aot-hang-local-20260713`, dev tip `360d478c` rebased onto
`origin/dev` `a7680d77` plus this session's own commit `7ae137e4`. Covered
the 8 AOT classes from Bucket 1 below
(`AutowiredAnnotationBeanRegistrationAotContributionTests`,
`BeanDefinitionMethodGeneratorTests`, `BeanRegistrationsAotContributionTests`,
`CommonAnnotationBeanRegistrationAotContributionTests`,
`ConfigurationClassPostProcessorAotContributionTests`,
`ApplicationContextAotGeneratorTests`,
`PersistenceAnnotationBeanPostProcessorAotContributionTests`,
`TestContextAotGeneratorIntegrationTests`) plus the 3 `CompilationException`
classes from Bucket 2 below (`InjectionCodeGeneratorTests`,
`BeanDefinitionPropertiesCodeGeneratorTests`,
`InstanceSupplierCodeGeneratorTests`) — the "AOT bean-registration TIMEOUT
cluster" and "in-memory javac `CompilationException`" sub-clusters this doc
already flagged as likely related.

**They are confirmed to share one root cause.** A large, unrelated fix
cluster landed on `dev` on 2026-07-13
([`testcompiler-annotation-classes-not-found-cluster-FIXED.md`](../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md))
that made forced-native `JavacFileManager.list` GC-safe (pinning across
GC-unsafe windows) and fixed a `resource:`-URL handler-delegation gap in the
same in-memory-`TestCompiler` pipeline these 11 classes all use
(`org.springframework.core.test.tools.TestCompiler`/`DynamicClassLoader`).
That fix is real and necessary — but it changed, rather than resolved, the
symptom for most of these classes:

- **9 of the 11 still hang** exactly as Bucket 1 describes: full ceiling on
  both the batch attempt and the individual retry, `found=0/succ=0/fail=0`,
  no FAILCAUSE, no crash. This now includes all 3 of the "`CompilationException`"
  classes (`InjectionCodeGeneratorTests`,
  `BeanDefinitionPropertiesCodeGeneratorTests`,
  `InstanceSupplierCodeGeneratorTests`) — before the fix, `list()` silently
  truncated large package listings, so these three failed *fast* on a bogus
  "cannot find symbol"; now that `list()` is correct, they no longer fail
  fast — they hang, identically to the other 6. (Confirming this needed a
  correctness fix of its own along the way: `spring-orm`'s main/test-fixtures
  jars weren't built by a bare `./gradlew testClasses` in this worktree,
  which made `InjectionCodeGeneratorTests`/`PersistenceAnnotationBeanPostProcessorAotContributionTests`
  spuriously report `NoClassDefFoundError` at first; `./gradlew jar
  testFixturesJar` fixed the classpath, not a VM bug.)
- **2 of the 11 no longer hang at all**:
  `PersistenceAnnotationBeanPostProcessorAotContributionTests` now reliably
  completes in 15-70s (varies with machine load) with 2/8 passing, and
  `CommonAnnotationBeanRegistrationAotContributionTests` completes in
  ~70-85s with 2/8 passing under light load (borderline against a 90s probe
  ceiling under heavy concurrent-build load on this shared machine, but
  nowhere near the original 1500s ceiling either way). Both now fail with
  real, distinct, *unrelated-to-the-hang* residual causes instead:
  - `PersistenceAnnotationBeanPostProcessorAotContributionTests`'s 6
    failures are 100% `IllegalStateException: Could not initialize plugin:
    interface org.mockito.plugins.MockMaker` — the already-documented,
    large/architectural Mockito inline-mock-maker self-attach gap
    ([`kafka-suite-bugs/bug-09-mockito-inline-mockmaker-selfattach.md`](../internal/kafka-suite-bugs/bug-09-mockito-inline-mockmaker-selfattach.md)).
    Not a new bug; out of scope here.
  - `CommonAnnotationBeanRegistrationAotContributionTests`'s 6 failures are
    two new, distinct causes: (a) `java.lang.VerifyError:
    org/springframework/aot/hint/ReflectionTypeReference.<init>: at bytecode
    offset 13: invokespecial <init>: uninitializedThis receiver requires the
    constructor owner to be the current class ... or its superclass, found
    org/springframework/aot/hint/AbstractTypeReference` (plus a cascading
    `NoClassDefFoundError` on the same class once it fails to verify) — a
    genuinely new, narrow bytecode-verifier bug, not investigated further
    here; and (b) `IllegalArgumentException: Could not generate code for
    ...PackagePrivateFieldResourceSample__ResourceAutowiring::apply:
    parameter 1 of type ... is not supported`, which looks like a Spring AOT
    codegen limitation around package-private cross-package field injection
    — also not investigated further. Neither is filed as its own doc yet;
    flagging here rather than guessing at a root cause.

**One real, narrower bug found and FIXED along the way** (commit `7ae137e4`,
this session): CratonVM's native `Files.walkFileTree` (`p98_walk_dir` in
`native-builtins/src/phases_late.rs`) handed every `FileVisitor.
preVisitDirectory`/`visitFile` callback a placeholder `BasicFileAttributes`
object allocated with **zero fields**, instead of the canonical 5-field
layout used everywhere else in the file. Real javac's own
`JavacFileManager$ArchiveContainer.list()` visitor (used while indexing the
sample's 48-jar classpath, including `kotlin-stdlib`/`kotlin-reflect`/
`groovy`) calls `attrs.isRegularFile()` on this placeholder while scanning
every jar entry — hitting the GC guard's out-of-bounds-field-read path
~500+ times per class (confirmed via `CRATONVM_DBG_OOBFIELD` backtraces,
all landing in `isRegularFile()` at `phases_late.rs:26151`).
`isRegularFile()` happened to come out correct by luck (it defensively
coerces the dropped read to a typed `Int`), but the sibling `isDirectory()`
native did not — it returned the raw (out-of-bounds) `get_field` result
verbatim for a `()Z`-descriptor method, a live type-confusion bug
(`Value::Object(None)` where a boolean was expected) waiting for a
different `FileVisitor` to trip over it. Fixed by giving every
`walkFileTree` callback object the real 5-field layout with real
`is_dir`/`size` data, and hardening `isDirectory()` the same defensive way
`isRegularFile()` already was. **Verified**: the OOB-read warning burst is
eliminated entirely (0 occurrences, down from 500+, confirmed by rerunning
`BeanDefinitionMethodGeneratorTests` before/after). **This fix does NOT
resolve the hang** — confirmed by reproducing the identical TIMEOUT
before and after, on the same binary modulo this one change.

**The hang itself remains unresolved.** What was ruled out this session,
using a new permanent diagnostic added along the way
(`CRATONVM_DBG_HANG_SAMPLE`, gated/cheap, periodically eprintln's the method
being invoked in `execute_invoke_kind`):
- Not a deadlock — the hung process's CPU time climbs steadily (confirmed
  via repeated `Get-Process` sampling: ~100% of one core, continuously).
- Not a tight 2-3-method infinite loop — `CRATONVM_DBG_HANG_SAMPLE` shows
  genuine progression through *different* real-javac-internal methods over
  time (`PoolReader.getUtf8`, `JavaFileManager.inferBinaryName`,
  `Name.Table.fromString`/`append`, `Scope$Entry.<init>`,
  `Scope$ScopeListenerList.symbolAdded`, `Symtab.enterClass`,
  `Symbol$ClassSymbol.<init>`, `PathFileObject.<init>`), at a sustained
  ~20,000-55,000 interpreted calls/sec that does not visibly collapse
  toward zero over a 180s+ single-attempt sampling window (weak evidence
  against a classic quadratic blowup, not conclusive over the full 1500s).
- Not the `BasicFileAttributes` bug above (fixed, confirmed insufficient).
- Not caught by `CRATONVM_DBG_STALE_OBJREF` (the existing hard-panic
  stale-native-ObjectRef assertion) — it never fired during a 90s repro.
- **Not a JIT instance-method invocation-tierup gap.** A plausible-sounding
  lead: CratonVM's invocation-count tier-up historically only fired for
  `execute_invokestatic_cached`, leaving short-loop *instance* hot methods
  (exactly what `Scope`/`Name`/`ClassReader` accessors are) permanently
  interpreted unless OSR or JIT-callee-inlining reached them. **This gap was
  already closed and made default-ON weeks before this investigation**
  (`CRATONVM_JIT_VIRTUAL_TIERUP`, commit `948df81c`, "make instance-method
  invocation tier-up (B) default-ON"; the one JIT codegen bug that used to
  block it, a `Matcher.search` virtual-dispatch-bail miscompile, was
  root-caused and its skip-list ban *removed* the same day — see
  `vm/src/jit/skip_list.rs` around the "bug-03 layer C" comment). It was
  therefore already active in every reproduction above. Verified directly
  anyway on `AutowiredAnnotationBeanRegistrationAotContributionTests`,
  same machine load, same 90s window, `CRATONVM_DBG_HANG_SAMPLE` sampling:
  explicit `CRATONVM_JIT_VIRTUAL_TIERUP=1` (= default) reached ~1.2M
  interpreter calls before timing out; `=0` (disabled) reached ~1.0M — a
  ~20% difference, not the 100x+ effect that would indicate this is the
  dominant bottleneck, and both runs still hit the full TIMEOUT. Both traces
  show the same hot-method mix (`Name.isEmpty`/`hashCode`,
  `CharacterDataLatin1.getProperties`, `ClientCodeWrapper.isTrusted`,
  `StringBuilder.append`). Clean negative result: JIT tier-up policy is not
  what's gating this hang.

**HotSpot baseline (2026-07-13, same worktree, `run-suite.sh hotspot`, real
`java.exe`, no CratonVM involved)** — how long does this actually take on a
real JVM:

| Class | HotSpot result | Elapsed |
|---|---|--:|
| `AutowiredAnnotationBeanRegistrationAotContributionTests` | OK 14/14 | 59.1s |
| `BeanDefinitionMethodGeneratorTests` | OK 34/34 | 73.6s |
| `ApplicationContextAotGeneratorTests` | OK 40/40 | 155.9s |

All three pass cleanly and finish in under 3 minutes on HotSpot — including
`ApplicationContextAotGeneratorTests`, the most expensive of the three
(largest generated-code surface, most CGLIB/reflection-heavy fixtures).
**This rules out "these are just inherently expensive AOT-codegen tests that
happen to need close to 1500s."** They don't; HotSpot needs 1-3 minutes.
CratonVM not finishing any of the 9 hung classes within a 1500s ceiling — 10x
to 25x the *slowest* HotSpot baseline above, and 25x-1500x the *fastest* —
is a severe gap, not a marginal one.

**Characterization: leans toward a workload-specific disproportionate cost,
not (only) a uniform interpreter/JIT throughput gap**, though this session's
tooling can't fully separate the two. Reasoning: a "CratonVM is just N times
slower at everything" story requires N to be roughly 20-25x (to explain
`ApplicationContextAotGeneratorTests` alone needing >1500s against a 156s
HotSpot baseline) up to 100x+ (for the 59s-73s baselines, or given that the
9 hung classes never finish at all, not even slowly-but-boundedly within
1500s). A uniform 20-100x interpreter gap of that magnitude, specifically
and only for this kind of workload, would be a very unusual outlier relative
to CratonVM's general performance posture elsewhere in the project (nothing
else in project history shows a *general-purpose* interpreter/JIT gap in
that range against HotSpot; JIT tier-up is confirmed active per above, and
the `CRATONVM_DBG_HANG_SAMPLE` throughput — 20,000-55,000 real interpreted
method calls/sec, sustained, not collapsing — is not itself abnormally slow
for an interpreter loop). That combination (normal-looking per-call
throughput, but the *total* task apparently needing on the order of
100x-1000x+ HotSpot's wall time to finish, if it finishes at all) is more
consistent with CratonVM doing **substantially more total work** for the
same nominal compile than HotSpot does — i.e. some form of eager-vs-lazy
discrepancy or a caching/completion-state gap inflating the effective
symbol/class count touched — layered on top of, not instead of, ordinary
interpreter overhead. This is not conclusively proven; it is this session's
best-supported reading of the evidence gathered, and a real profiler could
still overturn it (e.g. by showing the call graph really does only touch a
small, bounded symbol set and the cost is genuinely per-call, in which case
"broad perf gap" would be the better description after all).

**Leading, unconfirmed hypothesis**: the sample's effective test classpath
is unusually large for this kind of test (48 jars for `spring-beans`,
including `kotlin-stdlib`, `kotlin-reflect`, `groovy`, `mockito`, `reactor`),
and real javac's own `ClassFinder`/`ClassReader`/`Scope`/`Symtab` symbol-
completion machinery — executing as ordinary interpreted/JIT-compiled
bytecode under CratonVM, not a native shortcut — does a volume of work that
does not complete inside the 1500s ceiling even at the throughput observed
above. Distinguishing "genuinely enormous but finite work, just far slower
under CratonVM's interpreter than HotSpot" from "a CratonVM-specific
caching/completion-state gap causing needless reprocessing of
already-completed symbols/packages" needs either a call-count-attributed
sampling profiler with matching symbols (no `gdb`/`cdb`/`windbg`/`wpa` with
usable symbols were available in this Windows environment — this VM ships
DWARF debug info via the GNU/MinGW toolchain, which `wpr`/`wpa` cannot
resolve) or a side-by-side HotSpot-vs-CratonVM instrumented call-count
comparison. Neither was feasible in the time available this session.

**Methodology note for future sessions reusing a copied
`apps/spring-suite-runner` directory in a fresh worktree**: its
`meta/all-classes.tsv` (gitignored, machine-local) records each test class's
**absolute** module path at `discover` time. If it's copied from another
checkout without rerunning `./run-suite.sh discover` there, `run-suite.sh`
silently reads test classes and jars from the *old* checkout's absolute
paths — even with `SPRING` exported correctly, and even though the new
worktree's own `cratonvm-testcp.txt` files are perfectly correct. This bit
this investigation: the first several reproduction runs this session were
unknowingly reading Spring test classes/jars from `/c/craton/cratonvm` (the
shared, actively-mutating main checkout this session was explicitly told
never to touch) rather than the intended worktree. Running `discover` with
`SPRING` pointed at the intended checkout regenerates it correctly; all
final numbers quoted above are from the corrected, properly-isolated run.

### Summary table — all 11 classes covered this session

| Class | Status 2026-07-13 | Notes |
|---|---|---|
| `AutowiredAnnotationBeanRegistrationAotContributionTests` | **Still hangs** | HotSpot baseline: 59.1s, 14/14 OK |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | **Still hangs** | HotSpot baseline: 73.6s, 34/34 OK |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | **Still hangs** | not HotSpot-timed this session |
| `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests` | No longer hangs; FAIL 2/8 | new residual: `VerifyError` in `ReflectionTypeReference.<init>` (bytecode verifier, not filed yet) + an AOT codegen `IllegalArgumentException` (not filed yet) |
| `context.annotation.ConfigurationClassPostProcessorAotContributionTests` | **Still hangs** | not HotSpot-timed this session |
| `context.aot.ApplicationContextAotGeneratorTests` | **Still hangs** | HotSpot baseline: 155.9s, 40/40 OK |
| `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` | No longer hangs; FAIL 2/8 | residual is the already-tracked Mockito self-attach gap (`bug-09`), not new |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | **Still hangs** | not HotSpot-timed this session |
| `orm.jpa.support.InjectionCodeGeneratorTests` | **Now hangs** (was FAIL/fast) | classpath gotcha fixed along the way (`spring-orm` jars weren't built) |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | **Now hangs** (was FAIL/fast) | |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | **Now hangs** (was FAIL/fast) | |

**Fixed and pushed to `dev`** (`7ae137e4`): `Files.walkFileTree`'s zero-field
`BasicFileAttributes` placeholder — a real, standalone correctness/type-
confusion bug, confirmed NOT the cause of the hang.

**Ruled out for the 9-class hang**: deadlock; a tight 2-3-method infinite
loop; the `BasicFileAttributes` bug above; `CRATONVM_DBG_STALE_OBJREF`
(didn't fire); JIT instance-method tier-up policy (already default-ON,
toggling it changes throughput ~20%, not the dominant factor); "these tests
are just inherently this slow" (HotSpot finishes the 3 timed ones in
59-156s).

**Still open**: the hang/severe-slowdown itself. Best current
characterization: likely a workload-specific disproportionate cost in
javac's `ClassFinder`/`ClassReader`/`Scope`/`Symtab` symbol-completion
machinery over this sample's unusually large 48-jar classpath (probably
processing far more classes/symbols than HotSpot's lazy completion would
for the identical compile), rather than a uniform interpreter/JIT throughput
gap — but this session's tooling (no symbol-capable native profiler
available on this Windows machine) couldn't conclusively distinguish that
from "just a very large uniform slowdown for this specific code shape."

**Diagnostics left in place for the next session** (both permanent, gated,
default-off, negligible cost when unset):
- `CRATONVM_DBG_HANG_SAMPLE=1` — periodically prints the method being
  invoked in `execute_invoke_kind` (`vm/src/runtime/interpreter.rs`,
  `vm/src/runtime/env_cache.rs::dbg_hang_sample`), every 200,000 calls. Cheap
  way to see a hung process's last-known activity without a debugger.
- `CRATONVM_DBG_OOBFIELD=<substr>` (pre-existing) — dumps a Rust backtrace
  on every out-of-bounds field read whose class name contains `<substr>`;
  used to pin the `BasicFileAttributes` bug precisely.
- A proper next step would be a call-count-attributed sampling profiler
  with matching symbols (this build's DWARF debug info isn't readable by
  Windows' `wpr`/`wpa`; `perf`/`samply`-style tooling would need to be
  brought in, or the investigation moved to a Linux host), or instrumenting
  `ClassReader.readClassFile`/`ClassFinder.fillIn` call counts directly
  (Rust-side, at the native javac-bridge boundary) to compare against a
  HotSpot JFR/async-profiler trace of the same class for a true apples-to-
  apples "how many classes actually get completed" count.

## 2026-07-13 local investigation — `ImportSelectorTests` `StackOverflowError` root-caused to Mockito `spy()`, not Spring (still OPEN)

Reproduced entirely locally (Azure host unreachable), worktree
`cratonvm-wt-importselector-local-20260713`, dev tip `dbf7827c` (merged
forward to `0b0d852d` after the investigation; the merged commits touch
unrelated files, confirmed by diff — nothing Mockito/ThreadLocal/reflection-
related landed in between), binary
`cratonvm-importselector-local.exe`.

**HotSpot baseline**: 9/9 pass, ~52s (`run-suite.sh hotspot`). Confirms this
is entirely CratonVM-specific.

**The original hypothesis in this doc was wrong.** This is *not* infinite
recursion in Spring's `ImportSelector`/`ConfigurationClassParser` cycle
detection — `ImportSelectorTests`'s own import graphs are shallow (2-3
levels deep, by design in the test fixtures) and could never legitimately
need anywhere near a stack-overflowing depth. The real signature: **of the
9 test methods, the exact 4 that pass are the 4 that don't call
`Mockito.spy(...)`, and the exact 5 that fail with `StackOverflowError` are
the 5 that call `spy(new DefaultListableBeanFactory())` + `inOrder(...)`
verification.** This is a Mockito `spy()` bug, confirmed to reproduce with
**zero Spring context involved at all** — see
[`docs/internal/repros/mockito-spy-hierarchy-recursion/`](../internal/repros/mockito-spy-hierarchy-recursion/)
for the full repro kit and decompiled root-cause chain (`javap -p -c`
against the real mockito-core 5.23.0 / byte-buddy 1.18.3 jars, not guessed
from memory). Minimal repro (`SpyDLBFProbe.java` in that directory): create
a real `DefaultListableBeanFactory`, `spy()` it, call
`spy.registerSingleton("x", "y")` **once** — `StackOverflowError` in ~139s
real time, with `--nojit` making no difference (rules out a JIT miscompile:
same failure, same rough timing, interpreter-only).

**Root cause chain** (see the repro kit's README for the full decompiled
detail): `spy()` of a non-final class uses Mockito's **inline** mock maker,
which retransforms (`Instrumentation.retransformClasses`) the bytecode of
**every class in the hierarchy** in place (confirmed via
`-Dnet.bytebuddy.dump=`: `DefaultListableBeanFactory`,
`AbstractAutowireCapableBeanFactory`, `AbstractBeanFactory`,
`FactoryBeanRegistrySupport`, `DefaultSingletonBeanRegistry`,
`SimpleAliasRegistry` all get advice-woven bodies; the receiver's runtime
class stays `DefaultListableBeanFactory`, no subclass is created). Every
redefined method's entry checks
`MockMethodDispatcher.get(identifier, this).isMocked(this)` before
deciding whether to intercept. `isMocked()` delegates to
`MockMethodAdvice$SelfCallInfo.checkSelfCall(Object)` — a
`ThreadLocal<Object>`-based guard (`if (o == get()) { set(null); return
false; } return true;`) whose entire purpose is recognizing "this is a
reflective 'call the real method' invocation re-entering the same advised
method" (unavoidable because `Lookup.unreflect()` on a public method always
produces a virtually-dispatching handle, per JDK semantics) and letting it
fall through to the unmodified original body instead of re-intercepting
forever. **This guard does not appear to terminate the recursion on
CratonVM.** The live `KRUN_STACK=1` stack trace shows an exact repeating
~15-frame cycle bouncing between `DefaultListableBeanFactory
.registerSingleton` (line 1491, the real body's own `super.
registerSingleton(...)` call) and `DefaultSingletonBeanRegistry
.registerSingleton` (line 142, its own advice entry), through
`MockMethodDispatcher.handle` → `InstrumentationMemberAccessor.invoke` →
back to `DefaultListableBeanFactory.registerSingleton`, forever.

**Ruled out this session, each with a direct, targeted, decompiled-bytecode-
informed empirical test** (not guesses — every one of these was verified to
match HotSpot before testing CratonVM, then run on CratonVM):
- **Not a JIT miscompile.** `--nojit` reproduces the identical
  `StackOverflowError` in the same rough time (~116s vs ~139s with JIT) —
  interpreter-level, not JIT-specific.
- **Not broken reflection.** `Class.getMethod()`/`getDeclaringClass()`/
  `getDeclaredMethods()` correctly identify that `DefaultListableBeanFactory`
  overrides `DefaultSingletonBeanRegistry.registerSingleton`, byte-for-byte
  matching HotSpot, **both before and after** the hierarchy has been
  retransformed (`OverrideProbe.java`, `SpyDLBFProbe.java` STEP4 in the
  repro kit).
- **Not `MockMethodAdvice.isOverridden()` returning the wrong boolean.**
  Called ByteBuddy's own `MethodGraph.Compiler` directly (the exact
  algorithm `isOverridden()` uses) against the POST-RETRANSFORM
  `spy.getClass()` — correctly resolves `registerSingleton`'s representative
  to `DefaultListableBeanFactory`, both for the overridden
  (`DefaultSingletonBeanRegistry`-declared) and non-overridden
  (`DefaultListableBeanFactory`-declared) method objects, matching HotSpot
  exactly (`SpyThenGraphProbe.java`).
- **Not a generic stale-ThreadLocal-value-across-GC bug.** A standalone
  simulation of the exact `replace()`/`checkSelfCall()` pattern — store an
  object reference in a `ThreadLocal`, force two `System.gc()` cycles with
  ~150MB of intervening garbage, then compare the stored value against a
  fresh reference to the same logical object via `==` — works correctly on
  CratonVM (`SelfCallProbe.java`), confirming `native_tl_get`/`native_tl_set`
  (`native-builtins/src/phases_early.rs`, GC-safe via `add_global_root`/
  `resolve_global_root`) are not the gap for this specific access pattern.

**Separate, confirmed-real performance finding (not the cause of the
recursion, but worth its own fix):** computing ByteBuddy's
`MethodGraph.Compiler` for a class **freshly retransformed** by Mockito
takes **~70 seconds** on CratonVM (`SpyThenGraphProbe.java`), vs. instant
for the same computation against an un-retransformed class. This isn't
what causes the infinite loop (the loop's own per-iteration cost is fast —
total time-to-overflow, ~139s, is consistent with one ~70s cold
`MethodGraph` compile plus ~69s of many fast recursive frames, not
thousands of 70s computations), but it's a real, independently-reproducible
slowdown specific to reflecting over post-redefinition classes.

**Leading, unconfirmed hypothesis for the next session**:
`MockMethodDispatcher.get(identifier, instance)` — the bootstrap-injected
static bridge every redefined method's advice entry AND
`SerializableRealMethodCall.invoke()` independently call to reach the ONE
shared `MockMethodAdvice` instance (and hence its ONE `selfCallInfo`
`ThreadLocal` object) — may not reliably resolve to the same object across
all these call sites and all classes in the retransformed hierarchy on
CratonVM. If two call sites see two different `MockMethodAdvice` instances
(e.g. because of how CratonVM tracks the identity/static-state of a
bootstrap-appended, dynamically-injected class), each would carry its own
distinct `selfCallInfo`, and the ThreadLocal-based guard would never see a
match between the "set" (in `SerializableRealMethodCall.invoke()`, right
before the reflective call) and "check" (in the redefined method's advice
entry, on re-entry) — cleanly explaining unconditional non-termination
without requiring any single check to return a "wrong" answer in isolation.
This was **not** directly confirmed this session — the next step is a
targeted instrumentation of `MockMethodDispatcher.get()`'s resolution
(e.g. printing `System.identityHashCode()` of the returned dispatcher from
multiple call sites within one recursive chain, or a CratonVM-side trace of
every `Class` object minted for the name
`org.mockito.internal.creation.bytebuddy.inject.MockMethodDispatcher`),
which needs a rebuild cycle this session didn't have budget for after the
~62-minute initial release build plus the empirical work above.

**Why this is unrelated to every other cluster in this doc**: none of the
other TIMEOUT/hang clusters involve `Mockito.spy()` — they use plain
`mock()` (already verified working end-to-end for creation, stubbing,
`verify()`, per
[`bug-09-mockito-inline-mockmaker-selfattach.md`](../internal/kafka-suite-bugs/bug-09-mockito-inline-mockmaker-selfattach.md)
and
[`spring-boot-groovy-indy-mockito-mock-dispatch.md`](../internal/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md))
or no Mockito at all. `spy()`'s `CALLS_REAL_METHODS` default answer is the
first workload in this codebase's history to exercise Mockito's
cross-hierarchy "call the real method, skip re-interception" path at
all — `mock()`'s default answer never invokes real method bodies, so this
exact path was never exercised by any of the prior, now-fixed Mockito work.

**Still OPEN.** No fix attempted — the guard mechanism above is deep inside
Mockito/ByteBuddy's own real, unmodified bytecode (not a CratonVM native to
patch directly), and every specific hypothesis narrow enough to safely fix
was empirically refuted this session. A confident fix needs the
`MockMethodDispatcher.get()` identity instrumentation described above
first.

## 2026-07-16 local investigation — `ImportSelectorTests`: original repro no longer reproduces, but the real class exposes a NEW, more severe failure mode on 2/5 `spy()` sub-tests (FIXED — see 2026-07-16 joint verification addendum at the end of this section)

Worktree `cratonvm-importselector-20260716` on the Azure host, branch
`fix/importselector-spy-soe-20260716-141435`, dev tip `6c517cd9` (fetched
fresh; no local/uncommitted changes anywhere else in the host's shared
checkout affect this worktree). Binary `cvm-importselector-baseline`
(release build). Java home `/data/data/jdk25-real`.

**Task**: continue the 2026-07-13/07-15 investigation (see the two sections
above and `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s entry) — reconcile the
tension between "the raw `MethodGraph.Compiler` computation is correct in
isolation" and "the live recursion behaves as if `isOverridden`/the
self-call guard never terminates", then fix it.

**Step 1 — rerun the existing isolated repro (`SpyDLBFProbe.java`) first,
per standing instructions.** It **no longer reproduces**: 2/2 clean runs,
`spy(new DefaultListableBeanFactory())` + one `spy.registerSingleton("x",
"y")` call returns normally every time, no `StackOverflowError`, no crash.
This is a real change from the 2026-07-13 finding (same probe, same
mechanism, reliably reproduced `StackOverflowError` in ~139s back then). No
code was changed to produce this — it's the state of `dev` as fetched. Not
yet bisected to a specific commit; nothing in the intervening `dev` history
between the two investigations obviously touches Mockito/ThreadLocal/
reflection (same caveat as the 07-13 note), so this may be an incidental
side effect of unrelated GC/interpreter work rather than a deliberate fix.

**Step 2 — rerun the real `ImportSelectorTests` class.** First attempt used
the shared `/data/tmp/aotfix-runs/combined_cp_dedup.txt` classpath (per the
documented fast-iteration workflow) and got a **contaminated classpath**:
it concatenates `cratonvm-testcp.txt` dumps from ~20+ different worktrees
built at different times, and contains two different `byte-buddy` versions
(1.18.3 and 1.18.8) on one classpath simultaneously. This produced
`AbstractMethodError: method org/junit/platform/engine/TestEngine.getId()
...has no Code attribute` and a `ClassCastException` inside JUnit Platform's
own launcher — both classpath-contamination artifacts, **not** CratonVM
bugs, and not the bug under investigation. **Fix**: use a single spring
module's own freshly-generated classpath instead, e.g.
`/data/data/wt-osr-other516-20260708-2131/apps/spring-framework/spring-context/build/cratonvm-testcp.txt`
— single `byte-buddy-1.18.3`, single `mockito-core-5.23.0`, single
`junit-platform-launcher-6.1.1`. **Future sessions reusing the
`aotfix-runs`/`mockk-tmp` shared classpath dumps should prefer a
single-module `cratonvm-testcp.txt` over the multi-worktree combined dump**
unless the multi-module combined classpath is specifically needed (e.g.
`integration-tests`).

With the clean classpath, running all 9 methods in one JVM (`MethodRun`)
still aborts before finishing (see Step 3) — so sub-tests were run
individually instead (`MethodRun ImportSelectorTests <methodName>`), which
is also a more precise signal per-method regardless.

**Result: 3 of the 5 `spy()`-using sub-tests now PASS cleanly** —
`importSelectors`, `importSelectorsWithGroup`,
`importSelectorsSeparateWithGroup` all report `1 tests successful` with no
warnings beyond the usual harmless `[CCE] enhance:` CGLIB logging and
speculative-slot GC guard messages. This matches the isolated-probe result:
whatever caused the unconditional self-call-guard recursion for a single
`registerSingleton` call in isolation appears to no longer be triggered for
these three test bodies either.

**The remaining 2 of 5 — `importSelectorsWithNestedGroup` and
`importSelectorsWithNestedGroupSameDeferredImport` — still fail, but via a
NEW and more severe failure mode than the original clean
`StackOverflowError`.** Both deterministically abort (`SIGABRT`, exit 134)
after ~23-25 seconds, every run, preceded by:
```
GC: young object-start walk stopped at an implausible extent  young_cursor=2209856 young_used=<varies>
[GC-ARRAY-GUARD] array_length(non-array): kind_byte=0 class_id=0 elem_byte=0 stored_len=0 obj=<addr> (#1/5; set CRATONVM_GC_ARRAY_GUARD_BT=1 for backtrace)
Stale pointer detected in invokevirtual receiver (ptr=<addr>, all-zero header) — falling back to CP class java/util/Set
```
followed by a cascade of "stale pointer"/"out-of-bounds field" guard
messages against zeroed-out object headers, then the abort. In one rerun
with `RUST_BACKTRACE=1 CRATONVM_GC_ARRAY_GUARD_BT=1`, the corruption instead
surfaced as a **different terminal error** (`AbstractMethodError: method
java/lang/CharSequence.length()I has no Code attribute`) while JUnit's own
`MutableTestExecutionSummary.printTo` tried to format the (by-then already
corrupted) test summary — i.e. the corruption is real heap damage that
manifests differently run-to-run depending on what code happens to touch
the damaged region next, not a single deterministic exception type.

**Ruled out this session**: heap-size/GC-pressure as the driving variable.
Reran `importSelectorsWithNestedGroup` with `--Xmx 512m` (vs. CratonVM's
default, much smaller) — the corruption point was **byte-for-byte
identical**: `young_cursor=2209856` in both runs, despite `young_used`
differing (`67113352` at 512m vs. a smaller value at default). A heap-
pressure/GC-timing bug would be expected to move that offset (or not
trigger at all) under a much larger heap; an identical cursor value
regardless of heap size means this is a **deterministic correctness bug
tied to a specific allocation count/pattern**, not a rare collection-timing
race. (This is also evidence — not proof — that it's a *different* bug from
the open, heap-size-sensitive
`stream-arraylist-gc-pressure-heap-corruption-found-20260714.md` /
`-Xmx32m` finding; worth a cross-check by whoever owns that doc. NOTE: a
concurrent 2026-07-16 session investigating `ApplicationContextAotGeneratorTests`
/ `TestContextAotGeneratorIntegrationTests` — see
`CRATONVM-SPRING-GENUINE-BUGLIST.md`'s "2026-07-16 dedicated re-triage"
bullet — independently found what looks like the SAME class of bug
(all-zero-header stale pointers hitting several unrelated JUnit-Platform/
javac-internal classes within milliseconds of each other, reproducing
identically under JIT and `--nojit` and across heap sizes), on a completely
different test class with no Mockito `spy()` involved at all. This strongly
suggests a general, currently-open, cross-cutting GC heap-corruption bug —
not something specific to Mockito `spy()` or `ImportSelectorTests` — worth
a joint investigation rather than two separate ones.)

**Not root-caused to a specific Rust source line this session** — ran out
of budget after the build (13.5 min release build under host contention),
the classpath-contamination detour, and the heap-size differential test.
**Working theory** (consistent with, but not proof of, the 2026-07-15
`isOverridden`/self-call-guard hypotheses in the sections above and in
`CRATONVM-SPRING-GENUINE-BUGLIST.md`): this is very likely **the same
underlying guard-doesn't-terminate bug**, not a new one — the simple probe
and 3/5 real sub-tests no longer hit it (or hit it 0 times), but the two
"nested group" tests exercise a deeper/wider `spy()` interaction (more
distinct import selectors × grouped/deferred processing × `inOrder(...)`
verification touching more distinct advised methods) that still triggers
some bounded-but-large number of unwanted recursive
re-interceptions — enough to allocate heavily and corrupt the young
generation, but for reasons not yet understood, no longer enough (or no
longer of the right shape) to hit CratonVM's own Java-level stack-depth
check and throw a clean, catchable `StackOverflowError` the way it used to.
This theory is **not confirmed** — it needs a rebuild with a call counter
on the recursive advice-entry path (or `KRUN_STACK=1` on one of the two
still-failing sub-tests) to see whether recursion is happening at all before
concluding it's the same mechanism just running longer. Given the
independent AOT-cluster finding above, it's equally plausible this is a
general GC bug unrelated to Mockito that any sufficiently allocation-heavy,
reflection/bytecode-generation-heavy workload can trigger.

**Concrete next steps for whoever picks this up**:
1. Get a `KRUN_STACK=1` (or equivalent) live trace specifically on
   `importSelectorsWithNestedGroup` — confirm whether the same
   `DefaultListableBeanFactory.registerSingleton` ⇄
   `DefaultSingletonBeanRegistry.registerSingleton` cycle from the 2026-07-13
   trace is still occurring (just failing to terminate for longer / more
   iterations before corrupting memory), or whether this is now a
   completely different call shape (e.g. a different pair of overridden
   methods, given nested/grouped deferred imports invoke more distinct
   `DefaultListableBeanFactory` methods through the spy).
2. Use `CRATONVM_GC_ARRAY_GUARD_BT=1` for a backtrace at the first guard
   trip, and the `CRATONVM_DBG_A2` forensic probe referenced in
   `gc/src/gen_heap.rs`'s corruption-diagnostic comment (dumps whether a
   rejected address was ever header-written by an allocator, and by which
   allocation path — interpreter TLAB, `gen_heap`, JIT inline-alloc, or
   TLAB-tail-filler) to identify which allocator produced the corrupt
   object at the reproducibly-identical `young_cursor=2209856` offset.
3. Coordinate with whoever owns the `ApplicationContextAotGeneratorTests`
   2026-07-16 re-triage (same day, same symptom shape, different test
   class) — if the two converge on one root cause, this becomes a single,
   higher-priority, cross-cutting GC bug rather than two separate
   Spring-suite residuals.
4. If the `KRUN_STACK` trace in step 1 confirms the same Mockito cycle, the
   actual fix target is still the `MockMethodDispatcher.get()`/
   `isOverridden` identity question from the 2026-07-15 `GENUINE-BUGLIST.md`
   entry and the "leading, unconfirmed hypothesis" above — this session did
   not add new evidence toward or against that specific hypothesis, only
   toward the observable symptom shape changing.

### 2026-07-16 joint verification addendum — FIXED, confirmed by rebuild+rerun

Concrete next step #3 above ("coordinate with whoever owns the
`ApplicationContextAotGeneratorTests` re-triage — if the two converge on one
root cause, this becomes a single, higher-priority, cross-cutting GC bug")
is what this task did. A third, independent, concurrent 2026-07-16 session
(investigating `PersistenceAnnotationBeanPostProcessorAotContributionTests`,
see `CRATONVM-SPRING-GENUINE-BUGLIST.md`) hit an identical-looking
stale-ObjectRef/all-zero-header crash during `TestCompiler`'s in-process
`javac` compile and traced it to an unrelated dev commit that landed mid-investigation:
`fb15be63` ("fix(gc): GAP_FILLER_CLASS_ID not special-cased in new young-GC
exact-walk loops") — its description ("misparsing the TLAB gap-filler
sentinel broke the young-GC exact object-start walk early, leaving
everything allocated afterward outside the exact set — `mark_young`
silently drops those live objects and the non-moving sweep reclaims them as
garbage, i.e. mass stale-pointer/all-zero-header corruption") matches this
class's symptom precisely, and matches the `ApplicationContextAotGeneratorTests`
symptom precisely too (per the cross-reference two paragraphs above). This
task verified the fix closes **both** independently-found instances.

Fresh worktree `/data/data/wt-gcbug-verify-20260716`, `origin/dev` tip
`47151b27` (confirmed `fb15be63` is an ancestor via `git merge-base
--is-ancestor`), full `cargo build --release` (35m48s under heavy Azure-host
contention — unrelated to the fix itself), real JDK 25, `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`,
same `KRun`/`MRun` JUnit-Platform launcher pattern and single-module
`spring-context` classpath as the original 2026-07-16 investigation above.

**Result: `9/9` methods pass, including both previously-crashing "nested
group" methods:**
```
RESULT org.springframework.context.annotation.ImportSelectorTests found=9 succ=9 fail=0 skip=0 abort=0 ms=47126 status=OK
```
Grepping the full run log for the corruption signature (`Stale pointer
detected`, `GC-ARRAY-GUARD`, `implausible extent`, `LOADERR`) returns **zero
matches**. `importSelectorsWithNestedGroup` and
`importSelectorsWithNestedGroupSameDeferredImport` — the two methods that
deterministically `SIGABRT`ed on `GC: young object-start walk stopped at an
implausible extent` every run in the pre-fix investigation above — now
complete cleanly with no warnings beyond the usual harmless CGLIB/`[CCE]`
logging.

One methodology gotcha hit and worked around during this verification, worth
recording for future investigators on this host: an initial single-method
run via `MRun ImportSelectorTests importSelectorsWithNestedGroup` and an
initial full-class run both failed uniformly on all 5 `spy()`-using methods
with `java.lang.IllegalStateException: Could not initialize plugin: interface
org.mockito.plugins.MockMaker` — a *different* failure from both the original
`StackOverflowError` and the GC corruption, and initially concerning. This
turned out to be a host-environment artifact, not a CratonVM bug: the Azure
host's root filesystem (`/`) was at 100% (`0` bytes available per `df`),
which broke Mockito's self-attach mechanism's write of its agent jar to the
default `/tmp` (on root). Re-running with `TMPDIR`/`-Djava.io.tmpdir` pointed
at the roomy `/data` partition instead resolved it immediately (the `9/9 OK`
result above is from that rerun). Anyone hitting a cold `MockMaker` init
failure on this host that doesn't match either of this bug family's two known
signatures should check `df -h /` and redirect `TMPDIR` before assuming it's
a new VM bug.

**Not this session's fix; attributing correctly rather than claiming
credit.** No code change was needed or made — this is a verification-only
confirmation, corroborating the `PersistenceAnnotationBeanPostProcessorAotContributionTests`
(0/3 crashes after `fb15be63` vs. 6/6 before) and `ApplicationContextAotGeneratorTests`
(`LOADERR` → `found=40`, 0 corruption lines) results — three independent test
classes, three independent investigating sessions, one shared root cause,
one fix. See `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s `ApplicationContextAotGeneratorTests`
entry for the sibling verification detail.

### 2026-07-17 second independent re-confirmation — separate task, separate host state, still 9/9

A separate, later task was assigned to root-cause and fix this exact bug from
the original briefing (Mockito `spy()` corruption, `isOverridden` prime
suspect) without initially being told it was already closed. Rather than
trust the FIXED status above at face value, it re-verified from scratch with
a deliberately maximally-independent setup, since by this point in the
session the host's disk-pressure cleanup had deleted *every* prior
`spring-framework` checkout, worktree, and `cratonvm-testcp.txt` on the host
(including `/data/data/wt-gcbug-verify-20260716` referenced above) — nothing
could be reused even if desired.

Setup: fresh `git clone` of upstream `spring-projects/spring-framework`
(`7.1.0-SNAPSHOT`, current HEAD as of 2026-07-17), fresh `cargo build
--release` of CratonVM at `origin/dev` tip `56728b1a` (`fb15be63` confirmed an
ancestor via `git merge-base --is-ancestor`), fresh `:spring-context:testClasses`
Gradle build (dependency cache reused via a copied, not symlinked,
`GRADLE_USER_HOME/caches/modules-2` — a straight symlink back onto the
host's chronically-full root filesystem fails on any new dependency
resolution with `No space left on device`), single `byte-buddy-1.18.3` /
`mockito-core-5.23.0` on the classpath (verified no duplicate versions),
`MethodRun`/JUnit-Platform-launcher pattern, real JDK 25,
`CRATONVM_DEFAULT_HEAP_MAX_MB=2048`.

**Result, run twice for determinism: `RESULT started=9 succeeded=9 failed=0`
both times**, including both individually-run and full-class invocations of
`importSelectorsWithNestedGroup`/`importSelectorsWithNestedGroupSameDeferredImport`,
zero corruption-signature log lines, zero `StackOverflowError`. Independently
corroborates the FIXED status — three investigating sessions, three
independently-built environments, one shared root cause (`fb15be63`), always
`9/9`.

Two environment gotchas hit and worked around, both host-state artifacts and
not CratonVM bugs, recorded here since they cost real time and could bite
future sessions on this host: **(1)** a Gradle build-cache-restored
(`FROM-CACHE`) `:spring-context:compileJava`/`testClasses` produced an
incomplete `classes/java/main` tree on the very first attempt — a real
`.class` file (`StandardBeanExpressionResolver$1`) was present on disk yet
CratonVM raised `NoClassDefFoundError` loading it, reproducing on **every**
`spy()`-using test including previously-green ones (`importSelectors`), which
made it look like a regression at first. This is not a CratonVM bug: forcing
`--rerun-tasks --no-build-cache` on the affected Gradle tasks produced a
correct tree and the error vanished on every subsequent run, strongly
suggesting the cached build-cache entry itself was corrupted by one of this
session's many root-filesystem-100%-full episodes during extraction.
**(2)** both `GRADLE_USER_HOME` and `-Djava.io.tmpdir` need to be redirected
off `/` (e.g. to `/data/tmp`) on this host — Mockito's self-attach boot-jar
write and Gradle's own dependency-cache writes both throw `IOException`/
`No space left on device` on the chronically-full root filesystem otherwise,
which surfaces as `IllegalStateException: Mockito could not self-attach...`
or a Gradle configuration failure that has nothing to do with either the
Mockito recursion bug or the GC bug.

**Not this task's fix either; attributing correctly.** No CratonVM source was
changed by this task. This addendum's only contribution is a second,
maximally-independent confirmation that the FIXED status holds, plus the two
environment gotchas above for future sessions.

## 2026-07-13 local investigation — `web.service.registry.*` residuals (both root-caused, neither fixed — still OPEN)

Reproduced entirely locally, worktree
`cratonvm-wt-webserviceregistry-local-20260713`, dev tip `edca766e` (merged
forward from `dbf7827c`), binary `cratonvm-websvcreg-local.exe`. Diagnostics
added this session (commit `b34679e5`, kept in place): `CRATONVM_IAE_TRACE2`
(per-element resolved-value dump in `create_annotation_proxy`) and a
widened `CRATONVM_ANN_TRACE` gate covering `Import`/`ImportHttpServices`.

### `ImportHttpServiceRegistrarTests` — `ClassCastException`, root-caused, not fixed

> **2026-07-16 update:** the exact root cause is now known — a cross-loader
> `MergedAnnotation$Adapt` enum-identity split (`Adapt.CLASS_TO_STRING.isIn()`
> returns a false negative comparing an application-loader `Adapt` constant
> against a fork-loader one), traced live via instrumented Spring source
> (not CratonVM's annotation/reflection layer, and NOT `AnnotationTypeMapping
> .getMappedAnnotationValue`/`Method` identity as hypothesized below — that
> path was traced and is clean). Pinned to `resolve_field_ref`
> (`vm/src/runtime/interpreter.rs`) resolving a `getstatic`'s field-owning
> class via a loader-blind fallback where `CONSTANT_Class` resolution
> (`resolve_class_loader_aware`) already has a loader-faithful one. A fix
> along those lines was implemented, built, and REVERTED after it corrupted
> heap state (stale pointers / `ClassId(0)` / spurious `NoSuchMethodError`) —
> likely a GC-safety precondition the field-opcode fast path doesn't
> currently satisfy for a re-entrant `loadClass()` call. See
> `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s `@Import` attribute CCE entry for
> the full trace evidence, the reverted diff's location, and next-step
> guidance. The narrative below (SoftReference hypothesis, `Method`-identity
> hypothesis) is superseded but kept for history.

**Confirmed 3/5 pass, 2/5 fail** (`basicListingWithAot`, `basicScanWithAot`
fail; `basicListing`, `basicScan`, `clientType` pass). The passing 3 call
`registrar.registerHttpServices()` directly and never touch
`ConfigurationClassParser`. The failing 2 go through
`ApplicationContextAotGenerator.processAheadOfTime` →
`ConfigurationClassParser.collectImports` →
`SourceClass.getAnnotationAttributes(Import.class.getName(), "value")`
(`ConfigurationClassParser.java:577`, then `:1119`'s
`(String[]) annotationAttributes.get(attribute)` cast), which throws
`ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;`.

**Both failing tests are `@CompileWithForkedClassLoader`.** Confirmed via
the live stack trace that the failure happens on the SECOND, forked-loader
re-execution (`CompileWithForkedClassLoaderExtension.intercept` took the
`invocation.proceed()` branch, meaning `testClass.getClassLoader()` was
already the forked `CompileWithForkedClassLoaderClassLoader` at the point
of failure) — i.e. the test class, its nested config class, and (per
`ClassLoader.loadClass`'s default parent-first delegation crossing into
`findClass`) `ImportHttpServices`/`Import` themselves all get freshly
re-defined under that loader.

**Built a fast (~2s), reliable, 1:1-faithful repro** — no need to run the
full 5-test class or wait on the suite runner to iterate:
- `Repro7` (`org.springframework.core.test.tools.Repro7`, same package as
  the real `CompileWithForkedClassLoaderClassLoader` to access its
  package-private constructor) creates a fresh forked loader, reloads
  `Repro7Body` through it, and invokes `Repro7Body.run()` reflectively —
  exactly mirroring `CompileWithForkedClassLoaderExtension.runTest`'s own
  `Launcher`+`selectMethod`+reflective-invoke shape (a driver that keeps
  Spring classes on the ORIGINAL loader, like an earlier attempt of mine,
  reproduces a DIFFERENT, unrelated split-package error even on real
  HotSpot — the whole test body must be reloaded and invoked together for
  a faithful repro).
- `Repro7Body.run()` (`org.springframework.web.service.registry.Repro7Body`)
  registers a `@ImportHttpServices`-annotated nested config class and calls
  `ApplicationContextAotGenerator.processAheadOfTime` — reproduces the
  IDENTICAL `ClassCastException` at the identical stack trace, 100% of the
  time.
- **Verified correct on real HotSpot** (both the driver+body pair and every
  intermediate simplification along the way).

**Ruled out, with direct empirical evidence** (not guesses):
- **Not the array/scalar attribute-value construction.** `CRATONVM_IAE_TRACE2`
  confirms `Import`'s `value` element is built as a proper `Class[]` of
  length 1 (`Object(cid=12 name="java/lang/Class" is_array=true len=1)`) at
  proxy-construction time, every time it's constructed.
- **Not `method_annotations()` mis-scoping.** `Import.value()` and
  `ImportHttpServices.value()` share the exact same name AND descriptor
  (`()[Ljava/lang/Class;`), raising the hypothesis that a method-annotation
  lookup keyed insufficiently (e.g. by name only) could leak
  `ImportHttpServices.value()`'s own `@AliasFor("types")` onto
  `Import.value()` (which has no annotations on it in real Spring source).
  The widened `CRATONVM_ANN_TRACE` trace directly refutes this:
  `ctx.method_annotations(class_id=<Import's own cid>, "value", ...)`
  consistently and correctly returns 0 annotations, every single time it's
  queried across the whole run.
- **Not JIT-specific.** `--nojit` reproduces the identical exception,
  identical stack trace.
- **Not a heap-size/GC-timing race in the simple sense.** Reproduces 100%
  of the time regardless of `--Xmx` (tested default and `2g`) — this is a
  deterministic defect given this exact workload shape, not a rare
  collection-timing coincidence.
- **Isolating just the classloader-fork + annotation-read step is NOT
  sufficient to reproduce it.** A narrower repro (`Repro6`) that reloads
  the config class and `ImportHttpServices` through a fresh forked loader
  and then directly calls `AnnotationUtils.validateAnnotation` +
  `AnnotationMetadata.introspect(...).getAnnotationAttributes(Import...)`
  — WITHOUT the full `ApplicationContextAotGenerator` pipeline — passes
  cleanly, correctly returning `{value=[ImportHttpServiceRegistrar]}` as a
  `String[]`. The bug needs BOTH the forked-loader reload AND the fuller
  AOT/bean-registration processing pipeline to manifest; the annotation
  metadata API in isolation is fine.

**Leading, unconfirmed hypothesis**: Spring's own `AttributeMethods.cache`
and `AnnotationTypeMappings.cache` (`org.springframework.core.annotation`)
are both `ConcurrentReferenceHashMap`s, whose default reference type is
`SOFT` for both keys and values — i.e. Spring's own per-annotation-type
metadata (including cached/mirrored attribute values resolved once during
`AnnotationTypeMapping` construction) is held behind `SoftReference`s. This
is exactly the shape of construct that would surface a latent bug in how
CratonVM's GC updates (or fails to update) a `Reference`'s `referent`
pointer versus how it decides which soft/weak referents survive a
collection — a stale/wrong-address referent read back after a GC event
would manifest as exactly this symptom (a resurrected, wrong-typed object —
here a bare `java.lang.Class` — where a `Class[]` used to be). This was
**not directly confirmed** — it requires either instrumenting
`gc/src/reference.rs`'s soft-reference processing/relocation path directly,
or a decompiled-bytecode-level trace of
`AnnotationTypeMapping`/`AttributeMethods`'s own caching (in the style of
this session's `ImportSelectorTests` Mockito investigation, see the section
above) to see exactly which cached value gets read back wrong and from
where. Neither was completed this session — each further experiment here
costs a ~20-30 minute release rebuild (this build uses `lto="fat"`,
`codegen-units=1`) plus test time, and this session's budget for this
cluster ran out at the hypothesis stage.

**Not fixed.** Repro kit (`Repro.java`/`Repro2.java`/.../`Repro7.java`,
`Repro7Body.java`, all under `org.springframework.{core.test.tools,web.service.registry}`)
was left in the session scratchpad, not committed (throwaway harness code,
not part of the CratonVM source tree) — regenerate from this doc's
description if picked up again; each file is small and the progression
from `Repro2` (fails to reproduce) through `Repro7` (reproduces) is
instructive for why the forked-loader+full-pipeline combination is
necessary.

### `GroupsMetadataValueDelegateTests` — fatal `class not found`: ROOT-CAUSED AND FIXED (commit `9bca11f5`), verification blocked by an unrelated concurrent regression

**Root cause, confirmed precisely.** Built a fast (~seconds, not the real
suite's ~22min) standalone repro that runs all 8
`GroupsMetadataValueDelegateTests` scenarios, each under its own **fresh**
`CompileWithForkedClassLoaderClassLoader` (exactly mirroring how JUnit5
re-forks per `@Test` method under a class-level
`@CompileWithForkedClassLoader`). It crashed with the identical fatal
`class file error: class not found: .../GroupsMetadata__TestCode` on
**scenario 1** — the first point at which *two different* forked loaders
each hold their own distinct class named `GroupsMetadata__TestCode`
(Spring's `ClassNameGenerator` always produces this exact name,
deterministically, for this feature/target pair — every scenario/test
method generates a same-named-but-different class under its own loader).

The mechanism: reflective `Method.invoke()` on a **static** method (`get()`
on the generated class — static methods are never virtually dispatched)
fell through `native_method_invoke`'s final dispatch branch, which called
`ctx.invoke(&class_name, ...)` — resolving the declaring class **by name**
through the loader-blind global path
(`load_class_concurrent` → `ClassManager::get_loaded_class_id`). That
lookup has a pre-existing, deliberate, documented behavior (from an earlier
Groovy-multi-loader fix): it returns "ambiguous, not a guess" the moment
**two or more different user-defined loaders** each register their own
distinct class under the identical simple name — entirely correct in
isolation, but the caller here had no business re-resolving by name at
all, since the reflective `Method` object already carries an unambiguous,
correctly-resolved declaring `ClassId` (via its own `clazz` mirror). Once
ambiguous, the lookup falls through to `find_class_bytes_delegated`, which
only ever checks the bootstrap/extension/application loaders — never any
user-defined one — producing the observed hard, uncatchable
`ClassNotFoundError`-shaped VM abort instead of a normal, catchable
`ClassNotFoundException`.

**Confirmed NOT fixed by the 2026-07-13 `TestCompiler`/`JavacFileManager.list`
GC-safety fix** (`10831b7b`/`a02322e4`) — a genuinely different defect in
the same general pipeline.

**Fix** (commit `9bca11f5`): added `NativeContext::invoke_by_class_id`
(default impl falls back to the existing name-based `invoke()` for
mocks/tests — zero behavior change for every other caller) plus its real-VM
implementation `invoke_by_class_id_shared` (mirrors `invoke_shared` but
skips `load_class_concurrent` entirely, using the caller-supplied `ClassId`
directly), and switched `native_method_invoke`'s static-method dispatch
branch to use it whenever a declaring `ClassId` is already available (which
is always, for a normally-obtained `Method` object).

**Verification status — fix confirmed via the isolated repro, full
suite-runner confirmation currently blocked by an unrelated regression.**
Rebuilding and re-running the same 8-scenario fast repro against the fixed
binary: the exact `class not found: GroupsMetadata__TestCode` abort is gone
from every scenario (0 occurrences, was reproducible 100% of the time
before). However, a **separate, unrelated regression landed on `dev`
between diagnosis and this fix** (`d8092acb`, "fix-tests-real-jdk-contracts",
2026-07-14 17:09 UTC) that breaks `java.util.Locale` bootstrap in real-JDK
mode (`InternalError: null property: java.home` /
`NoClassDefFoundError: java/util/Locale` — see the dedicated writeup at
[`docs/known-issues/vm/locale-real-jdk-bootstrap-noclassdeffounderror.md`](vm/locale-real-jdk-bootstrap-noclassdeffounderror.md),
filed by a different, concurrent session; root-caused there as a VM
bootstrap-ordering bug, fix not yet landed as of this writing). That
regression now fires **before** `TestCompiler.compile()` ever reaches the
code path this fix touches, so the full real-suite run currently reports
`LOADERR`/early failure for both `ImportHttpServiceRegistrarTests` and
`GroupsMetadataValueDelegateTests` regardless of this fix. To isolate the
two changes, this fix was additionally verified by building **from the fix
commit alone** (`9bca11f5`, i.e. before merging the `d8092acb` regression)
in a scratch worktree: the 8-scenario repro no longer hit the ambiguous-
loader abort on the scenarios it reached (a debug/unoptimized build makes
in-memory `javac` compilation extremely slow — each scenario takes minutes
— so exhausting all 8 scenarios in that configuration was not completed,
but the scenarios that did complete, including scenario 0, ran cleanly with
no regression in behavior).

**Update 2026-07-14, end-to-end confirmation** (the `java.home`/`Locale`
regression this was blocked on is now fixed, `f62d2073`): reran
`GroupsMetadataValueDelegateTests` through the real `run-suite.sh` against a
binary built from this fix merged with the latest `dev`. The hard VM abort
is **confirmed gone** — `found=8` (was `found=0`, ABEND) — but the class
still doesn't pass: all 8 now fail on a normal, catchable
`IllegalStateException: WritableContent did not append any content`, a
different and much more mundane defect one layer further into the same
AOT-codegen output-writing path. **Net status: the specific bug this
section targeted (the hard, uncatchable ClassNotFoundError-shaped abort) is
FIXED and confirmed** — the class now gets meaningfully further and fails
like an ordinary test instead of crashing the whole VM — **but the class as
a whole is still not green**; the `WritableContent` defect is a new, distinct,
not-yet-investigated residual for a future session.

For completeness, `ImportHttpServiceRegistrarTests` was re-verified the same
way and is **unchanged**: still 3/5, still failing at the same
`ConfigurationClassParser.parse` → `ClassCastException` point described
above — none of today's other fixes touched this one.

## 2026-07-13 local investigation #5 — Missing `ApiVersionStrategy` bean: RESOLVED; both classes now hit a different, new deadlock (still OPEN)

Reproduced entirely on the Azure host (`20.83.144.174`, back up after a
reboot), worktree `/data/data/cratonvm-apiversionstrategy-20260713`, dev tip
`79773121` (fast-forwarded from this session's own `819ab93e` build tip —
diffed the intervening commits; none touch annotation/reflection/CGLIB/
monitor code, so the findings below hold at both tips), binary
`cratonvm-apiversionstrategy.bin`.

**The originally-documented bug no longer reproduces.** Three independent,
increasingly faithful repros against the real `spring-webflux`/`spring-context`/
`spring-beans` classes on this dev tip all **succeed**, matching HotSpot:
1. A minimal `@Configuration` class with a `@Bean` method returning `null`
   (typed `org.jspecify.annotations.Nullable`) consumed by a second `@Bean`
   method's `@Qualifier(...) @Nullable` parameter — succeeds, `thing=null`.
2. The same shape but with the `@Bean` methods declared on a **non-`@Configuration`
   superclass** inherited by a `@Configuration(proxyBeanMethods = false)`
   subclass (mirroring `WebFluxConfigurationSupport` → `DelegatingWebFluxConfiguration`
   exactly, including a 2-parameter method where only the second parameter is
   `@Nullable`) — succeeds.
3. The literal real classes: `@Configuration @EnableWebFlux static class WebConfig {}`
   (byte-for-byte what both failing test classes use), fetching
   `RequestMappingHandlerMapping` and `RouterFunctionMapping` (both consumers
   of the possibly-null `webFluxApiVersionStrategy` bean) from a real
   `AnnotationConfigApplicationContext` — succeeds, both beans created, no
   exception.

Traced the actual mechanism while building these repros (for the next
session, in case of a regression): `AbstractAutowireCapableBeanFactory`/
`ConstructorResolver` resolve `@Bean` factory-method candidates via
`ClassUtils.getUserClass(factoryClass)` (`ConstructorResolver.java:456`),
which unwraps a CGLIB-enhanced `@Configuration` proxy back to the **original**
class before reflecting for parameter annotations — so CratonVM's native
CGLIB-enhancer shim (`native-builtins/src/cglib_enhancer.rs`, which
deliberately leaves `@Bean` methods **with parameters** un-overridden per its
own doc comment) is never in the annotation-resolution path here regardless.
The actual gate is `DependencyDescriptor.isRequired()` →
`Nullness.forMethodParameter()` → `Parameter.getAnnotatedType()`, which reads
the real classfile's `RuntimeVisibleTypeAnnotations` — the exact API fixed by
the 2026-07-01 JSpecify TYPE_USE reflection work
(`docs/internal/spring/SC-jspecify-nullness-reflection.md`). That fix is what
resolved this bug, just never reconfirmed against these 2 specific classes
until now.

**New finding: both classes now deadlock instead of throwing.** Reran both
through the real `suite-run.sh` harness exactly as specified
(`BATCH=1 BATCH_TO=600 ONE_TO=600 CRATONVM_DEFAULT_HEAP_MAX_MB=2048`): both
hit the full double-timeout window (batch attempt + individual retry, 600s
each) with `found=0/succ=0/fail=0` — no `BeanCreationException`, no crash.
HotSpot baseline for `CrossOriginAnnotationIntegrationTests` (same argfile,
real `java`): **68/68 pass in 3.6s total**. A dedicated single, longer
(`timeout 1500`, `CRATONVM_DBG_HANG_SAMPLE=1`) attempt at the same class
showed genuine forward progress at first — 9 distinct embedded-server
(Tomcat/Jetty/Reactor-Netty/Undertow) start/stop cycles across the class's
68 parameterized sub-tests over about 8 minutes, sampled method mix
continuously varying (`ConcurrentReferenceHashMap`, `AttributeMethods`,
`AnnotationTypeMappings`, `MergedAnnotation`, `AntPathMatcher` — Spring's
annotation-introspection and handler-mapping machinery, not a tight loop) —
but then **stopped making any progress at all**: the process's CPU time
stopped advancing entirely (confirmed via repeated `ps -o time` samples 20s
apart, byte-for-byte identical), i.e. a genuine hang, not just extreme
slowness.

**Root-caused via a live `gdb` thread dump** (`sudo gdb -p <pid> -batch -ex
'thread apply all bt'`, all 13 threads): the single thread actually running
Java (`main-vm`) is blocked in
`vm/src/threading/monitor.rs`'s `MonitorTable::enter` → `entry_condvar.wait`,
reached via `monitor_enter` (`vm/src/vm/vm_exec.rs:4671`) called from
`sem_release_n_inner` (`native-builtins/src/lib.rs:59824`, backing
`java.util.concurrent.Semaphore.release()`), itself reached from deep inside
a `Stream.forEach`/`ArrayList.forEach`/`Optional.ifPresent` lambda chain
(WebFlux/Reactor internals). **Every other one of the 13 threads is
independently idle** — 8 Jetty `QueuedThreadPool` workers and 1 Reactor
`boundedElastic` thread parked in `LockSupport.park`
(`native_lock_support_park_nanos`) waiting for work, the JIT background
compiler thread waiting on its `CompilationQueue` condvar, the JDK
`Common-Cleaner` thread in a periodic `Thread.sleep` — **none hold any Java
monitor**. Per `MonitorTable::enter`'s own logic
(`vm/src/threading/monitor.rs:437-471`), the blocking thread only reaches the
`wait()` loop when `state.owner` is `Some(other_thread_id)`; since no live
thread holds it, `owner` must be a **stale entry left by a thread that has
since exited without a matching `monitor_exit`** (a lock leak), not a live
deadlock cycle between two running threads.

**Leading, unconfirmed hypothesis** (not fixed this session — budget spent
confirming the original bug's resolution and root-causing this new one):
an unbalanced native `monitor_enter`/`monitor_exit` pair somewhere in the
WebFlux/Reactor/Jetty call path this test exercises — most likely an early
`return`/`?`-propagated error or panic-unwind between the two calls in some
native shim (the project's own memory flags exactly this class of bug:
"if-let mutex guard held across blocking else branch" — `audit \`if let .*
.lock()\` in natives`), or a Rust-level analogue in one of the natives on
this call path (`sem_release_n_inner`/`sem_acquire_blocking` themselves look
correctly balanced on inspection — every path pairs `monitor_enter` with
`monitor_exit` before returning — so the leak is more likely in a *different*
native that also happens to lock the same Java object, or in whatever
Reactor/Jetty code takes a `synchronized` block on it). Because the object
is freshly allocated per test iteration (a new `AnnotationConfigApplicationContext`
+ new bean graph every one of the 68 sub-tests), this is also consistent with
this project's well-documented "recycled object address/id" bug family (G1
`pointer_map` recycled-destination ambiguity, the stale-`ObjectRef` static
sweep) — a GC'd object's monitor-table entry surviving into a same-address
freshly-allocated `Semaphore` (or whatever object backs this particular
monitor) would produce exactly this symptom: permanently "owned" by a
thread_id that no longer maps to any live thread.

**Not fixed.** This is a different, unrelated bug from what this session was
asked to investigate (explicitly out of scope: hangs) — flagging it because
it is the actual, current blocker for both `CrossOriginAnnotationIntegrationTests`
and `RequestMappingMessageConversionIntegrationTests` (which share the same
`@EnableWebFlux`/`AbstractHttpHandlerIntegrationTests` harness and so very
plausibly hit the identical deadlock). Next step for a future session:
reproduce with `CRATONVM_DBG_MONENTER=1` (gates the wait-site-snapshot
diagnostic already wired into `monitor_enter`, see
`vm/src/threading/monitor.rs`'s `mon_enter_dump_enabled`) to capture the
*leaking* thread's frame at the moment it last held this monitor, or audit
every native that calls `ctx.monitor_enter`/`monitor_exit` on a `Semaphore`-
or `Reactor`-adjacent object for an unbalanced early-return path.

**Status of the 2 classes this session was assigned**: both `BeanCreationException`
occurrences are gone (verified 3 ways above); both classes still fail to
complete (TIMEOUT, confirmed via the real `suite-run.sh` harness at
`BATCH_TO=600/ONE_TO=600`, and via a dedicated 1500s single-attempt probe)
due to this newly-found, unrelated deadlock. No code fix landed this session
— nothing needed fixing for the assigned bug, and the newly-found deadlock is
a substantial, separate investigation of its own.

## 2026-07-14 follow-up — Semaphore deadlock FIXED and verified; a second, unrelated bean-header bug found and fixed; full end-to-end verification currently blocked by a THIRD, unrelated, severe regression

Continuing in the same worktree/branch (`/data/data/cratonvm-apiversionstrategy-20260713`,
`fix/apiversionstrategy-cluster-20260713`), at the user's explicit request to
push the `Semaphore.release()` deadlock from the section above through to a
real fix.

### 1. `Semaphore.release()` STW-barrier deadlock — FIXED and verified

Commit `b6fffebf` (merged to `origin/dev` at `9ca83d62`). All 5 native
Semaphore call sites (`sem_acquire_blocking`, `sem_release_n_inner`,
`sem_try_acquire_inner`, `native_sem_try_acquire_timeout`,
`native_sem_drain_permits`, all in `native-builtins/src/lib.rs`) switched
from a raw `ctx.monitor_enter(this)` to `ctx.monitor_enter_gc_safe(this)` —
the exact same fix shape as `native_cdl_await`'s `CountDownLatch` fix
(commit `51a508e1`, and independently, `6d332a1e`/`87fbb718` on a parallel
branch that merged in around the same time — same root cause, same
narrow opt-in mechanism, no duplicate-fix conflict since both landed the
identical `NativeContext::monitor_enter_gc_safe` API). A raw `monitor_enter`
never marks the thread GC-blocked, so a contended wait stays counted in the
STW cross-thread JIT-takeover barrier's `expected` set forever, deadlocking
against an owner that is itself GC-blocked waiting on that same pause.

**Verified two ways**:
- `SemCorrectness.java` (single-thread permit accounting, cross-thread
  release/acquire handoff, 16-thread contended mutex doing 8000 atomic
  increments) passes identically to HotSpot post-fix — confirms the fix
  didn't introduce a race or correctness regression.
- Both target classes, which previously TIMEOUT-ed (burning the full
  double-timeout window, `found=0/succ=0/fail=0`), now run to completion
  through the real `suite-run.sh` harness in a **single attempt**:
  `CrossOriginAnnotationIntegrationTests` 68/68 found+executed in 318-372s
  (was TIMEOUT), `RequestMappingMessageConversionIntegrationTests` 160/160
  found+executed in ~1328s (was TIMEOUT). The deadlock itself is
  conclusively gone.

### 2. New finding underneath the deadlock: `Objects.toString(Object[, String])` never virtually dispatched — root-caused and FIXED

With the deadlock gone, both classes now complete but still show 0% pass —
every parameterized sub-test on Jetty/Jetty Core/Tomcat fails with
`HttpClientErrorException$BadRequest: 400` / "Bad HostPort" / `the
character [@] is never valid in a domain name` (Reactor Netty fails
separately with `IllegalStateException: failed to create a child event
loop` — not investigated, looks unrelated).

Root-caused via a minimal, fast (~1s) non-Spring repro
(`HttpClient5HostRepro.java`: a bare `com.sun.net.httpserver.HttpServer` +
one request through real Apache HttpComponents5, capturing the actual
`Host` header the server receives): the client sends the literal text
`org.apache.hc.core5.net.URIAuthority@<hex>` as the `Host` header instead
of `host:port`. Decompiled the real httpclient5-5.6/httpcore5-5.4.2
bytecode (`javap -p -c`, not guessed): `RequestTargetHost.process()`
(httpcore5) builds the header via `httpRequest.addHeader("Host",
authority)` — the `(String, Object)` overload, with a `URIAuthority`
passed as a raw `Object` — and `BasicHeader`'s constructor stores the
value via `java.util.Objects.toString(value, null)`. Both `RestClient`'s
auto-detected default backend and the explicitly-configured
`HttpComponentsClientHttpRequestFactory` route through this same
httpclient5 code, so **both** target classes hit it identically even
though only one explicitly configures HttpComponents.

Isolated the exact broken native: `native_objects_to_string`
(`native-builtins/src/lib.rs`, backing `Objects.toString(Object)` and
`Objects.toString(Object, String)`) built the identity-hash form
(`ClassName@hex`) directly from `identity_hash_code` for every non-String
object, **without ever invoking the object's own overridden `toString()`**.
Confirmed this was the sole outlier — not a general toString/concat
regression — via a targeted repro (`URIAuthorityRepro.java`): a directly
constructed `URIAuthority.create("localhost:8080")`'s `.toString()`,
implicit string concat, `String.valueOf(Object)`, and
`StringBuilder.append(Object)` **all already produced `"localhost:8080"`
correctly** on this dev tip (the general "Object.toString uses identity
hash not virtual dispatch" bug class was already fixed, commit `d7116160`)
— but `Objects.toString(auth)` and `Objects.toString(auth, "default")`
both still returned the identity form. `Objects.hashCode(Object)` was
independently confirmed already-correct (its own fallback arm already
calls `ctx.invoke_virtual(obj, "hashCode", ...)`) — this was an isolated
regression in the `toString` sibling only, no other siblings found on
inspection of the two other `format!("{}@{:x}", ...)` call sites in the
tree (both are legitimate last-resort fallbacks that already attempt
`invoke_virtual(..., "toString", ...)` first).

**Fix** (commit `7b1d6ff3`, local to the branch, not yet pushed — see
"blocked" note below): `native_objects_to_string` now dispatches through
`invoke_to_string` (`native-builtins/src/lang_string.rs`'s existing helper,
the same one `String.valueOf(Object)` already used correctly) instead of
hand-rolling the identity string.

**Verified** via the isolated repros only (`URIAuthorityRepro.java`,
`HttpClient5HostRepro.java` — both now produce `"localhost:8080"` /
correct captured Host header). **NOT yet verified end-to-end** against the
two Spring classes — blocked by finding #3 below, discovered while trying
to do exactly that.

### 3. BLOCKING: a third, severe, unrelated regression — `InternalError: null property: java.home` from early `Locale` use

While rebuilding to run the full suite against fix #2, hit a NEW crash:
any real-JDK-mode program that touches `java.util.Locale` early in its
execution (confirmed with a bare `"X".toLowerCase(Locale.ROOT)`
one-liner, `LocaleRepro.java`) now throws
`java.lang.InternalError: null property: java.home` from
`Locale.<clinit>` → `Locale.createConstant` → `BaseLocale.<clinit>` →
`StaticProperty.<clinit>` → `StaticProperty.getProperty("java.home")`.
This is **not** a corner case: it fires inside the real `KRun` Spring test
harness too — both target classes now `LOADERR` in ~5-8ms (before any
test even loads) when run through `suite-run.sh` with a binary built past
this regression.

**Confirmed 100% unrelated to both fixes above**, via a clean, isolated
A/B rebuild at the *exact same commit* (`9ca83d62`) with and without the
`Objects.toString` fix (`git stash` / `git stash pop`): the crash
reproduces identically either way. It is a genuine, pre-existing
regression already on `origin/dev`, independent of anything from this
session.

**Bisected with certainty** (parallel incremental rebuilds — much faster
than a fresh-worktree bisection since an already-built worktree's
`target/` cache makes each step ~4 min instead of ~20+ — in two worktrees
simultaneously to halve the number of rounds) to commit **`d8092acb`**
("fix-tests-real-jdk-contracts"): its parent `c6216cb2` is clean (`LocaleRepro`
passes), `d8092acb` itself crashes, tested and reconfirmed multiple times
each. Range searched: `5dc7b6f8..9ca83d62` (42 commits).

**Leading, unconfirmed hypothesis for the next session** (not fixed —
out of scope for this session's assigned task, and a from-scratch trace
of the actual registration/drop pipeline is needed before touching code):
`d8092acb`'s diff adds `native_methods.set_drop_synthetic_stubs(true)` to
the real-JDK-mode bootstrap paths in `vm/src/vm/vm_init.rs` ("Do not let
approximations shadow the real JDK bytecode"). There is a suspicious
PRE-EXISTING duplicate registration for
`jdk/internal/misc/VM.getSavedProperty(String)String` — the native
`StaticProperty`'s real bytecode calls internally to resolve properties
like `java.home`: one in `native-builtins/src/phases_early.rs` (~line 2622,
an always-null stub tagged `NativeKind::Bridge`) and the correct one in
`native-builtins/src/lib.rs` (~line 31927,
`lang_system::native_vm_get_saved_property`, which reads the real property
store). This project's own history flags "duplicate native registrations
— verify which wins (last-writer-wins)" as a recurring footgun class.
Whether `set_drop_synthetic_stubs` is what flips which of these two
registrations survives (and why the survivor now returns null for
`java.home` when it didn't before `d8092acb`) was **not traced this
session** — flagged as the concrete next step, not re-guessed.

**Status**: `Objects.toString` fix (#2) is committed locally
(`7b1d6ff3`) but **withheld from push** pending this investigation, purely
out of caution about pushing on top of a host that may be mid-bisection
by a follow-up session — the fix itself is independently correct and
low-risk (confirmed via isolated repro, touches one native function, no
interaction with the java.home bug found). The `Semaphore` fix (#1) is
already pushed and unaffected. A dedicated follow-up task has been filed
for finding #3 given its severity (plausibly breaks a large fraction of
the suite silently, since touching `Locale` early is extremely common).

**Unrelated operational note**: while bisecting, hit a `cc-rs`/SQLite
build failure caused by the Azure host's root filesystem (`/`, NOT
`/data`) being at 100% capacity (`/tmp` had ~70MB free of 29G). Worked
around by setting `TMPDIR=/data/data/<subdir>` for cargo builds; did not
attempt to clean up `/home/victor`'s large shared caches
(`.gradle`/`.rustup`/`.m2`/`.cargo`, ~10GB) or other sessions' preserved
binaries, all of which are shared across concurrent sessions on this host
and unsafe to delete unilaterally. This may be silently affecting other
concurrent sessions' builds too.

## 2026-07-14 follow-up #2 — java.home regression ROOT-CAUSED and FIXED (urgent, host-wide impact); a second instance of the same bug family also fixed; two more distinct issues surfaced, not yet investigated

Escalated to highest priority: the finding #3 regression from the section
above (`InternalError: null property: java.home`, bisected to commit
`d8092acb`) was confirmed independently reproducing on `origin/dev` and
flagged as plausibly affecting **every other concurrent session on this
host currently exercising real-JDK mode** — any of them touching
`java.util.Locale` early would hit this crash silently.

### Root cause, confirmed precisely

The original hypothesis (a duplicate/conflicting `VM.getSavedProperty`
registration) was **investigated and refuted** — tracing the actual
registration order showed the broken `phases_early.rs` stub for that
method is only reachable via `register_synthetic_overrides`, which real-JDK
mode never calls; the correct `lib.rs` implementation (tagged `Bridge`) is
the only one ever registered there.

Root-caused instead via a new permanent, gated diagnostic
(`CRATONVM_DBG_DROPPED_STUBS=1`, `native-api/src/registry.rs`, ~3 lines,
zero cost when unset) that lists every registration
`set_drop_synthetic_stubs` silently drops. Running it against the bare
`LocaleRepro.java` one-liner immediately showed the actual mechanism:
**every native backing `java.util.Properties`** (`getProperty`, `get`,
`put`, `size`, `keySet`, `forEach`, `putAll`, ~24 methods total) was being
dropped. `System.getProperties()` (`native-builtins/src/lib.rs`)
deliberately hands back a "lightweight synthetic Properties object" whose
inherited `Hashtable`/`ConcurrentHashMap` backing is **never populated** —
real JDK 25 `Properties`/`Hashtable` bytecode dereferences a `map` field
that stays permanently null on this object, so these overrides are the
*only* thing that makes it behave like a `Map` at all (this is explicitly
documented in the existing code comment right above the
`System.getProperties()` registration). `register_properties_sidetable`
(`native-builtins/src/properties_sidetable.rs`) registered its entire
~24-method surface with no explicit category of its own, inheriting
whatever was ambient at each of its ~3 call sites — `Bridge` in some,
`SyntheticStub` in real-JDK mode's own call sites in `vm/src/vm/vm_init.rs`.
Once `d8092acb` made `set_drop_synthetic_stubs(true)` actually take effect
in real-JDK mode, the entire synthetic `Properties` object lost every
override that made it functional — including the `getProperty` lookup
`jdk.internal.util.StaticProperty`'s bootstrap path depends on for
`"java.home"`.

**Fix** (commit `f62d2073`, pushed): wrap `register_properties_sidetable`'s
whole body in `registry.with_category(NativeKind::Bridge, |registry| {
...})`, so its registrations no longer depend on the caller's ambient
category. Does **not** touch or revert `d8092acb`'s actual intended
behavior (`set_drop_synthetic_stubs` itself stays in effect for genuine
synthetic approximations, e.g. `ByteBuffer.allocate`/`allocateDirect` and
`CodingErrorAction`, confirmed still correctly falling through to real
bytecode — see spot-check below) or any of `d8092acb`'s 8 other,
unrelated fixes (JIT safety guard, EdDSA key factory, security-policy
escape handling, etc.).

**Verified**:
- `LocaleRepro.java` (`"X".toLowerCase(Locale.ROOT)`): now prints
  `RESULT lower=[localhost]` / `OVERALL SUCCESS` instead of crashing.
- `SpotCheck.java`: `ByteBuffer.allocate`/`allocateDirect` (confirming
  `d8092acb`'s own fix is untouched), a plain `new Properties()` instance,
  and both `System.getProperties().getProperty("java.home")` and
  `System.getProperty("java.home")` all match HotSpot.
- `SemCorrectness.java`, `URIAuthorityRepro.java`, `HttpClient5HostRepro.java`
  (this session's earlier fixes): all still pass — no interaction between
  the three fixes. `HttpClient5HostRepro` now captures the **correct** Host
  header (`localhost:<port>`) end-to-end through real Apache httpclient5,
  confirming the `Objects.toString` fix's real-world effect is intact.
- Real suite runner: `CrossOriginAnnotationIntegrationTests` no longer
  shows the `java.home` `LOADERR` (was crashing in ~5-8ms before any test
  loaded); it now runs all 68 sub-tests to completion in ~213s (down from
  ~372s pre-fix — the java.home/Properties bug was itself adding overhead
  throughout, not just at startup).

### Second instance of the same bug family, found and fixed

With `java.home` fixed, both target classes progressed further but then
failed **every** sub-test on **all 4 backends** uniformly with
`java.lang.NullPointerException: Cannot enter synchronized block because
"this.lock" is null`.

Same root-cause shape: `register_concurrent_natives`'s
`CopyOnWriteArrayList` registration block (`native-builtins/src/lib.rs`,
~line 57046 — `add`, `set`, `remove`, `clear`, `addIfAbsent`, etc.) also
inherited its ambient category instead of declaring one, and was also
`SyntheticStub` in real-JDK mode (confirmed via
`CRATONVM_DBG_DROPPED_STUBS` during a live
`CrossOriginAnnotationIntegrationTests` run). `CopyOnWriteArrayList.
<init>()V` is deliberately left unregistered so real bytecode constructs
`this.lock` correctly (a prior, already-correct fix, per the comment
already in this file) — but real JDK's own `add`/`set`/`remove`/`clear`
bytecode does `getfield lock; monitorenter` (confirmed via `javap -p -c`
against the real class), and these mutator natives exist specifically to
bypass that bytecode, not approximate it. Once dropped, every mutating
call on a real `CopyOnWriteArrayList` (used constantly by Spring's own
`BeanPostProcessor`/listener lists during `ApplicationContext` refresh —
both target classes refresh a context 68 and 160 times respectively) fell
through to bytecode requiring a lock object.

**Fix** (commit `68c44f62`, pushed): same `with_category(Bridge)` wrap,
scoped to just the COWAL registration block (the other classes registered
earlier in the same function — `ReentrantLock`, `Condition`, etc. — were
left untouched; no evidence found that they share the problem, and the
fix should stay as narrow as the evidence supports).

**Verified**: rerunning `CrossOriginAnnotationIntegrationTests` shows the
`"this.lock is null"` NPE is completely gone from all 4 backends.

### Two (at least) further, distinct, NOT-yet-investigated issues remain

With both fixes applied, `CrossOriginAnnotationIntegrationTests` still
fails all 68 sub-tests (258s), now with a **different FAILCAUSE per
backend** (no longer uniform — a good sign that each backend now fails on
its own, unrelated issue rather than one shared blocker):

| Backend | FAILCAUSE |
|---|---|
| Jetty | `NoClassDefFoundError: org/eclipse/jetty/http/MimeTypes$Mutable` (+ `ExceptionInInitializerError`) |
| Jetty Core | `NoClassDefFoundError: org/eclipse/jetty/http/MimeTypes$Mutable` |
| Reactor Netty | `IllegalStateException: failed to create a child event loop` (unchanged since the original 2026-07-13 investigation — see the section above; not caused by anything fixed today) |
| Tomcat | `IllegalStateException: org.apache.catalina.LifecycleException: Failed to initialize component [StandardServer[-1]]` |

None of these were investigated this session — flagging them here rather
than guessing. Given the `NoClassDefFoundError`/`LifecycleException`
shapes, it is plausible (not confirmed) that at least one of these is
*also* a `SyntheticStub`-drop casualty of the same `d8092acb` family
(worth checking with `CRATONVM_DBG_DROPPED_STUBS` first before assuming a
new, unrelated bug) — but this was not verified. `RequestMappingMessageConversionIntegrationTests`
was not rerun against the COWAL fix specifically (only against the
java.home fix alone, where it showed the same `"this.lock is null"` NPE
as `CrossOriginAnnotationIntegrationTests`, so the COWAL fix should apply
equally there, but this was not directly reconfirmed).

### Status

The two bugs this session was explicitly, urgently asked to confirm and
fix (the `java.home`/`Locale` regression and its root cause) are **fixed,
verified, and pushed** (`f62d2073`). A closely related second instance
(`CopyOnWriteArrayList`) was also found, fixed, and pushed (`68c44f62`).
**Both target classes still do not fully pass** — each of the 4
parameterized backends now fails on what looks like its own distinct,
unrelated issue, none investigated yet. This is a legitimately deep stack
of independent pre-existing bugs being uncovered one layer at a time as
each blocker is cleared, consistent with the pattern already documented
elsewhere in this doc (e.g. the AOT hang cluster, the original
`ApiVersionStrategy` investigation itself). Recommend a fresh, dedicated
session per remaining backend-specific failure rather than continuing
serially in this one.

**Unrelated but urgent operational note**: the Azure host's root
filesystem (`/`, distinct from `/data`) is now at **100% capacity, 0
bytes free** (was ~70MB free earlier this session) — `scp`/`cc`/anything
that writes to `/tmp` will fail outright for any concurrent session on
this host until this is addressed. Did not attempt to clean up
`/home/victor`'s large shared caches or other sessions' preserved
binaries (unsafe to delete unilaterally without coordination) — flagging
for whoever has host-level access/context to free space or move `/tmp` to
the `/data` volume (177G+ free there throughout this session).

## Bucket 1 — Genuinely hung (12/25)

Hit the full 1500s ceiling on **both** the batch attempt and the individual
retry — `found=0/succ=0/fail=0`, no output at all, no FAILCAUSE, no crash log
entry. These are real hangs, not slow tests:

- `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` — reconfirmed hung 2026-07-13, see below
- `beans.factory.aot.BeanDefinitionMethodGeneratorTests` — reconfirmed hung 2026-07-13, see below
- `beans.factory.aot.BeanRegistrationsAotContributionTests` — reconfirmed hung 2026-07-13, see below
- `cache.jcache.JCacheEhCacheAnnotationTests` — **no longer hangs** as of 2026-07-13 (later session), passes cleanly (67/68, 1 pre-existing `@Disabled`); see the dedicated section below
- `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests` — **no longer hangs** as of 2026-07-13, now completes with a distinct residual (`VerifyError` + AOT codegen limitation), see below
- `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` — **no longer hangs** as of 2026-07-13 (later session), passes cleanly (12/12); see the dedicated section below
- `context.annotation.ConfigurationClassPostProcessorAotContributionTests` — reconfirmed hung 2026-07-13, see below
- `context.annotation.InitDestroyMethodLifecycleTests` — **no longer hangs** as of 2026-07-13 (later session), passes cleanly (11/11, including its 2 AOT/`TestCompiler` methods); see the dedicated section below
- `context.aot.ApplicationContextAotGeneratorTests` — reconfirmed hung 2026-07-13, see below
- `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` — **no longer hangs** as of 2026-07-13, now completes with a distinct residual (Mockito self-attach, already tracked elsewhere), see below
- `test.context.aot.TestContextAotGeneratorIntegrationTests` — reconfirmed hung 2026-07-13, see below
- `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` — **no longer hangs** as of 2026-07-13 (later session), passes cleanly (10/10) but slowly (~7 minutes, ~45x HotSpot); see the dedicated section below

9 of these 12 are AOT bean-registration/code-generation classes (same cluster
flagged in the `-125` doc's "AOT bean-registration TIMEOUT cluster"). See
["2026-07-13 local investigation"](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open)
above: 8 of these 9 were retested (all but `test.context.junit.jupiter.
parallel.ParallelExecutionSpringExtensionTests`), confirmed to share one
root cause with the Bucket 2 `CompilationException` classes below, and the
hang itself remains OPEN (one narrower, unrelated bug was found and fixed
along the way).

## 2026-07-13 (later session) — 4 non-AOT Bucket-1 classes: all 4 no longer hang, no code change needed

Assigned scope: the 4 Bucket-1 classes that are neither part of the AOT
bean-registration cluster above nor `ImportSelectorTests` — `cache.jcache.
JCacheEhCacheAnnotationTests`, `context.annotation.
ComponentScanParserBeanDefinitionDefaultsTests`, `context.annotation.
InitDestroyMethodLifecycleTests`, and `test.context.junit.jupiter.parallel.
ParallelExecutionSpringExtensionTests`. These four don't share an obvious
naming pattern and were investigated as four independent hypotheses.

**Setup**: Azure host `20.83.144.174`, fresh worktree
`/data/data/wt-standalone-hangs-20260713` (`git worktree add` off
`origin/dev`, fetched fresh at session start — tip `819ab93e`), `apps/`
copied from `wt-osr-other516-20260708-2131` with all 25
`build/cratonvm-testcp.txt` files' stale absolute paths rewritten to point at
the new worktree (the exact trap this doc's "Methodology note" above warns
about — caught and fixed before running anything). `cargo build --release`
clean build (~4.5 min), binary copied to `cratonvm-standalone-hangs.bin`.
Same methodology as the original investigation: `suite-run.sh`,
`BATCH=1 BATCH_TO=1500 ONE_TO=1500 CRATONVM_DEFAULT_HEAP_MAX_MB=2048`.

**Result: none of the 4 reproduce as hangs any more.** Each was run in
isolation first, then all 4 together in one consolidated rerun as a second,
independent confirmation — both rounds agree closely (times in seconds,
well under the 1500s ceiling both times):

| Class | Run 1 | Run 2 (confirm) | Result |
|---|--:|--:|---|
| `context.annotation.InitDestroyMethodLifecycleTests` | 51s (11/11) | 48s (11/11) | OK, all 11 tests incl. the 2 `TestCompiler`/AOT ones |
| `cache.jcache.JCacheEhCacheAnnotationTests` | 229s (67/68) | 215s (67/68) | OK, 67 succeed / 1 pre-existing `@Disabled` / 0 fail |
| `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests` | 251s (12/12) | 236s (12/12) | OK, all pass |
| `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` | 485s (10/10) | 416s (10/10) | OK, all pass — slow (~7 min) but not hung |

No Rust code changes were made or needed for any of the 4 — `git status` in
the worktree confirmed zero tracked-file diffs. All four appear to be
incidental beneficiaries of *other* unrelated fixes that landed on `dev`
between whenever the original 2026-07-11 hang data was gathered and this
session's fetch (`819ab93e`), the same pattern already seen above for
`CommonAnnotationBeanRegistrationAotContributionTests` and
`PersistenceAnnotationBeanPostProcessorAotContributionTests`. Per-class
detail:

- **`JCacheEhCacheAnnotationTests`**: a live gdb backtrace (`sudo gdb -p
  <pid> -batch -ex 'thread apply all bt'`) taken ~90s into the run caught
  the main-vm thread inside `gc_prune_dead_collection_overlays` /
  `remove_overlay_owner_key` (`native-collections/src/lib.rs`) during a
  routine `maybe_gc()` pass — the collection-overlay side-table pruning
  mechanism. `git log origin/dev --oneline` shows commit `acbea991`
  ("fix(gc): propagate collection overlays from live owners", merged via
  `75f11d95`) as an ancestor of this session's build tip. That fix (a
  different session's work, already on `dev` before this session started)
  is the most likely explanation: the test no longer needed the ~50+ minute
  double-timeout window the original 2026-07-11 data recorded, and instead
  completes normally in well under 4 minutes.
- **`ComponentScanParserBeanDefinitionDefaultsTests`**: its two XML fixtures
  (`defaultWithNoOverridesTests.xml`, `defaultLazyInitTrueTests.xml`) both
  contain a real `<context:component-scan base-package="org.springframework.
  context.annotation" .../>`, so this class does real classpath/directory
  scanning via Spring's `ClassPathScanningCandidateComponentProvider`
  (contrary to what its 12 individually-simple test bodies would suggest).
  That scanning path depends on `Files.walkFileTree`/`BasicFileAttributes`
  correctness for directory traversal — exactly the mechanism fixed for an
  unrelated reason in commit `7ae137e4` ("Files.walkFileTree visitor
  callbacks get a real BasicFileAttributes", landed 2026-07-13, also an
  ancestor of this session's build tip), which specifically called out that
  the previous zero-field placeholder made `isDirectory()` return a raw,
  wrong `Value::Object(None)` for a `()Z`-descriptor method. A directory
  walker silently getting `isDirectory()` wrong is exactly the kind of bug
  that could make a classpath scan do drastically more (or repeated/
  incorrect) work. Plausible root cause, not proven by a before/after diff
  (the "before" binary wasn't rebuilt to confirm) — flagged as the leading
  hypothesis rather than a certainty.
- **`InitDestroyMethodLifecycleTests`**: only 2 of its 11 test methods use
  the in-memory-javac `TestCompiler`/AOT pipeline (the same machinery as the
  still-OPEN 9-class AOT hang cluster documented above); the other 9 are
  plain bean-factory/lifecycle tests with no AOT involvement. A priori this
  looked likely to inherit the AOT cluster's still-open hang. It did not:
  the whole class, including both AOT methods, completes in well under a
  minute. The likely explanation is scale, not a different mechanism — this
  class's AOT-generated surface is a single small bean
  (`CustomAnnotatedPrivateSameNameInitDestroyBean`/
  `SubPackagePrivateInitDestroyBean`) compiled against `spring-context`'s
  own test classpath, not the ~48-jar classpath (`kotlin-stdlib`,
  `kotlin-reflect`, `groovy`, `mockito`, `reactor`, ...) that the AOT
  cluster's own doc section above identifies as the likely disproportionate-
  cost trigger. Consistent with, not contradicting, that cluster's "still
  OPEN" status — this class's AOT workload was just never large enough to
  hit it.
- **`ParallelExecutionSpringExtensionTests`**: flagged going in as the class
  most likely to expose a CratonVM-specific JUnit-parallel/`ForkJoinPool`
  gap. It is genuinely slow — ~7–8 minutes for 10 outer `@RepeatedTest`
  iterations × 1000 inner `@RepeatedTest` sub-tests
  (`Constants.PARALLEL_EXECUTION_ENABLED_PROPERTY_NAME=true`,
  `PARALLEL_CONFIG_DYNAMIC_FACTOR_PROPERTY_NAME=10`,
  `PARALLEL_CONFIG_EXECUTOR_SERVICE_PROPERTY_NAME=WORKER_THREAD_POOL`) — but
  it is not hung; it completes and passes on **3 independent runs**
  (485s, 416s, 481s), matching the ~513s figure from the prior `2ba4aae9`
  ("Fix Spring JUnit parallel residual") investigation on 2026-07-08 almost
  exactly.

  **Root cause of the slowdown, confirmed via 5 sequential live gdb
  snapshots** (`thread apply all bt`, ~2–3s apart) during the 3rd run: real
  OS worker threads genuinely exist and are created per JUnit's own naming
  convention (`junit-1-worker-`, `junit-2-worker-`, ...), but **only one is
  ever actively executing bytecode at any given snapshot** — the others
  (including the `main-vm` orchestrator thread, consistently parked in
  `native_lock_support_park`/`LockSupport.park()` across all 5 snapshots)
  sit idle. The active worker's own OS thread identity changed between
  snapshots (`junit-1-worker-` LWP 334411 in snapshot 1 was gone by
  snapshot 2, replaced by a new `junit-2-worker-` LWP 335897 that stayed
  active through snapshot 5) — i.e. exactly one thread does all the work at
  a time, and a fresh thread periodically takes over, rather than N threads
  genuinely running concurrently. This matches source: `java/util/concurrent/
  ForkJoinTask`'s `fork()`/`join()`/`invoke()`/`get()` are globally
  overridden in `native-builtins/src/phases_early.rs` (~line 7976) as a
  **lazy, single-thread synchronous emulation** — `fork()` is a no-op that
  just returns `this` (the task is never actually handed to another worker
  or queued), and `join()`/`invoke()`/`get()` all run `compute()`
  synchronously on the calling thread if not already done (comment in that
  file: "Eager fork was overflowing the host stack on deeply-recursive
  RecursiveTask probes" — a known, intentional trade-off from earlier work,
  not something newly discovered here). JUnit's `ForkJoinPoolHierarchical
  TestExecutorService` uses exactly this `RecursiveAction`-based fork/join
  pattern for its parallel test executor (`ExclusiveTask`), so under this
  emulation the "parallel" executor's recursive test-tree fan-out collapses
  to ordinary sequential recursion on whichever single thread happens to be
  driving it at the time — explaining the ~45x-vs-HotSpot slowdown (no
  parallelism speedup despite the 10x dynamic worker-count factor) without
  any deadlock or correctness break for this specific test's usage pattern.
  Separately, `native-builtins/src/phases_late.rs` (~line 70837) gives
  `ForkJoinPool.commonPool()`/`asyncCommonPool()` a synthetic proxy, but a
  custom `new ForkJoinPool(...)` (as JUnit's `WORKER_THREAD_POOL` config
  uses) has no dedicated native fast path and runs as ordinary interpreted
  bytecode over CratonVM's thread primitives.

  `git log --all --oneline --grep=ForkJoin -i` and `--grep=parallel -i` were
  also searched per the task brief's suggestion; the existing ForkJoin-
  adjacent fixes on `dev` (`ae574d8f`/`c9da1f68`/`ebc4bb85` "gcstress
  residual forkjoin fix", `743da7b1`/`ce258204` "Phaser/ForkJoinPool hang"
  fix) address narrower, different mechanisms (GC-stress root stability and
  a `CompletableFuture.runAsync` exception-swallowing hang, respectively),
  not general worker-pool throughput or the fork/join synchronous-emulation
  behavior described above.

  This class's ~45x slowdown vs. HotSpot is therefore a real,
  well-understood (if still unresolved) performance gap — the fork/join
  synchronous-emulation design already in the codebase, not a new bug — but
  at current dev tip it finishes inside the 1500s ceiling with a comfortable
  margin (~3x on all 3 runs), so it is reclassified out of Bucket 1 rather
  than treated as an open hang. A future session wanting genuine JUnit
  parallel-test speedup under CratonVM would need to make `ForkJoinTask.
  fork()` actually dispatch to other pool worker threads instead of the
  current no-op-fork/synchronous-join emulation — a larger undertaking
  (the original eager-fork approach was reverted for stack-overflow reasons
  on deep `RecursiveTask` recursion, so a real fix likely needs an explicit
  work queue rather than reverting that change) that is out of scope here.
  If a future session sees this class exceed 1500s, suspect either host
  contention (this is a shared, busy machine) or an actual regression, and
  re-open.

**Why the original 2026-07-11 data showed `found=0/succ=0/fail=0` at the
full ceiling for all 4**: not established with certainty for any of the
four. The likely explanation for 3 of the 4 (`JCacheEhCacheAnnotationTests`,
`ComponentScanParserBeanDefinitionDefaultsTests`,
`InitDestroyMethodLifecycleTests` — all of which now finish in under 5
minutes, nowhere near the 1500s ceiling even loaded) is a genuine bug fixed
by later, unrelated `dev` work (`acbea991`/`7ae137e4` are the leading
candidates, per-class above). `ParallelExecutionSpringExtensionTests` is the
closest call — its ~7 minute runtime combined with a busier/more-contended
host at the time of the original 25-class batch run could plausibly have
pushed it over 1500s without any code-level hang at all; it may never have
been a genuine infinite hang, just a very slow test caught by a shared,
loaded host.

## Bucket 2 — Slow but completes (10 unresolved)

Real result landed well under 1500s (or right at the boundary for one). Not
hangs — but 10/12 are near-total failures, so the slowness itself may be part
of the same underlying bug (e.g. retry/backoff before ultimately failing)
rather than a coincidence:

| Class | Status | Elapsed | Pass/Total | First FAILCAUSE |
|---|---|--:|--:|---|
| `orm.jpa.support.InjectionCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 206s | 3/10 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL -> **TIMEOUT as of 2026-07-14** | 169s -> 600s+ (2x) | 0/16 -> 0/0 | `ServletException`/`UnsatisfiedDependencyException` (no `MessageHandler` bean) -> **bean/startup bug no longer reproduces**; 2026-07-16: NOT a real hang; ROOT-CAUSED (second follow-up) to the server dispatching the client's single STOMP CONNECT frame more than once, tripping Spring's own "Session already exists" guard -> STOMP ERROR + close (both Jetty+Tomcat) -- NOT an HTTP keep-alive/must-close issue as the first follow-up guessed. See the 2026-07-16 second follow-up section |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | FAIL → **TIMEOUT as of 2026-07-13** | 492s → 600s×2 (+1500s dedicated probe) | 0/68 → 0/0 | `BeanCreationException`: no `ApiVersionStrategy` bean → **bean bug fixed**, now deadlocks in `Semaphore.release()`'s monitor instead, see [2026-07-13 update #5](#2026-07-13-local-investigation-5--missing-apiversionstrategy-bean-resolved-both-classes-now-hit-a-different-new-deadlock-still-open) |
| `web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests` | **FIXED 2026-07-14** | 445s -> 149s | 211/241 -> **241/241** | Two native bugs, both fixed (`cd90774e`, `72a9ad40`): `PrintWriter.write(String)` bypassed subclass `write(String,int,int)` overrides (broke Spring test fixture auto-flush); `Matcher.group(int)` assumed cached text was always `java.lang.String`, threw spurious `NoSuchMethodError` on a general `CharSequence` (e.g. `AntPathMatcher`'s `MaxAttemptsCharSequence`) |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 693s | 0/47 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 730s | 4/26 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL, root-caused 2026-07-13, reconfirmed unchanged 2026-07-14 (still OPEN) | 763s (25s on the 2026-07-14 isolated rerun) | 3/5 | `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;` in `ConfigurationClassParser$SourceClass.getAnnotationAttributes` — see dedicated section below |
| `web.service.registry.GroupsMetadataValueDelegateTests` | **ABEND FIXED 2026-07-14** (`9bca11f5`); now FAIL on a new, distinct residual (still OPEN) | 1039s (25s combined w/ above on the 2026-07-14 rerun) | 0/8 | was fatal VM error `class file error: class not found: .../GroupsMetadata__TestCode` (FIXED); now `IllegalStateException: WritableContent did not append any content` — see dedicated section below |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | FAIL → **TIMEOUT as of 2026-07-13** | 1132s → 600s×2 | 0/160 → 0/0 | `BeanCreationException`: no `ApiVersionStrategy` bean (same as `CrossOriginAnnotationIntegrationTests`) → **bean bug fixed**, now TIMEOUTs the same way, see [2026-07-13 update #5](#2026-07-13-local-investigation-5--missing-apiversionstrategy-bean-resolved-both-classes-now-hit-a-different-new-deadlock-still-open) |
| `context.annotation.ImportSelectorTests` | **FIXED (verified 2026-07-16)** | 47s | **9/9 OK** | Was `StackOverflowError` (Mockito `spy()` recursion), then a GC heap-corruption crash on 2 "nested group" sub-tests (2026-07-16); the corruption was closed by unrelated concurrent commit `fb15be63` — rebuild+rerun confirms all 9 methods pass cleanly, 0 corruption-signature lines — see the 2026-07-16 joint verification addendum in the dedicated section below |

Notable sub-clusters within this bucket (candidates for shared root cause):

- **In-memory javac `CompilationException`** (3 classes: `InjectionCodeGeneratorTests`,
  `BeanDefinitionPropertiesCodeGeneratorTests`, `InstanceSupplierCodeGeneratorTests`)
  — same AOT-codegen compilation machinery as the TIMEOUT cluster above and
  the (now-fixed) `core.test.tools.CompiledTests`/`TestCompilerTests`. **Update
  2026-07-13**: confirmed to be the SAME root cause, not just related — after
  the 2026-07-13 `TestCompiler`/`JavacFileManager.list` GC-safety fix landed
  on `dev`, all three now hang identically to the Bucket 1 AOT classes
  instead of failing fast (the old truncated-listing bug was making them fail
  fast on a bogus "cannot find symbol"; now that's fixed, they just hang like
  the rest). See the
  [2026-07-13 local investigation](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open)
  section above — still OPEN.
- **`web.service.registry.*` residuals** (2 classes: `ImportHttpServiceRegistrarTests`,
  `GroupsMetadataValueDelegateTests`) — both root-caused to the same general
  area (`@CompileWithForkedClassLoader`'s custom-ClassLoader machinery
  interacting with Spring's AOT/test-compiler pipeline), but with two
  DIFFERENT specific defects. `GroupsMetadataValueDelegateTests`'s fatal
  VM-abort defect (a loader-blind, name-based class re-resolution ambiguity
  in reflective static-method `Method.invoke()`) is **FIXED** (`9bca11f5`),
  though the class still doesn't fully pass (new, distinct, unrelated
  `WritableContent` residual). `ImportHttpServiceRegistrarTests`'s
  `ClassCastException` remains OPEN. See the dedicated section below.
- **Missing `ApiVersionStrategy` bean** (2 classes: `CrossOriginAnnotationIntegrationTests`,
  `RequestMappingMessageConversionIntegrationTests`) — **resolved as of
  2026-07-13**: the `BeanCreationException` no longer reproduces (confirmed
  3 ways against current dev). Both classes now TIMEOUT instead, due to an
  unrelated, newly-found deadlock in `Semaphore.release()`'s internal
  monitor — see the
  [2026-07-13 update #5](#2026-07-13-local-investigation-5--missing-apiversionstrategy-bean-resolved-both-classes-now-hit-a-different-new-deadlock-still-open)
  section above. Still OPEN, but for a different reason than originally
  documented.
- `ImportSelectorTests`'s `StackOverflowError` is unrelated to the above
  clusters. **Root-caused 2026-07-13** (see the dedicated section below):
  it is a Mockito `spy()` cross-class-hierarchy real-method recursion, not
  Spring `ImportSelector`/`ConfigurationClassParser` recursion as originally
  guessed — reproduces standalone with no Spring context involved at all.

## 2026-07-14 local investigation — StompWebSocketIntegrationTests: one real bug fixed, class still doesn't pass (a different, deeper hang) — OPEN

Worktree `cratonvm-stompws-20260713` on the Azure host, branch
`fix/stompws-cluster-20260713`, merged to `origin/dev` (commit `0bb89ebf`,
plus a doc-only follow-up).

**What was fixed and verified real:** `SocketChannel.read`/`write`/`accept`
(plain blocking mode) and `AsynchronousSocketChannel`'s underlying
blocking-recv-faked-as-async read/write natives
(`native-builtins/src/phases_late.rs`) had **zero `begin_blocking_region`/
`end_blocking_region` bracket** around the genuinely-blocking OS `recv()`/
`send()`/`accept()` syscall. A thread parked in one of these can never reach
a JIT-takeover safepoint on its own, so a concurrent STW pause that expects
every mutator to cooperate waits forever (`pending=1 taken=0`) — this is the
same "STW cross-thread JIT takeover" bug class documented in
[`../../known-issues/tomcat/stw-crossthread-jit-takeover-hang-cluster.md`](tomcat-08-07/stw-crossthread-jit-takeover-hang-cluster.md),
applied here to a different subsystem (raw socket I/O rather than locks/
`IoFuture`). `origin/dev` already had an independent, concurrently-landed
fix for the *async*-channel half of this (same root cause, found via a
different investigation) but without ObjectRef-relocation tracking across
the blocking window; the merge kept this session's more complete
`end_blocking_region_refs`-based version. This fix is real, confirmed via
live gdb (thread genuinely parked in `tcp_read`/`recv()` with no
`begin_blocking_region` before the fix), and is independently valuable
(protects any future test that hits these exact native call sites during a
concurrent GC pause) even though — see below — it doesn't make this specific
class pass.

**Why the class still doesn't pass:** the original `ServletException`/
`UnsatisfiedDependencyException` ("no `MessageHandler` bean") failure no
longer reproduces at all — likely fixed as an incidental side effect of
other AOT/annotation-processing work that landed on `dev` this week, the
same pattern seen with the `ApiVersionStrategy` bean cluster below. With
that gone, the test gets much further: it actually opens a STOMP connection
and starts exchanging messages, then hits a **genuine, different hang** —
confirmed via a live gdb `thread apply all bt` on a stuck 2026-07-14 rerun
(binary `cratonvm-stompws-final.bin`, both a 600s batch attempt and the
600s individual crash-recovery retry timed out identically):

- `main-vm` (the test's own thread) is parked in a plain
  `LockSupport.park()` (`native_lock_support_park`), reached via a
  reflective `Method.invoke` call chain — consistent with a test-framework
  timeout/await helper (e.g. a `CountDownLatch.await(timeout)` or
  `CompletableFuture.get(timeout)` wrapper) waiting on a result that never
  arrives.
- A `SimpleAsyncTaskExecutor`-spawned thread is correctly parked inside
  `tcp_read()`/`recv()` — **with the STW-cooperation fix above already
  covering this exact call site** (`phases_late.rs` line ~38221, the
  `AsynchronousSocketChannel` async-channel read path) — genuinely blocked
  waiting for incoming socket data that never arrives, not spinning or
  deadlocked at the VM level.

This is **not** the STW-takeover bug: `begin_blocking_region` is correctly
in effect (verified by inspecting the frame — the fix from this session is
active on the exact code path caught mid-hang), so a concurrent STW pause
would NOT wait on this thread. The test is stuck because **no STOMP message
ever arrives** on that socket — a functional gap somewhere in message
routing/broker delivery, not a VM-level concurrency bug. Root cause not yet
found; needs tracing on the server (broker) side to see whether it's
sending the expected frame at all, or a client-side subscription/session
bug. Left as the open item for a future session.

## 2026-07-16 follow-up — StompWebSocketIntegrationTests: NOT a hang, NOT a VM concurrency bug; root-caused to a premature server-side `SocketChannel.close()` right after the WS handshake — OPEN, precise next step identified

Worktree `wt-stompws-20260716-141512` on the Azure host, branch
`fix/stompws-msgdelivery-20260716-141512`, synced to `origin/dev`. Task: pick
up the 2026-07-14 investigation above (last known state: "no STOMP message
ever arrives ... needs tracing on the server (broker) side").

**Reproduced fresh on current `dev`.** Running the full class still times out
(`timeout 120` on `KRun` never emits a `RESULT` line, log grows to ~1.8M
lines/120s). But that turned out to be a **red herring about the nature of
the problem**, not evidence of a true infinite hang — see below.

### The 2026-07-14 hypothesis ("no message ever arrives, needs server tracing") is now resolved

Added `System.err` tracing directly into a scratch copy of the test class
(client `afterConnectionEstablished`/`handleTextMessage`, server
`SimpleController.handle()`) and ran the two `sendMessageToController`
parameterizations **in isolation** (the other 7 `@ParameterizedWebSocketTest`
methods commented out, so there's no ambiguity about which sub-test produced
which trace line). Result: the class does **not** hang forever — it finishes
in ~30s with a normal `RESULT ... status=FAIL`, both parameterizations
(`server=Jetty` and `server=Tomcat`, both with the `Standard` — i.e. Tomcat's
own JSR-356 — client) failing the same
`assertThat(...latch.await(10, SECONDS)).isTrue()` assertion. Trace evidence
for both parameterizations:

```
[STOMPTRACE] client: afterConnectionEstablished, sending msg0=CONNECT...
WARN [org.apache.tomcat.websocket.WsRemoteEndpointImplClient] Write to the
  remote endpoint failed. ... (ExecutionException: java.io.IOException:
  write failed: Broken pipe (os error 32))
```

The client's **very first write after the handshake** — the STOMP `CONNECT`
frame — fails with a genuine OS-level `EPIPE`. This is CratonVM's own real
(non-synthetic) `AsynchronousSocketChannel` Future-write path
(`native-io/src/async_socket.rs::aio_asc_write_future` → a worker-pool
`fd_table().tcp_write()` on a real `TcpStream`, confirmed by the
`"write failed: {e}"` message format — that exact `Display` formatting,
`(os error N)`, is CratonVM's own Rust `std::io::Error` formatting, not
anything Tomcat prints itself, so the underlying `send()` really did receive
`EPIPE`). Tomcat's own client-side `blockingSendTimeout` (default 20000ms) is
what eventually surfaces the failure as a JUnit assertion failure rather than
a true hang — that 20s-per-sub-test-invocation, multiplied across all 16
parameterizations (8 methods × 2 servers) in the full class, is what pushes
the **class total past the suite's 600s per-class ceiling** and gets it
bucketed as TIMEOUT rather than FAIL. **This fully explains the "TIMEOUT"
symptom without any infinite loop or VM-level concurrency bug** — the
2026-07-14 gdb snapshot that found `main-vm` parked in plain
`native_lock_support_park()` was simply catching the process mid-run at a
point where a CountDownLatch/Future wait happened to be live, not evidence of
an unbounded park; `parkNanos`/`parkUntil`'s timeout plumbing
(`native-builtins/src/lib.rs` ~50432-50570) was independently re-audited this
session and is correct.

### EPIPE root cause narrowed to the SERVER side, with millisecond-precision evidence

Added a temporary-but-kept diagnostic hook,
`CRATONVM_DBG_SC_CLOSE=1` (`native-io/src/socket_channel.rs::sc_close`,
commit `dd1cddec` — see its doc comment), that traces every real
`java.nio.channels.SocketChannel.close()` with the local/peer address and a
wall-clock timestamp. Correlated against a millisecond-stamped
`System.currentTimeMillis()` STOMPTRACE line at the exact moment the client's
`afterConnectionEstablished` fires:

```
[STOMPTRACE] t=1784224734832 client: afterConnectionEstablished, sending msg0=CONNECT...
[SC_CLOSE]   t=1784224734869 id=0x60000001 local=127.0.0.1:46311 peer=127.0.0.1:51988   (Δ = 37ms, Jetty variant)
...
[STOMPTRACE] t=1784224749055 client: afterConnectionEstablished, sending msg0=CONNECT...
[SC_CLOSE]   t=1784224750835 id=0x60000003 local=127.0.0.1:42617 peer=127.0.0.1:47334   (Δ = 1.78s, Tomcat variant)
```

`SocketChannel.close()` is the native that `org.apache.tomcat.util.net.
NioChannel`/Jetty's `SocketChannelEndPoint` call when the **servlet
container itself** decides a connection is done — it is not anything the
`AsynchronousSocketChannel`-based WS *client* transport touches (that's a
completely separate native module, `aio_asc_close`). So this is
unambiguously the **server** — both the Jetty-backed and Tomcat-backed
embedded test server — closing its just-accepted connection within
tens-of-milliseconds to ~2 seconds of completing the WebSocket upgrade
handshake, before the client's first post-handshake frame can be written.
This reproduces identically for both servlet containers, which points at
something generic (not container-specific) in how CratonVM's environment
interacts with the post-Upgrade connection hand-off — most likely each
container's own standard HTTP/1.1 keep-alive/"must-close" determination
(normally suppressed for a `101 Switching Protocols` hand-off) firing
because some signal the container relies on to recognize "this connection
was upgraded, don't apply normal end-of-request socket bookkeeping" isn't
correctly observed under CratonVM. No CratonVM-specific native code
intercepts the JDK/Servlet upgrade APIs at all (`grep -r "HttpUpgradeHandler
|UpgradeToken|isUpgrade"` across the tree: zero hits) — so bytecode-level
Tomcat/Jetty logic is running unmodified; the defect is in some lower-level
primitive it depends on (a `SocketChannel`/`SelectionKey` state, a response
header value CratonVM computes differently for the 101 response, or similar)
that this session did not narrow further.

**Not caused by the diagnostic itself**: re-ran the same repro against the
binary built *before* the `socket_channel.rs` change — identical EPIPE/close
timing. Also confirmed the diagnostic hook introduces no regression on an
unrelated, larger WS test class (`WebSocketConfigurationTests`, 4/4 OK).

**Next step for whoever picks this up**: run with `CRATONVM_DBG_SC_CLOSE=1`
and add a Java-side stack trace at the moment of `sc_close` (this session
looked for a `NativeContext::thread_stack_trace`-style hook but it requires a
`Thread` object handle that wasn't readily available from inside
`native-io`; either thread that through or capture the trace from the Java
side via a custom `Filter`/`HandshakeInterceptor` wrapping the
`OutputStream`) to get the exact Tomcat/Jetty call site issuing the `close()`
— that pinpoints whether it's a keep-alive/`Content-Length` determination, a
poller/selector re-registration gap, or something else. `WebSocketConfigurationTests`
and `WebSocketHandshakeTests` (both `extends AbstractWebSocketIntegrationTests`
but neither sends a message right after the handshake) are unaffected — this
narrows the trigger specifically to "the connection is written to
immediately after the handshake," not the handshake/upgrade machinery
itself (which already has substantial prior fix history: DF07,
`e6e96b1d`, `0bb89ebf`).

Not fixed this session — the exact Java-level trigger for the premature
`close()` needs one more round of tracing. `CRATONVM_DBG_SC_CLOSE=1` is
merged to `dev` (commit `dd1cddec`) as a zero-cost-when-unset diagnostic aid
for that next round.

## 2026-07-16 second follow-up — StompWebSocketIntegrationTests: ROOT-CAUSED to duplicate server-side dispatch of the client's single CONNECT frame; the "HTTP keep-alive/must-close" hypothesis above is REFUTED — OPEN, fix not yet safe to attempt

Worktree `wt-stompws-callsite-20260716-221418` on the Azure host, branch
`fix/stompws-callsite-20260716-221418`, synced to fresh `origin/dev`
(`5d564ca7`). Task: pick up the "next step" from the follow-up above (a
Java-side stack capture at the moment of `sc_close`) to find the exact
Tomcat/Jetty call site.

**Step 1 — added the stack capture, confirmed the finding still holds.**
`NativeContext::capture_stack_trace(0)` (defined on the `NativeContext` trait
itself, `native-api/src/registry.rs`) needs no `Thread` object handle at all —
the earlier sessions search for a `thread_stack_trace`-style hook was solving
the wrong problem; `capture_stack_trace` simply walks the CURRENT threads own
live Java call stack, which is exactly the thread executing `sc_close`'s
native body (the one that called `SocketChannel.close()`). Added inside the
existing `CRATONVM_DBG_SC_CLOSE` gate in `sc_close`
(`native-io/src/socket_channel.rs`), printing each frame innermost-first.
Purely additive and zero-cost/zero-behavior-change when the env var is unset
(regression-checked: `WebSocketConfigurationTests` 4/4 OK with the var unset,
matching the prior sessions baseline; `WebSocketHandshakeTests` showed 4/6
failures but ALL as `IOException: Blocking write timeout` from
`WsRemoteEndpointImplBase` — a pre-existing, unrelated failure mode, not
introduced by this change, confirmed by the code being entirely inside the
unset env-var gate so it cannot execute in that run at all).

**Step 2 — first stack capture (JIT on) initially suggested a totally
different, alarming hypothesis (an in-place `Frame.getOpCode()` corruption
turning a TEXT send into a CLOSE) — this was chased in detail and ultimately
superseded, kept here for anyone retracing the path:**

```
[SC_CLOSE_STACK] (innermost first)
  at org/eclipse/jetty/util/IO.close(IO.java:623)
  ...
  at org/eclipse/jetty/websocket/core/WebSocketCoreSession.abort(WebSocketCoreSession.java:561)
  at org/eclipse/jetty/websocket/core/WebSocketCoreSession.closeConnection(WebSocketCoreSession.java:229)
  at org/eclipse/jetty/websocket/core/WebSocketCoreSession.lambda$sendFrame$0(WebSocketCoreSession.java:516)
  at org/eclipse/jetty/util/Callback$4.succeeded(Callback.java:202)
  ... (WebSocketFlusher / IteratingCallback machinery) ...
  at org/eclipse/jetty/websocket/core/WebSocketCoreSession.sendFrame(WebSocketCoreSession.java:519)
  at org/eclipse/jetty/websocket/core/OutgoingFrames.sendFrame(OutgoingFrames.java:42)
  at org/springframework/web/socket/adapter/jetty/JettyWebSocketSession.sendTextMessage(JettyWebSocketSession.java:203)
  at org/springframework/web/socket/messaging/StompSubProtocolHandler.sendToClient(StompSubProtocolHandler.java:527)
  at org/springframework/web/socket/messaging/StompSubProtocolHandler.handleMessageToClient(StompSubProtocolHandler.java:514)
```

`javap -p -c -constants` against the real `jetty-websocket-core-common-12.1.10.jar`
(`WebSocketCoreSession.sendFrame(OutgoingEntry)` and
`WebSocketSessionState.onOutgoingFrame(Frame)`) confirmed the callback is
ONLY wrapped with an auto-close (`lambda$sendFrame$0` -> `closeConnection`)
when `frame.getOpCode() == 8` (WS CLOSE). `javap` against
`jetty-websocket-jetty-common-12.1.10.jar`s `WebSocketSession.sendText`
confirmed it always constructs a brand-new, unshared `new Frame((byte)1)`
(TEXT) with no pooling. This made it LOOK like a corrupted/misread opcode on
a genuine TEXT send. **This hypothesis is REFUTED by step 3 below** — it was
an artifact of catching one of (at least) two concurrent/racing close
attempts; the "real" trigger, reproduced consistently under `--nojit`, is
different (see next).

**Step 3 — reran under `--nojit`: identical SC_CLOSE count (2 per run), but
a completely different, unambiguous stack, present on BOTH backends:**

```
  at org/eclipse/jetty/websocket/core/WebSocketCoreSession.abort(...)
  at org/eclipse/jetty/websocket/core/WebSocketCoreSession.closeConnection(...)
  ... (legitimate synchronous WS CLOSE-frame send) ...
  at org/eclipse/jetty/websocket/common/WebSocketSession.close(WebSocketSession.java:163)
  at org/springframework/web/socket/adapter/jetty/JettyWebSocketSession.closeInternal(JettyWebSocketSession.java:223)
  at org/springframework/web/socket/adapter/AbstractWebSocketSession.close(AbstractWebSocketSession.java:142)
  at org/springframework/web/socket/handler/ConcurrentWebSocketSessionDecorator.close(...)
  at org/springframework/web/socket/messaging/StompSubProtocolHandler.sendErrorMessage(StompSubProtocolHandler.java:420)
  at org/springframework/web/socket/messaging/StompSubProtocolHandler.handleError(StompSubProtocolHandler.java:387)
  at org/springframework/web/socket/messaging/StompSubProtocolHandler.handleMessageFromClient(StompSubProtocolHandler.java:370)
  at org/springframework/web/socket/messaging/SubProtocolWebSocketHandler.handleMessage(...)
  at .../JettyWebSocketHandlerAdapter.onWebSocketText(...)
```

This is Spring's **own, real, unmodified, working-as-designed** error path:
`handleMessageFromClient`s inner `catch (Throwable ex) { ... handleError(session, ex, message); }`
-> (no custom `errorHandler` configured in this test) `sendErrorMessage`
-> sends a STOMP `ERROR` frame, then unconditionally `session.close(CloseStatus.PROTOCOL_ERROR)`
in a `finally` block. Identical under JIT and `--nojit` — **not a JIT
miscompile.**

**Step 4 — instrumented Spring's real `StompSubProtocolHandler.java` source
directly** (copied to `patch-src/`, temporary `System.getenv("CV_STOMP_TRACE")`-gated
`System.err.println`s only, no logic changes; compiled with `javac` against
the existing test classpath and prepended ahead of it — same technique as
the `ImportHttpServiceRegistrarTests` investigation). This immediately found
the real exception:

```
[CV_STOMP_TRACE] channel-send failure in session <id> command=CONNECT: java.lang.IllegalStateException: Session already exists
```

— i.e. Spring's `Assert.state(prevInfo == null, "Session already exists")`
inside `handleMessageFromClient`s `isConnect` branch
(`this.sessions.putIfAbsent(sessionId, info)` returned non-null). **This
assertion is genuine, correct, by-design Spring protocol-correctness
behavior for a duplicate CONNECT on the same session** — the defect is
upstream: something is delivering the client's single CONNECT frame to
`handleMessageFromClient` more than once.

**Step 5 — confirmed the duplicate delivery directly.** Added entry-point
tracing (`System.identityHashCode` of the `WebSocketMessage` and its
payload, plus `decoder.decode(byteBuffer)`s returned message count) to the
same patched source. Result, Jetty parameterization:

```
[CV_STOMP_TRACE] handleMessageFromClient ENTRY session=<id> msgIdentity=324636 payloadIdentity=324632 payload=CONNECT
[CV_STOMP_TRACE] decoded 1 message(s) for session <id>
[CV_STOMP_TRACE] handleMessageToClient session=<id> command=CONNECTED    <- normal CONNECTED reply sent
[CV_STOMP_TRACE] handleMessageFromClient ENTRY session=<id> msgIdentity=325447 payloadIdentity=325442 payload=CONNECT   <- SECOND, independently-constructed dispatch
[CV_STOMP_TRACE] decoded 1 message(s) for session <id>
[CV_STOMP_TRACE] channel-send failure ... IllegalStateException: Session already exists
```

The client-side trace (`[STOMPTRACE] ... sending msg0=CONNECT`) fires exactly
ONCE per session — client-side double-send is ruled out. The two server-side
`ENTRY` calls have DIFFERENT `msgIdentity` AND DIFFERENT `payloadIdentity` —
two genuinely separate `TextMessage`/`byte[]` objects, each independently
decoded to exactly 1 STOMP message (so it's not `BufferingStompDecoder`
returning duplicates from one buffer either) — meaning the WebSocket
transport layer itself handed Jetty's frame parser the CONNECT bytes twice.

**Correlated against physical socket reads via live `gdb`** (`break sc_read`
with a `commands` counter, `--batch -x` script, no rebuild needed since the
symbol `sc_read` — demangled — is present in the release binary): **exactly
2 `sc_read` calls occur, both BEFORE either `handleMessageFromClient ENTRY`.**
Only after both reads does the first dispatch fire (a clean decode+process
cycle ending in the CONNECTED reply), and only afterward does the SECOND
dispatch fire — with NO third `sc_read` call. This is most consistent with:
the two physical `SocketChannel.read()` calls each returned a full,
independent copy of the CONNECT frame's bytes (a duplicate-delivery at the
native read layer, not a re-parse of one already-consumed buffer at the
Java/Jetty level) — **not proven at the byte level this session** (would need
a byte-count/checksum dump inside `sc_read` itself, e.g. via a rebuild with a
targeted diagnostic, or a `gdb` script that reads the destination
`ByteBuffer`s backing memory at the breakpoint) but the strongest
remaining hypothesis given the evidence gathered.

**Confirmed on BOTH backends, ruling out a Jetty-only or Tomcat-only parser
bug as the sole explanation — same underlying trigger, different amplification
per container.** Rerunning with `-Djava.io.tmpdir=/data/tmp/<writable>` (works
around the pre-existing, unrelated `/tmp`-full-on-this-host issue that was
blocking the Tomcat parameterization entirely) showed the IDENTICAL
`ENTRY`x2 -> `Session already exists` pattern for the Jetty session. For the
Tomcat session, however, the SAME `payloadIdentity` (i.e. the literal SAME
cached message object, not a fresh duplicate) was redelivered to
`handleMessageFromClient` **4088 times** in one run before the connection was
finally torn down — a genuine, severe redelivery spin specific to Tomcat's
own connection-handling retry logic, presumably triggered by the SAME
duplicate-delivery/stuck-buffer-state condition but amplified very
differently by each container's own read loop.

**Not fixed this session.** The mechanism is precisely pinned at the
Java/Spring level (duplicate dispatch of one incoming WebSocket frame,
confirmed cross-backend, confirmed not a JIT bug, confirmed not caught by
`CRATONVM_DBG_STALE_OBJREF`), but the exact native call site responsible for
handing the SAME (or a duplicated copy of the) CONNECT frame bytes to each
container's frame parser twice was not pinned to a specific Rust source line
this session — doing so safely needs either (a) a targeted rebuild adding a
byte-count/checksum diagnostic directly inside `sc_read`
(`native-io/src/socket_channel.rs`) to prove/disprove "two physical reads
return identical bytes", or (b) deeper live-`gdb` inspection of the
destination `ByteBuffer`s backing memory at each `sc_read` breakpoint hit
without a rebuild. Given this codebases own history of confident-but-wrong
low-level I/O/GC fixes causing heap corruption (e.g. the `resolve_field_ref`
attempt documented in `CRATONVM-SPRING-GENUINE-BUGLIST.md`s `@Import`
attribute CCE entry), no fix was attempted without that byte-level
confirmation.

**Repro assets** (Azure host, persisted): worktree
`/data/data/wt-stompws-callsite-20260716-221418` (branch
`fix/stompws-callsite-20260716-221418`, contains only the `sc_close` stack-
capture diagnostic, committed); run directory `/data/tmp/stompws-callsite-run/`
— `MethodRun.java` (single-class JUnit5 launcher, copied from
`/data/tmp/aotfix-runs/MethodRun.java`), `patch-src/`/`patch-out/` (the
instrumented `StompSubProtocolHandler.java`, env-gated on `CV_STOMP_TRACE`,
compile with `javac -cp "$(cat /data/tmp/stompws-cp.txt)" -d patch-out
patch-src/.../StompSubProtocolHandler.java`, then prepend `patch-out` ahead
of the rest of the classpath). Classpath dump: `/data/tmp/stompws-cp.txt`
(single-module, from the earlier 2026-07-16 session, still valid). Binary:
`/data/tmp/cvm-stompws-callsite.bin` (release, includes the `sc_close` stack
capture; `CRATONVM_DBG_SC_CLOSE=1` to enable). `gdb` trace script:
`/data/tmp/trace_sc_read.gdb` (`break sc_read` + a hit counter, `gdb -batch -x
trace_sc_read.gdb --args <binary> ...`). Run logs:
`/data/tmp/stompws-callsite-run{1..7}*.log`,
`/data/tmp/stompws-callsite-gdb{1..4}*.log`.

**Next step for whoever picks this up**: add a byte-level diagnostic to
`sc_read` (`native-io/src/socket_channel.rs`) — print a short hash/hex-dump
prefix of the bytes actually written into the destination buffer on each
call, gated behind a new env var — and rerun the exact repro above (Jetty
parameterization is enough; it reproduces with just 2 reads, no need to
chase Tomcats redelivery-storm amplification first). If the two reads
return identical bytes, the bug is a genuine duplicate-delivery / non-
consuming-read defect somewhere between the OS `recv()` call and how
`sc_read` reports/advances what it delivered to the Java-level
`SocketChannel.read(ByteBuffer)` caller — compare against the already-fixed
sibling bug classes in this exact area (`ByteBuffer.mark/reset` aliasing,
`DirectByteBuffer.put` byte loss) for a plausible, already-understood
mechanism to check first. If the two reads return DIFFERENT bytes (i.e. the
duplication is NOT at the read layer), the investigation needs to move to
Jetty's/Tomcat's own frame-parser/fill-and-parse loop to see why each
container believes there are two independent, back-to-back CONNECT frames on
the wire when the client only wrote one.

## Bucket 3 — Immediate crash, not a hang (0/25 — FIXED 2026-07-13)

- `scripting.groovy.GroovyScriptFactoryTests` — was **ABEND**, `rc=139`
  (SIGSEGV), crashing during VM bootstrap warmup (`Post-clinit fixup` lines
  only, no test discovery output), `found=0`. **FIXED, commit `2724ea5b` on
  `dev`.**

  **Root cause**: a JIT codegen bug in the speculative-inlining rollback path
  (`try_emit_inline`, `jit/src/x64.rs`). When a speculative inline attempt for
  a callee bails partway through, the compiler already rewound the code
  buffer position, operand stack, oop-mark vector, and spill cursor — but did
  **not** roll back seven other deferred patch-list `Vec`s
  (`exception_check_stubs`, `deopt_stubs`, `forward_patches`,
  `jump_table_patches`, `self_call_patches`, `bounds_check_stubs`,
  `null_check_store_stubs`). Any bytecode instruction the abandoned inline
  attempt simulated (e.g. an inlined `invoke*` via
  `emit_post_invoke_exception_check`) could push a raw buffer offset onto one
  of those Vecs. That offset is only meaningful while it still points at the
  placeholder bytes live when it was recorded; after a bail the buffer is
  rewound and the fall-through normal-call path emits *different* code over
  that same range, but the stale offset survived and was blindly patched
  later — once, at the very end of `compile_bytecode`, over the FINAL,
  already-reused buffer — corrupting whatever real instruction now occupied
  that offset.

  Concretely, on `groovyjarjarasm.asm.Handler.getExceptionTableSize`
  (`return 2 + 8 * getExceptionTableLength(firstHandler)`, pulled in hot by
  Groovy's ASM-based class generation under `GroovyScriptFactoryTests`, and
  JIT-compiled very early in bootstrap) a stale `exception_check_stubs` entry
  from a rewound inline attempt got 4-byte-patched into the middle of the
  *kept* method's precise-maps safepoint-id store, replacing its bytecode-PC
  immediate with garbage and clobbering the REX.W prefix of the very next
  store instruction. Execution ran straight off the end of the mangled `mov`
  into undefined bytes that happened to decode as a wild memory-writing
  `ADD`, producing an immediate SIGSEGV the instant the (extremely hot)
  method next ran — well before JUnit test discovery even started, matching
  the observed `found=0` / `Post-clinit-fixup`-only crash signature exactly.
  Root-caused via a live gdb attach on the JIT-compiled method (mapped
  `r-xp` region with no symbol, located precisely via a `CRATONVM_DBG_JIT_NAMES`
  entry-address diagnostic added during the investigation) plus a targeted
  `CRATONVM_DBG_SPID` eprintln bisection confirming the safepoint-id store's
  operands were corrupted; `CRATONVM_NO_PRECISE_JIT_MAPS=1` (which skips the
  clobbered code path entirely) was the confirming A/B signal before the
  precise fix landed.

  **Fix**: snapshot the length of all seven deferred patch-list `Vec`s before
  a speculative inline attempt and `truncate()` them back on bail, mirroring
  the pre-existing buffer/stack/oop-mark/spill-cursor rollback.

  **Verified**: rebuilt from a clean worktree synced to the merged `dev` tip
  (`2724ea5b`) and reran the real class through `apps/spring-suite-runner/suite-run.sh`
  with the doc's exact recipe (`BATCH=1 BATCH_TO=1500 ONE_TO=1500
  CRATONVM_DEFAULT_HEAP_MAX_MB=2048`): `found=38 succ=21 fail=17`, `crashes.log`
  empty — no more SIGSEGV, full test discovery/execution now happens. The
  remaining 17 failures are pre-existing, unrelated Spring/Groovy functional
  issues (not crashes), out of scope for this ticket.

## Raw data

- Merged results: 8 shards, `suite-run.sh`, `BATCH=1 BATCH_TO=1500 ONE_TO=1500`,
  `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`.
- Full per-class FAILCAUSE and crash-log detail pulled from
  `/data/tmp/hang25-s{0..7}/{failcauses,crashes}.log` on the Azure host.
