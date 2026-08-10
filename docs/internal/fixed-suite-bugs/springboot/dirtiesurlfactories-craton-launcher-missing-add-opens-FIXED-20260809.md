# `@DirtiesUrlFactories` classes failed 100% under CratonVM — the suite runner never gave CratonVM's launch the `--add-opens=java.base/java.net` HotSpot's launch always got

**Status: FIXED 2026-08-09** (branch `fix/springboot-add-opens-java-net-20260809`).
Filed 2026-08-07 as
`docs/known-issues/springboot/dirtiesurlfactories-craton-launcher-missing-add-opens-20260807.md`,
itself a consolidation of three independently-filed duplicates
(`multipart-and-websocket-missing-add-opens-java-net-runner-gap-20260807.md`,
`webfluxmanagementchildcontext-add-opens-not-forwarded-to-craton-20260807.md`).

## What the original doc got right

All of it. The symptom, the mechanism, and the prescribed fix were correct:

* `org.springframework.boot.testsupport.web.servlet.DirtiesUrlFactoriesExtension`
  runs `ReflectionTestUtils.setField(URL.class, "factory", null)` in **both**
  `beforeEach` and `afterEach`, catches `InaccessibleObjectException`, and
  rethrows `IllegalStateException: Unable to reset field. Please run with
  '--add-opens=java.base/java.net=ALL-UNNAMED'`. Every test method in an
  affected class therefore fails twice over — hence 100%, not partial.
* CratonVM's `setAccessible` module-encapsulation gate was **fixed** on
  2026-08-06 (`7c92363bd`). Before that it did not enforce the module boundary,
  so the flag was moot on CratonVM and its absence was invisible.
* `run-spring-boot-suite.ps1`'s `New-ProcessRecord` applied
  `--add-opens=java.base/java.net=ALL-UNNAMED` in the `hotspot` branch only
  (`948df715a`, 2026-07-17), deliberately, for exactly that reason — while the
  comment directly above it said "Apply it universally."

So the VM got *more* correct and a pre-existing harness asymmetry went live.
Not a VM regression.

## What changed to close it

### 1. The harness gap (the doc's prescribed fix)

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`, `New-ProcessRecord`:
the flag is now built once into `$addOpensJavaNet` above the `if ($Vm -eq
'hotspot')` split and applied in **both** arms, so "universally" is structural
rather than a comment a future edit can contradict.

`apps/spring-boot-suite-runner/run-single-method.ps1` had the identical gap and
is CratonVM-only, so it was silently 100%-red for any method of an affected
class. Fixed the same way. The original doc did not mention this script.

### 2. A real CratonVM defect the doc's "not investigated further" section asked about

The doc closed with:

> Whether CratonVM's `--add-opens` command-line flag parsing (as opposed to the
> module-open bookkeeping it feeds) actually exists and behaves correctly was
> not verified in this pass — worth a quick standalone check.

It was worth it. The flag exists, is parsed, reaches `ModuleRegistry`, and does
grant the access — but it **over-granted**, and the divergence is observable
from Java.

`VmConfig::parse_add_exports` (which serves both `--add-exports` and
`--add-opens`) folded the JDK's `ALL-UNNAMED` target into the empty string. The
empty string is `ModuleRegistry`'s marker for an *unqualified* edge — open to
every module in the process. `ALL-UNNAMED` is not that: it opens to unnamed
modules only. Measured against Temurin 25 with
`probes/AddOpensFlagProbe.java`, under
`--add-opens=java.base/java.net=ALL-UNNAMED`:

| line | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `open java.net unqualified` (`Module.isOpen(String)`) | false | **true** | false |
| `open java.net toSelf` (`Module.isOpen(String, Module)`) | true | true | true |
| `URL.factory setAccessible` | OK | OK | OK |

Two consequences: `Module.isOpen(pkg)` lied, and any *named* module in the
process got the deep-reflection grant along with the unnamed one.

This is the same conflation, one layer up, that the
`Module.addOpens(String, Module)` **native** already had to fix with its own
sentinel — see `UNRESOLVED_TARGET_MODULE` in
`native-builtins/src/phases_late/reflect_invoke.rs`, added after widening a
Mockito `InstrumentationMemberAccessor` open to "everyone" made
`Module.isOpen("java.lang")` answer true and broke
`AotIntegrationTests#endToEndTestsForBeanOverrides`. The launcher path was
never audited for it.

Fix: `classloading/src/module.rs` gains `ALL_UNNAMED_TARGET` and
`resolve_edge_target`, so `add_exports`/`add_opens` resolve the token to a
*qualified* edge naming `UNNAMED_MODULE`; `parse_add_exports` passes the token
through verbatim instead of collapsing it.

