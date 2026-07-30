# Spring-related JIT bans in `vm/src/jit/skip_list.rs` — inventory, ban-lift experiment, and removal

**Status: ✅ RESOLVED (2026-07-30). All 11 bans inventoried here have been
REMOVED from `vm/src/jit/skip_list.rs`.** The 2026-07-30 morning experiment
(Part 2) found all 8 javac-family bans still necessary on dev `351bf59b0`;
re-running the same experiment the same evening on dev `9ac1feffe` found none
of them necessary any more — the underlying JIT defects had been fixed by dev
drift in between. The three 2026-07-29 Spring Boot bans were tested for the
first time in this session (the earlier session had no `apps/spring-boot`) and
are likewise no longer necessary. The gates, the `SkipReason` variants, and
the unit tests asserting the bans were in force are all gone; those tests now
assert the opposite (JIT-ELIGIBILITY), so a silent re-introduction is a test
failure.

## Context

Requested while triaging the CratonVM Spring-framework suite's remaining
non-passed classes (see `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s "2026-07-27
suite-wide reconfirmation" section — several of the residual classes are in
the AOT/in-process-javac `CompilationException` cluster, which this doc's
bans were the known cause of). Two tasks: (1) inventory every currently-live
Spring-related ban in `skip_list.rs`, distinguishing it from the file's large
volume of *historical/removed* bans; (2) actually lift the relevant bans on a
live build and re-run the affected classes to check whether any are stale
(safe to remove) — this was explicitly flagged as an open follow-up inside
the file's own `TYPES-ERASURE.1` comment ("would need a full regression pass
with those seven bans removed and only this one in place... left as a
follow-up").

Parts 1-2 were done on Azure host `20.83.144.174`, worktree
`/data/data/wt-springsuite8b-20260726`, real JDK 25, `apps/spring-suite-runner`.
Part 3 (the removal) used worktree `/data/data/wt-jitbanretire-20260730`
(branch `fix/jit-ban-retire-20260730`) plus a second, detached worktree
`/data/data/wt-jitbanoldtip-20260730` pinned at `351bf59b0` to reproduce the
old behaviour as a control.

## Part 1 — inventory of the bans (all now REMOVED)

Confirmed by reading `vm/src/jit/skip_list.rs` directly (grep is unreliable
against this file — many hits are inside multi-hundred-line historical
comment blocks describing bans that were later REMOVED; only the live `if
class_name == ... && method_name == ... { return Some(SkipReason::X); }`
gate sites counted as "active").

### javac-internal bans (in-process `TestCompiler`/AOT codegen family)

Not literally `org/springframework/*`, but existed *because of* — and were
directly responsible for the residual failures in — Spring's AOT
code-generation test suite (`beans.factory.aot.*`, `core.test.tools.*`,
`test.context.aot.*`). Also shared with H2/Hibernate's in-process javac use
(`ClassSymbolComplete`).

| `SkipReason` | Banned method | Origin |
|---|---|---|
| `JavacToolContext` | `com/sun/tools/javac/api/JavacTool.getTask` | SPRING-TESTCOMPILER.1 (2026-07-18) |
| `ClassReaderReadClass` | `com/sun/tools/javac/jvm/ClassReader.readClass` | SPRING-TESTCOMPILER.2 (2026-07-20) — doc comment called this "very likely the true root cause behind most of the AOT cluster's `CompilationException` failures" |
| `ClassFinderFillIn` | `com/sun/tools/javac/code/ClassFinder.fillIn` | SPRING-TESTCOMPILER.3 family |
| `ClassReaderReadInnerClasses` | `com/sun/tools/javac/jvm/ClassReader.readInnerClasses` | same family |
| `ClassReaderReadAttrs` | `com/sun/tools/javac/jvm/ClassReader.readAttrs` | same family |
| `ClassSymbolComplete` | `com/sun/tools/javac/code/Symbol$ClassSymbol.complete` | HIB-STOREDPROC-JIT.1 (2026-07-23) — H2 `CREATE ALIAS ... AS $$`, same in-process-javac family |
| `TypesErasure` | `com/sun/tools/javac/code/Types.erasure` | TYPES-ERASURE.1 (2026-07-25) — flagged as possibly subsuming several of the above |
| `JavaPoetCodeBlockBuilderAdd` | `org/springframework/javapoet/CodeBlock$Builder.add` | SPRING-TESTCOMPILER.4 (2026-07-21) — Spring shades `com.palantir.javapoet` to this package at build time |

### Spring Boot bans (added 2026-07-29)

Not exercised by the `spring-framework` suite — they target
`org/springframework/boot/*` and friends as exercised by
`BasicErrorControllerIntegrationTests`, a `spring-boot`-module test. **Not
tested in Part 2** (that worktree had no `apps/spring-boot`); tested for the
first time in Part 3.

| `SkipReason` | Banned method |
|---|---|
| `SpringBootConditionReportMapping` | `org/springframework/boot/autoconfigure/condition/ConditionEvaluationReport.lambda$recordConditionEvaluation$0` |
| `SpringBootJdkHttpRequestHeaderComparator` | `org/springframework/http/client/JdkClientHttpRequest.lambda$buildRequest$0` |
| `SpringAnnotatedMetadataAttributeCollector` | `org/springframework/core/type/AnnotatedTypeMetadata.getAllAnnotationAttributes` |

### Removed / historical (already inactive before this session)

Broad **package-level** bans, lifted after root-causing, mostly 2026-07-26:
`org/springframework/util/` (SPB.1 — root cause was a GC-root-scanning gap,
not JIT, fixed in `gc/src/vm_heap.rs`), `org/springframework/core/` (SPB.2),
`org/springframework/beans/factory/` + `.../support/`,
`org/springframework/boot/loader/`, `org/springframework/web/reactive/`
(SPB.9b/9c), `org/springframework/boot/context/properties/bind/` +
`.../context/` (SPB.4/.4b/.4c), `SpringBootModifiedClassPathLoader`
(SPRINGBOOT-WITHOUT-JACKSON.2 — orphaned `SkipReason` enum variant, gate
removed, has its own regression test asserting JIT-eligibility).

## Part 2 — ban-lift regression test (2026-07-30 morning, dev `351bf59b0`)

Disabled the 8 javac-internal-family bans above (`if false && /*
BAN-LIFT-EXPERIMENT */ class_name == ...`), built a release binary from that
source, ran the classes each ban's own doc comment names as affected, real
JDK 25, jit-real.

### Baseline (ban ACTIVE, same dev tip `351bf59b0`)

| Class | Result |
|---|---|
| `aot.nativex.FileNativeConfigurationWriterTests` | OK 7/7 |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | FAIL 10/34 |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | OK 24/26 |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT (700s) |

### Ban-lifted (same dev tip, all 8 gates disabled)

| Class | Result | Notes |
|---|---|---|
| `aot.nativex.FileNativeConfigurationWriterTests` | OK 7/7 | unchanged |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | **FAIL 0/34** | regressed; failcause = `org.springframework.core.test.tools.CompilationException: Unable to compile source` — the EXACT symptom the ban exists to prevent |
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | **FAIL 2/26** | severe regression, same `CompilationException` symptom |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | TIMEOUT (700s) | unchanged — already broken independent of these bans |
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | **FAIL 0/14** | regressed — but via a **different, new-looking** symptom; Part 3 shows it is ban-independent |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | FAIL 22/44 | already-known-broken class, no same-tip baseline captured |
| `beans.factory.aot.CodeWarningsTests` | FAIL 19/23 | same |
| `core.test.tools.TestCompilerTests` | FAIL 4/22 | same |
| `context.index.processor.CandidateComponentsIndexerTests` | FAIL 4/24 | same |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL 0/4 | same |

Part 2's conclusion at the time: **all 8 tested bans still necessary**, no
stale bans found, `TYPES-ERASURE.1`'s consolidation question still open, and
one new lead (the `StackWalker` NPE) not investigated.

## Part 3 — re-test and removal (2026-07-30 evening, dev `9ac1feffe`)

### Method

Instead of an `if false &&` source edit, this session added a temporary
build-time kill switch (`CRATONVM_JIT_UNBAN=<SkipReason>[,...]|all`) inside
`should_skip_jit_internal`, so each ban could be lifted per-run without a
rebuild. The switch was deleted again as part of the final removal patch — it
exists in no committed source.

Three binaries:

* `cratonvm-oldtip-20260730.bin` — dev `351bf59b0` + kill switch, i.e. Part
  2's exact tip. This is the **control**: it must reproduce Part 2's
  failures, otherwise a clean result on the new tip proves nothing (a
  differential that changed nothing looks exactly like a differential that
  proved something).
* `cratonvm-jitban-20260730.bin` — dev `9ac1feffe` + kill switch.
* `cratonvm-nobans-20260730.bin` — dev `9ac1feffe` with the gates physically
  deleted, to verify the shipped end state rather than the env-var simulation
  of it.

### The javac family is no longer miscompiled

The repo's own witness for this family is `JavacConsolidationProbe.java` —
200 varied in-process `ToolProvider.getSystemJavaCompiler()` compilations per
run, written for the 2026-07-26 consolidation experiment and recovered here
from `efc6fc2748^` (repros were dropped from git on 2026-07-28).

| Binary | Bans | `JavacConsolidationProbe 200` |
|---|---|---|
| dev `351bf59b0` | **lifted** | **FAIL at iteration 2**, then 3, 4, 5 — aborts early |
| dev `9ac1feffe` | active | OK 200/200 |
| dev `9ac1feffe` | **lifted** | **OK 200/200** |
| gates deleted (`nobans`) | n/a | OK 200/200, 3 runs |

Three purpose-written probes agree (sources kept on the host at
`/data/data/wt-jitbanretire-20260730/probes/`; not committed, per the
2026-07-28 "remove repros from git" decision):

| Probe | old tip + lifted | new tip + lifted | `nobans` build |
|---|---|---|---|
| `JavacLoopRepro` — 40× compile `@Deprecated class TrivialN`, `TYPES-ERASURE.1`'s own shape | FAIL (22-40 of 40 bad) | OK 40/40 | OK 40/40 |
| `JavacLoopRepro2` — 60× compile against a TYPE_USE-annotated `@FunctionalInterface` read back as a classfile | FAIL from iteration 1 | OK 60/60 | OK 60/60, 3 runs |
| `H2AliasProbe` — 60× H2 `CREATE ALIAS ... AS $$`, `HIB-STOREDPROC-JIT.1`'s own shape | FAIL from iteration 1 | — | OK 60/60, 3 runs |

`H2AliasProbe`'s old-tip failure names the defect outright, which is a useful
record of what this family actually was:

```
java.lang.InternalError: JIT dispatch into
com/sun/tools/javac/jvm/ClassReader.readInnerClasses(...) failed: internal
error: precise deoptimization unavailable for ...readInnerClasses(...) at
bci 41 (the frame could not be materialised from its map, ... reason
OsrExit); refusing side-effecting replay
```

### The Spring AOT suite is unchanged by the removal

Ten classes, run end-to-end on dev `9ac1feffe` in three configurations: bans
active, bans lifted via the kill switch, and the `nobans` build with the gates
physically deleted. **All three columns are identical, class by class.**

| Class | bans active | bans lifted | gates deleted |
|---|---|---|---|
| `beans.factory.aot.InstanceSupplierCodeGeneratorTests` | OK 24/26 | OK 24/26 | OK 24/26 |
| `beans.factory.aot.BeanDefinitionMethodGeneratorTests` | FAIL 10/34 | FAIL 10/34 | FAIL 10/34 |
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | FAIL 0/14 | FAIL 0/14 | FAIL 0/14 |
| `aot.nativex.FileNativeConfigurationWriterTests` | OK 7/7 | OK 7/7 | OK 7/7 |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | OK 44/44 | OK 44/44 | OK 44/44 |
| `beans.factory.aot.CodeWarningsTests` | OK 23/23 | OK 23/23 | OK 23/23 |
| `core.test.tools.TestCompilerTests` | OK 22/22 | OK 22/22 | OK 22/22 |
| `context.index.processor.CandidateComponentsIndexerTests` | OK 24/24 | OK 24/24 | OK 24/24 |
| `context.aot.ApplicationContextAotGeneratorTests` | FAIL 8/40 | FAIL 8/40 | FAIL 8/40 |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL 0/4 | FAIL 0/4 | FAIL 0/4 |

Four of these are markedly *better* than Part 2's numbers from a few hours
earlier (`CodeWarningsTests` 23/23 was 19/23, `TestCompilerTests` 22/22 was
4/22, `CandidateComponentsIndexerTests` 24/24 was 4/24,
`BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` 44/44 was 22/44) —
more dev drift, unrelated to the bans.

The four still-failing classes fail **identically with and without the bans**,
so they are not caused by the removal. See "Residuals" below.

### The three Spring Boot bans

Witness: `BasicErrorControllerIntegrationTests`
(`module/spring-boot-webmvc`), driven through `sb-runner/SbRunner` against the
standalone Spring Boot checkout at
`/data/data/springboot-jsonreader-deprecation-20260718` (that module's
`build/cratonvm-test-cp.txt` held stale Windows paths and was regenerated with
`:module:spring-boot-webmvc:cratonvmTestCp`).

The failure is intermittent, so each arm was run 12×:

| Binary | Bans | Runs | Result |
|---|---|---|---|
| dev `351bf59b0` | **lifted** | 12 | **1 hard VM abort** — `internal error: checkcast: not an object reference`, i.e. `SPRINGBOOT-HTTP-HEADER-COMPARATOR.1`'s exact documented "fatal invalid-reference checkcast"; 11 clean |
| dev `9ac1feffe` | **lifted** | 12 | 12/12 clean (26/26 tests each) |
| dev `9ac1feffe` | active | 12 | 12/12 clean (background control) |
| gates deleted (`nobans`) | n/a | 12 | 12/12 clean |

A narrower standalone probe (`CondReportProbe`, 15 000
`recordConditionEvaluation` calls plus a typed-map read-back per arm) was
written first and is **not** a valid witness — it passes on the old tip with
the bans lifted too. Recorded here so nobody re-derives it: the
`ConditionEvaluationReport` corruption needs the full autoconfiguration boot,
not merely hot calls into the lambda.

### Proof the lift was not a no-op

Per the standing lesson that a ban removal must be shown to actually change
what gets compiled (`CRATONVM_DBG_JIT_COMPILED=1`):

* `com/sun/tools/javac/code/Types.erasure`,
  `com/sun/tools/javac/code/Symbol$ClassSymbol.complete`,
  `com/sun/tools/javac/code/ClassFinder.fillIn` and
  `org/springframework/boot/autoconfigure/condition/ConditionEvaluationReport.lambda$recordConditionEvaluation$0`
  are all listed as compiled with the bans lifted, and none of them with the
  bans active.
* `ClassReader.readClass`/`.readAttrs`/`.readInnerClasses`,
  `JavacTool.getTask` and `CodeBlock$Builder.add` did **not** reach the
  backend in the probe workloads even with their bans lifted and
  `CRATONVM_JIT=threshold=1` — they are refused earlier, at the first-call
  compile path's native-shadow scan (`jit_method_calls_native_shadowed`), so
  their bans were already unobservable in those workloads. That is *not* the
  basis for removing them: the old-tip control above shows
  `readInnerClasses` being compiled and failing on `351bf59b0` under the H2
  workload, so the removal rests on a real differential on a workload that
  does compile them.
  (Note the flag spelling: `CRATONVM_JIT_THRESHOLD=1` is accepted with a
  deprecation warning but does *not* take effect — the working spelling is
  `CRATONVM_JIT=threshold=1`. Several enqueue traces showing
  `invocations=500` under `CRATONVM_JIT_THRESHOLD=1` gave this away.)
* Both admission gates were checked: `grep -rn 'com/sun/tools/javac' vm/src
  jit/src` finds no deny-prefix mirror in `jit/src/lib.rs` — unlike
  `hsqldb_`/`xerces_schema_`/`jaxb_mapping_`, this family only ever had the
  `skip_list.rs` gate.

### Which commit fixed the javac family?

`git bisect` over the 62 commits between `351bf59b0` and `9ac1feffe`, driven
by `JavacLoopRepro` with all 8 bans lifted, lands on **`62f289e71`
"fix(jit): isolate Matrix scratch from operand cache"** — a 7-line change that
turns off the pure-kernel deferred operand cache (which owns R8/R9 across
bytecodes) whenever the matrix-dot pre-header lowering, which uses those same
registers, is active.

Take that with a caveat: `kernel_operand_cache`/`matrix_dot_loops` do not
exist at all in `351bf59b0`'s `jit/src/x64.rs`, so the *old* tip's breakage
cannot be the same defect. There were at least two independent causes in this
range — one fixed somewhere mid-range, and the R8/R9 clobber introduced and
then fixed inside it — and `git bisect`'s monotonicity assumption only lets it
name the last transition. What is solid is the endpoint measurement: the
family reproduces on `351bf59b0` and does not reproduce on `9ac1feffe`.

That history is also why the removal note left in `skip_list.rs` says to
prefer fixing the lowering over re-adding a per-method ban: this exact family
of javac methods was broken twice in two weeks by unrelated x64 backend work,
and a per-method ban hides that instead of surfacing it.

## Residuals — NOT caused by these bans, handed off

1. **`StackWalker$StackFrame.getDeclaringClass()` returns null again.** Part 2
   flagged this as a "new lead" seen only with the bans lifted. It now
   reproduces with the bans **active**, and with `--nojit`, so it is not a JIT
   issue at all. Same symptom and call path as the bug closed at `61dbe35e6`
   (log4j-api's `StackLocator.getCallerClass` walks back to the frame of the
   class whose `<clinit>` is currently running; that frame's declaring class
   comes back null). Written up separately as
   `docs/known-issues/stackwalker-getdeclaringclass-null-regression-20260730.md`.
   Kills `AutowiredAnnotationBeanRegistrationAotContributionTests` (0/14) and
   `TestContextAotGeneratorIntegrationTests` (0/4).
2. **`BeanDefinitionMethodGeneratorTests` 10/34 and
   `ApplicationContextAotGeneratorTests` 8/40** remain broken identically in
   every arm. Both were already broken before this session; neither is a
   JIT-ban effect.
3. **`TYPES-ERASURE.1`'s consolidation question** ("does banning
   `Types.erasure` alone subsume the other seven?") is now moot — all eight
   are gone.

## What changed in the tree

* `vm/src/jit/skip_list.rs` — 11 gate blocks and their comments deleted, the
  11 now-unreferenced `SkipReason` variants deleted, 6 ban-asserting unit
  tests inverted to assert JIT-eligibility, 1 new test added covering the five
  methods that had no test of their own. `cargo test --release -p cratonvm-vm
  --lib skip_list`: 70 passed / 3 failed, the same 3 that fail on the
  unmodified file (`classify_complex_ctor_with_putfield`,
  `complex_ctor_keeps_constructor_ban`,
  `generated_proxy_class_is_jit_eligible_after_proxy_jitcall_1_removal`),
  verified by an A/B against the pristine copy.
* This document, moved from `docs/known-issues/jit-bans/` to
  `docs/internal/jit-bans/` per the "known-issues holds only UNFIXED bugs"
  convention.
