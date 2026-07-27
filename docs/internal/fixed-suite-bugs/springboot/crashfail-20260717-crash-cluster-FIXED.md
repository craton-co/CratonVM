# 2026-07-17 rerun: 5 fatal CRASHes, 2 distinct clusters — FIXED

> **Update 2026-07-23:** confirming this doc's prediction — the Spring Boot
> checkout has indeed moved `OpenTelemetryBaggagePropagationIntegrationTests`
> to package `.opentelemetry.autoconfigure` (visible in `craton-rerun-20260723`'s
> `group4.tsv`), so it's runnable again (no more `NoClassDefFoundError:
> ExceptionUtils`, no more fatal CRASH). It now FAILs instead — but on a new,
> unrelated symptom (`UniqueIdSelector [test-template-invocation:...] could
> not be resolved`, tied to its `@ForkedClassPath` annotation and the
> `ModifiedClassPathExtension` mechanism), not the `ExceptionUtils` gap this
> doc describes. Documented as Case A in
> [`modifiedclasspath-aether-network-hang-cluster-FIXED.md`](modifiedclasspath-aether-network-hang-cluster-FIXED.md)'s
> 2026-07-23 update. `BraveBaggagePropagationIntegrationTests` (the sibling
> class in `spring-boot-micrometer-tracing-brave`) and the two
> `ModifiedClassPathExtension*ParameterizedTests` classes were not
> re-checked this session (out of this task's assigned class list).

## Resolution (2026-07-18)

The late `ExceptionUtils` manifestation was already covered by the current
precise JIT-root and Conscrypt/JUL dispatch regressions on `dev`; the supplied
Spring Boot checkout has since moved its two tracing tests to `autoconfigure`,
so the historical class names are no longer runnable. The still-reproducible
Jetty crash was fixed by preventing Conscrypt's Windows JNI `JNI_OnLoad`
registration from mutating CratonVM's JNI tables, while registering the small
real-JDK Conscrypt initialization bridge required by Jetty's ALPN discovery.

`SslServerCustomizerTests` now passes 6/6 in both JIT and `--nojit` modes.
The separate current-fixture `ModifiedClassPathExtensionOverridesParameterizedTests`
assertion is an external dependency-version expectation (`spring-context 7.0.7`
versus 4.1.0.RELEASE), not a VM failure.

---

**Status: OPEN — found 2026-07-17**, `craton-rerun-20260717` (see
`apps/spring-boot-suite-runner/RESULTS-20260717.md`), against the 510-class
set still not `PASS` as of the 2026-07-16 snapshot. All 5 confirmed
CratonVM-specific (0/5 reproduce on the same-scope HotSpot baseline).

## Cluster 1 — `NoClassDefFoundError: org/junit/platform/commons/util/ExceptionUtils` (4 classes)

| Module | Class | Wall time |
|---|---|---:|
| `module/spring-boot-micrometer-tracing-brave` | `BraveBaggagePropagationIntegrationTests` | 186.7s |
| `module/spring-boot-micrometer-tracing-opentelemetry` | `OpenTelemetryBaggagePropagationIntegrationTests` | 183.9s |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionForkParameterizedTests` | 95.6s |
| `test-support/spring-boot-test-support` | `ModifiedClassPathExtensionOverridesParameterizedTests` | 108.2s |

Stack (identical across all 4, `SbRunner.java:36`):

```
Exception in thread "main" java/lang/NoClassDefFoundError: org/junit/platform/commons/util/ExceptionUtils
	at SbRunner.main(SbRunner.java:36)
	at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute(SessionPerRequestLauncher.java:67)
	...
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.failOrExecuteEngine(EngineExecutionOrchestrator.java:218)
	at org/junit/platform/launcher/core/EngineExecutionOrchestrator.executeEngine(EngineExecutionOrchestrator.java:263)
	at org/junit/platform/launcher/core/OutcomeDelayingEngineExecutionListener.reportEngineFailure(EngineExecutionOrchestrator.java:94)
	at org/junit/platform/launcher/core/DelegatingEngineExecutionListener.executionFinished(DelegatingEngineExecutionListener.java:47)
	at org/junit/platform/launcher/core/StackTracePruningEngineExecutionListener.executionFinished(StackTracePruningEngineExecutionListener.java:43)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-tracing-brave.org.springframework.boot.micrometer.tracing.brave.-27a51db29cb2.err.log`

