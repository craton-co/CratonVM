# `TestClassServerTest.testInvalidPackage` — CratonVM doesn't throw `ClassNotFoundException` for an invalid package name that HotSpot does

Status: open — small, low-severity residual; confirmed CratonVM-specific via fresh HotSpot comparison

Date observed: 2026-07-13 (HotSpot re-baseline against `hotspot-refresh-v2-shard1`, compared against CratonVM's
`nonpassed-before-refresh2-shard{1,2,3,4}` results)

## Summary

`test-framework/remote :: org.keycloak.testframework.remote.runonserver.TestClassServerTest::testInvalidPackage`
fails under CratonVM:

```
=> org.opentest4j.AssertionFailedError: Expected java.lang.ClassNotFoundException to be thrown, but nothing was thrown.
   org.junit.jupiter.api.AssertThrows.assertThrows(AssertThrows.java:74)
```

The test asserts that resolving a class from a deliberately-invalid package name throws
`ClassNotFoundException`; under CratonVM, no exception is thrown at all. This class was already flagged as a
known low-priority residual in the 2026-07-07 investigation pass (not root-caused then). It now has a confirmed
HotSpot-vs-CratonVM comparison: HotSpot passes this test cleanly (fresh, non-stale distribution), CratonVM does
not, confirming this is a genuine (if minor) CratonVM classloading behavior gap rather than a harness artifact.

## Next steps

1. Find the exact "invalid package" input the test uses (`TestClassServerTest` source, `testInvalidPackage`
   method) and trace CratonVM's classloading path for that lookup — likely CratonVM's classloader silently
   returns null/some fallback instead of raising `ClassNotFoundException` for certain malformed package/class
   name inputs.
2. Single test, single method — low priority relative to the other findings from this pass, but cheap to fix
   once located.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-testclassserver-invalidpackage -ClassList <(printf 'module\tclass\ntest-framework/remote\torg.keycloak.testframework.remote.runonserver.TestClassServerTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh2-20260712.exe -JdkHome $jdk
```

## Evidence

CratonVM failure: `apps/keycloak-suite-runner/.suite/results/nonpassed-before-refresh2-shard1/all-jit/logs/test-framework_remote.org.keycloak.testframework.remote.runonserver.TestClassServerTest.out.log`.
Fresh HotSpot PASS: `apps/keycloak-suite-runner/.suite/results/hotspot-refresh-v2-shard1/hotspot-jit/results.tsv`.

---

## FIXED 2026-07-14

**Root cause (confirmed, not the "malformed package/class name input" hypothesis
in "Next steps" above):** `URLClassLoader` was never actually consulting its own
URL classpath (file- or HTTP-based) when constructed with an explicit `null`
parent and used via the ordinary `loadClass(String)` path — the same
underlying defect as `docs/internal/spring-boot-probe-sweep/SBR-14-urlclassloader-parent-null-bypassed.md`
(open at the time, now closed by this fix too). Two stacked bugs:

1. `receiver_overrides_find_class` (`native-builtins/src/classloader.rs`)
   treated a bare `java/net/URLClassLoader` instance (no further subclass) as
   a "builtin base with no override", so `ClassLoader.loadClass`'s native
   reimplementation (`cl_real_load_class_base` /
   `cl_load_class_base_delegation`) never deferred to `findClass` at all —
   it went straight to CratonVM's flat global class store, which happily
   resolves any class already on the app's own classpath (e.g.
   `org.keycloak.representations.idm.RealmRepresentation`, a compile-time
   dependency of the test module), completely bypassing the loader's own
   URL search and the `permittedPackages` check it exists to enforce.
2. CratonVM's URLClassLoader machinery had **no real HTTP support at all** —
   `loader_constructor_url_paths`/`extract_url_path` treat every constructor
   URL as a local filesystem path (stripping `file:`/`jar:` prefixes), so an
   `http://` URL's path component was silently treated as a (non-existent)
   local directory. Even after fixing (1), the loader would still find
   nothing via its own "URL search" and fall through to a *second*
   unconditional global-store fallback in `ucl_real_find_class`.

**Fix** (`native-builtins/src/classloader.rs`, `classloader_real.rs`,
`http_client.rs`):
- `receiver_overrides_find_class` now recognizes `java/net/URLClassLoader`
  itself as having a genuine, non-trivial `findClass` (its own URL/HTTP
  search), not just subclasses that redeclare it.
- Added a real HTTP(S) GET path (`loader_constructor_http_bases` +
  `http_base_from_url` + `fetch_http_resource`, reusing the existing
  `http_client::perform_request` TCP/TLS client already used by
  `java.net.http.HttpClient`) so an `http://`/`https://` URLClassLoader entry
  actually fetches class bytes over the network instead of being silently
  skipped.
- When a URLClassLoader's own URL search (filesystem and/or HTTP) is
  attempted and definitively misses — e.g. every HTTP base answers a
  non-200 status, matching Keycloak's `TestClassServer` returning 403 for a
  non-permitted package — the resulting `ClassNotFoundException` now
  propagates instead of silently falling through to the flat global class
  store (`ucl_try_define_local_class`'s new `Some(Err(...))` return, and a
  new `find_class_is_urlclassloader_native` gate in both
  `cl_real_load_class_base` and `cl_load_class_base_delegation` that
  distinguishes this authoritative miss from the pre-existing, intentionally
  lenient HIB-CV-24/SBR-14 fallback for genuine user `findClass` overrides,
  which is unchanged).

**Verified** via an isolated A/B repro against both a `permit` and `deny`
real external HTTP server (Python `http.server`, standing in for Keycloak's
`TestClassServer` — see the historical follow-up note below for why the literal upstream
test still can't be run end-to-end): baseline (`dev` HEAD) resolves the class
through the app loader regardless of the server's response and never throws;
fixed binary correctly (a) fetches real bytes and defines the class under the
`URLClassLoader`'s own identity when permitted, and (b) throws
`ClassNotFoundException` when denied. Also regression-checked two pre-existing,
intentionally-preserved behaviors are unchanged: a null-parent `ClassLoader`
subclass with a genuine user-declared `findClass` override that throws still
falls back to the global store (HIB-CV-24), and a `URLClassLoader` subclass
that overrides `findClass` directly (Jasper `JasperLoader`-style) still
resolves correctly under its own identity.

Merged to `dev`.

**Follow-up resolved 2026-07-14**: the two unrelated HTTP-server blockers identified after this classloader fix are now closed: `String.getBytes()` once again produces response bytes, and [`HttpExchange.getRequestURI()` now returns a complete URI](httpserver-exchange-requesturi-getpath-empty-FIXED.md) for the handler's path routing. This document's original classloader isolation defect remains independently verified; the literal upstream Keycloak class was not rerun as part of this documentation update.