`parse_add_reads` deliberately still maps `ALL-UNNAMED` to the empty string —
a read edge names a source module, where `""` is the unnamed module's own name
rather than a wildcard. That asymmetry is now spelled out at both sites.

## Blast radius: eight classes, not seven

The original doc listed seven and flagged as "plausible but unverified" that a
class reaching the same reflection through a differently-named fixture could
have been missed. It was:

**`module/spring-boot-security` —
`org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests`**

carries `@DirtiesUrlFactories` directly (line 66) and is confirmed red pre-fix /
green post-fix in the A/B below. It did not surface in the three triage passes
because they read the failing rows of a partial 2026-08-06 shard set rather
than the annotation's closure. Its last full-suite row was `PASS` on
2026-08-05 — i.e. *before* the encapsulation fix made the flag load-bearing.

The closure is now enumerated rather than sampled, and it is exactly eight.
Method:

* `DirtiesUrlFactoriesExtension` is the **only** site in the whole Spring Boot
  tree that reflects on a `java.net` type —
  `grep -rn 'setField\((URL|URLConnection|InetAddress|Socket|HttpURLConnection)\.class'`
  over `apps/spring-boot` returns one hit. So `@DirtiesUrlFactories` really is
  the whole population; there is no differently-named fixture doing the same
  thing by hand.
* Six classes carry the annotation directly; `AbstractServletWebServerServletContextListenerTests`
  carries it and has exactly two concrete subclasses (Tomcat, Jetty). JUnit
  Jupiter finds class-level `@ExtendWith` up the superclass chain, so both
  inherit it.

| Module | Class | pre-fix (unfixed runner) | HotSpot, serial |
|---|---|---|---|
| `module/spring-boot-tomcat` | `SslConnectorCustomizerTests` | FAIL 8/8 | PASS 8 |
| `module/spring-boot-tomcat` | `autoconfigure.TomcatWebServerFactoryCustomizerTests` | FAIL 66/66 | PASS 66 |
| `module/spring-boot-tomcat` | `servlet.TomcatServletWebServerServletContextListenerTests` | FAIL 2/2 | PASS 2 |
| `module/spring-boot-jetty` | `autoconfigure.servlet.JettyServletWebServerServletContextListenerTests` | FAIL 2/2 | PASS 2 |
| `module/spring-boot-servlet` | `autoconfigure.MultipartAutoConfigurationTests` | FAIL 12/12 | PASS 12 |
| `module/spring-boot-websocket` | `autoconfigure.servlet.WebSocketMessagingAutoConfigurationTests` | FAIL 13/13 | PASS 13 |
| `module/spring-boot-webflux` | `autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests` | FAIL 5/5 | PASS 5 |
| **`module/spring-boot-security`** | **`autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests`** | **FAIL 1/1** | **PASS 1** |

Every pre-fix failure log carries `Unable to reset field`; the HotSpot column
(8/8, 109 tests, 0 failures, 79s serial) is this suite's own reference for what
these classes should do.

## Verification

Everything below used one binary,
`cratonvm-addopens-20260809.exe` (release, branch head), against the same
Temurin 25 at `C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`.

**Paired probe.** `probes/AddOpensFlagProbe.java`, four arms —
(HotSpot, CratonVM) x (with, without the flag). Transcripts in
`probes/AddOpensFlagProbe.expected.txt`. Post-fix CratonVM is byte-identical to
HotSpot in both arms. The probe is discriminating rather than vacuous: the
WITHOUT arm is red on both VMs, and two control lines
(`open java.util toSelf`, `ArrayList.elementData setAccessible`) stay denied in
every arm, so a gate that simply stopped enforcing would not pass it.

**Runner A/B.** The same eight classes, the same binary, the same class list —
the only variable is which copy of `run-spring-boot-suite.ps1` launches them:

* pre-fix runner (the unmodified copy in the main worktree): **8/8 FAIL**,
  every one carrying `Unable to reset field` in its stdout log.
* post-fix runner (this branch): **`Unable to reset field` count is zero in
  all eight logs**, parallel and serial alike.

Running the unmodified runner as the red arm — rather than reasoning about what
it would have done — is what makes the green arm mean anything.

**The eight classes are NOT certified green, and this host cannot certify
them.** With the flag granted, seven of the eight still fail — but every single
failure block in every log is "cannot start a web server on port 8080"
(`ConnectorStartFailedException` / `ApplicationContextException: Unable to
start web server`), with **zero** occurrences of this doc's defect:

