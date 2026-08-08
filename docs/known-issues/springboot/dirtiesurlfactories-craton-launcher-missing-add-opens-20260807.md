# `@DirtiesUrlFactories`/`java.net` reflection classes fail 100% on the 20260806 Windows full-suite run — the CratonVM launch args never got `--add-opens=java.base/java.net`

**Status: OPEN — harness bug, not a CratonVM correctness bug. Filed 2026-08-07.**

**Consolidation note:** three parallel triage passes independently found this
exact root cause for disjoint sets of affected classes and each filed its own
doc (`multipart-and-websocket-missing-add-opens-java-net-runner-gap-20260807.md`,
`webfluxmanagementchildcontext-add-opens-not-forwarded-to-craton-20260807.md`).
Folded into this one, the most complete of the three — all 7 known affected
classes are listed in "Affected classes" below; the other two doc files were
removed.

## Symptom

Seven classes, all failing **every single test** with the identical stack shape, on
`craton-fullsuite-windows-20260806` (all shards, `all-jit`):

| Module | Class | Seconds | tests / failed |
|---|---|---:|---:|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.SslConnectorCustomizerTests` | 3.272 | 8/8 |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests` | 12.757 | 66/66 |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests` | 2.107 | 2/2 |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` | 1.435 | 2/2 |
| `module/spring-boot-servlet` | `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests` | 4.122 | 12/12 |
| `module/spring-boot-websocket` | `org.springframework.boot.websocket.autoconfigure.servlet.WebSocketMessagingAutoConfigurationTests` | 2.270 | 13/13 |
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests` | 2.050 | 5/5 |

Every failure, on every test method in every one of these seven classes, is byte-identical:

```
=> java.lang.IllegalStateException: Unable to reset field. Please run with '--add-opens=java.base/java.net=ALL-UNNAMED'
   org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:58)
   org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.beforeEach(DirtiesUrlFactoriesExtension.java:45)
   Suppressed: java.lang.IllegalStateException: Unable to reset field. ...
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.afterEach(DirtiesUrlFactoriesExtension.java:40)
   Caused by: java.lang.reflect.InaccessibleObjectException: Unable to make member accessible: module java.base does not "opens java.net" to unnamed module (use --add-opens to grant access)
     org.springframework.util.ReflectionUtils.makeAccessible(ReflectionUtils.java:803)
     org.springframework.test.util.ReflectionTestUtils.setField(ReflectionTestUtils.java:202)
     org.springframework.test.util.ReflectionTestUtils.setField(ReflectionTestUtils.java:124)
     org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension.reset(DirtiesUrlFactoriesExtension.java:55)
