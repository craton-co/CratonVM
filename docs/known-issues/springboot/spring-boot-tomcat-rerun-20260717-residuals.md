# `module/spring-boot-tomcat` 2026-07-17 rerun: 3 unrelated FAILs + embedded-server throughput-wall HANGs

**Status: PARTIALLY OPEN — found 2026-07-17, re-triaged 2026-07-19.** Items 2
and 3 below are now FIXED (on branch
`codex/fix-springboot-tomcat-rerun-closure-20260718-019f7681`, not yet merged
to `dev`). Item 1 remains OPEN with a narrower, corrected root-cause writeup —
its original "JKS wrong password" framing was disproven by a real fix (alias
enumeration order) that turned out not to be the actual cause of the test
failure. Item 4 is an accepted non-bug limitation, unchanged. Doc stays in
`known-issues/` per the "no fixed bugs stay in known-issues, but a doc with
one still-open item stays" triage rule until item 1 lands.

This module contributed 6 non-passing classes to this triage batch, splitting
into several unrelated root causes: 3 single-class FAILs (each a distinct
mechanism) and 3 HANGs that are a known, previously-characterized CratonVM
throughput limitation (a 4th HANG class from this module,
`TomcatServletWebServerServletContextListenerTests`, is a **different**
livelock signature already tracked in
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md) —
not repeated here). The `SslConnectorCustomizerTests` FAIL below is also
cross-filed as corroborating evidence in
[`ssl-pem-pkcs12-store-parse-failure-cluster.md`](ssl-pem-pkcs12-store-parse-failure-cluster.md)
— that cross-file stands as-is; today's session did not re-verify the PEM/PKCS12
cluster itself, only `SslConnectorCustomizerTests`.

## 1. `SslConnectorCustomizerTests` — 2/8 tests still fail; original "wrong password" framing was wrong

**Status: OPEN.** 2/8 tests still fail, same as 2026-07-17:

```
JUnit Jupiter:SslConnectorCustomizerTests:sslEnabledProtocolsConfiguration()
    => java.lang.AssertionError:
Expecting actual not to be null
       org.springframework.boot.tomcat.SslConnectorCustomizerTests.sslEnabledProtocolsConfiguration(SslConnectorCustomizerTests.java:157)
JUnit Jupiter:SslConnectorCustomizerTests:sslEnabledMultipleProtocolsConfiguration()
    => java.lang.AssertionError:
Expecting actual not to be null
       org.springframework.boot.tomcat.SslConnectorCustomizerTests.sslEnabledMultipleProtocolsConfiguration(SslConnectorCustomizerTests.java:140)
```

Both failing tests set `ssl.setKeyPassword("password")` on `test.jks` without
calling `setKeyStorePassword(...)` (the 3 passing cipher tests in the same
class only set `setKeyStorePassword("secret")`, no separate key password).
The connector fails non-fatally (`LifecycleException: Protocol handler
initialization failed`, logged but not propagated by Tomcat's `LifecycleBase`,
with no stack trace beneath the ERROR line), leaving `SSLHostConfig` never
configured, so `getEnabledProtocols()` returns `null` — that's what the
assertion actually reports.