| class | `Unable to reset field` | port-bind failures | total failure blocks |
|---|---:|---:|---:|
| `SslConnectorCustomizerTests` | 0 | 0 | 0 |
| `TomcatWebServerFactoryCustomizerTests` | 0 | 72 | 36 |
| `TomcatServletWebServerServletContextListenerTests` | 0 | 4 | 2 |
| `JettyServletWebServerServletContextListenerTests` | 0 | (Jetty wording) | 2 |
| `MultipartAutoConfigurationTests` | 0 | 20 | 10 |
| `WebSocketMessagingAutoConfigurationTests` | 0 | 26 | 13 |
| `WebFluxManagementChildContextConfigurationIntegrationTests` | 0 | 16 | 5 |
| `SecurityFilterAutoConfigurationEarlyInitializationTests` | 0 | 2 | 1 |

The cause is the measurement environment, not the change: throughout these
runs an unrelated CratonVM process from **another worktree**
(`CratonVM-symlink-20260809`, pid 21388) held `0.0.0.0:8080` continuously —
sampled ten times over twenty seconds, occupied every time — on a host running
14 concurrent CratonVM processes across 6 worktrees. These classes start
embedded containers on the default port 8080, and they had never run to
completion before this fix, so nothing had ever exercised that collision.

Two controls were run rather than assuming:

* `probes/ServerSocketPortContentionProbe.java`, paired against Temurin 25 with
  8080 held. CratonVM and HotSpot agree on **every** bind row — wildcard fails,
  loopback succeeds, `ServerSocketChannel` fails in all three `SO_REUSEADDR`
  settings. CratonVM's bind semantics are not the difference.
* The HotSpot arm re-run *while 8080 was held*: 8/8 PASS. HotSpot finishes
  these classes in ~10s each where CratonVM takes 60–415s, so its exposure to
  a flapping port is a fraction of CratonVM's. That is a throughput
  consequence, and it is the subject of a separate open doc
  (`tomcat-jetty-servletwebserverfactorytests-300s-budget-overrun-20260807.md`,
  ~4–4.5x HotSpot on the same workload) — not something this fix introduced or
  can settle.

So: the defect this doc is about is closed and proven closed. Certifying these
eight classes green needs a re-run on an idle host, and that is called out in
the residuals below rather than papered over.

**Unit tests.** `classloading::module` gains three regression tests pinning the
`ALL-UNNAMED` semantics as three separate facts (grants the unnamed accessor /
leaves `isOpen(pkg)` false / does not reach a named module), because a fix that
only satisfies the first passes the reflection path while still lying to
`Module.isOpen` and over-granting. The `--add-exports` half is pinned too, so a
later edit cannot fix one direction and leave the other conflated.
`vm/tests/new19_module_access.rs`'s `--add-opens ...=ALL-UNNAMED` test now
passes the real token instead of `""` — with `""` it was green whether or not
the ALL-UNNAMED path worked at all.

## Left open, deliberately

**Four classes carry a commented-out `@DirtiesUrlFactories` in the vendored
`apps/spring-boot` checkout** and so cannot hit this bug today:
`JettyWebServerFactoryCustomizerTests`, `AbstractServletWebServerFactoryTests`
(→ `JettyServletWebServerFactoryTests`, `TomcatServletWebServerFactoryTests`),
`AbstractReactiveWebServerAutoConfigurationTests` (→ Jetty/Netty/Tomcat
`ReactiveWebServerAutoConfigurationTests`), and
`NettyReactiveWebServerAutoConfigurationTests`. Confirmed absent from the
compiled bytecode too (`javap -v` finds no `DirtiesUrlFactories` constant in
the `build/classes` artifacts the runner actually executes), so this is the
live state, not a stale source file.

Not re-enabled here. `apps/` is gitignored and unversioned, the checkout's
mtimes are uniform (all 2026-07-11 17:25, the extraction time) so there is no
local-edit evidence either way, and `module/spring-boot-reactor-netty`'s own
`build.gradle` has no `--add-opens` line — which is consistent with the
reactive fixture's annotation being genuinely off upstream rather than disabled
here as a workaround. Re-enabling would need a Gradle test recompile and an
upstream diff to justify it. The runner now grants the flag unconditionally, so
if those annotations ever go live they are already covered.

**Re-run the eight classes on an idle host.** Nothing about the fix is
outstanding, but the green column above is HotSpot's, not CratonVM's, for the
reason set out under Verification. The re-run needs a host with no other
CratonVM process holding port 8080.

**A separate `ServerSocketChannel` divergence, found by the contention probe
and unrelated to this doc.** On the *control* row — binding a port nothing
holds — CratonVM and HotSpot disagree twice:

```
                          HotSpot 25                      CratonVM
channel control 18087 :   OK true  localAddr=/[::]:18087  OK false localAddr=/127.0.0.1:18087
```

