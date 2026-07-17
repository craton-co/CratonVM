# `StaticResourceJarsTests` — JAR/URL-encoded-path resource lookup failures

**Status: OPEN — found 2026-07-17, not root-caused**

## Symptom

Module `module/spring-boot-web-server`, class `StaticResourceJarsTests`, 3 failures:

- `closesJarFromNonCachedConnection()`: `AssertionError: Expecting code to raise a throwable.` — an exception the test expects never happens.
- `includeJarWithStaticResourcesWithUrlEncodedSpaces()` and `includeJarWithStaticResourcesWithPlusInItsPath()`: both `AssertionError: Expected size: 1 but was: 0 in: []` — static resources inside a JAR whose path contains URL-encoded spaces or a literal `+` are not found at all.

Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard*/logs/module_spring-boot-web-server.StaticResourceJarsTests.out.log` (lines 18-34).

## Root cause

**Not root-caused — no CratonVM source read for this yet.** The URL-encoding angle (spaces, `+`) plausibly relates to `.par`/JAR classpath URI-decoding issues noted in memory ([[reference_par_classpath_extension_uri_decode]]), but this was **not verified** — needs someone to actually read the relevant JAR-URL-connection/URI-decoding path in CratonVM before attributing it there. Do not treat the memory-reference connection as confirmed.

## Affected classes

- `module/spring-boot-web-server` | `StaticResourceJarsTests`