```

Logs (results.tsv rows and hashed log paths):
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s4/all-jit/logs/module_spring-boot-tomcat.*` (Ssl/Tomcat factory customizer/context listener),
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s3/all-jit/logs/module_spring-boot-jetty.*` (Jetty context listener).

## Why these seven classes and not others

`org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension`
(`apps/spring-boot/test-support/spring-boot-test-support/src/main/java/org/springframework/boot/testsupport/web/servlet/DirtiesUrlFactoriesExtension.java:48-61`)
runs before and after **every** test method and does:

```java
ReflectionTestUtils.setField(URL.class, "factory", null);
```

— i.e. `java.net.URL.factory` via reflection, catching `InaccessibleObjectException` and
rethrowing exactly the message quoted above. Two classes carry `@DirtiesUrlFactories`
directly (`TomcatWebServerFactoryCustomizerTests`, `SslConnectorCustomizerTests`); two more
inherit it from `AbstractServletWebServerServletContextListenerTests`
(`module/spring-boot-web-server/src/testFixtures/.../AbstractServletWebServerServletContextListenerTests.java:41`),
their common Tomcat/Jetty base class; and the remaining three
(`MultipartAutoConfigurationTests`, `WebSocketMessagingAutoConfigurationTests`,
`WebFluxManagementChildContextConfigurationIntegrationTests`) pick it up the same way
through their own web-server test fixtures. `spring-boot-webflux`'s own `build.gradle`
documents the requirement directly (`jvmArgs += "--add-opens=java.base/java.net=ALL-UNNAMED"`,
line 68) — independent confirmation this isn't a CratonVM-specific need, Gradle's own test
task already knew to add it for HotSpot. Because the extension's `beforeEach`/`afterEach` both
run it, and both throw, every test method in these seven classes fails twice over
(one exception, one suppressed) — hence 100% failure rate in every one of them, not a
partial/flaky failure.

Grepped every other Spring Boot module for `@DirtiesUrlFactories`/`DirtiesUrlFactories`
usage: no other class in the suite carries it (directly, via the Tomcat/Jetty base class, or
via its own fixture), which is why the confirmed blast radius is exactly these seven. Not
exhaustively verified beyond grep — a module invoking the same reflection through a
differently-named fixture wouldn't show up in that search.

## Root cause: real, deliberate CratonVM fix + a suite-runner gap it exposed

**This is a harness bug in `run-spring-boot-suite.ps1`, not a VM regression.** Two things
combined on 2026-08-06 to produce it:

**1. CratonVM's `setAccessible` module-encapsulation check was fixed the same day** — commit
`7c92363bd` ("fix(reflect): the JEP 403 gate was unregistered by a duplicate 1150 lines
below it", 2026-08-06 14:44 UTC). Before that fix, `setAccessible(true)` on any `java.base`
member silently succeeded on CratonVM regardless of module opens (a duplicate, unchecked
native registration in `register_essential_natives_with_shims` was last-writer-wins over the
checked one — see `native-builtins/src/lang_class.rs:4697-4759`,
`enforce_set_accessible_gate`'s own doc comment). This is exactly what
`fixed-suite-bugs/springboot/mockito-silently-selects-fallback-location-and-memberaccessor-FIXED.md`
observed and noted as *fidelity-only* on 2026-07-27 ("CratonVM does not enforce strong
encapsulation... including a `java.lang.ProcessEnvironment.theEnvironment` read that
requires `--add-opens` on real HotSpot") — that observation was correct **at the time**, and
is now stale: the 08-06 fix closes exactly that gap, verified against Temurin 25 with a
24-question paired probe (`probes/SetAccessibleModuleProbe.java`), both with and without
`--add-opens`, both **identical** to HotSpot.

**2. The suite runner only ever added `--add-opens=java.base/java.net=ALL-UNNAMED` to the
HotSpot launch path, never to CratonVM's.** `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`,
`New-ProcessRecord` (around line 798):

```powershell
if ($Vm -eq 'hotspot') {
    $file = $JavaExe
    # ... comment says "Apply it universally -- opens are additive and harmless
    # for modules that don't need it."
    $args = @("-Xmx$MaxHeap", '-Dfile.encoding=UTF-8', '-Djava.awt.headless=true', '--add-opens=java.base/java.net=ALL-UNNAMED')
    ...
} else {
    $file = $ExePath
    $args = @('--java-home', $JdkPath, '--Xmx', $MaxHeap)
    ...   # no --add-opens anywhere in this branch
}
```

This was added in `948df715a` (2026-07-17, "fix(spring-boot-suite-runner): apply
--add-opens=java.base/java.net for HotSpot runs") **deliberately scoped to the `hotspot`
arm only** — its own commit message explains why: at the time CratonVM did not enforce the
module boundary at all, so the flag was moot there. That was true for three more weeks. The
08-06 encapsulation fix above made it no longer true, and nobody updated the `else` branch
to match — despite the comment directly above the HotSpot-only application literally saying
"Apply it universally," it was never applied to the branch that now needs it.

So: the VM fix is correct and verified (CratonVM now matches HotSpot's `InaccessibleObjectException`
behavior exactly), and the test failures are the harness never granting CratonVM's launch the
same `--add-opens` HotSpot's launch has always gotten. **Not a regression in CratonVM's Java
semantics** — the opposite: CratonVM got *more* correct, and the harness's asymmetric
`--add-opens` application, previously invisible, became live.

## What would fix it

Move (or duplicate) the `--add-opens=java.base/java.net=ALL-UNNAMED` argument into the
`else` branch of `New-ProcessRecord` in `run-spring-boot-suite.ps1` (the line that builds
`$args` for the CratonVM executable, currently `@('--java-home', $JdkPath, '--Xmx', $MaxHeap)`
with no opens flag at all), matching what the `hotspot` branch already does. This is a
harness change, not a VM change, and out of scope for this investigation pass (no source
changes made here per instructions) — flagging it precisely for whoever picks this doc up.

## Not investigated further here

Whether CratonVM's `--add-opens` command-line flag parsing (as opposed to the module-open
bookkeeping it feeds) actually exists and behaves correctly was not verified in this pass —
worth a quick standalone check (`cratonvm --add-opens=java.base/java.net=ALL-UNNAMED ...`
against a small repro that reads `URL.factory` via reflection) before assuming the harness
fix above is the *whole* story.

## Affected classes

- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.SslConnectorCustomizerTests` (8/8 fail)
- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests` (66/66 fail)
- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests` (2/2 fail)
- `module/spring-boot-jetty` — `org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` (2/2 fail)
- `module/spring-boot-servlet` — `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests` (12/12 fail)
- `module/spring-boot-websocket` — `org.springframework.boot.websocket.autoconfigure.servlet.WebSocketMessagingAutoConfigurationTests` (13/13 fail)
- `module/spring-boot-webflux` — `org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests` (5/5 fail)

All seven confirmed via `results.tsv`'s inline HotSpot cross-check to `PASS` on the HotSpot
baseline for this same run (`hotspot-baseline: PASS 0/N` appended to each failing row) —
consistent with the harness giving HotSpot its `--add-opens` and not CratonVM.

**Plausible but unverified:** any other class in `jetty`/`security`/`servlet`/`tomcat`/
`webflux`/`websocket` whose tests depend on the same reflective `URL.factory` reset through a
differently-named fixture (not grepped for specifically) could share this same gap without
having shown up in the three independent triage passes that found the seven above.
