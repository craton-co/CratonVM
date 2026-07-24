# Spring Boot WebServer stop/destroy suppressed stack traces - fixed

**Status: FIXED - verified 2026-07-18**

## Symptom

`ReactiveWebServerApplicationContextTests` and
`ServletWebServerApplicationContextTests` expected the failure from a mocked
`WebServer.stop()` or `WebServer.destroy()` call to appear in AssertJ's printed
exception trace. The four assertions use `withStackTraceContaining(...)` for
`"WebServer has failed to stop"` or `"WebServer has failed to destroy"`.

## Root cause and fix

The Mockito late-restub theory was disproved. Spring Boot correctly attached
the secondary exception with `Throwable.addSuppressed(...)`; CratonVM's native
stack-trace renderer initially printed only the primary exception and its cause
chain. Commit `98db56763760b199d27201166495c4c8f640a8c5`
(`fix: retain throwable traces across threads`) added recursive suppressed
throwable rendering in `../../../../native-builtins/src/lang_misc.rs`:

- reads real suppressed exception entries;
- prints indented `Suppressed:` sections;
- preserves common-frame elision and cycle protection for suppressed and cause
  chains.

That commit is reachable from the `dev` baseline used for this closure.

## Validation

Built the dedicated release binary
`C:\craton\cargo-target-webserver-suppressed-20260718-019f768e\release\cratonvm.exe`
from this worktree (Cargo target directory was unique to the task). It passed
both affected classes in each execution mode:

| Mode | Classes | Tests | Result |
|---|---:|---:|---|
| JIT | 2 | 45 | PASS - 0 failures, aborts, or crashes |
| `--nojit` | 2 | 45 | PASS - 0 failures, aborts, or crashes |

Focused result files are retained in the task worktree under
`.suite-webserver-suppressed/results/`.

## Affected tests

- `module/spring-boot-web-server` -
  `ReactiveWebServerApplicationContextTests`
- `module/spring-boot-web-server` -
  `ServletWebServerApplicationContextTests`
