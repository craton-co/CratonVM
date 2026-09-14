# JDK-only corpus runner

Status: **landed, partially exercised** (2026-08-12).
Implementation: `regression-suite/corpus/`.
Usage doc: `regression-suite/corpus/README.md`.

## 1. Why

`regression-suite/run.sh` is 72 small deterministic vectors, currently 69
passed / 2 failed. It is a good gate on the shapes it encodes and it
**licenses almost nothing about real applications**.

Phase 2 of the JDK-only roadmap retires ~3956 native bridges that shadow real
JDK bytecode. The tractable method is to retire by *family* and let real
applications adjudicate whether the retirement broke anything. That method has
no instrument. The corpus is not the constraint — it is already built on this
host — the missing piece is a runner, and the existing per-app runners are the
obstacle, not the solution:

- the H2 runner (`apps/h2database-suite-runner/run-h2-suite.sh`) is written for
  a Linux host (`JDK25=/home/victor/jdk25`) and requires
  `$H2_ROOT/craton-testcp.txt`, which **does not exist** on this host, so its
  own `ensure_built` precondition fails;
- the Tomcat runner is described as wanting a ~20-minute `ant` build.

Both are worked around the same way: **compose the classpath by hand from the
build output that already exists on disk.**

## 2. Corpus inventory as measured, versus as claimed

Measured on this host 2026-08-12 by direct filesystem census. There are **two
complementary corpus roots**, not one, and they are not mirrors:
`C:/craton/cratonvm/apps` and `C:/craton/apps`.

| corpus | root with build output | measured shape | wired? |
|---|---|---|---|
| H2 | `cratonvm/apps/h2database/h2` | 1114 classes, 687 test-classes | yes |
| Spring Framework 7.1 | `cratonvm/apps/spring-framework` | 41 jars, 25 class dirs; module jars under `<m>/build/libs` | yes |
| Tomcat 12 | `apps/tomcat` | 2798 + 1881 classes, 35 jars in `output/build/lib` | yes |
| commons-math 4 | `apps/commons-math` | 18 jars, 19 class dirs; 782 classes in legacy | yes |
| bc-java | `apps/bc-java` | 3085 classes in `core/build/classes` | yes |
| hibernate-orm 8 | `apps/hibernate-orm` | 438 jars, 14 class dirs — **but see §3** | blocked |
| Keycloak | `cratonvm/apps/keycloak` | 764 jars, 124 class dirs | inventoried, not wired |
| WildFly | `cratonvm/apps/wildfly` | 5203 jars, 242 class dirs | inventoried, not wired |
| Elasticsearch | `apps/elasticsearch` | 59 jars, 50 class dirs | inventoried, not wired |
| Kafka | `cratonvm/apps/kafka` | 255 jars, 28 class dirs | inventoried, not wired |
| Spring Boot | — | **genuinely absent** | n/a |

The four "inventoried, not wired" corpora have build output and are wireable by
adding a `corpora.d/<name>.sh`; this lane stopped at the ones with the highest
Phase 1 value rather than shipping four definitions it had not measured a
classpath for. Recording the counts is the point: they are the evidence that
the work remaining is a definition file, not a build.

### Discrepancies against the roadmap's claims

1. **Spring Boot — roadmap is right, the directory is a decoy.**
   `cratonvm/apps/spring-boot` exists, which reads as a contradiction, but it
   contains only `sb-runner` (two `.java` files), zero jars and zero `classes`
   directories. The corpus is genuinely absent. Generalised: **the presence of
   a `*-suite-runner` directory is not evidence that a corpus is built**, and
   this host has nine of them.

2. **Tomcat does not need the ant build.** `output/classes`,
   `output/testclasses` and `output/build/lib` are already fully populated. The
   ~20-minute build described in the brief is not on the critical path here.

3. **The two H2 trees are not interchangeable, and the wrong one looks
   plausible.** `apps/h2database/h2/target/classes` contains exactly **one**
   file — `../../apps/META-INF/versions/21/org/h2/util/Utils21.class` — no
   `org/h2/Driver.class`, and `target/test-classes` is empty. Meanwhile
   `cratonvm/apps/h2database/h2` is fully built **but has no `ext/` directory**,
   while the *unbuilt* root has all 13 dependency jars. **Neither root alone
   composes a working H2 classpath.** The runner unions them.

4. **Hibernate's dumped classpath is dead twice over** — see §3.

## 3. Hibernate: a fixture failure that would have read as a VM defect

`apps/hibernate-orm/cratonvm-test-classpath.txt` is a 40174-byte dumped
classpath. Two independent problems:

- every hibernate-relative line is prefixed
  `C:\craton\CratonVM\apps\hibernate-orm\...`, and **there is no hibernate-orm
  under `cratonvm/apps`** on this host (only a `hib-suite-runner`). The runner
  rewrites this prefix onto the live root.
