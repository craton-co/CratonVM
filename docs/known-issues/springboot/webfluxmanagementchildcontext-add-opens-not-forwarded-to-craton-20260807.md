# `WebFluxManagementChildContextConfigurationIntegrationTests` FAILs 5/5 on Windows full-suite — the runner never forwards `--add-opens=java.base/java.net=ALL-UNNAMED` to the CratonVM launch

**Status: OPEN — harness bug in `run-spring-boot-suite.ps1`, not a CratonVM defect.**

## Symptom

`module/spring-boot-webflux`'s
`org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests`
FAILed 5/5 (all tests in the class) on the 2026-08-06 Windows full-suite run,
2.050s
(`craton-fullsuite-windows-20260806-s4/all-jit/logs/module_spring-boot-webflux.org.springframework.boot.webflux.autoconfigure.a-4f03cf739f2e.{out,err}.log`).
`results.tsv` records `hotspot-baseline: PASS 0/5` for the same class in the
same shard, so this is CratonVM-only.

Every one of the 5 tests fails identically:

```
=> java.lang.IllegalStateException: Unable to reset field. Please run with '--add-opens=java.base/java.net=ALL-UNNAMED'
   org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:58)
   org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.beforeEach(DirtiesUrlFactoriesExtension.java:45)
   Suppressed: java.lang.IllegalStateException: Unable to reset field. Please run with '--add-opens=java.base/java.net=ALL-UNNAMED'
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:58)
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.afterEach(DirtiesUrlFactoriesExtension.java:40)
   Caused by: java.lang.reflect.InaccessibleObjectException: Unable to make member accessible: module java.base does not "opens java.net" to unnamed module (use --add-opens to grant access)
     org.springframework.util.ReflectionUtils.makeAccessible(ReflectionUtils.java:803)
     org.springframework.test.util.ReflectionTestUtils.setField(ReflectionTestUtils.java:202)
     org.springframework.test.util.ReflectionTestUtils.setField(ReflectionTestUtils.java:124)
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:55)
```

`DirtiesUrlFactoriesExtension` (a JUnit extension in `spring-boot-testsupport`
that every test in this class picks up) reflectively resets a private static
field on `java.net.URL` before/after each test, and needs
`--add-opens=java.base/java.net=ALL-UNNAMED` to do it — exactly the same
requirement the module's own `build.gradle` documents:

```
apps/spring-boot/module/spring-boot-webflux/build.gradle:68:	jvmArgs += "--add-opens=java.base/java.net=ALL-UNNAMED"
```

## Root cause: `run-spring-boot-suite.ps1` only forwards the flag to `hotspot` launches

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`'s `New-ProcessRecord`
builds two different argument lists depending on `$Vm`:

```
apps/spring-boot-suite-runner/run-spring-boot-suite.ps1:798-811
  if ($Vm -eq 'hotspot') {
    $file = $JavaExe
    # Several modules' own build.gradle add --add-opens=java.base/java.net=ALL-UNNAMED
    # to their Gradle `test` task JVM args (jetty/security/servlet/tomcat/webflux/
    # websocket -- reflective field reset in their web-server test fixtures). This
    # runner launches SbRunner directly instead of through Gradle's test task, so
    # none of those per-module jvmArgs apply; without it those classes fail with
    # "IllegalStateException: Unable to reset field" on real HotSpot too, which is
    # a harness gap, not a genuine VM behavior difference. Apply it universally --
    # opens are additive and harmless for modules that don't need it.
    $args = @("-Xmx$MaxHeap", '-Dfile.encoding=UTF-8', '-Djava.awt.headless=true', '--add-opens=java.base/java.net=ALL-UNNAMED')
    if ($NoJit) { $args += '-Xint' }
    ...
  } else {
    $file = $ExePath
    $args = @('--java-home', $JdkPath, '--Xmx', $MaxHeap)
    ...
    if ($NoJit) { $args += '--nojit' }
    if ($CratonArgs.Count -gt 0) { $args += $CratonArgs }
    $args += @('-Dfile.encoding=UTF-8', '-Djava.awt.headless=true')
    ...
  }
```

The `hotspot` branch's comment explains, correctly, *why* the flag has to be
added by the runner at all (it invokes `SbRunner` directly instead of through
Gradle's `test` task, so the module's own `build.gradle` `jvmArgs` never
apply) — and fixed exactly this gap for HotSpot launches. But the `else`
branch (the CratonVM launch path — `$ExePath`, i.e.
`cratonvm-fullsuite-20260806.exe`) never got the same treatment: its `$args`
array has no `--add-opens` entry at all, and nothing in `$CratonArgs` (the
suite's global extra-flags list) adds one either.

CratonVM itself supports the identical `--add-opens=<module>/<package>=<target>`
CLI syntax and enforces module encapsulation for reflective access the same
way HotSpot does — `vm/src/config.rs` parses `add_opens: Vec<(String, String,
String)>` from `--add-opens`/`--add-exports` (see
`vm/src/config.rs:496-498,1062`), and `vm/src/vm/vm_init.rs:1064-1065` wires
each parsed triple into `ModuleRegistry::add_opens`. The
`InaccessibleObjectException` in the log is CratonVM's module system doing
exactly what it is supposed to do when nobody told it to open
`java.base/java.net` — the runner just never asks it to, unlike the parallel
HotSpot path that already learned this lesson for the same six modules
(jetty/security/servlet/tomcat/webflux/websocket, per the comment's own
list).

This is a harness omission, not a CratonVM correctness bug: CratonVM enforces
the module boundary correctly given the flags it was actually launched with;
the flags just never arrive for the `craton` VM. The same six modules the
HotSpot-branch comment already names are all candidates for the identical gap
on the CratonVM side of the runner (not independently verified for the other
five this session — this doc only confirms the WebFlux instance, since that
is the one the 2026-08-06 run's failures actually hit).

## Not a repeat of this class's earlier hang/loader history

`docs/internal/fixed-suite-bugs/springboot/webfluxmanagementchildcontext-hibernatevalidator-classloader-hang-FIXED.md`
tracked this exact class's history through several rounds of a real hang and
then a real `ObjectProvider`/loader-identity bug, closed with "5/5 PASS in
both `--nojit` and JIT modes" as of its last update. That symptom shape
(`IllegalStateException: Unstarted application context ...` /
`NoSuchBeanDefinitionException`) does not appear anywhere in this run's logs
at all — every one of the 5 failures here is the `--add-opens` one, thrown
before any Spring context work happens (`DirtiesUrlFactoriesExtension` is a
`beforeEach`/`afterEach` JUnit hook, outside the test body entirely). This is
a new, unrelated failure mode surfacing on top of that already-fixed
class, not a regression of it.

## Suggested fix (not applied — investigation/documentation only)

Add the same `--add-opens=java.base/java.net=ALL-UNNAMED` entry to the
`else` branch's `$args` in `New-ProcessRecord`
(`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`, around line 814),
mirroring the `hotspot` branch immediately above it. This is a one-line
runner change with no CratonVM source involved.

## Affected classes

- `module/spring-boot-webflux` — `org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests`
  (5/5 FAIL, confirmed this session)
- Plausible but unverified this session: any class in `jetty`/`security`/
  `servlet`/`tomcat`/`websocket` whose tests depend on
  `DirtiesUrlFactoriesExtension` or another `--add-opens=java.base/java.net`
  consumer, per the HotSpot-branch comment's own module list.
