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

## Layer 3 — `Path.toAbsolutePath()` mangled an existing Windows path — FIXED
CratonVM presents as Windows (`os.name=Windows 11`, `file.separator=\`), but its
`java.nio.file.Path` natives run on Rust `std::path`, which in this build uses **UNIX**
semantics (`MAIN_SEPARATOR='/'`). The concrete break: the active `toAbsolutePath`
native (`native-builtins/src/phases_late.rs`) called `std::fs::canonicalize()` on the
receiver. For an **existing** path that returns a Windows `\\?\C:\…` verbatim path,
which the native rendered as `//?/C:/…` (the `\\?\` prefix was not stripped, unlike
`toRealPath`). WildFly's `Environment.validateWildFlyDir` does
`home.toAbsolutePath().normalize().resolve("jboss-modules.jar")` on the mixed-separator
`jboss.dist` → CratonVM produced `/?/C:/…` → `Files.notExists(...)` → the bogus
"invalid directory" (though `jboss-modules.jar` exists).

**Fix:** the JDK's `toAbsolutePath()` never resolves symlinks or requires existence
(that is `toRealPath`) — it only makes a *relative* path absolute. So drop the
`canonicalize()`: return drive-letter / UNC / leading-separator paths unchanged, and
anchor a relative path to the CWD. Verified ([`MixedSep2`](../../wildfly-suite/repro/MixedSep2.java),
[`AbsCheck`](../../wildfly-suite/repro/AbsCheck.java)): `toAbsolutePath().normalize()`
now yields a clean path, `Files.exists(...resolve("jboss-modules.jar"))` is **true**,
and `validateWildFlyDir` passes (the `WFLYLNCHR0003` "invalid directory" error is gone).
`toAbsolutePath` now matches HotSpot for relative / `..` / absolute inputs (modulo `/`
vs `\`, CratonVM's existing internal convention). No suite regression on sampled classes.

(Residual: `getNameCount`/`getName` still under-count `\`-separated paths under UNIX-mode
`std::path` — `getNameCount("a\\b\\c")=1` vs 3 — but those aren't on WildFly's launcher
path; left as follow-up.)

## Layer 4 (open) — `java.net.ServerSocket.getImpl` NPE on the managed-container port check
With Layer 3 fixed, the managed container starts up, provisions the server config
(`Copying resources … to target\wildfly\standalone\configuration`), and reaches its
port-availability check, where CratonVM throws:
```
NullPointerException: monitorenter in java/net/ServerSocket.getImpl pc=14
  at CommonManagedDeployableContainer.isPortAvailable / waitOnPorts
```
i.e. `ServerSocket.getImpl()`'s synchronized block has a null monitor (the `impl`/lock
is not initialised by CratonVM's `ServerSocket` natives). A separate CratonVM bug — the
next blocker on this path. (And [Gap C / bug-03](bug-03-regex-perf-deployment-build.md)
— the slow deployment-archive build — still gates the eventual deploy.)

## Note — even fully fixed, Gap C still blocks this path
The Surefire fork runs the Arquillian **client** under CratonVM, which builds the
ShrinkWrap deployment archive — that is [bug-03 / Gap C](bug-03-regex-perf-deployment-build.md)
(regex ~50–600× slower), so the archive build is minutes-to-never regardless. The
KRun harness (no Surefire) is the simpler client path; it hits the same Gap C.