**Investigated and ruled OUT as a runner/classpath artifact.** This exact
signature was suspected to be a "pathing-jar manifest truncation" runner bug
back in the 2026-07-11 run notes (`RESULTS-20260711.md` "HANG rerun"
section, `OriginTrackedYamlLoaderTests`). Checked this round:

- The module's `cratonvm-test-cp.txt` genuinely lists
  `junit-platform-commons-6.0.3.jar` on the classpath.
- The brave example ran for **186 seconds** before failing — not an
  instant classpath-resolution failure, which a genuinely-missing jar
  would produce at JVM startup.
- The failure happens deep inside JUnit Platform's own
  **failure-reporting path** (`EngineExecutionOrchestrator.failOrExecuteEngine`
  → `reportEngineFailure`), i.e. *after* some other real failure already
  occurred within the test engine, when the engine tries to format/report
  that failure and needs `ExceptionUtils` to do so.
- All 4 logs are preceded by a long sequence of
  `cratonvm::gc::guard` `gen_heap::get_field: out-of-bounds field read
  dropped` warnings on `org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`
  (self-described by the guard as benign/absorbed — "class layout is
  correct; the bug is in the caller's slot computation" — and present in
  466/510 = 91% of this round's logs overall, including many `PASS`es, so
  not on its own predictive of this failure).

**Hypothesis (not confirmed):** `ExceptionUtils` fails to (re)load
specifically when reached very late in a long-running class, via a code
path JUnit only exercises during failure reporting — i.e. a real CratonVM
classloading gap that's rare/late-triggering rather than a classpath
completeness problem. Needs a dedicated repro (isolate a call to
`org.junit.platform.commons.util.ExceptionUtils` late in a long-running
JUnit run, ideally after deliberately forcing an unrelated test failure) —
not attempted this round.

## Cluster 2 — `SslServerCustomizerTests` fatal native crash (1 class)

| Module | Class | Wall time | Exit code |
|---|---|---:|---:|
| `module/spring-boot-jetty` | `SslServerCustomizerTests` | 9.7s | `-1073740791` (`0xC0000409` = `STATUS_STACK_BUFFER_OVERRUN`, Windows `__fastfail`) |

No `SBRUNNER_RESULT` line — the process aborted before JUnit could report
anything (0 tests recorded). Log tail immediately before the abort:

```
WARN keystore: JKS key integrity check failed (wrong password?)
WARN keystore: JKS key integrity check failed (wrong password?)
WARN cratonvm_classloading::jar_signer: JDK cacerts=C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot\lib\security\cacerts PKCS#12 parse failed: PKCS#12: bag decode failed
WARN cratonvm_classloading::jar_signer: jar signer: rejecting signer block: SignerInfo is missing authenticatedAttributes — refusing to skip integrity check
FINE [org.conscrypt.NativeLibraryLoader] -Dorg.conscrypt.native.workdir: C:\Users\Victor\AppData\Local\Temp
<process aborts here, no further output>
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-jetty.org.springframework.boot.jetty.SslServerCustomizerTests.out.log`
(and matching `.err.log`).

**Not root-caused this round.** `STATUS_STACK_BUFFER_OVERRUN` is a
Windows-level `/GS` stack-cookie violation, i.e. a real native memory-safety
bug, not a caught Java exception — the crash happens either during
Conscrypt's native library load (`NativeLibraryLoader`, right before the
abort) or shortly after, plausibly related to (but not confirmed connected
to) the JKS keystore parse failures logged just before it (this test
exercises `SslServerCustomizer`'s keystore/truststore loading for a Jetty
SSL connector — a self-signed test keystore that CratonVM's JKS/PKCS12
parser is choking on, immediately followed by a native crash, is a
plausible causal chain but not verified). Needs a native debugger
(cdb/gdb) attached to reproduce and get a real stack trace before assigning
a root cause — out of scope for this triage pass.

## Reproduce

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm-spring-boot-rerun0717.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME -Category others `
  -ClassList <a module/class TSV with a `module\tclass` header containing just these 5 rows>
```
