# Spring TIMEOUT cluster — 1500s diagnostic rerun (hung vs. slow)

| | |
|---|---|
| **Status** | OPEN (12 genuinely hung, 10 slow-but-failing, 0 crash — 1 FIXED; 2 non-residual items removed). **2026-07-13 update**: 8 Bucket-1 + 3 Bucket-2 classes reconfirmed locally — one narrower bug fixed (`7ae137e4`), the hang itself still OPEN; see the 2026-07-13 section below. **2026-07-13 update #2**: `context.annotation.ImportSelectorTests`'s `StackOverflowError` root-caused — it is a Mockito `spy()` cross-hierarchy recursion, **unrelated to Spring's `ImportSelector` mechanism** (the original hypothesis below was wrong); still OPEN, see its own section. **2026-07-13 update #3**: both `web.service.registry.*` residuals (`ImportHttpServiceRegistrarTests`, `GroupsMetadataValueDelegateTests`) root-caused to `@CompileWithForkedClassLoader`'s custom-ClassLoader machinery interacting with Spring's AOT/test-compiler pipeline — two distinct defects, neither fixed; still OPEN, see dedicated section. **2026-07-13 update #4**: the 4 non-AOT, non-`ImportSelectorTests` Bucket-1 classes (`cache.jcache.JCacheEhCacheAnnotationTests`, `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests`, `context.annotation.InitDestroyMethodLifecycleTests`, `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests`) **no longer hang** — reconfirmed clean on 2 independent runs each against a freshly-built `origin/dev` tip; see the dedicated section below. No new code was needed — all 4 were incidental beneficiaries of other unrelated fixes already on `dev`. **2026-07-13 update #5**: the "missing `ApiVersionStrategy` bean" `BeanCreationException` (2 classes: `CrossOriginAnnotationIntegrationTests`, `RequestMappingMessageConversionIntegrationTests`) **no longer reproduces** — confirmed fixed (likely a side effect of earlier JSpecify/reflection work), but both classes now fail a different way instead: a genuine **deadlock in `Semaphore.release()`'s internal monitor**, confirmed via a live `gdb` thread dump. Still OPEN, new root cause, see dedicated section. **2026-07-13 update #6**: Bucket 3's `scripting.groovy.GroovyScriptFactoryTests` SIGSEGV **FIXED** (`2724ea5b`, pushed to `dev`) — root cause was a JIT codegen bug (stale deferred patch-list offsets surviving a rewound speculative-inline attempt, corrupting a later safepoint-id store in a hot, frequently-recompiled method); see dedicated section below. |
| **Discovered** | 2026-07-11, following up on the 25 classes that hit TIMEOUT in the
125-class scoped rerun (dev `9948295e`, standard 120s timeout — see
[`CRATONVM-SPRING-GENUINE-BUGLIST-125.md`](../internal/CRATONVM-SPRING-GENUINE-BUGLIST-125.md)). |

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

## 2026-07-13 local investigation — `web.service.registry.*` residuals (both root-caused, neither fixed — still OPEN)

Reproduced entirely locally, worktree
`cratonvm-wt-webserviceregistry-local-20260713`, dev tip `edca766e` (merged
forward from `dbf7827c`), binary `cratonvm-websvcreg-local.exe`. Diagnostics
added this session (commit `b34679e5`, kept in place): `CRATONVM_IAE_TRACE2`
(per-element resolved-value dump in `create_annotation_proxy`) and a
widened `CRATONVM_ANN_TRACE` gate covering `Import`/`ImportHttpServices`.

### `ImportHttpServiceRegistrarTests` — `ClassCastException`, root-caused, not fixed

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

### `GroupsMetadataValueDelegateTests` — fatal `class not found`, root-caused, not fixed

**Confirmed still ABEND**, identical symptom to the pre-existing entry:
`[cratonvm] main-vm run() returned Err: Error in thread "main" class file
error: class not found: org/springframework/web/service/registry/GroupsMetadata__TestCode`.
This is a **hard, uncatchable VM-level fatal error** (not a normal Java
`ClassNotFoundException` that JUnit could report as a test failure) — the
whole process aborts (`rc=1`), hence `ABEND` rather than `FAIL`.

**Confirmed NOT fixed by the 2026-07-13 `TestCompiler`/`JavacFileManager.list`
GC-safety fix** (commit `10831b7b`/`a02322e4`, already in this session's
history) — despite that fix targeting the exact same in-memory-`javac` +
`DynamicClassLoader` pipeline this test also uses, and despite having
fixed a 7-class cluster with a similarly-shaped symptom. Re-verified on a
binary built from dev tip `edca766e` (well after that fix landed): byte-
for-byte identical crash, same class name, same message.

