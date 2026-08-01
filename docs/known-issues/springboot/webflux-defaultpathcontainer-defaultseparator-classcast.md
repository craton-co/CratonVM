# WebFlux `DefaultPathContainer` `DefaultSeparator` checkcast report

**Status: OPEN — REGRESSED 2026-07-31.**

## Original report

`org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests`
had previously reported four reactive GraphQL failures wrapped as
`WebClientRequestException`, whose inner failure was a `ClassCastException`
from `java.lang.Object` to
`org.springframework.http.server.DefaultPathContainer$DefaultSeparator`.
The original analysis did not identify a CratonVM source location and
considered both stale/moved-GC references and duplicate class identity as
possible mechanisms.

## Closure

No targeted code change is required on the current `dev` head. A fresh
task-specific release binary was built and the entire affected class is now
green in both execution modes.

The user-supplied `C:\craton\CratonVM\apps\spring-boot` fixture was first
preflighted but could not be used as validation evidence: its generated
classpath lacked `Configurations.class`, and its incomplete source tree lacked
`build-plugin\spring-boot-antlib`. HotSpot consequently failed discovery with
`NoClassDefFoundError`, before any VM execution. Validation therefore used the
complete equivalent fixture at
`C:\craton\CratonVM-spring-boot-residual-20260728\apps\spring-boot`; its
HotSpot baseline passed all 17 tests.

## Validation

- HotSpot JDK 25 baseline: 17 tests, 0 failed, 0 aborted, 0 failed containers.
- CratonVM release, JIT on: 17 tests, 0 failed, 0 aborted, 0 failed containers
  (`SBRUNNER_RESULT tests=17 failed=0 aborted=0 skipped=0 containersFailed=0`).
- CratonVM release, `--nojit`: 17 tests, 0 failed, 0 aborted, 0 failed
  containers (`SBRUNNER_RESULT tests=17 failed=0 aborted=0 skipped=0
  containersFailed=0`).

The prior report is retired rather than attributing a cause to an
unreproduced failure.

## Regression note (2026-07-31)

Recurred in a full-suite rerun (`craton-rerun-20260731`, `all-jit`), this time
in a different WebFlux-routed test class —
`org.springframework.boot.integration.actuate.endpoint.IntegrationGraphEndpointWebIntegrationTests`
(`module/spring-boot-integration`) — rather than the originally-reported
`GraphQlWebFluxAutoConfigurationTests`. Same exact defect signature: a
`ClassCastException: java.lang.Object cannot be cast to
org.springframework.http.server.DefaultPathContainer$DefaultSeparator` thrown
from `DefaultPathContainer.createFromUrlPath(DefaultPathContainer.java:98)`
via `RequestPath.parse` → `AbstractServerHttpRequest.getPath`, surfacing as a
suppressed exception on both the WebFlux routing path
(`AbstractHandlerMethodMapping.getMappingsByDirectPath`) and the
exception-handling path (`ExceptionHandlingWebHandler`), turning every
request into a `500 INTERNAL_SERVER_ERROR`:

