# Most of today's 56-class common-FAIL cluster clears under `--nojit`

| | |
|---|---|
| **Status** | ✅ **FIXED 2026-09-07.** All 56 classes now pass under JIT-on, bar one that fails identically on HotSpot and one that is correct but slow. The `--nojit` lever is no longer needed. |
| **Root cause** | ONE defect, not the two mechanisms this page originally described: the optimizing (IR) tier planted an unresumable uncommon trap at an `invokedynamic`, in methods that had already committed a side effect. See the retired `testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster` write-up for the full derivation. |
| **Fix** | `IrBuilder::trap_replay_is_safe` (`jit/src/ir.rs`). Kill switch `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0`. |
| **Scope** | The 56 classes common to all three GC arms of the 2026-09-07 full 2848-class run (`gc3-{gen,g1,zgc}-jit-real-all-20260907-*`). |
| **Measured** | Azure host `20.80.105.49`, worktree `/data/wt-l6-spring`, real JDK 25. |

## Result

The 56-class list was recomputed from the three arms' `results.tsv`
(`status == FAIL` in all three; the 66-class three-way intersection of
*non-OK* includes 9 `LOADERR` and 1 `TIMEOUT` that are not this cluster) and
re-run on one binary, twice, with only the guard's kill switch varying:

| arm | OK | FAIL | TIMEOUT | test-methods |
|---|---:|---:|---:|---|
| guard ON (the fix) | **54** | 1 | 1 | found=1670 passed=1653 failed=6 |
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | 0 | **56** | 0 | found=1684 passed=950 failed=723 |

Same binary, same host, same classpath, back to back. Nothing else varied.

The two non-OK rows in the fixed arm:

* `aot.nativex.FileNativeConfigurationWriterTests` — `FAIL 9/3/6`, and
  **HotSpot fails it identically** (`FAIL 9/3/6`, re-measured today). Already
  recorded in `not-cratonvm-bugs-consolidated.md`; unchanged by this fix and
  not a CratonVM bug.
* `beans.factory.aot.BeanRegistrationsAotContributionTests` — `TIMEOUT` only
  at the suite's default 180 s per-class cap. Run alone with `--one-to 2400`
  it is **`OK found=14 succ=14 fail=0`, matching HotSpot's 14/14** — in 1067 s
  against HotSpot's 10.7 s. The correctness half is fixed; the throughput gap
  is its own open page, `beanregistrations-verylarge-throughput-20260907.md`.

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
this page was sound and its conclusion was right for the wrong target: the
`||` short-circuit in `InjectionMetadata.needsRefresh` cannot evaluate wrongly,
and it did not. The bean post-processor never got a chance to be wrong; a
method on that path trapped unresumably and the `InternalError` surfaced as
Spring's `"Post-processing of merged bean definition failed"` wrapper. Every
class this page named under that signature now passes:

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
code all have that shape, and so does javac, and so — as the retired
`precise-deoptimization-unavailable-cross-suite-crash` write-up shows — does
H2, in three application methods with no javac anywhere near them.

## Reproducing (historical)

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --only 'GlobalCorsConfigIntegrationTests$' --tag repro
# CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0 restores the failure on a fixed binary.
```

## Related

- The retired `testcompiler-injit-mode-silent-compile-failure-19-class-aot-cluster`
  write-up — the full root-cause derivation and the fix's evidence.
- The retired `precise-deoptimization-unavailable-cross-suite-crash` write-up
  — the same defect's H2 half (8 crash classes, all closed).
- `../../../known-issues/spring/not-cratonvm-bugs-consolidated.md` —
  `FileNativeConfigurationWriterTests`.
- `../../../known-issues/spring/beanregistrations-verylarge-throughput-20260907.md`
  — the one residual this fix exposed rather than closed.
- `aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`
  — architecturally adjacent, distinct, and confirmed unrelated.