**A real, distinct bug was found and fixed this session, but it does not
explain this failure — recorded here to save the next investigation from
re-treading it:** `KeyStore.aliases()`'s native (`engine_aliases` in
`native-builtins/src/keystore.rs`) alphabetically sorted alias names instead
of preserving file/insertion order (`LoadedKeyStore.entries` was a
`HashMap`). `test.jks` has 2 `PrivateKeyEntry`s (`spring-boot`, `test-alias`);
Tomcat's `SSLUtilBase.getKeyManagers()` picks the *first* key-bearing alias
off `ks.aliases()` when none is configured, so an alphabetical (CratonVM) vs.
file-order (real-JDK `LinkedHashMap`) enumeration can silently pick a
*different* entry than HotSpot would on any keystore with multiple private
keys. Fixed by switching `LoadedKeyStore.entries` to an `indexmap::IndexMap`
(new direct dependency, `native-builtins/Cargo.toml`) and dropping the
`sort_unstable()` in `engine_aliases`. Verified against real-JDK 25 with a
standalone repro (`KeyStore.load` → `aliases()`/`isKeyEntry()`/`getKey()` →
build an in-memory `KeyStore` via `setKeyEntry` → `KeyManagerFactory.init` →
`SSLContext.init` → `SSLEngine.setEnabledProtocols`): CratonVM now matches
HotSpot byte-for-byte at every step (same alias picked, same `getKey()`
format/length, same `SSLContext`/`SSLEngine` behavior) — **and yet
`SslConnectorCustomizerTests` still fails identically with this fix
in place**, proving the "JKS wrong password" framing from 2026-07-17 was
never the actual cause: the `WARN keystore: JKS key integrity check failed`
lines are an **expected, harmless** load-time artifact (`load_jks` in
`keystore.rs` deliberately tries — and is expected to fail — a per-entry
decrypt with the *store* password at load time, deferring real decryption to
whatever password `KeyStore.getKey()` receives later; see the `FIX
(httpserver-pkcs12-20260706)` comment on `keystore_set_pending_km_identity_with_password`
in the same file), not a symptom of the real defect.

**Still not root-caused.** The failure reproduces only through Tomcat's own
`SSLUtilBase`/`JSSEUtil`/`SSLHostConfig*` wiring (real, vendored Apache
Tomcat sources under `apps/tomcat/java/org/apache/tomcat/util/net/`), not
through any direct `KeyStore`/`KeyManagerFactory`/`SSLContext`/`SSLEngine`
API sequence tried this session. Next step: instrument (or step through)
`SSLHostConfigCertificate`/`SSLUtilBase.getSSLContext()` specifically for the
`keyPassword`-without-`keyStorePassword` case to find where it diverges from
the direct-API repro above — likely something Tomcat-internal (protocol/cipher
negotiation setup, `SSLHostConfig` default-population order, or a second,
not-yet-found keystore/key-manager code path) rather than the JCA layer
itself. The dead-code note below is also worth clearing away first, in case
it's hiding a second attempt at the same fix:

`native-builtins/src/x509_manager.rs`'s `kmf_engine_init` (registered on
`sun/security/ssl/KeyManagerFactoryImpl$SunX509`) had an in-flight,
uncommitted edit (from a prior agent session) threading the
`KeyManagerFactory.init` key password through to
`keystore_set_pending_km_identity_with_password` — a reasonable-looking fix
for the same "key password differs from store password" idea. Traced with
debug instrumentation this session: **that function is never called** for
this test (or, seemingly, at all in this build) — `javax/net/ssl/
KeyManagerFactory.getInstance()` is intercepted directly in
`phases_late.rs`'s `register_p68_ssl` and returns a fully synthetic
`javax/net/ssl/KeyManagerFactory` object (never a real
`KeyManagerFactoryImpl$SunX509` SPI instance), so `x509_manager.rs`'s
SPI-level registration is unreachable dead code on this code path. The
*actual* active `"javax/net/ssl/KeyManagerFactory"` → `"init"` registration
(same file, `phases_late.rs`, ~line 44480) already threads the key password
through the same way and pre-dates this session. The `x509_manager.rs` edit
was left in place (harmless, and plausibly live in a different
build/provider-selection mode) rather than reverted, but it is **not** the
fix for this bug and shouldn't be mistaken for one in a future session.

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.SslConnectorCustomizerTests.err.log`
(and matching `.out.log`).

**Original 2026-07-17 root-cause note (superseded above, kept for history):**
this class was filed in [`ssl-pem-pkcs12-store-parse-failure-cluster.md`](ssl-pem-pkcs12-store-parse-failure-cluster.md)
as a third module independently hitting `"JKS key integrity check failed"` —
that cross-file is now known to be coincidental (the WARN is expected/
harmless, per above), not evidence of one shared root cause. Not re-verified
this session; that doc's own PEM/PKCS12 findings may still be valid on their
own merits, just not for the reason this doc originally cited.

## 2. `TomcatEmbeddedWebappClassLoaderTests` — FIXED (functional); narrow URL-formatting residual remains

**Status: FIXED for resource resolution.** Both tests now find the resource
(previously null/empty); both still fail on exact URL string formatting only
(see residual below). Original symptom, 2/2 tests failing outright with
`null`/`[]`:

```
JUnit Jupiter:TomcatEmbeddedWebappClassLoaderTests:getResourceFindsResourceFromParentClassLoader()
    => org.opentest4j.AssertionFailedError:
