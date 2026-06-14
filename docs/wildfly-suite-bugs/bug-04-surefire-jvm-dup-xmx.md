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

## Layer 2 — Surefire ForkedBooter runs fine; managed-container start fails
After the dup-`-Xmx` fix the fork starts but exits **1**. Diagnosed by capturing the
Surefire temp booter jar + prop files mid-run (they're `deleteOnExit`-ed) and running
the `surefirebooter.jar` directly under CratonVM:
- The thin booter jar's manifest `Class-Path:` (relative `../../../../.m2/...`) is
  resolved correctly **once the jar is at the original directory depth** (CratonVM's
  `-jar` manifest Class-Path support works — a first manual run failed only because
  the captured jar was moved, breaking the relative paths).
- CratonVM then **successfully runs ForkedBooter → JUnit Platform → Arquillian** and
  reaches managed-container startup, where it throws:
  ```
  IllegalArgumentException: WFLYLNCHR0003: Invalid directory, could not find
  'jboss-modules.jar' in 'C:\craton\…\smoke/target/wildfly'
  ```
  …though `jboss-modules.jar` **exists** there.

## Layer 3 (root blocker, open) — CratonVM native `Path` uses UNIX separator semantics
CratonVM presents as Windows (`os.name=Windows 11`, `file.separator=\`), but its
`java.nio.file.Path` natives delegate to Rust `std::path`, which in this build uses
**UNIX** semantics (`MAIN_SEPARATOR='/'`: `/` splits, `\` is an ordinary char). So
Windows-style paths mis-parse. Measured ([`SepTest`](../../wildfly-suite/repro/SepTest.java),
[`MixedSep2`](../../wildfly-suite/repro/MixedSep2.java)):

| input | op | HotSpot | CratonVM |
|-------|----|---------|----------|
| `aa\bb\cc` | `getNameCount` | 3 | **1** |
| `C:\x\y/z/w` | `normalize` | `C:\x\y\z\w` | `C:\x\y/z/w` |
| `…smoke/target/wildfly` (mixed) | `getNameCount` | 9 | **3** |
| `…smoke/target/wildfly` | `toAbsolutePath().normalize()` | `C:\…\wildfly` | **`/?/C:/…/wildfly`** (mangled) |

WildFly's `Environment.validateWildFlyDir` does `home.toAbsolutePath().normalize()`
on the mixed-separator `jboss.dist` → CratonVM mangles it → `Files.notExists(...)` →
the bogus "invalid directory".

**Fix direction (dedicated work, not a one-liner):** CratonVM's `Path` natives
(`getNameCount`, `getName`, `getParent`, `getRoot`, `isAbsolute`, `normalize`,
`toAbsolutePath`, `resolve`) must implement **Windows** path semantics when the guest
is Windows (split on both `/` and `\`, recognise the `X:` drive root) instead of
delegating to UNIX `std::path`. This is core NIO shared by the whole suite and several
other apps (with accumulated `/?/` / `/C:/` URI workarounds), so it needs its own
change + full re-test — deferred rather than rushed here.

Note: even with this fixed, the managed/KRom client path then hits
[Gap C / bug-03](bug-03-regex-perf-deployment-build.md) (regex too slow to build the
deployment archive).

## Note — even fully fixed, Gap C still blocks this path
The Surefire fork runs the Arquillian **client** under CratonVM, which builds the
ShrinkWrap deployment archive — that is [bug-03 / Gap C](bug-03-regex-perf-deployment-build.md)
(regex ~50–600× slower), so the archive build is minutes-to-never regardless. The
KRun harness (no Surefire) is the simpler client path; it hits the same Gap C.