**Leading, unconfirmed hypothesis, narrowed via code reading (not yet
empirically instrumented — each attempt costs ~22 minutes via the real
suite runner, on top of the ~20-30 minute rebuild)**:
`GroupsMetadataValueDelegateTests` is itself `@CompileWithForkedClassLoader`
at the class level. `DynamicClassLoader`'s constructor
(`org.springframework.core.test.tools.DynamicClassLoader`) special-cases
exactly this situation: when its `parent` is a
`CompileWithForkedClassLoaderClassLoader`, it does NOT define freshly-
compiled classes (like `GroupsMetadata__TestCode`) on itself — it
reflectively invokes the parent's package-private `defineDynamicClass(name,
bytes, off, len)`, which calls `super.defineClass(...)` — i.e. the new class
is defined on the PARENT forked loader, not on the `DynamicClassLoader`
instance. Later, `Compiled.getInstance(Object.class, generatedClass.getName()
.reflectionName())` calls `this.classLoader.loadClass(className)` where
`this.classLoader` IS the `DynamicClassLoader` (the child), relying on
ordinary `ClassLoader.loadClass` parent-delegation to find the class on the
parent that actually defined it. This is structurally the same "does a
user-defined loader correctly report/find a class it (or a linked sibling)
defined" shape as the already-fixed `SC-custom-classloader-ignored.md` bug
family, but for a NEW specific pattern (reflectively-invoked
`defineClass` on a DIFFERENT loader instance than the one later asked to
`loadClass` it) that doesn't appear to be covered by that fix. The fact that
the failure is a hard VM-level "class file error" rather than a normal,
catchable `ClassNotFoundException` additionally suggests the actual failing
resolution may not even be going through the Java-level
`loadClass`/`findClass` override machinery at all, but some lower-level
internal symbol resolution CratonVM performs directly against its global
class table during bytecode execution (e.g. resolving a constant-pool
reference to `GroupsMetadata__TestCode` from inside
`ReflectionUtils.findMethod`/`Method.invoke` in
`Compiled`/`getGeneratedCodeReturnValue`) — this needs direct confirmation.

**Not fixed.** Next step: a targeted, minimal repro of exactly this
pattern (a `TestCompiler.forSystem()` compile under a real, minimal
`@CompileWithForkedClassLoader`-shaped two-loader setup, generating one
throwaway class and loading it back via the child `DynamicClassLoader`)
would let this be iterated in seconds rather than the ~22-minute real-suite
cost — this session ran out of budget before building that narrower repro
for this specific class (the effort instead went toward the
`ImportHttpServiceRegistrarTests` repro above, which shares the
`@CompileWithForkedClassLoader` machinery but fails at a different point).

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
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL | 169s | 0/16 | `ServletException` / `UnsatisfiedDependencyException` (no `MessageHandler` bean) |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | FAIL → **TIMEOUT as of 2026-07-13** | 492s → 600s×2 (+1500s dedicated probe) | 0/68 → 0/0 | `BeanCreationException`: no `ApiVersionStrategy` bean → **bean bug fixed**, now deadlocks in `Semaphore.release()`'s monitor instead, see [2026-07-13 update #5](#2026-07-13-local-investigation-5--missing-apiversionstrategy-bean-resolved-both-classes-now-hit-a-different-new-deadlock-still-open) |
| `web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests` | **FIXED 2026-07-14** | 445s -> 149s | 211/241 -> **241/241** | Two native bugs, both fixed (`cd90774e`, `72a9ad40`): `PrintWriter.write(String)` bypassed subclass `write(String,int,int)` overrides (broke Spring test fixture auto-flush); `Matcher.group(int)` assumed cached text was always `java.lang.String`, threw spurious `NoSuchMethodError` on a general `CharSequence` (e.g. `AntPathMatcher`'s `MaxAttemptsCharSequence`) |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 693s | 0/47 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 730s | 4/26 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL, root-caused 2026-07-13 (still OPEN) | 763s (10s on the 2026-07-13 isolated rerun) | 3/5 | `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;` in `ConfigurationClassParser$SourceClass.getAnnotationAttributes` — see dedicated section below |
| `web.service.registry.GroupsMetadataValueDelegateTests` | ABEND, root-caused 2026-07-13 (still OPEN) | 1039s (1306s on the 2026-07-13 rerun) | 0/8 | fatal VM error `class file error: class not found: .../GroupsMetadata__TestCode` — see dedicated section below |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | FAIL → **TIMEOUT as of 2026-07-13** | 1132s → 600s×2 | 0/160 → 0/0 | `BeanCreationException`: no `ApiVersionStrategy` bean (same as `CrossOriginAnnotationIntegrationTests`) → **bean bug fixed**, now TIMEOUTs the same way, see [2026-07-13 update #5](#2026-07-13-local-investigation-5--missing-apiversionstrategy-bean-resolved-both-classes-now-hit-a-different-new-deadlock-still-open) |
| `context.annotation.ImportSelectorTests` | FAIL, root-caused 2026-07-13 (still OPEN) | 1456s (734s on the 2026-07-13 rebuild) | 4/9 | `StackOverflowError` — Mockito `spy()` recursion, not Spring; see dedicated section below |

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
  `GroupsMetadataValueDelegateTests`) — both now root-caused to the same
  general area (`@CompileWithForkedClassLoader`'s custom-ClassLoader
  machinery interacting with Spring's AOT/test-compiler pipeline), but with
  two DIFFERENT specific defects. Neither fixed. See the dedicated section
  below.
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
