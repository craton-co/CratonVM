# `spring-boot-webmvc` residuals: root-path forward-to-error-page 404, and two multi-boot classes timing out at 300s — FIXED / not-a-bug

**Status: FIXED — verified 2026-07-19**

> **Regression note (2026-07-23):** Cluster B's `http_parse_url` fix (the
> `'/' | '?' | '#'` authority/path split below) is **no longer present** in
> the code — confirmed absent at both this worktree's HEAD and `origin/dev`'s
> tip as of 2026-07-27. `BasicErrorControllerIntegrationTests` reproduces the
> exact same 5/26 "bad port" failures again in the `craton-rerun-20260723`
> results. Root-caused to a silent merge: `b0dd2e726` (2026-07-07, merged into
> `dev` after this fix landed) carried its own rewrite of `http_parse_url`
> based on the pre-fix version, silently dropping the `?`/`#` handling while
> adding unrelated `userinfo`-authority parsing. See
> [`../../known-issues/springboot/http-parse-url-query-only-authority-split-regression-20260723.md`](../../known-issues/springboot/http-parse-url-query-only-authority-split-regression-20260723.md)
> for the full analysis and re-fix direction. Cluster A (`MappingMatch`
> `<clinit>`) and the timeout-override config change are unaffected by this.

## Cluster A — `RemappedErrorViewIntegrationTests#forwardToErrorPage`: root mapping never invoked under a context path — FIXED

### Root cause

`mapping_match_static` (`native-builtins/src/lib.rs`) resolved
`jakarta/servlet/http/MappingMatch`'s static enum-constant fields via
`ctx.class_id_by_name(...)`, which only guarantees the class's metadata is
loaded — **not** that its `<clinit>` has run. Reached before any Java
bytecode had otherwise touched `MappingMatch`, the enum constant fields
(`PATH`, `DEFAULT`, `CONTEXT_ROOT`, `EXACT`, ...) were still `null`.
`ApplicationMapping`/`HttpServletMapping` construction consumed that `null`
mapping type, and Spring MVC's servlet-mapping resolution for the exact-root
pattern (`@RequestMapping("/")`) under a configured context path (`/spring`)
treated the request as unmapped — producing an immediate Tomcat 404 for
`/spring/` before `DispatcherServlet`, and therefore the throwing `home()`
handler, ever ran.

### Fix

`mapping_match_static` now calls `ctx.ensure_class_initialized(...)` instead
of `ctx.class_id_by_name(...)`, guaranteeing `MappingMatch`'s `<clinit>` has
populated the enum constants before any static field is read. Same
established API used at ~627 other native call sites in this codebase for
the same "read a static field that requires `<clinit>` to have run" pattern.

### Validation

Targeted Spring Boot JIT run against a fresh release build:
`RemappedErrorViewIntegrationTests` — **2/2 PASS** (was 1/2, `forwardToErrorPage`
failing). `directAccessToErrorPage` (previously-passing sibling) still passes.

## Cluster B — `WebMvcAutoConfigurationTests` / `BasicErrorControllerIntegrationTests` HANG at 300s — not a bug, genuine per-boot throughput

### Root cause

Both classes independently boot many full Spring contexts /
embedded-Tomcat instances per class (`WebMvcAutoConfigurationTests`'s many
`ApplicationContextRunner`-based methods; `BasicErrorControllerIntegrationTests`'s
83 `@Test` methods, each with its own embedded-Tomcat + `DispatcherServlet`
boot/teardown cycle). Live timestamp reconstruction (this doc's OPEN version)
already showed continuous log activity right up to the exact 300s kill point,
not a long silent stretch — confirming genuine cumulative per-boot overhead
exceeding the suite's default 300s class timeout, not a deadlock.

A second, independent bug compounded the `BasicErrorControllerIntegrationTests`
symptom once the timeout was raised enough to observe it: `http_parse_url`
(`native-builtins/src/net_phase_e.rs`) split a URL's authority at the first
`/`, so a query-only request target immediately following the host:port with
no path separator (`http://localhost:56989?trace=true`, produced by
`JdkClientHttpRequest`/`TestRestTemplate` for query-only paths) left the
literal text `56989?trace=true` as the port substring, which failed `u16`
parsing with `bad port in <url>` — surfacing as 5 real test failures once the
class could run to completion instead of aborting on the artificial timeout.

### Fix

- `run-spring-boot-suite.ps1`: added per-class timeout overrides —
  `WebMvcAutoConfigurationTests` = 7200s, `BasicErrorControllerIntegrationTests`
  = 1800s — matching the throughput-only conclusion above (config-only change,
  no VM behavior change).
- `http_parse_url`: the authority/path split now also breaks on `?` and `#`,
  not just `/`. When the first following-character is `?` or `#`, the path is
  synthesized as `/` + that suffix (`/?trace=true`) instead of letting it leak
  into the port text.

### Validation

Targeted Spring Boot runs against a fresh release build (both fixes
included):
- `WebMvcAutoConfigurationTests` — **PASS** at 594.0s (JIT) / 725.4s (`--nojit`),
  both comfortably inside the new 7200s override.
- `BasicErrorControllerIntegrationTests` — **26/26 PASS** at 528.5s (JIT),
  comfortably inside the new 1800s override. (Prior to the `http_parse_url`
  fix landing, the same class — now running to completion instead of timing
  out — surfaced 5/26 `FAIL` from the `bad port` defect above; confirmed fixed
  in the same build.)

Also re-verified `docs/internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md`'s
GC fix is still present and this class's earlier CRASH remains fixed — this
doc's HANG (now PASS) was always a distinct, later symptom on top of that
fix, not a regression of it.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.RemappedErrorViewIntegrationTests` |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.WebMvcAutoConfigurationTests` |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` |

Note: `JettyClientHttpRequestFactoryBuilderTests.buildWhenHadReadTimeout()`
flagged as out-of-scope in the OPEN version of this doc was not investigated
here either — it is an unrelated `Duration`/property-mapping issue.
