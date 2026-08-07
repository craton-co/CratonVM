# `MultipartAutoConfigurationTests` / `WebSocketMessagingAutoConfigurationTests` — every test FAILs with `IllegalStateException: Unable to reset field` — runner harness gap, not a VM bug

**Status: OPEN, but the root cause is in the suite runner script, not CratonVM.**

## Symptom

2026-08-06 full-suite Windows run (`craton-fullsuite-windows-20260806-s4/all-jit`),
`-Xmx 2g`, 300s/class, default Generational GC:

| Class | Module | Result | Tests failed |
|---|---|---|---:|
| `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests` | `module/spring-boot-servlet` | FAIL, 4.122s | 12/12 |
| `org.springframework.boot.websocket.autoconfigure.servlet.WebSocketMessagingAutoConfigurationTests` | `module/spring-boot-websocket` | FAIL, 2.270s | 13/13 |

Every test in both classes fails identically, in `beforeEach`/`afterEach`
(not the test body itself):

```
=> java.lang.IllegalStateException: Unable to reset field. Please run with '--add-opens=java.base/java.net=ALL-UNNAMED'
   org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:58)
   org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.beforeEach(DirtiesUrlFactoriesExtension.java:45)
   Suppressed: java.lang.IllegalStateException: Unable to reset field. ...
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:58)
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.afterEach(DirtiesUrlFactoriesExtension.java:40)
   Caused by: java.lang.reflect.InaccessibleObjectException: Unable to make member accessible: module java.base does not "opens java.net" to unnamed module (use --add-opens to grant access)
     org.springframework.util.ReflectionUtils.makeAccessible(ReflectionUtils.java:803)
     org.springframework.test.util.ReflectionTestUtils.setField(ReflectionTestUtils.java:202)
     org.springframework.test.util.ReflectionTestUtils.setField(ReflectionTestUtils.java:124)
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:55)
```

`hotspot-baseline` for both classes is clean (`PASS 0/12`, `PASS 0/13` in the
`results.tsv`).

Logs:
- `craton-fullsuite-windows-20260806-s4/all-jit/logs/module_spring-boot-servlet.org.springframework.boot.servlet.autoconfigure.M-3522db47dd3e.{out,err}.log`
- `craton-fullsuite-windows-20260806-s4/all-jit/logs/module_spring-boot-websocket.org.springframework.boot.websocket.autoconfigu-06b735f8a994.{out,err}.log`

## Root cause: confirmed, in `run-spring-boot-suite.ps1`, not CratonVM

`DirtiesUrlFactoriesExtension` (Spring Boot's own test-support extension for
these two web-server-fixture classes) reflectively resets a `java.net`-package
static field between tests via `ReflectionTestUtils.setField`, which needs
`--add-opens=java.base/java.net=ALL-UNNAMED` under JPMS on JDK 17+.

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`'s `New-ProcessRecord`
function builds the launch `$args` differently for the two VM arms:

```powershell
if ($Vm -eq 'hotspot') {
    ...
    # Several modules' own build.gradle add --add-opens=java.base/java.net=ALL-UNNAMED
    # to their Gradle `test` task JVM args (jetty/security/servlet/tomcat/webflux/
    # websocket -- reflective field reset in their web-server test fixtures). ...
    # Apply it universally -- opens are additive and harmless for modules that
    # don't need it.
    $args = @("-Xmx$MaxHeap", '-Dfile.encoding=UTF-8', '-Djava.awt.headless=true', '--add-opens=java.base/java.net=ALL-UNNAMED')
    ...
} else {
    $file = $ExePath
    $args = @('--java-home', $JdkPath, '--Xmx', $MaxHeap)
    ...
    if ($CratonArgs.Count -gt 0) { $args += $CratonArgs }
    $args += @('-Dfile.encoding=UTF-8', '-Djava.awt.headless=true')
    ...
}
```

(`run-spring-boot-suite.ps1:798-825`.) The comment on the `hotspot` branch
explicitly documents *why* the flag is needed and says it should be applied
"universally" — but the `--add-opens=java.base/java.net=ALL-UNNAMED` literal
is only ever added to the `hotspot` arm's `$args`. The `craton` (`else`)
branch has no equivalent line, and `$CratonArgs` (the only other place a flag
could be injected into that branch) defaults to `@()` — nothing in the
current invocation supplies it. So every `craton` launch of these two classes
runs **without** the open, while every `hotspot` launch gets it, which is
exactly the FAIL-craton/PASS-hotspot split in the `results.tsv` row.

Confirmed CratonVM itself supports the flag correctly — it is not a missing
feature: `vm-cli/src/main.rs:534` defines `--add-opens` as a real
`clap` argument (`MODULE/PKG=TARGET`), it is recognized as a value-taking
option in the CLI's own separate-token-parsing table
(`vm-cli/src/main.rs:1062`), and parsed entries are pushed into
`config.add_opens` (`vm-cli/src/main.rs:3442-3448`) exactly like
`--add-exports`/`--add-reads`. If the runner passed
`--add-opens=java.base/java.net=ALL-UNNAMED` to the `craton` launch the same
way it does for `hotspot`, `DirtiesUrlFactoriesExtension.reset` would resolve
through the same JPMS opens path HotSpot uses.

This is not a fidelity or enforcement bug in CratonVM's module system
(`classloading/src/module.rs`'s `check_deep_reflection_access` path, which is
what throws `InaccessibleObjectException` here, is real, JPMS-registry-backed
enforcement — see `docs/internal/arch-2026-07-26/proxy-and-modules.md` §3 for
a full audit of that subsystem, done independently of this class). CratonVM
is correctly enforcing JPMS opens; the harness simply never asked it to open
`java.base/java.net`.

## Fix (not applied — investigation/documentation only per this triage's scope)

In `run-spring-boot-suite.ps1`'s `New-ProcessRecord`, add
`'--add-opens=java.base/java.net=ALL-UNNAMED'` to the `craton`-arm `$args`
array (the `else` branch, around line 814), mirroring the `hotspot` arm. Both
classes should then pass identically to HotSpot — nothing in either class's
own failure content (there isn't any; every failure is exactly the same
`beforeEach`/`afterEach` reflection exception) suggests any other residual
once the flag reaches the process.

## Affected classes

- `module/spring-boot-servlet` — `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests` (12/12 fail)
- `module/spring-boot-websocket` — `org.springframework.boot.websocket.autoconfigure.servlet.WebSocketMessagingAutoConfigurationTests` (13/13 fail)

Both fail via the identical `DirtiesUrlFactoriesExtension.reset` stack; the
runner comment at `run-spring-boot-suite.ps1:800-802` names
jetty/security/servlet/tomcat/webflux/websocket as the module family that
needs this open, so other classes in those modules using the same fixture
base could show the same symptom if/when they hit this extension — not
independently confirmed here, flagged for whoever fixes the runner script to
check while they're in the area.