```
JUnit Jupiter:IntegrationGraphEndpointWebIntegrationTests:graph(WebTestClient):WebFlux
  => java.lang.AssertionError: Status expected:<200 OK> but was:<500 INTERNAL_SERVER_ERROR>
JUnit Jupiter:IntegrationGraphEndpointWebIntegrationTests:rebuild(WebTestClient):WebMvc
  => java.lang.AssertionError: Status expected:<204 NO_CONTENT> but was:<500 INTERNAL_SERVER_ERROR>
JUnit Jupiter:IntegrationGraphEndpointWebIntegrationTests:rebuild(WebTestClient):WebFlux
  => java.lang.AssertionError: Status expected:<204 NO_CONTENT> but was:<500 INTERNAL_SERVER_ERROR>
```

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-integration.org.springframework.boot.integration.actuate.endpoint.Integ-cf6a6f777358.{out,err}.log`

The 2026-07-29 "closure" above never identified a source-level root cause —
it only observed that a freshly built binary happened to pass. The bug was
never actually fixed, only unreproduced at that moment; this rerun shows it
is still live (or was reintroduced) somewhere in the request-path/array
handling that `DefaultPathContainer.createFromUrlPath` exercises. Given the
"Object where a typed value was expected" shape, this remains consistent with
the original two suspected mechanisms (stale/moved-GC reference or duplicate
class identity across loaders) — not re-diagnosed this session; treat the
prior root-cause discussion as a starting point, not a conclusion.

## Regression note 2 (2026-07-31) — same defect in a WebMvc (servlet) class, escalating to a SIGSEGV

Also hit in the same `craton-rerun-20260731`/`all-jit` round, in a plain
WebMvc/servlet class rather than a WebFlux/reactive one —
`org.springframework.boot.webmvc.autoconfigure.actuate.endpoint.web.WebMvcHealthEndpointAdditionalPathIntegrationTests`
(`module/spring-boot-webmvc`). `DefaultPathContainer` is shared between
Spring's WebFlux and WebMvc request-path parsing, so this confirms the defect
is not WebFlux-specific despite this doc's filename/original title. Three
separate `GET /healthz` requests across three Tomcat boot cycles in the run
each hit the identical exception:

```
ERROR [...[dispatcherServlet]] Servlet.service() for servlet [dispatcherServlet] in context with path [] threw exception [Request processing failed: java.lang.ClassCastException: java.lang.Object cannot be cast to org.springframework.http.server.DefaultPathContainer$DefaultSeparator] with root cause (java/lang/ClassCastException: java.lang.Object cannot be cast to org.springframework.http.server.DefaultPathContainer$DefaultSeparator)
```

On the 4th boot cycle in the same process, the run escalated to a hard
SIGSEGV instead of a caught `ClassCastException`:

```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF7A19B3771
Faulting access: read at address 0x0000000100000004
thread: "http-nio-auto-8-exec-1"
gc young-gen policy: moving (Cheney young copy)
gc young-gen last incomplete-coverage reason: compiled-frame-oop-not-published
```

Not confirmed whether the SIGSEGV is the same underlying "`Object` in a slot
that should hold a typed reference" corruption escalating (e.g. the bad value
this time landing somewhere dereferenced unsafely instead of hitting a
`checkcast`), or an unrelated second defect coinciding in the same run — not
re-diagnosed at the source level this session. The
`compiled-frame-oop-not-published` GC annotation on the crashing frame is
worth noting given this cluster's other members (see the
`springboot-basicerrorcontroller-checkcast-abort-20260731.md` doc in this
same directory) trace to moving-young-generation root-publication gaps; a
future investigation should check whether they share a cause.

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.actuate.endpoint.w-5454e505205d.{out,err}.log`

## Regression note 3 (2026-07-31) — 2 more WebMvc/servlet classes, `module/spring-boot-security`

Also seen in the assigned-class triage of the same 2026-07-31 rerun batch, in
two Spring Security actuator web-integration classes:

- `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests`
  (`module/spring-boot-security`, run `craton-rerun-20260731`/`all-jit`) — 6 of
  9 tests failed, all `Status expected:<200|401 ...> but was:<500
  INTERNAL_SERVER_ERROR>`. The `.err.log` shows the request that triggers the
  exception passes through a Jersey `ResourceConfig` servlet as well as the
  plain `se1-actuator-endpoint` servlet, and both paths hit the identical
  root cause:
  `ERROR [...[org.glassfish.jersey.server.ResourceConfig]] Servlet.service()
  ... threw exception (java/lang/ClassCastException: java.lang.Object cannot
  be cast to org.springframework.http.server.DefaultPathContainer$DefaultSeparator)`,
  traced through `ServletRequestPathFilter.doFilter` →
  `ServletRequestPathUtils.parseAndCache` → `RequestPath.parse` →
  `DefaultPathContainer.createFromUrlPath(DefaultPathContainer.java:98)` —
  the exact same call site as the two regressions above. Logs:
  `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.se-a0d9d711811f.{out,err}.log`
- `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests`
  (`module/spring-boot-security`, run `craton-hangverify-20260731`/`all-jit`)
  — 4 of 10 tests failed, same identical
  `ClassCastException: java.lang.Object cannot be cast to
  org.springframework.http.server.DefaultPathContainer$DefaultSeparator`
  signature. Logs:
  `apps/spring-boot-suite-runner/.suite/results/craton-hangverify-20260731/all-jit/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.w-f4cd03f47d82.{out,err}.log`

Both confirm this defect is general to servlet-path request handling (not
WebFlux- or WebMvc-specific, not tied to a single module) — any code path
that calls `ServletRequestPathUtils.parseAndCache`/`RequestPath.parse` can
hit it. Not re-diagnosed at the source level this session.

## Affected classes

- `module/spring-boot-graphql` — `org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests` (original report)
- `module/spring-boot-integration` — `org.springframework.boot.integration.actuate.endpoint.IntegrationGraphEndpointWebIntegrationTests` (2026-07-31 regression)
- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.actuate.endpoint.web.WebMvcHealthEndpointAdditionalPathIntegrationTests` (2026-07-31 regression, WebMvc/servlet manifestation, also escalates to SIGSEGV)
- `module/spring-boot-security` — `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests` (2026-07-31 regression, Jersey servlet manifestation)
- `module/spring-boot-security` — `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` (2026-07-31 regression, plain servlet manifestation)
