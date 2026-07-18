# Spring proxy layout-probe livelock blocking broad HttpClient consumer classes

**Status: OPEN — observed 2026-07-18**

## Scope

This is explicitly separate from the fixed JDK HttpClient builder-state issue.
Method-level selection proves the three affected `TestRestTemplate` redirect
assertions pass; the broad class hangs before those tests execute.

## Reproduction

With the JDK 25 real-JDK CLI and JIT enabled, the suite runner killed these
classes at its 300-second per-class boundary:

| Class | Result |
|---|---|
| `ReactiveHttpClientAutoConfigurationTests` | HANG, 300.027 s |
| `TestRestTemplateTests` | HANG, 300.055 s |

Both stderr logs repeat `gen_heap::get_field` out-of-bounds reads against
many `org/springframework/core/$Proxy28` instances with a one-slot layout.
No JUnit result line is emitted by the broad class runs.

The individual previously failing consumer methods all pass when selected
alone in JIT and `--nojit` modes:

- `jdkBuilderCanBeSpecifiedWithSpecificRedirects`
- `withClientSettingsUpdateRedirectsForJdk`
- `withClientSettingsRedirectsForJdk`

The evidence points to a Spring proxy-layout/startup livelock outside the
HttpClient builder transport. Root cause is not yet established.
