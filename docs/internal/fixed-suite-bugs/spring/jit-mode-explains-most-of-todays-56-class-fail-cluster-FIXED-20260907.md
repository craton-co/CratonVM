# Most of today's 56-class common-FAIL cluster clears under `--nojit`

| | |
|---|---|
| **Status** | ✅ **FIXED 2026-09-07.** All 56 classes pass under JIT-on, bar one that fails identically on HotSpot and two that are correct but over the runner's per-class cap. Both mechanisms this page described are closed. The `--nojit` lever is no longer needed. |
| **Root cause** | ONE defect, not the two mechanisms this page originally described: the optimizing (IR) tier planted an unresumable uncommon trap at an `invokedynamic`, in methods that had already committed a side effect. |
| **Fix** | Two changes landed the same day from two lanes — the deopt sink now resumes the frame instead of aborting, and the trap is no longer planted at all. Full derivation in `testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster-FIXED-20260907.md`. |
| **Scope** | The 56 classes common to all three GC arms of the 2026-09-07 full 2848-class run (`gc3-{gen,g1,zgc}-jit-real-all-20260907-*`). |
| **Measured** | Azure host `20.80.105.49`, worktree `/data/wt-l6-spring`, real JDK 25. |

## Evidence

### On the merged tree — the sink fix alone is not enough

The sink fix closes the abort. It does **not** close the silent replay, and
that is what most of these 56 classes were actually dying of. One binary
carrying both changes, the 56-class list, one variable:

| arm | OK | FAIL | TIMEOUT | test-methods failed |
|---|---:|---:|---:|---:|
| default (this change) | **54** | 1 | 1 | **6** of 1670 |
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | 34 | **21** | 1 | **302** of 1670 |

Each arm launched through `env -i`, so nothing exported in the session shell
could decide one — see the measurement note on the sibling page for why that
sentence is here.

Twenty classes and 296 test methods separate the arms. The families are exactly the ones this page
grouped under Spring's `"Post-processing of merged bean definition failed"`
wrapper plus the reactive/WebSocket set: `cache.config.EnableCachingTests`
(27 of 74 without the guard), `cache.jcache.*`, `cache.aspectj.*`, the
`web.reactive.result.method.annotation.*` group, `WebSocketIntegrationTests`,
`StompWebSocketIntegrationTests`, `ResourceHttpRequestHandlerIntegrationTests`.

Those two arms ran back to back on a shared host, so they were confirmed
ABBA-interleaved over six of the affected classes:

| slot | arm | classes | passed | failed |
|---:|---|---|---:|---:|
| 1 | default | OK=6 | 315 | 0 |
| 2 | `…GUARD=0` | FAIL=6 | 133 | 182 |
| 3 | `…GUARD=0` | FAIL=6 | 133 | 182 |
| 4 | default | OK=6 | 315 | 0 |

Byte-identical between repeats of each arm — 133/182 twice, 315/0 twice. That
is deterministic, not host load.

### The two non-OK rows in the fixed arm

* `aot.nativex.FileNativeConfigurationWriterTests` — `FAIL 9/3/6`, and
  **HotSpot fails it identically** (`FAIL 9/3/6`, re-measured today). Already
  recorded in `not-cratonvm-bugs-consolidated.md`; unchanged by this fix and
  not a CratonVM bug.
* `beans.factory.aot.BeanRegistrationsAotContributionTests` — `TIMEOUT` at the
  suite's default 180 s per-class cap, not a failure.
  `test.context.aot.AotIntegrationTests` joins it on a loaded host and fits
  inside the cap on a quiet one. Both were re-run alone with `--one-to 2400`:

  | class | status | found | succ | fail | skip | wall |
  |---|---|---:|---:|---:|---:|---:|
  | `BeanRegistrationsAotContributionTests` | OK | 14 | 14 | 0 | 0 | 1067 s |
  | `AotIntegrationTests` | OK | 4 | 2 | 0 | 2 | 334 s |

  Both match HotSpot's own result for the class (14/14, and 4 found / 2 succ /
  2 skip). HotSpot runs the first in 10.7 s, which is the gap tracked at
  `beanregistrations-verylarge-throughput-20260907.md`; `AotIntegrationTests`
  is only just over the cap and crosses it under host load.

### Before the sink fix — the refusal on its own

Measured on a binary built from `dev` @ `e90fa0274` plus this change only, so
this pair isolates the refusal from the sink fix:

| arm | OK | FAIL | TIMEOUT | test-methods |
|---|---:|---:|---:|---|
| refusal ON | **54** | 1 | 1 | found=1670 passed=1653 failed=6 |
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | 0 | **56** | 0 | found=1684 passed=950 failed=723 |

## The two "mechanisms" were one

