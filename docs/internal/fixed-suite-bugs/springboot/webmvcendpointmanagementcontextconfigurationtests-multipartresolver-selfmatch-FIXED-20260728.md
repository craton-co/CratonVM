# `WebMvcEndpointManagementContextConfigurationTests` multipart-resolver self-match residual — CLOSED

**Status: CLOSED — verified 2026-07-28**

The 2026-07-23 document recorded an untraced hypothesis: the
`DispatcherServletAutoConfiguration` factory method named `multipartResolver`
might have resolved its own product while Spring was autowiring its
`MultipartResolver` parameter. It did not identify a CratonVM source location
or a confirmed mechanism.

## Current result

On current `origin/dev`, the original class is clean in both execution modes;
the linked `DispatcherServletAutoConfigurationTests` companion is clean too.
The latter includes the `renamesMultipartResolver` regression path, where a
user-defined resolver has a non-canonical bean name and Spring must register
the canonical Servlet resolver without resolving the factory method itself.

| Module | Class | JIT | `--nojit` |
|---|---|---:|---:|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.actuate.web.WebMvcEndpointManagementContextConfigurationTests` | 3/3 PASS | 3/3 PASS |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.DispatcherServletAutoConfigurationTests` | 13/13 PASS | 13/13 PASS |

The tests used a fresh CratonVM release binary built from current `origin/dev`
in an isolated target directory. The Spring Boot source for the original
specified fixture and the complete validation fixture had the identical SHA-256
for `WebMvcEndpointManagementContextConfigurationTests.java`. The supplied
fixture's generated runtime classpath was stale (it omitted
`org.springframework.boot.context.annotation.Configurations` and therefore
failed even on HotSpot); the complete fixture was used read-only after that
preflight established the discrepancy.

For comparison, HotSpot ran the same two classes at 3/3 and 13/13 PASS.

## Resolution

There is no remaining reproducer and no evidence that a generic Java
`HashSet`, Spring's creation tracking, or `@ConditionalOnBean` requires a new
VM workaround. The prior report is therefore retired as an already-resolved
residual rather than preserving an unconfirmed root-cause claim.

The focused manifest used for the verification was
`apps/spring-boot-suite-runner/webmvc-multipartresolver-affected-20260728.tsv`.