- after that rewrite, **57 of 241 entries exist and 183 do not**. The missing
  ones are gradle module-cache jars
  (`.gradle/caches/modules-2/files-2.1/jakarta.persistence/...`) that have been
  **evicted** since the dump. No path rewriting recovers them.

Composing the 57 survivors and running anyway would produce a wall of
`NoClassDefFoundError`s indistinguishable from a sweeping linkage regression.
The definition therefore **refuses**, with the diagnosis, when dead > live.
Repair is a gradle resolve — a build, and out of this lane's scope.

## 4. Design

### 4.1 Resolution by built output, never by existence

Each corpus declares `CORPUS_ROOT_CANDIDATES` and a `corpus_is_built` predicate
that tests for a **specific expected artefact** (for H2:
`target/classes/org/h2/Driver.class` *and*
`target/test-classes/org/h2/test/TestBase.class`). The decoy in §2.3 is exactly
what this defends against.

### 4.2 The liveness wrapper

`CorpusMain` wraps `main`-kind workloads and prints `CORPUS-START` /
`CORPUS-END` / `CORPUS-THROW` / `CORPUS-NOMAIN`.

The justification is measured, not stylistic. `run-h2-suite.sh:174` classifies
purely by exit code, and on this host `org.h2.test.unit.TestBitStream` on
HotSpot 25 **exits 0 having printed nothing at all**. A silent `rc=0` cannot
separate "ran and passed" from "never ran" from "launcher resolved no main and
exited 0". Those have disjoint suspects and must not share a cell.

`CORPUS-END completed=true` is printed on the **normal return path**, not from
the shutdown hook, so that hook-delivery differences between two VMs cannot
fail every workload for one shared reason. The hook covers only the
`System.exit` path and labels itself `completed=exit`, so a hook difference
appears as its own signature.

### 4.3 Four failure shapes

`NOSTART` / `CRASH` / `TIMEOUT` / `RAN`, never merged. **A `TIMEOUT` is very
often a SIGSEGV with no result line**, so it is reported as an unadjudicated
hard failure carrying that warning inline, and never as slowness. Default
timeout is 600 s: this host has a suite vector needing ~4 m 55 s, and a 120 s
default manufactures phantom errors that read as independent defects.

The VM's own watchdog is disabled (`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, as
`run.sh:443` does) so `timeout` is the single authority on the wall — two
independent killers make a `TIMEOUT` row impossible to attribute.

### 4.4 Oracle discipline

HotSpot 25 is the oracle. It never receives the mode flag or `--cv-args`. The
two disagreeing-precondition cases — `ORACLE-UNUSABLE` and `ORACLE-VACUOUS`
(`tests=0`, or aborted == tests) — are reported as **unadjudicated**, mirroring
`harness-guard.sh:166`'s G4 for the small-vector suite. A run in which nothing
was adjudicated exits 2.

The comparison key is only the wrapper markers plus any `SBRUNNER_RESULT` line.
Real applications emit timestamps, temp paths, thread names and heap addresses;
diffing raw output would manufacture divergences at a rate that buries real
ones.

### 4.5 Not a benchmark

Wall-clock throughput on this shared, loaded host is worthless — three clean
interleaved A/B pairs have previously turned out to be pure load drift. The
`cv_ms`/`hs_ms` columns exist solely for timeout selection and are labelled
non-metric in the TSV header itself.

## 5. Exercised

- `list`, `info`, `discover` — run for all six corpora. H2 resolves to the
  built root over the decoy; discovery yields **217** concrete test classes
  (intersecting built class files with the source-side
  `public class X extends TestBase|TestDb` rule, so abstract bases and the
  whole-suite driver `TestAll` are excluded).
- `run h2 --class org.h2.test.unit.TestBitStream` — both arms, against a
  **stale 2026-07-26 main-tree binary** (this worktree has no `target/`; this
  lane does not build the VM). HotSpot `RAN` rc=0; CratonVM printed
  `CORPUS-START` then produced nothing and was killed at 300 s → `CV-TIMEOUT`.
  **This is a harness result, not a verdict on any current build.**
- `info hibernate` — refuses with the eviction diagnosis, as designed.
- The JUnit arm (`kind=junit`, `SbRunner`) is **wired but not exercised**: no
  Spring/Tomcat/commons-math workload has been run through it.

## 6. Residuals

1. The JUnit arm is unexercised end-to-end.
2. Keycloak, WildFly, Elasticsearch and Kafka are inventoried but have no
   `corpora.d` definition.
3. Hibernate is blocked on a gradle resolve.
4. No corpus has been run under `--jdk-only`, which is the mode the campaign is
   named for. Nothing in this design prevents it; it needs a VM binary.
