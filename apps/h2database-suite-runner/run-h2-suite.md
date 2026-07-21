# H2 Database Suite Runner

`run-h2-suite.sh` runs the H2 Database Engine test suite under CratonVM or
HotSpot, one process per test class. It targets Linux hosts (e.g. the Azure
build host) and is invoked from bash.

Unlike the Spring Boot / Elasticsearch runners, H2 needs no custom JUnit
launcher class. Every `org.h2.test.*` class that extends `TestBase` or
`TestDb` already carries H2's own upstream convention:

```java
public static void main(String... a) throws Exception {
    TestBase.createCaller().init().testFromMain();
}
```

`testFromMain()` runs `config.beforeTest()`, the test's `test()` method, then
`config.afterTest()`, letting any exception/`AssertionError` propagate out of
`main()` uncaught. So each class is already its own one-process-per-class
entry point (exit code 0 = pass, non-zero = fail) - the exact same shape the
ES runner gets from `org.junit.runner.JUnitCore <class>`, just built in.
217 (of 218) discovered classes were verified to follow this convention.

## Layout

```text
apps/h2database-suite-runner/
  run-h2-suite.sh     # driver
  run-h2-suite.md     # this guide
  meta/               # generated class indexes
  out/                # generated run logs/results
```

## Prerequisites

- H2 checkout at `apps/h2database/h2` (a plain `h2database/h2database` clone,
  not git-tracked in this repo - see "Restoring the checkout" below).
- Maven (`mvn`) on `PATH`.
- A real JDK 25 at `/home/victor/jdk25` (or pass `-JDK25`/set `$JDK25`).
- For CratonVM runs, a uniquely named executable. The runner looks for:
  1. `$CRATONVM_BIN`
  2. `target/release/cratonvm-h2database-suite`
  3. `target/release/cratonvm`

## Restoring the checkout

`apps/h2database` is not tracked by git (`apps/` is gitignored repo-wide,
like `apps/spring-boot` and `apps/elasticsearch`). On a fresh host, copy the
known-good reference checkout over (bytecode/sources are platform-independent):

```bash
tar czf h2database.tar.gz -C apps h2database \
  --exclude='h2database/h2/target' --exclude='h2database/h2/data' \
  --exclude='h2database/h2/error.lock' --exclude='h2database/h2/error.txt'
scp h2database.tar.gz <host>:<worktree>/apps/
ssh <host> "cd <worktree>/apps && tar xzf h2database.tar.gz && rm h2database.tar.gz"
```

## One-time setup

`setup` compiles H2's main + test sources and generates the test classpath:

```bash
cd apps/h2database-suite-runner
./run-h2-suite.sh setup
```

This runs `mvn test-compile` (compiles `src/main` + `src/test` + `src/tools`,
and copies the `.sql`/`.properties` test resources the script tests need) and
`mvn dependency:build-classpath -DincludeScope=test` to a Linux-native,
`:`-joined `apps/h2database/h2/craton-testcp.txt`.

## Discovery

```bash
./run-h2-suite.sh discover
```

Scans `apps/h2database/h2/src/test/org/h2/test/**/*.java` for concrete
(non-abstract) classes matching `extends TestBase` / `extends TestDb` and
writes:

```text
meta/all-classes.tsv     fully.qualified.ClassName
```

This intentionally mirrors what `org.h2.test.TestAll` itself does (its
`test()` method is just a long hand-written `addTest(new TestXxx())` list) -
the regex-based discovery here is a generic substitute for that hardcoded
list so newly added H2 test classes are picked up automatically.

## Categories

```text
passed            classes whose baseline result status was PASS
failed / others   every other class: FAIL, HANG, CRASH, or never run
```

```bash
./run-h2-suite.sh categorize out/jit-real-all-YYYYMMDD-HHMMSS/results.tsv
# or, to run the canonical jit-real baseline first:
./run-h2-suite.sh categorize
```

## Commands

```bash
./run-h2-suite.sh setup
./run-h2-suite.sh discover
./run-h2-suite.sh categorize [results.tsv]

./run-h2-suite.sh run     [options]
./run-h2-suite.sh quad    [options]
./run-h2-suite.sh hotspot [options]
```

| Option | Meaning | Default |
|---|---|---:|
| `--category passed\|failed\|all` | class set | `all` |
| `--jit on\|off` | JIT toggle for `run` | `on` |
| `--jdk real\|synthetic` | JDK backend for `run` | `real` |
| `--start N` | 1-based start index | `0` |
| `--count N` | number of classes, `0` for all | `0` |
| `--only REGEX` | filter fully-qualified class names | none |
| `--shard I/M` | round-robin shard | none |
| `--class-to S` | per-class timeout seconds | `300` |
| `--max-heap H` | heap for both VMs | `1g` |
| `--tag NAME` | output directory prefix | none |

Examples:

```bash
# First 50 classes, CratonVM JIT-on, real JDK.
./run-h2-suite.sh run --category all --count 50

# Only the previously-failing classes, JIT off.
./run-h2-suite.sh run --category failed --jit off

# All four CratonVM modes at once.
./run-h2-suite.sh quad --category all --count 218

# HotSpot timing baseline for the same slice.
./run-h2-suite.sh hotspot --category all --count 218
```

### The four CratonVM modes

| Mode | JIT | JDK backend | CratonVM flags |
|---|---|---|---|
| `jit-real` | on | real JDK | `--java-home <JDK25> --Xmx <heap>` |
| `nojit-real` | off | real JDK | `--java-home <JDK25> --Xmx <heap> --nojit` |
| `jit-syn` | on | synthetic JDK | `--Xmx <heap>` |
| `nojit-syn` | off | synthetic JDK | `--Xmx <heap> --nojit` |

`hotspot` runs the same class slice directly under `$JDK25/bin/java` for a
timing/correctness baseline.

## Per-class isolation

H2's `TestBase.BASE_TEST_DIR` is `./data` (CWD-relative), and
`TestBase.logThrowable()` writes `error.lock`/`error.txt` the same way. Each
class therefore runs in its own scratch working directory
(`out/<mode-tag>/workdirs/<class>`), deleted immediately after the class
finishes, so parallel/successive runs never collide and disk usage stays
bounded across a full-suite pass.

## Output

```text
out/<tag-><mode>-<category>-<timestamp>/
  results.tsv     idx  class  status  rc  ms  tests  mode  log  note
  timing.tsv      class  ms  status  mode
  summary.txt     status tally + wall-clock
  run.log         driver log
  logs/           per-class combined stdout+stderr
  workdirs/       per-class scratch CWD (removed after each class)
```

Status values:

- `PASS` - process exited 0.
- `FAIL` - process exited non-zero, no crash fingerprint (typically an
  uncaught `AssertionError`/exception from `test()`).
- `HANG` - hit `--class-to` and was killed (`timeout` exit 124/137).
- `CRASH` - fatal signal/panic/access-violation fingerprint in the combined
  log, regardless of exit code.

Runs are resumable in spirit (each mode/category/timestamp gets a fresh
output directory); rerun `categorize` against the latest `results.tsv` to
refresh `passed.tsv`/`others.tsv` before slicing a retry.
