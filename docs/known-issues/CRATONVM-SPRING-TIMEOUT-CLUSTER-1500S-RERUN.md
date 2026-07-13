# Spring TIMEOUT cluster — 1500s diagnostic rerun (hung vs. slow)

| | |
|---|---|
| **Status** | OPEN (12 genuinely hung, 10 slow-but-failing, 1 crash; 2 non-residual items removed). **2026-07-13 update**: 8 Bucket-1 + 3 Bucket-2 classes reconfirmed locally — one narrower bug fixed (`7ae137e4`), the hang itself still OPEN; see the 2026-07-13 section below. |
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

## Bucket 1 — Genuinely hung (12/25)

Hit the full 1500s ceiling on **both** the batch attempt and the individual
retry — `found=0/succ=0/fail=0`, no output at all, no FAILCAUSE, no crash log
entry. These are real hangs, not slow tests:

- `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` — reconfirmed hung 2026-07-13, see below
- `beans.factory.aot.BeanDefinitionMethodGeneratorTests` — reconfirmed hung 2026-07-13, see below
- `beans.factory.aot.BeanRegistrationsAotContributionTests` — reconfirmed hung 2026-07-13, see below
- `cache.jcache.JCacheEhCacheAnnotationTests`
- `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests` — **no longer hangs** as of 2026-07-13, now completes with a distinct residual (`VerifyError` + AOT codegen limitation), see below
- `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests`
- `context.annotation.ConfigurationClassPostProcessorAotContributionTests` — reconfirmed hung 2026-07-13, see below
- `context.annotation.InitDestroyMethodLifecycleTests`
- `context.aot.ApplicationContextAotGeneratorTests` — reconfirmed hung 2026-07-13, see below
- `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests` — **no longer hangs** as of 2026-07-13, now completes with a distinct residual (Mockito self-attach, already tracked elsewhere), see below
- `test.context.aot.TestContextAotGeneratorIntegrationTests` — reconfirmed hung 2026-07-13, see below
- `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests`

9 of these 12 are AOT bean-registration/code-generation classes (same cluster
flagged in the `-125` doc's "AOT bean-registration TIMEOUT cluster"). See
["2026-07-13 local investigation"](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open)
above: 8 of these 9 were retested (all but `test.context.junit.jupiter.
parallel.ParallelExecutionSpringExtensionTests`), confirmed to share one
root cause with the Bucket 2 `CompilationException` classes below, and the
hang itself remains OPEN (one narrower, unrelated bug was found and fixed
along the way).

## Bucket 2 — Slow but completes (10 unresolved)

Real result landed well under 1500s (or right at the boundary for one). Not
hangs — but 10/12 are near-total failures, so the slowness itself may be part
of the same underlying bug (e.g. retry/backoff before ultimately failing)
rather than a coincidence:

| Class | Status | Elapsed | Pass/Total | First FAILCAUSE |
|---|---|--:|--:|---|
| `orm.jpa.support.InjectionCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 206s | 3/10 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL | 169s | 0/16 | `ServletException` / `UnsatisfiedDependencyException` (no `MessageHandler` bean) |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | FAIL | 492s | 0/68 | `BeanCreationException`: no `ApiVersionStrategy` bean |
| `web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests` | FAIL | 445s | 211/241 | `AssertionFailedError` (mostly passing — a real partial failure) |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 693s | 0/47 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | FAIL → **TIMEOUT as of 2026-07-13** | 730s | 4/26 | `CompilationException: Unable to compile source` → now hangs instead, see [2026-07-13 update](#2026-07-13-local-investigation--aot-bean-registration-hang-cluster--in-memory-javac-compilationexception-cluster-confirmed-to-share-one-root-cause-still-open) |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL | 763s | 3/5 | `ArrayIndexOutOfBoundsException` / `DiscoveryIssueException` |
| `web.service.registry.GroupsMetadataValueDelegateTests` | FAIL | 1039s | 1/8 | `ArrayIndexOutOfBoundsException` / `DiscoveryIssueException` |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | FAIL | 1132s | 0/160 | `BeanCreationException`: no `ApiVersionStrategy` bean (same as `CrossOriginAnnotationIntegrationTests`) |
| `context.annotation.ImportSelectorTests` | FAIL | 1456s | 4/9 | `StackOverflowError` |

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
  `GroupsMetadataValueDelegateTests`) — the original JUnit-discovery signature
  is no longer the common failure. An isolated rerun of
  `ImportHttpServiceRegistrarTests` now reaches Spring parsing and fails with
  `ClassCastException: java.lang.Class cannot be cast to [Ljava.lang.String;`
  from `ConfigurationClassParser$SourceClass.getAnnotationAttributes`.
  This points to incomplete `Class[]`-to-`String[]` annotation-map adaptation.
  The current isolated probe for `GroupsMetadataValueDelegateTests` instead
  stops before JUnit with a missing generated helper,
  `GroupsMetadata__TestCode`; it needs a generated-test-aware probe before a
  VM root cause can be assigned.
- **Missing `ApiVersionStrategy` bean** (2 classes: `CrossOriginAnnotationIntegrationTests`,
  `RequestMappingMessageConversionIntegrationTests`) — both WebFlux, both fail
  every parameterized variant (Jetty, Jetty Core, ...) with the identical
  `BeanCreationException` chain; looks like a missing/unregistered default
  bean rather than a per-test issue.
- `ImportSelectorTests`'s `StackOverflowError` is unrelated to the above
  clusters — likely infinite recursion somewhere in import-selector
  resolution, worth its own investigation.

## Bucket 3 — Immediate crash, not a hang (1/25)

- `scripting.groovy.GroovyScriptFactoryTests` — **ABEND**, `rc=139` (SIGSEGV),
  crashes during VM bootstrap warmup (`Post-clinit fixup` lines only, no test
  discovery output), `found=0`. This is a crash-on-load, categorically
  different from the TIMEOUT/hang classes above — was previously
  misclassified as TIMEOUT purely because it also exceeded 120s (the crash
  itself doesn't happen instantly; something before it is slow too).

## Raw data

- Merged results: 8 shards, `suite-run.sh`, `BATCH=1 BATCH_TO=1500 ONE_TO=1500`,
  `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`.
- Full per-class FAILCAUSE and crash-log detail pulled from
  `/data/tmp/hang25-s{0..7}/{failcauses,crashes}.log` on the Azure host.