expected: jar:file:C:\Users\...\junit-.../test.war!/WEB-INF/classes/test.txt
 but was: null
       org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests.lambda$getResourceFindsResourceFromParentClassLoader$0(TomcatEmbeddedWebappClassLoaderTests.java:55)
       org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests.withWebappClassLoader(TomcatEmbeddedWebappClassLoaderTests.java:80)

JUnit Jupiter:TomcatEmbeddedWebappClassLoaderTests:getResourcesOnlyFindsResourcesFromParentClassLoader()
    => org.opentest4j.AssertionFailedError:
Expecting actual:
  []
to contain exactly (and in same order):
  [jar:file:C:\Users\...\junit-.../test.war!/WEB-INF/classes/test.txt]
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests.err.log`
(and matching `.out.log`).

**Root cause, confirmed: hypothesis (a) from 2026-07-17.**
`classloading/src/class_path.rs`'s `parse_jar_subdir_spec` (parses a
`<archive>!/<prefix>/` classpath token) only accepted `.jar`/`.zip` outer
archive extensions, so a `URLClassLoader` rooted at `<war>!/WEB-INF/classes/`
(exactly what `withWebappClassLoader`'s parent loader is constructed with)
was never recognized as an archive-subdirectory classpath entry at all — the
`.war` extension fell through untreated, and `getResource`/`getResources`
always returned nothing.

**Fixed:**
- `parse_jar_subdir_spec` no longer restricts the outer archive's extension —
  any `<path>!/<prefix>/` spec is accepted; the caller still verifies the
  named file exists and is a readable ZIP before it contributes anything, so
  this can't turn an arbitrary resource miss into a bogus classpath entry.
- `ClassPath::add_path` now recognizes a `!/`-subdir spec *before* treating
  the token as a plain filesystem path, building a `NestedDirectory` entry
  (via `build_nested_directory_from_jar`) rooted at the WAR/EAR/PAR's
  internal prefix.
- 2 new unit tests added (`class_path.rs`):
  `jar_subdirectory_spec_accepts_war_archives`,
  `classpath_new_finds_resource_in_war_subdirectory`. Full `class_path::`
  test suite (88 tests) passes.

**Residual (narrow, precisely diagnosed — not re-filed as a separate doc,
small enough to track inline):** `getResourceFindsResourceFromParentClassLoader`
now finds the resource but still fails on exact URL string formatting:

```
expected: jar:file:C:\Users\...\test.war!/WEB-INF/classes/test.txt
 but was: jar:file:/C:/Users/.../test.war!/WEB-INF/classes/test.txt
