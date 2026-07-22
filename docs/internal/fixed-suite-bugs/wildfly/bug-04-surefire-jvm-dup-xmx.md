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

## Layer 4 — `ServerSocket.setReuseAddress` NPE on the managed-container port check — FIXED
With Layer 3 fixed, the managed container provisions the server config and reaches its
port-availability check (`isPortAvailable` → `new ServerSocket(port); setReuseAddress(true)`),
where CratonVM threw:
```
NullPointerException: monitorenter in java/net/ServerSocket.getImpl pc=14
  at ServerSocket.setReuseAddress(773) at ServerSocket.getImpl(249)
```
**Root cause:** CratonVM's synthetic `java.net.ServerSocket` surface registers
`<init>`/`bind`/`accept`/… but **not** `setReuseAddress`, so that call fell through to
real JDK bytecode → `getImpl()` does `synchronized (this.socketLock)` (JDK 25,
`getImpl` pc 8-13) on a `socketLock` the synthetic `<init>` never initialises → NPE.

**Fix** (`native-builtins/src/net_phase_e.rs`): service `setReuseAddress`/`getReuseAddress`
directly as no-ops on both `ServerSocket` and `DatagramSocket` (the synthetic stubs do
not model SO_REUSEADDR; `isPortAvailable` calls both). Verified ([`SS3`](../../wildfly-suite/repro/SS3.java)):
`new ServerSocket(0).setReuseAddress(true)` and the `DatagramSocket` equivalent now
succeed. End-to-end, the managed container **passes the port check** and proceeds to
launch the server.

## Layer 5 — `ProcessBuilder.start` rejected the quoted java path — FIXED
With Layer 4 fixed, the managed container builds the standalone-server command and
launches it via `ProcessBuilder.start`, which failed:
```
IOException: ProcessBuilder.start failed:
  program="\"C:\Program Files\Eclipse Adoptium\…\bin\java\"" : os error 123 (ERROR_INVALID_NAME)
```
WildFly's `StandaloneCommandBuilder` wraps the space-containing java path in double
quotes (`"C:\Program Files\…\bin\java"`). The OS `CreateProcess` takes the program and
args separately, so a surrounding-quoted program is a literal filename → Windows error
123. **Fix** (`native-io/src/process.rs`, `spawn_and_wrap`): strip a matched surrounding
quote pair from the program before spawning — a `"` is illegal in a Windows filename, so
the pair is unambiguously quoting (mirrors the JDK's `ProcessImpl`). Verified
([`PBQuote`](../../wildfly-suite/repro/PBQuote.java)): a quoted program path now launches
(`rc=0`); unquoted unchanged ([`PBRepro`](../../wildfly-suite/repro/PBRepro.java), no
regression).

## End of the discrete-bug chain → Gap C (perf)
With Layers 1–5 fixed, the CratonVM Arquillian **client** now runs end-to-end up to the
deployment-archive build: a stack dump lands it in
`DeploymentGenerator → loadAuxiliaryArchives → …ArquillianDeploymentAppender.buildArchive
→ AssetUtil.getFullPathForClassResource` — i.e. **no more discrete bugs gate the path**;
the remaining blocker is [Gap C / bug-03](bug-03-regex-perf-deployment-build.md), the
ShrinkWrap archive build being ~400–1000× too slow (native-bridged char accessors). That
is a JIT-intrinsics performance project, tracked separately.

## Note — even fully fixed, Gap C still blocks this path
The Surefire fork runs the Arquillian **client** under CratonVM, which builds the
ShrinkWrap deployment archive — that is [bug-03 / Gap C](bug-03-regex-perf-deployment-build.md)
(regex ~50–600× slower), so the archive build is minutes-to-never regardless. The
KRun harness (no Surefire) is the simpler client path; it hits the same Gap C.
