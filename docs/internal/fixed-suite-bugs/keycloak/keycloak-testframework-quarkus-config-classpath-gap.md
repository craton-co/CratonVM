# Keycloak test-framework Quarkus config classpath gap

Status: fixed on 2026-07-06 — `org.keycloak.testframework.config.Config` now bootstraps; the top-level `linkage error: no class def found: org/keycloak/testframework/config/Config` crash is gone.

## Fix

This was a classpath-completeness gap, not a CratonVM defect: `quarkus-core`
and several of its runtime dependencies were entirely absent from
`../../../../apps/keycloak/kc-universal-cp.txt`, so `Config.initConfig()`'s
`SmallRyeConfigBuilder` chain died with a top-level `NoClassDefFoundError`
before any test code ran. Iterating the repro (add jar → rerun → next
missing class, same pattern as the earlier smallrye-config/quarkus-core
fixes) surfaced five layers, most of which were already fully resolved in
other checked-out modules and just needed carrying over to
`kc-universal-cp.txt`:

1. `quarkus-core-3.33.1.1.jar` itself, plus its transitive closure already
   resolved by `../../../../apps/keycloak/quarkus/config-api/cratonvm-full-cp.txt`
   (`quarkus-arc`, `quarkus-vertx`, `quarkus-netty`, `quarkus-mutiny`,
   `arc`, `quarkus-security`, etc. — 21 jars) — this is the specific gap the
   doc originally reported (`CharsetConverter`/`MemorySizeConverter`/
   `InetSocketAddressConverter`).
2. The full `io.smallrye.common:smallrye-common-*` 2.16.0 family (12 jars:
   `classloader`, `cpu`, `expression`, `function`, `io`, `net`, `os`,
   `process`, `ref`, `resource`, `version`, `vertx-context`) —
   `smallrye-common-classloader`'s `ClassPathUtils` is needed by
   `PropertiesConfigSource`.
3. `org.ow2.asm:asm-9.7.jar` — `io.smallrye.config.ConfigMappingGenerator`
   (reached from `SmallRyeConfigBuilder.build()`) generates `@ConfigMapping`
   proxy classes via raw ASM `ClassWriter`/`ClassVisitor` at runtime. No
   module checked out in this repo declares ASM as a dependency, so no
   per-module `cratonvm-full-cp.txt` dump ever surfaces it — this one had to
   be found by decompiling `ConfigMappingGenerator`'s constant pool.
4. `jboss-logmanager-3.2.1.Final.jar` (already resolved by
   `quarkus/config-api/cratonvm-full-cp.txt`) — without the real jar,
   CratonVM's classpath-miss fallback produced a `LogManager` that failed
   `ClassCastException: org.jboss.logmanager.LogManager cannot be cast to
   java.util.logging.LogManager`, and `PatternFormatter`'s constructor came
   up as a synthetic-stub `NoSuchMethodError`.
5. `quarkus-bootstrap-runner-3.33.1.1.jar` — the real
   `io.quarkus.bootstrap.logging.InitialConfigurator` (with its
   `DELAYED_HANDLER` field) lives here, not in `quarkus-core` (which only
   ships `Target_io_quarkus_bootstrap_logging_InitialConfigurator`, a
   GraalVM native-image `@TargetClass` substitution stub with a
   deceptively-similar name but no `DELAYED_HANDLER` field of its own).

With all five layers added (~40 new entries), `AccountConsoleDisabledTest`
now runs past `Config.initConfig()` and Quarkus's `LoggingSetupRecorder`
entirely — JUnit 5 starts the actual test method. It then hits a distinct,
unrelated residual: `NoClassDefFoundError:
org/keycloak/testframework/database/EnterpriseDbDatabaseSupplier` (a
`test-framework/db-edb` class that fails to link despite being present and
on the classpath — likely a Testcontainers/Docker-dependent chain, not
investigated further here; this is a normal JUnit-reported test failure, not
a VM-crashing linkage error, so it does not block other test classes that
don't touch the EnterpriseDB supplier).

## Generator script

Per this doc's original "Next Steps," added
`../../../../apps/keycloak-suite-runner/generate-kc-universal-cp.ps1`: given a Maven
module already has its own `cratonvm-full-cp.txt`
(`mvn dependency:build-classpath` dump), the script can pull a *named*
module's resolved jars into `kc-universal-cp.txt` (`-Modules
"quarkus/config-api"`) instead of hand-grepping the local `.m2` repo for
each missing class. It defaults to a dry run (prints the diff, never
writes) and requires `-Apply` to actually update the file — a blind
`-Full` union of every module's classpath was tried and rejected: it
produces a ~475-entry / ~47 KB classpath string vs. the curated file's
~250 / ~22 KB, large enough to hit "Argument list too long" when
`run-keycloak-suite.ps1` passes it directly on a command line (confirmed
empirically), and risks introducing jar version conflicts (e.g. two
different `commons-io` versions) that the curated file never had.

## Validation

- Repro (`docs/known-issues/keycloak-testframework-quarkus-config-classpath-gap.md`'s
  own `AccountConsoleDisabledTest` invocation): before the fix,
  `linkage error: no class def found: org/keycloak/testframework/config/Config`
  aborted the whole process before any JUnit container started. After: `3
  containers found`, `2 containers successful`, JUnit Jupiter's own engine
  runs the test method and reports a normal (non-crashing)
  `NoClassDefFoundError` for the unrelated `EnterpriseDbDatabaseSupplier`
  residual noted above.
- Built and used a dedicated `cratonvm-quarkuscp.exe` binary
  (branch `fix/kc-testframework-quarkus-classpath-20260706`) — this fix
  touches only the local classpath file (gitignored, not part of this
  commit) and a new PowerShell script; no VM/Rust source changed.

## Repro (kept for reference)

```powershell
$KC  = "C:/craton/CratonVM/apps/keycloak"
$CV  = "C:/craton/CratonVM/target/release/cratonvm.exe"
$JDK = "C:/Program Files/Java/jdk-25"
$CP  = "$KC/kc-runner;" + (Get-Content "$KC/kc-universal-cp.txt" -Raw).Trim()
& $CV --java-home $JDK --stack-dump-on-timeout 0 -cp $CP KcRunner org.keycloak.tests.account.AccountConsoleDisabledTest
```