```

`webInfClassesUrlString` (test helper, `.java:91-93`) builds the parent
loader's URL by raw string concatenation —
`"jar:file:" + war.getAbsolutePath() + "!/WEB-INF/classes/"` — bypassing
`File.toURI()` entirely, so on Windows it embeds the literal backslash-laden
absolute path (`C:\Users\...\test.war`) straight into the URL string. Real-JDK's
`URLClassLoader`/`JarLoader` preserves that exact spelling when constructing
derived resource URLs (Java's `URL`/`URLStreamHandler` machinery doesn't
rewrite path separators in an opaque path component); CratonVM's
`ClassPath::find_resource`/`find_resources` (`class_path.rs`, all of the
`JarFile`/`NestedDirectory`/`NestedJar`/`JmodFile` match arms, e.g. line
~3371/3388/3395/3414/3421/3439/3450) unconditionally normalize to the
canonical forward-slash, single-leading-slash form
(`format!("jar:file:/{p}!/{name}")` after `p.replace('\\', "/")`) — correct
and necessary for the overwhelming majority of real cases (matching what
`File.toURI()` itself produces), but it discards the caller's original
(here, deliberately unusual) path spelling. Fixing this properly would mean
threading the *original* per-entry URL string through the classpath
registration/lookup pipeline instead of re-deriving it from a `PathBuf` —
a real architectural change, not attempted this session given how narrow and
synthetic-input-specific the mismatch is (only reachable when a
dynamically-added `URLClassLoader` entry's `URL` was built by hand with
embedded backslashes, bypassing `File.toURI()`).

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests.err.log`
(and matching `.out.log`) — from the original 2026-07-17 run; not re-captured
this session (fixed in a local worktree, not yet re-run through the full
suite harness).

## 3. `TomcatMetricsAutoConfigurationTests` — FIXED (independently, by concurrent `dev` work)

**Status: FIXED.** Re-run 2026-07-19: **5/5 tests pass**, including the 2 that
failed on 2026-07-17 (`autoConfiguresTomcatMetricsWithEmbeddedServletTomcat`,
`autoConfiguresTomcatMetricsWithEmbeddedReactiveTomcat`). This was not fixed
by anything in this session's own changes (which only touched
`keystore.rs`/`x509_manager.rs`/`phases_early.rs`/`class_path.rs`) — this
worktree was 78 commits behind `origin/dev` at the start of this session, and
fast-forwarding onto current `dev` (`ff15c8bd8`) brought in a substantial JMX
rewrite (`native-builtins/src/jmx.rs`, ~276 changed lines in that range of
history) plus other MBeanServer-related fixes already landed by concurrent
sessions since 2026-07-17. Whichever of those closed the gap, it's closed now
— not independently re-diagnosed this session, just re-verified.

**Original 2026-07-17 root-cause hypothesis (superseded, kept for history):**
2/5 tests failed with `Expecting actual not to be null` on
`registry.find("tomcat.sessions.active.max"/"tomcat.threads.current").meter()`,
hypothesized as Tomcat's `Manager`/`ThreadPool` MBeans never being registered
with (or queryable from) CratonVM's `MBeanServer`. A same-day research pass
(before the fix was found to already be upstream) actually cast doubt on that
specific framing too: `TomcatMetrics.registerSessionMetrics()` (Micrometer,
real bytecode) creates `tomcat.sessions.active.max` from `Manager::
getActiveSessionsMax` directly, with **no JMX involved at all** — so the
JMX-gap hypothesis couldn't have explained both failing assertions by itself;
`TomcatMetricsBinder.findManager(applicationContext)` returning `null` (a
Tomcat-container-tree/timing gap upstream of JMX entirely) was the
better-supported alternative. Moot now that the tests pass, but worth noting
in case of a regression: don't assume "JMX gap" without re-checking this.

## 4. Embedded-Tomcat-per-test-method throughput wall (3 HANGs)

