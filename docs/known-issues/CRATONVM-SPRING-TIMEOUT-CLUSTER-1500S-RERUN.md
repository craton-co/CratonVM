# Spring TIMEOUT cluster — 1500s diagnostic rerun (hung vs. slow)

| | |
|---|---|
| **Status** | OPEN (12 genuinely hung, 10 slow-but-failing, 1 crash; 2 non-residual items removed) |
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

## Bucket 1 — Genuinely hung (12/25)

Hit the full 1500s ceiling on **both** the batch attempt and the individual
retry — `found=0/succ=0/fail=0`, no output at all, no FAILCAUSE, no crash log
entry. These are real hangs, not slow tests:

- `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests`
- `beans.factory.aot.BeanDefinitionMethodGeneratorTests`
- `beans.factory.aot.BeanRegistrationsAotContributionTests`
- `cache.jcache.JCacheEhCacheAnnotationTests`
- `context.annotation.CommonAnnotationBeanRegistrationAotContributionTests`
- `context.annotation.ComponentScanParserBeanDefinitionDefaultsTests`
- `context.annotation.ConfigurationClassPostProcessorAotContributionTests`
- `context.annotation.InitDestroyMethodLifecycleTests`
- `context.aot.ApplicationContextAotGeneratorTests`
- `orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests`
- `test.context.aot.TestContextAotGeneratorIntegrationTests`
- `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests`

9 of these 12 are AOT bean-registration/code-generation classes (same cluster
flagged in the `-125` doc's "AOT bean-registration TIMEOUT cluster") — a
shared root cause in that pipeline remains the leading hypothesis, not yet
investigated.

## Bucket 2 — Slow but completes (10 unresolved)

Real result landed well under 1500s (or right at the boundary for one). Not
hangs — but 10/12 are near-total failures, so the slowness itself may be part
of the same underlying bug (e.g. retry/backoff before ultimately failing)
rather than a coincidence:

| Class | Status | Elapsed | Pass/Total | First FAILCAUSE |
|---|---|--:|--:|---|
| `orm.jpa.support.InjectionCodeGeneratorTests` | FAIL | 206s | 3/10 | `CompilationException: Unable to compile source` |
| `web.socket.messaging.StompWebSocketIntegrationTests` | FAIL | 169s | 0/16 | `ServletException` / `UnsatisfiedDependencyException` (no `MessageHandler` bean) |
| `web.reactive.result.method.annotation.CrossOriginAnnotationIntegrationTests` | FAIL | 492s | 0/68 | `BeanCreationException`: no `ApiVersionStrategy` bean |
| `web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests` | FAIL | 445s | 211/241 | `AssertionFailedError` (mostly passing — a real partial failure) |
| `beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests` | FAIL | 693s | 0/47 | `CompilationException: Unable to compile source` |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | FAIL | 730s | 4/26 | `CompilationException: Unable to compile source` |
| `web.service.registry.ImportHttpServiceRegistrarTests` | FAIL | 763s | 3/5 | `ArrayIndexOutOfBoundsException` / `DiscoveryIssueException` |
| `web.service.registry.GroupsMetadataValueDelegateTests` | FAIL | 1039s | 1/8 | `ArrayIndexOutOfBoundsException` / `DiscoveryIssueException` |
| `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` | FAIL | 1132s | 0/160 | `BeanCreationException`: no `ApiVersionStrategy` bean (same as `CrossOriginAnnotationIntegrationTests`) |
| `context.annotation.ImportSelectorTests` | FAIL | 1456s | 4/9 | `StackOverflowError` |

Notable sub-clusters within this bucket (candidates for shared root cause):

- **In-memory javac `CompilationException`** (3 classes: `InjectionCodeGeneratorTests`,
  `BeanDefinitionPropertiesCodeGeneratorTests`, `InstanceSupplierCodeGeneratorTests`)
  — same AOT-codegen compilation machinery as the TIMEOUT cluster above and
  the (now-fixed) `core.test.tools.CompiledTests`/`TestCompilerTests`; these
  three fail fast on compile rather than hang, so likely a related but
  distinct defect in the same subsystem.
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
