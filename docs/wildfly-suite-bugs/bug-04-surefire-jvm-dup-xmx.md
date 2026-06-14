# Bug 04 — CratonVM rejected as a Surefire `-Djvm=` fork (Gap A: duplicate `-Xmx`)

**Severity:** Medium (CratonVM-only CLI compat). Blocks using CratonVM as the
Maven-Surefire forked test JVM (`-Djvm=.../bin/java.exe`).

**Status: dup-`-Xmx` rejection FIXED** (`vm-cli/src/main.rs`). A deeper Surefire
ForkedBooter init failure remains (see "Remaining").

## Symptom
With CratonVM's `java` bin alias as the Surefire fork JVM, the fork dies instantly:
```
The forked VM terminated without properly saying goodbye. VM crash or System.exit called?
Process Exit Code: 2
```
`Process Exit Code: 2` is clap's "argument error" exit.

## Root cause (fixed part)
Surefire's fork command (captured from the Surefire error) passes **`-Xmx512m`
twice** — once from `surefire.memory.args`, once from the WildFly testsuite's
`jvm.args`:
```
java.exe --add-exports=... --add-opens=... -Xmx512m -Djboss.dist=... -Xmx512m ... -jar surefirebooter.jar <tmp> <jvmRun> <props> <props>
```
The real `java` launcher accepts repeated `-Xmx` (last wins). CratonVM normalizes
each `-Xmx512m` to clap's `--Xmx 512m`, and clap's single-value `--Xmx` aborts with
*"the argument '--Xmx <SIZE>' cannot be used multiple times"* → exit 2.

## Fix
`vm-cli/src/main.rs`: add `overrides_with` (self) to the repeatable single-value
flags so a duplicate is last-wins instead of a hard error, matching `java`:
```rust
#[arg(short = 'c', long = "classpath", alias = "cp", overrides_with = "classpath")]
classpath: Option<String>,
#[arg(long = "Xmx", value_name = "SIZE", overrides_with = "max_heap")]
max_heap: Option<String>,
```
Verified: `java.exe -Xmx512m -Djboss.dist=x -Xmx512m ... -cp … Main` now parses and
runs; the Surefire fork progresses past argument parsing (exit 2 → the fork starts).
CratonVM's `-jar <jar> <args>` + `System.exit(0)` handshake also confirmed clean
(rc=0, output flushed).

## Remaining (open, deeper)
After the arg fix the Surefire fork now exits **1** (was 2) — i.e. CratonVM parses
the full arg set and starts the `surefirebooter.jar`, but the
`org.apache.maven.surefire.booter.ForkedBooter` exits 1 during init (`Tests run: 0`,
no `.dumpstream` written, so no fork output was captured). Basic `-jar`+`System.exit`
works, so this is a booter-specific runtime gap (candidate: `ProcessHandle`-based
parent-liveness `PpidChecker`, or a provider class CratonVM can't load). Diagnosing
needs the fork's own stderr (the Surefire temp booter jar + prop files are deleted
on exit — capture them before cleanup, or run the booter manually).

## Note — even fully fixed, Gap C still blocks this path
The Surefire fork runs the Arquillian **client** under CratonVM, which builds the
ShrinkWrap deployment archive — that is [bug-03 / Gap C](bug-03-regex-perf-deployment-build.md)
(regex ~50–600× slower), so the archive build is minutes-to-never regardless. The
KRun harness (no Surefire) is the simpler client path; it hits the same Gap C.