| Class | Shape |
|---|---:|
| `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests` (66 `@Test` methods) | 33 full `tomcat.start()`/deploy cycles completed in ~290s before the shard timeout, still progressing (last cycle cut off mid-startup) |
| `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` (18 `@Test` methods) | Similar repeated `Initializing/Starting ProtocolHandler` cycles, killed mid-progress |
| `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` (42 `@Test` methods) | Same shape; `.out.log` ends with a bare `Interrupted!` (harness-killed) |

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactory-2331692b613b.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests.err.log`

All 3 show clean, error-free, repeated `Initializing ProtocolHandler` →
`Starting service [Tomcat]` → `Starting Servlet engine` → `Starting
ProtocolHandler` → `Stopping ProtocolHandler` cycles with no exceptions, no
stall between cycles, and steadily increasing connector-instance counters
(e.g. `http-nio-auto-9` through `http-nio-auto-33`) right up to the point
the shard timeout kills the process — i.e. genuine forward progress, not a
deadlock.

**Root cause: this is the same, already-characterized "embedded-server
deployment throughput wall" documented for the standalone Tomcat test suite
in `docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`.**
That investigation quantified the dominant cost as `update_root_snapshot`
overhead (called on every object-returning native call during Tomcat's
reflection-heavy webapp deployment, `gc`/`vm` internals) multiplied by
per-class method count — each of these 3 classes runs its embedded-server
`tomcat.start()`/deploy/serve/stop cycle **once per `@Test` method** (18-66
methods each), and even with the partial mitigations already landed there
(`CRATONVM_ROOTSNAP_CACHE`, `CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT`), a
single deploy still costs tens of seconds versus HotSpot's sub-second
deploy — comfortably explaining why HotSpot finishes all methods inside the
suite's per-class timeout while CratonVM does not, without any functional
defect. Confirmed applicable here (not just asserted by analogy) via the
observed per-instance cadence in these 3 logs (~8-9s/cycle for
`TomcatWebServerFactoryCustomizerTests`, consistent with that doc's
measured single-deploy cost) and the complete absence of any error/exception
anywhere in either log.

## Affected classes

| Module | Class | Issue | Status |
|---|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.SslConnectorCustomizerTests` | 1 (still 2/8 FAIL; "wrong password" framing disproven) | **OPEN** |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.TomcatEmbeddedWebappClassLoaderTests` | 2 (WAR resource resolution) | FIXED (functional); narrow URL-format residual |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.autoconfigure.metrics.TomcatMetricsAutoConfigurationTests` | 3 (MBean metrics not bound) | FIXED (upstream `dev`) |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests` | 4 (throughput wall) | Not a bug |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` | 4 (throughput wall) | Not a bug |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | 4 (throughput wall) | Not a bug |

## 2026-07-19 session summary

Continuing a prior agent's in-flight (uncommitted) work in
`C:\craton\CratonVM-sb-tomcat-rerun-closure-20260718-019f7681`
(branch `codex/fix-springboot-tomcat-rerun-closure-20260718-019f7681`):

- Fast-forwarded the worktree onto `origin/dev` (78 commits behind → caught
  up to `ff15c8bd8`) before continuing, per project convention — no conflicts
  on the touched files, and this is what surfaced item 3 already being fixed
  upstream.
- Landed a real fix for `KeyStore.aliases()` alias-enumeration order
  (`HashMap` → `indexmap::IndexMap` in `keystore.rs`, new `indexmap`
  dependency in `native-builtins/Cargo.toml`) — confirmed correct against
  real JDK 25 via a standalone repro, but confirmed **not** the cause of
  item 1's test failure (see item 1 above).
- Verified the prior agent's WAR/`.war`-classpath fix in `class_path.rs`
  (item 2) actually works: resource resolution went from total failure
  (`null`/`[]`) to correct resolution, with a narrow residual identified and
  documented (see item 2 above).
- Verified item 3 now passes 5/5, fixed independently by concurrent `dev`
  work pulled in by the fast-forward above.
- All keystore.rs (14) and class_path.rs (88) unit tests pass; no
  regressions.
- Not yet committed/merged to `dev` — code changes exist only in the
  worktree above as of this writeup.

Not covered here: `org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests`
(HANG) — see
[`junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md`](junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster.md),
a different (livelock, not throughput) signature.
