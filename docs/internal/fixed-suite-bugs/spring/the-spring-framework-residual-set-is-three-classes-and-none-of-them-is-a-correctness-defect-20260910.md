# The Spring Framework residual set is three classes, and none of them is a CratonVM correctness defect

| | |
|---|---|
| **Status** | Census, 2026-09-10/11. The reference for "what is left in the Spring Framework suite". |
| **Branch** | `claude/spring-residuals-20260910`, binary `cratonvm-springfix2-20260910`. |
| **Suite** | `apps/spring-framework` @ `69bf83ad71` (7.1.0-SNAPSHOT), 2 848 indexed classes, `run-suite.sh`, ZGC, real JDK 25, Azure `20.80.105.49`. |

## The number

| | pre-fix sweep (15:26) | post-fix sweep (19:38) | post-fix, non-`OK` rerun **alone** |
|---|---:|---:|---:|
| classes `OK` | 2 816 / 2 848 | 2 826 / 2 848 | **2 845 / 2 848** |
| test methods found | 30 899 | 30 462 | — |
| test methods **failed** | **123** | **22** | **6** |
| `LOADERR` | 9 | **0** | 0 |

The six remaining failed methods are all in one class, and that class fails the
same six on stock HotSpot.

## The three

| class | status | what it is | record |
|---|---|---|---|
| `aot.nativex.FileNativeConfigurationWriterTests` | `FAIL 3/9` | **Not a CratonVM bug.** HotSpot 25 fails the same six methods with the same counts on the same host, same classpath, same harness — re-verified 2026-09-11 (`HotSpot 2007 ms` / `CratonVM 773 ms`, both `found=9 pass=3 fail=6`). A JSONAssert `NON_EXTENSIBLE` comparison against a `"comment"` key that `RuntimeHintsWriter` writes on both VMs. | `not-cratonvm-bugs-consolidated.md` (retired beside this page) |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | `TIMEOUT` | **Correct, slow.** 14/14 when given time; ≥900 s against HotSpot's 66.5 s on the same loaded host (≈100× on an idle one). 10 001 bean definitions → generate + javac. | `known-issues/spring/beanregistrations-verylarge-throughput-20260907.md` |
| `test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` | `TIMEOUT` | **Correct, slow.** 10/10 repetitions, 10 000/10 000 nested tests `SUCCESSFUL`, 811 s against HotSpot's 26.3 s. Not a hang, and **not** parallelism — turning the parallel switch off widens the ratio. | `known-issues/spring/parallelexecutionspringextensiontests-…-20260910.md` |

A fourth class sits just inside the line and is the same species:
`test.context.aot.AotIntegrationTests`, `OK` at 906 s with `2/4 + 2 skip`,
matching HotSpot's `found=4 succ=2 fail=0 skip=2` exactly — HotSpot does it in
63 s.

## What closed this session

**One defect, 20 classes.** `native-builtins/src/generics.rs`'s type-variable
scope walk never climbed past its first level: the loop body shadowed the
binding it advanced. Every type variable declared by an enclosing scope came
back attributed to the immediate declaration with an `Object` bound. See
`the-type-variable-scope-walk-never-climbed-its-loop-body-shadowed-the-binding-20260910.md`.

**One damaged fixture, 9 classes.** The `LOADERR` cluster was 40 tracked files
deleted from the Spring checkout's working tree on 2026-09-01, hidden by a jar
three weeks older than the deletion. Restored and recompiled; 9/9 `OK` on both
VMs. See
`the-nine-loaderr-classes-were-forty-fixture-files-deleted-from-the-working-tree-20260910.md`.

## How to read a sharded sweep on this host

The post-fix sweep reported **22** non-`OK` classes; rerunning each alone leaves
**3**. Nineteen classes — `XmlBeanFactoryTests`, `PeriodicTriggerTests`,
`SseIntegrationTests`, `ServletAnnotationControllerHandlerMethodTests`,
`ApplicationContextAotGeneratorTests`, `TestClassScannerTests`, the websocket and
reactive integration classes — were load artefacts of eight shards competing with
five other sessions. The corroboration is in the clock, not in an argument:
`sum-class-ms` over the same 2 848 classes was **14.5 M** in the pre-fix sweep
(which had the host to itself) and **61.0 M** in the post-fix one, 4.2× for
identical work.

So a sharded full-suite sweep on this host is a **screen**, never a result.
Phase 2 — rerun every non-`OK` class alone — is not optional, and it is where
three of the twenty-two survived.

And a sweep-to-sweep diff is not a regression report. Five classes went
`OK` → `TIMEOUT` across the two sweeps, which reads exactly like the reflection
fix costing throughput. Two binaries one commit apart, run **concurrently** on
the same workload, said otherwise: 121.2 s median pre-fix, 112.5 s post-fix. The
transitions were the host's.