The request was `bind(new InetSocketAddress("0.0.0.0", 18087))` with
`setOption(SO_REUSEADDR, true)`. CratonVM reads `SO_REUSEADDR` back as
**false** after setting it to true, and reports the bound address as
**127.0.0.1** where a wildcard bind was asked for and HotSpot reports the
wildcard. A server that is really bound to loopback is unreachable off-box, and
an option that does not stick is one a framework cannot rely on. Not chased
here — it is a `java.nio.channels` bind-path issue with no connection to
`--add-opens` — but it is reproducible in one command with
`probes/ServerSocketPortContentionProbe.java` and wants its own investigation.

*(One item that was on this list is now closed — see "The runtime `…ToAllUnnamed`
edges" below.)*

## The runtime `…ToAllUnnamed` edges had the same conflation (fixed 2026-08-10)

The launcher was not the only place that folded `ALL-UNNAMED` into "everyone".
The same bug sat on the **runtime** path, where agents, ByteBuddy and Mockito's
`InstrumentationMemberAccessor` reach it. Audited and fixed in three
registration sites — the task that prompted this named one; the other two came
out of the audit:

| Site | Was | Now |
|---|---|---|
| `jboss_jdkspecific.rs` `addExportsToAllUnnamed0` | `native_module_add_exports_to_all0` (empty target) | `native_module_add_exports_to_all_unnamed0` |
| `lib.rs` `implAddExportsToAllUnnamed` | `native_module_impl_add_exports_all` (`target_index: None` → `""`) | `native_module_impl_add_exports_to_all_unnamed` |
| `lib.rs` `implAddOpensToAllUnnamed` | `native_module_impl_add_opens_all` (same) | `native_module_impl_add_opens_to_all_unnamed` |

The last two are what `java.lang.System$1`'s `addExportsToAllUnnamed` /
`addOpensToAllUnnamed` delegate to (`shared_secrets_bridge.rs`). The `opens`
half was not mentioned in the original widening's comment at all.

Deliberately **not** changed: `jboss_jdkspecific.rs`'s `is_open` branch passes
`""` for a genuinely `open module M { }`, where unqualified is the correct
edge.

`probes/ModuleToAllUnnamedProbe.java` drives the natives through
`SharedSecrets.getJavaLangAccess()` and reads back the *unqualified* queries —
the only observation that separates an over-grant from a correct grant, since
both let the unnamed module through:

| | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `export after unqualified` | false | **true** | false |
| `export after toSelf` | true | true | true |
| `open after unqualified` | false | **true** | false |
| `open after toSelf` | true | true | true |

Transcripts in `probes/ModuleToAllUnnamedProbe.expected.txt`.

### Re-measuring the `IllegalAccessError` vector

The widening's stated justification was that the only alternative considered
was *dropping* the edge, which produced spurious `IllegalAccessError`s.

The primary argument that narrowing cannot reopen that vector is structural,
not statistical: **the edge is not dropped.** `export after toSelf=true` above
is the grant to the unnamed module, and classpath code — which is what raised
those errors — lives in the unnamed module. What changes is only that *named*
modules stop receiving a grant HotSpot never gave them.

The empirical check is a same-runner, same-class-list A/B whose only variable
is the binary (widened vs narrowed), over 14 Mockito-using classes across
`spring-boot-actuator`, `-health`, `-jdbc`, `-http-client` and
`core/spring-boot-test`. Mockito 5.23 with `byte-buddy-agent` on the test
classpath means the inline mock maker self-attaches, so every `mock()` here
drives `Instrumentation.redefineModule` and therefore these very natives.

| | widened | narrowed |
|---|---|---|
| classes | 14 PASS | 14 PASS |
| tests / failed | 146 / 0 | 146 / 0 |
| logs containing `IllegalAccessError` | 0 | 0 |
| logs containing `InaccessibleObjectException` | 0 | 0 |

Per-class verdicts are identical.

**What this does and does not show.** The widened arm also produced zero
`IllegalAccessError`s, so this class set does not reproduce the original vector
— the A/B establishes *no regression*, not *the vector was reproduced and
survived*. Whatever raised those errors is not in this sample, and the note
that recorded them never named a class, so there is nothing left to re-run
against. That is why the structural argument above carries the weight and the
A/B is corroboration.

**Two other per-module Gradle `jvmArgs` the runner still does not replicate**,
both checked and both out of reach of this suite:
`core/spring-boot-testcontainers` needs
`--add-opens=java.base/java.util.concurrent=ALL-UNNAMED` but only on its
`dockerTest` task (Docker-gated, not in the runner's source sets), and
`integration-test/spring-boot-integration-tests` needs the `java.net` one but
is not in the runner's `-Subtrees` list. `core/spring-boot-autoconfigure`
declares the `java.net` flag on its `test` task with no
`@DirtiesUrlFactories` or `java.net` reflection left in its test sources —
vestigial, and covered anyway now that the flag is unconditional.
