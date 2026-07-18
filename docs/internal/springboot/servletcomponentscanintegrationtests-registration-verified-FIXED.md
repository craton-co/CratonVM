# `ServletComponentScanIntegrationTests` registration — verified fixed

**Status: FIXED — verified 2026-07-18**

## Original symptom

The 2026-07-17 Spring Boot rerun reported that
`ServletComponentScanIntegrationTests.componentsAreRegistered()` registered
only `TestServlet` where the test expected two servlets.

The current fixture makes the missing component unambiguous:
`TestMultipartServlet` is the second `@WebServlet`; it also carries
`@MultipartConfig`. The same scan must additionally register `TestFilter` and
`TestListener`.

## Verification

On current `dev` (`cf3a44e2a`), a freshly built, task-specific CratonVM
binary ran the exact class with the live Spring Boot checkout and its
module-generated test classpath:

| VM mode | Result | Time |
|---|---:|---:|
| HotSpot JIT | PASS | 3.1 s |
| CratonVM JIT | PASS | 7.5 s |
| CratonVM `--nojit` | PASS | 5.9 s |

All three methods passed in each run: `componentsAreRegistered()`,
`indexedComponentsAreRegistered()`, and `multipartConfigIsHonoured()`.
This proves both the ordinary classpath scan and the indexed scan now register
the multipart servlet, filter, and listener as expected.

## Resolution

No active CratonVM defect remains on current `dev`; this was a stale
known-issue record whose old failure signature came from the 2026-07-17
rerun. The original failure cannot be attributed to a current runtime path,
so no speculative code change was made. This record is retained for the
evidence trail and moved from `docs/known-issues` to `docs/internal`.
