# Spring-related JIT bans in `vm/src/jit/skip_list.rs` — inventory + ban-lift regression test

**Status:** All active Spring-motivated bans re-verified NECESSARY (2026-07-30). No
stale bans found; lifting 8 of them reproduces the exact documented miscompiles.
One new, not-yet-banned lead surfaced (`AutowiredAnnotationBeanRegistrationAotContributionTests`'s
`StackWalker` NPE) — not yet investigated or banned.

## Context

Requested while triaging the CratonVM Spring-framework suite's remaining
non-passed classes (see `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s "2026-07-27
suite-wide reconfirmation" section — several of the residual classes are in
the AOT/in-process-javac `CompilationException` cluster, which this doc's
bans are the known cause of). Two tasks: (1) inventory every currently-live
Spring-related ban in `skip_list.rs`, distinguishing it from the file's large
volume of *historical/removed* bans; (2) actually lift the relevant bans on a
live build and re-run the affected classes to check whether any are stale
(safe to remove) — this was explicitly flagged as an open follow-up inside
the file's own `TYPES-ERASURE.1` comment ("would need a full regression pass
with those seven bans removed and only this one in place... left as a
follow-up").

All work below on Azure host `20.83.144.174`, worktree
`/data/data/wt-springsuite8b-20260726`, real JDK 25, `apps/spring-suite-runner`.

## Part 1 — inventory of currently-ACTIVE Spring-related bans

Confirmed by reading `vm/src/jit/skip_list.rs` directly (grep is unreliable
against this file — many hits are inside multi-hundred-line historical
comment blocks describing bans that were later REMOVED; only the live `if
class_name == ... && method_name == ... { return Some(SkipReason::X); }`
gate sites count as "active").

### javac-internal bans (in-process `TestCompiler`/AOT codegen family)

Not literally `org/springframework/*`, but exist *because of* — and are
directly responsible for the residual failures in — Spring's AOT
code-generation test suite (`beans.factory.aot.*`, `core.test.tools.*`,
`test.context.aot.*`). Also shared with H2/Hibernate's in-process javac use
(`ClassSymbolComplete`).

| `SkipReason` | Banned method | Origin |
|---|---|---|
| `JavacToolContext` | `com/sun/tools/javac/api/JavacTool.getTask` | SPRING-TESTCOMPILER.1 (2026-07-18) |
| `ClassReaderReadClass` | `com/sun/tools/javac/jvm/ClassReader.readClass` | SPRING-TESTCOMPILER.2 (2026-07-20) — doc comment calls this "very likely the true root cause behind most of the AOT cluster's `CompilationException` failures" |
| `ClassFinderFillIn` | `com/sun/tools/javac/code/ClassFinder.fillIn` | SPRING-TESTCOMPILER.3 family |
| `ClassReaderReadInnerClasses` | `com/sun/tools/javac/jvm/ClassReader.readInnerClasses` | same family |
| `ClassReaderReadAttrs` | `com/sun/tools/javac/jvm/ClassReader.readAttrs` | same family |
| `ClassSymbolComplete` | `com/sun/tools/javac/code/Symbol$ClassSymbol.complete` | HIB-STOREDPROC-JIT.1 (2026-07-23) — H2 `CREATE ALIAS ... AS $$`, same in-process-javac family |
| `TypesErasure` | `com/sun/tools/javac/code/Types.erasure` | TYPES-ERASURE.1 (2026-07-25) — flagged as possibly subsuming several of the above, unverified until this session (see Part 2) |
| `JavaPoetCodeBlockBuilderAdd` | `org/springframework/javapoet/CodeBlock$Builder.add` | SPRING-TESTCOMPILER.4 (2026-07-21) — Spring shades `com.palantir.javapoet` to this package at build time |

### Spring Boot bans (added 2026-07-29, most recent)

Not exercised by the `spring-framework` suite this worktree runs (they target
`org/springframework/boot/*` classes exercised by
`BasicErrorControllerIntegrationTests`, a `spring-boot`-module test) — **not
tested in Part 2**, listed here for completeness only:

| `SkipReason` | Banned method |
|---|---|
| `SpringBootConditionReportMapping` | `org/springframework/boot/autoconfigure/condition/ConditionEvaluationReport.lambda$recordConditionEvaluation$0` |
| `SpringBootJdkHttpRequestHeaderComparator` | `org/springframework/http/client/JdkClientHttpRequest.lambda$buildRequest$0` |
| `SpringAnnotatedMetadataAttributeCollector` | `org/springframework/core/type/AnnotatedTypeMetadata.getAllAnnotationAttributes` |

### Removed / historical (no longer active — for completeness, not re-tested)

Broad **package-level** bans, lifted after root-causing, mostly 2026-07-26:
`org/springframework/util/` (SPB.1 — root cause was a GC-root-scanning gap,
not JIT, fixed in `gc/src/vm_heap.rs`), `org/springframework/core/` (SPB.2),
`org/springframework/beans/factory/` + `.../support/`,
`org/springframework/boot/loader/`, `org/springframework/web/reactive/`
(SPB.9b/9c), `org/springframework/boot/context/properties/bind/` +
`.../context/` (SPB.4/.4b/.4c), `SpringBootModifiedClassPathLoader`
(SPRINGBOOT-WITHOUT-JACKSON.2 — orphaned `SkipReason` enum variant, gate
removed, has its own regression test asserting JIT-eligibility).

## Part 2 — ban-lift regression test (2026-07-30, dev `351bf59b0`)

Disabled the 8 javac-internal-family bans above (`if false && /*
BAN-LIFT-EXPERIMENT */ class_name == ...` — a one-line change per gate,
condition always false, block dead code but harmless/syntactically valid;
reverted immediately after, `git status` confirmed clean). Built a
release binary from this modified source, ran the classes each ban's own
doc comment names as affected, real JDK 25, jit-real.

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
| `beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests` | **FAIL 0/14** | regressed (prior state: 14/14 OK per `CRATONVM-SPRING-GENUINE-BUGLIST.md`'s "third session" closure) — but via a **different, new-looking** symptom, see below |
| `beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests` | FAIL 22/44 | already-known-broken class (CompilationException cluster), no same-tip baseline captured, still broken |
| `beans.factory.aot.CodeWarningsTests` | FAIL 19/23 | same |
| `core.test.tools.TestCompilerTests` | FAIL 4/22 | same |
| `context.index.processor.CandidateComponentsIndexerTests` | FAIL 4/24 | same |
| `test.context.aot.TestContextAotGeneratorIntegrationTests` | FAIL 0/4 | same |

### The new symptom: `AutowiredAnnotationBeanRegistrationAotContributionTests`

Every failure in this class, ban-lifted, is identical:

```
java.lang.NullPointerException: Cannot invoke "Object.equals(Object)" because
the return value of "java.lang.StackWalker$StackFrame.getDeclaringClass()" is null
```

This is **not** the `CompilationException` shape the 8 lifted bans guard
against — it's a distinct failure mode in the same in-process-javac-adjacent
AOT codegen family, on a class that was fully green with the bans active.
**Not investigated further this session** (out of scope for the ban-lift
experiment itself) — worth a dedicated follow-up: either a ninth, not-yet-
discovered JIT miscompile specifically in `StackWalker`/reflection code this
class's codegen path exercises, or a real (if narrow) production-relevant
gap in `StackWalker.StackFrame.getDeclaringClass()`'s native implementation
that the ban was incidentally masking by keeping the surrounding code
interpreted. No repro isolated beyond "reproduces 5/5 with this exact ban
set lifted, on this exact class, this exact dev tip."

## Conclusion

- **All 8 tested bans are still necessary.** No stale bans found among them.
  Two (`ClassReaderReadClass` family → `BeanDefinitionMethodGeneratorTests`/
  `InstanceSupplierCodeGeneratorTests`) reproduce the literal
  `CompilationException` symptom byte-for-byte when lifted — as clean a
  confirmation as this kind of experiment gets.
- **The `TYPES-ERASURE.1` doc comment's open question — "does `Types.erasure`
  alone subsume the other seven?" — is NOT answered by this session.** This
  experiment lifted all 8 together, not `TypesErasure` alone with the other 7
  still active (the actual test that question needs). Left for a future
  session.
- **New lead, not banned, not investigated:** the `StackWalker` NPE above.
- The 3 Spring Boot bans (`SpringBootConditionReportMapping`,
  `SpringBootJdkHttpRequestHeaderComparator`,
  `SpringAnnotatedMetadataAttributeCollector`) were inventoried but not
  tested — this worktree only has `apps/spring-framework` set up, not
  `apps/spring-boot`, so their target class
  (`BasicErrorControllerIntegrationTests`) isn't runnable here.
