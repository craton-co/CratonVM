# `OAuth2ResourceServerAutoConfigurationTests` native-root publication slowdown — FIXED

**Resolved: 2026-07-21.**

## Cause

The VM rebuilt the full `update_root_snapshot` GC-root list at ordinary
object-returning native-call boundaries. Those boundaries are still runnable:
a moving collector waits for the mutator's next safepoint before reading its
snapshot. Rebuilding it at every return therefore added an O(stack-depth)
cost to the reflection-heavy JUnit/Spring execution path.

The same unnecessary publish existed for native-thrown Java exceptions. Both
object returns and exceptions already have a moving-GC-safe handoff root in
`native_pending_return`; blocking natives have their own pre-park deposit.

During real OAuth2 execution, Mockito also exposed an adjacent bootstrap
loader bug: `Class.forName(name, false, null)` rejected a class from a JAR
that Mockito had appended to the bootstrap search path. Explicit bootstrap
lookups now permit registered bootstrap-appended classes while preserving the
normal rejection for unrelated application names.

## Fix

- Publish `native_pending_return` only at a collector-visible safepoint or
  blocking transition; do not rebuild a snapshot on ordinary native returns
  or native-thrown exception handoffs.
- Keep the native return pinned until the caller has placed it on its operand
  stack.
- Permit bootstrap-appended Mockito dispatcher classes through the explicit
  bootstrap `Class.forName` path.
- Add focused VM tests that verify object returns and native-thrown exceptions
  are absent from a runnable thread's snapshot and appear at the next
  safepoint.

## Validation

Unique release binary:
`C:\craton\cargo-target-oauth2-rootsnapshot-20260721-019f86d0\release\cratonvm.exe`

Using the real Spring Boot fixture and suite runner with the normal
300-second per-class limit:

| Mode | Result | Time | Tests |
|---|---:|---:|---:|
| JIT | PASS | 208.115s | 52/52 |
| `--nojit` | PASS | 209.166s | 52/52 |

Focused VM root-handoff tests also pass for normal object results and
native-thrown Java exceptions.

## Related residual boundary

`JacksonAutoConfigurationTests` was subsequently fixed in
`docs/internal/springboot/jacksonautoconfigurationtests-severe-slowdown-FIXED-20260722.md`.
It shares the high-level reflection/JUnit workload shape, but at the time of
this OAuth2 change it still exceeded the normal 300-second class budget in
both JIT modes; it was not represented as fixed by this OAuth2 closure.