This page listed two and said whether they were related "was not determined".
They are the same defect, and the evidence is that fixing one thing fixed both
families in one run:

**Mechanism 1** (`TestCompiler`, 19 classes) — `ScopeImpl.remove` traps at its
`invokedynamic` after `Assert.check` has already run; javac catches the
resulting `InternalError`, prints its own crash banner to stderr, and returns
`false` with an empty `DiagnosticListener`. That is the whole "silent compile
failure".

**Mechanism 2** (`CommonAnnotationBeanPostProcessor` NPE) — the reasoning on
this page was sound and its conclusion was right: the `||` short-circuit in
`InjectionMetadata.needsRefresh` cannot evaluate wrongly, and it did not. A
method on that path was compiled with an unresumable trap, and what came back
was a `null` where the Java cannot produce one.

`probes/CacheCtxProbe.java` is the direct witness this page asked for and could
not build. It creates the same `AnnotationConfigApplicationContext` forty times
in one process and prints the full cause chain that the suite runner's
`FAILCAUSE` line truncates. On one binary, `env -i` so nothing leaks in:

| arm | result | site traps planted | refused |
|---|---|---:|---:|
| default | **all 40 contexts built** | 233 | **58** |
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | fails at iteration 27 | 269 | 0 |

and the failure is exactly the chain this page inferred from reading:

```
  CAUSE org.springframework.beans.factory.BeanCreationException: ...
        Post-processing of merged bean definition failed
  CAUSE java.lang.NullPointerException: null
      at CommonAnnotationBeanPostProcessor.postProcessMergedBeanDefinition(...:291)
      at AbstractAutowireCapableBeanFactory.applyMergedBeanDefinitionPostProcessors(...:1112)
```

Two arms each, repeated, byte-identical between repeats. The iteration-27 onset
is the tier-up: it is not a logic error and never was.

Every class this page named under that signature now passes:

| class | found | succ | fail |
|---|---:|---:|---:|
| `web.reactive…GlobalCorsConfigIntegrationTests` | 36 | 36 | 0 |
| `web.reactive…CrossOriginAnnotationIntegrationTests` | 68 | 68 | 0 |
| `web.reactive…CoroutinesIntegrationTests` | 40 | 40 | 0 |
| `web.reactive…JacksonHintsIntegrationTests` | 36 | 36 | 0 |
| `web.reactive…ProtobufIntegrationTests` | 20 | 20 | 0 |
| `web.reactive…RequestMappingIntegrationTests` | 20 | 20 | 0 |
| `web.reactive.function.server.DispatcherHandlerIntegrationTests` | 20 | 20 | 0 |
| `cache.config.EnableCachingTests` | 74 | 74 | 0 |
| `context.groovy.GroovyApplicationContextTests` | 4 | 4 | 0 |
| `context.index.processor.CandidateComponentsIndexerTests` | 24 | 24 | 0 |

`CrossOriginAnnotationIntegrationTests` was the tell all along. This page
recorded it surfacing
`JIT dispatch into CorsConfiguration.addAllowedOriginPattern(...) failed:
internal error: precise deoptimization unavailable ... refusing
side-effecting replay` and filed it as a *possible contributing factor*. It
was not a contributing factor; it was the mechanism, printed in full, for both
families.

`MultipartWebClientIntegrationTests` — listed here as "a different-looking
symptom entirely (HTTP 500), related or independent not determined" — is
`32/32` in the fixed arm and `FAIL` in the control arm. Same defect.

## Why `--nojit` worked, and why the whole list was one bug

`--nojit` cleared every class because the trap only exists in compiled code.
The reason the five sampled clusters generalised to all 56 is now clear: the
trigger is not a Spring construct at all, it is any sufficiently hot method
that has committed a side effect before reaching a lambda or string-concat
call site. Spring's AOT, reactive config, caching, Groovy and classpath-index
code all have that shape, and so does javac, and so — as
`../../fixed-bugs/precise-deoptimization-unavailable-cross-suite-crash-20260907-FIXED.md`
shows — does H2, in three application methods with no javac anywhere near
them.

## Reproducing (historical)

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --only 'GlobalCorsConfigIntegrationTests$' --tag repro
```

## Related

- `testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster-FIXED-20260907.md`
  — the full root-cause derivation, both fixes, and the throughput measurement.
- `../../fixed-bugs/precise-deoptimization-unavailable-cross-suite-crash-20260907-FIXED.md`
  — the same defect's H2 half (8 crash classes, all closed).
- `../../../known-issues/spring/not-cratonvm-bugs-consolidated.md` —
  `FileNativeConfigurationWriterTests`.
- `../../../known-issues/spring/beanregistrations-verylarge-throughput-20260907.md`
  — the one residual this fix exposed rather than closed.
- `aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`
  — architecturally adjacent, distinct, and confirmed unrelated.
