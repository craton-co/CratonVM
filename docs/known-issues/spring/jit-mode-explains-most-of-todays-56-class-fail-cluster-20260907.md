# Most of today's 56-class common-FAIL cluster clears under `--nojit`

| | |
|---|---|
| **Status** | OPEN. High-confidence aggregate finding; individual root causes only partially traced. |
| **Scope** | The 56 classes common to all three GC arms in the 2026-09-07 full 2,848-class run (`gc3-{gen,g1,zgc}-jit-real-all-20260907-*`). |

## Finding

Of the 56 common-FAIL classes, every one sampled so far **passes cleanly under
`--nojit`** on the same binary, same classpath:

| class | JIT-on | `--nojit` |
|---|---|---|
| `core.test.tools.TestCompilerTests` | `found=22 succ=3 fail=19` | `found=22 succ=22 fail=0` |
| `web.reactive.result.method.annotation.GlobalCorsConfigIntegrationTests` | FAIL (all 4 server variants) | `found=36 succ=36 fail=0` |
| `cache.config.EnableCachingTests` | FAIL | passes (batched run, `found=162 succ=162 fail=0` across this class + 2 more below) |
| `context.groovy.GroovyApplicationContextTests` | FAIL | passes (same batched run) |
| `context.index.processor.CandidateComponentsIndexerTests` | FAIL | passes (same batched run) |

One class (`aot.nativex.FileNativeConfigurationWriterTests`) is separately
confirmed as **not** a CratonVM bug at all — it fails identically on stock
HotSpot (`not-cratonvm-bugs-consolidated.md`), a JSON-fixture assertion issue.

The other ~50 were not individually re-verified under `--nojit` in this
session (an attempted full-batch rerun via `--only` with a 31-class regex
alternation was interrupted by an unrelated SSH channel drop before
completing) — but the five distinct clusters checked (AOT/codegen,
CORS/reactive-config bean creation, caching/AOP, Groovy, and the
classpath-index processor) span enough of the 56's variety that this reads as
the dominant explanation for the whole list, not a coincidence limited to a
few classes.

## Two distinct JIT-specific mechanisms identified so far

**1. Silent in-memory compile failure** (19-class AOT/`TestCompiler` cluster) —
see `testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster-20260907.md`
for the full writeup. `TestCompiler.compile()`'s `task.call()` returns falsy
with **zero** diagnostics reported through the `DiagnosticListener` — not a
normal javac error.

**2. `NullPointerException` in `CommonAnnotationBeanPostProcessor.postProcessMergedBeanDefinition`**
(at minimum `GlobalCorsConfigIntegrationTests`'s cluster, and by extension
plausibly others reporting the same `"Post-processing of merged bean
definition failed"` wrapper — `DispatcherHandlerIntegrationTests`,
`CoroutinesIntegrationTests`, `JacksonHintsIntegrationTests`,
`ProtobufIntegrationTests`, `RequestMappingIntegrationTests`, and the
`cache.config`/`cache.aspectj`/`cache.jcache.*` classes' identical
`internalAutoProxyCreator: Post-processing of merged bean definition failed`
signature — not individually confirmed, inferred from the shared wrapper
text). The crash site:

```java
// CommonAnnotationBeanPostProcessor.java:291
public void postProcessMergedBeanDefinition(...) {
    ...
    InjectionMetadata metadata = findResourceMetadata(beanName, beanType, null);
    metadata.checkConfigMembers(beanDefinition);   // NPE: metadata is null
}
```

`findResourceMetadata`'s cache lookup:

```java
InjectionMetadata metadata = this.injectionMetadataCache.get(cacheKey);
if (InjectionMetadata.needsRefresh(metadata, clazz)) { ... build and cache it ... }
return metadata;
```

`InjectionMetadata.needsRefresh` is `@Contract("null, _ -> true")` —
`metadata == null || metadata.needsRefresh(clazz)` **cannot** evaluate `false`
when `metadata` is `null`, by Java's short-circuit `||` semantics. For
`findResourceMetadata` to return `null`, either that short-circuit
evaluated wrongly under CratonVM's JIT, or some other path bypasses the
refresh-and-cache logic entirely. Not traced further into CratonVM's own
source in this session.

**Not yet determined**: whether these two mechanisms are related (e.g. both
downstream of one JIT correctness bug in null-handling or in a shared
inlining/deopt path) or are two separate defects that happen to share a
JIT-only symptom. `CrossOriginAnnotationIntegrationTests` — in the same
reactive-config family as `GlobalCorsConfigIntegrationTests` — additionally
surfaced a directly-visible JIT error in one run:
`JIT dispatch into CorsConfiguration.addAllowedOriginPattern(...) failed:
internal error: precise deoptimization unavailable ... refusing
side-effecting replay`, suggesting a deopt-path defect may be a contributing
or related factor, not confirmed as the same root cause as either mechanism
above.

> **That deopt defect is FIXED (2026-09-07)** —
> `internal/fixed-bugs/deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md`.
> It was one gap hitting three suites the same day: the entire H2 CRASH
> population, 7 hibernate-reactive classes, and this line. So this page should
> no longer carry it as an open suspect.
>
> **What that does NOT settle**, and why this page stays open: the abort was a
> hard, VISIBLE process failure, whereas the two mechanisms above are a silent
> compile failure and an NPE. A class can stop aborting and still fail for the
> reasons this page is actually about. Re-running
> `CrossOriginAnnotationIntegrationTests` on a current binary is the cheap next
> step, and until someone does, the only claim supported here is that one of
> the three symptoms it showed has a known fix.

## Not the same as the already-fixed sibling

`aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`
covers an architecturally-adjacent but distinct symptom (JIT compiling a
callee by class name, resolving to another loader's `DynamicClassFileObject`
copy, thrown as `IllegalArgumentException` from `javac`'s `inferBinaryName`).
Neither of today's two mechanisms shows that exception or that stack shape.
Whether the by-name-compile fix left a related class of defects unfixed was
not established here.

## Practical implication

**`--nojit` is a working, verified lever** for every class checked. Anyone
needing a clean Spring Framework baseline today can use it; it is not a fix,
and it costs JIT throughput on the whole suite, not just the affected
classes.

## Not done in this session

- Confirming the remaining ~50 of 56 classes individually (or at minimum,
  clustering them properly by exception signature before assuming they all
  share one of the two mechanisms above).
- Tracing either mechanism to a Rust source location (`CRATONVM_DBG_JITC=1`
  or equivalent instrumented rerun, per the pattern that closed the sibling
  bug).
- Checking whether `MultipartWebClientIntegrationTests` (HTTP 500 from the
  server, a different-looking symptom entirely) is related or independent.

## Reproducing

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --only 'GlobalCorsConfigIntegrationTests$' --tag repro           # fails
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run --jit off \
  --only 'GlobalCorsConfigIntegrationTests$' --tag repro-nojit     # passes
```

## Related

- `testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster-20260907.md`
- `not-cratonvm-bugs-consolidated.md` (this folder) — `FileNativeConfigurationWriterTests`
- `aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`
  — architecturally adjacent, already-fixed, distinct symptom
