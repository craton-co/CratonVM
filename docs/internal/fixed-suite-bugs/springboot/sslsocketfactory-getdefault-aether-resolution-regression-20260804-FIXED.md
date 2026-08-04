# `SSLSocketFactory.getDefault()` "no owning SSLContext" breaks Aether/Maven artifact resolution for `ModifiedClassPathClassLoader` tests — FIXED

**Status: FIXED 2026-08-04** — see "STATUS 2026-08-04: FIXED AND CLOSED" at the
end of this page for the runtime confirmation, the fix, the two residuals it
also closed, the five additional affected classes a cold-cache rerun found, and
the guards. Everything above that section is the original investigation, left
as written.

Previously OPEN — REGRESSED 2026-08-04. Previously fixed and closed
2026-07-26 (see `docs/internal/spring-boot-core39-residual-clusters-20260723.md`,
"Cluster C" item 1, under "STATUS 2026-07-26: all four clusters closed").
The exact same exception, with the exact same mechanism, reappeared in a
2026-08-04 residual rerun across 3 classes in 3 different modules.

## Symptom

Any test that uses Spring Boot test-support's `@ClassPathExclusions`/
`@ClassPathOverrides`/`@ForkedClassPath` (i.e. goes through
`ModifiedClassPathClassLoader`, which resolves the modified classpath's
extra/replacement Maven coordinates live via Eclipse Aether against Maven
Central over HTTPS) fails immediately with:

```
java.lang.IllegalStateException: Resolution failed after 5 attempts
   org.springframework.boot.testsupport.classpath.ModifiedClassPathClassLoader.resolveCoordinates(...)
 Caused by: org.eclipse.aether.resolution.DependencyResolutionException: Failed to collect dependencies at <coord>
 Caused by: org.eclipse.aether.collection.DependencyCollectionException: ...
 Caused by: org.eclipse.aether.resolution.ArtifactDescriptorException: ...
 Caused by: org.eclipse.aether.resolution.ArtifactResolutionException: ... Could not transfer artifact ... SSLSocketFactory has no owning SSLContext
 Caused by: org.eclipse.aether.transfer.ArtifactTransferException: ... SSLSocketFactory has no owning SSLContext
 Caused by: java.lang.IllegalStateException: SSLSocketFactory has no owning SSLContext
       org.apache.http.conn.ssl.SSLConnectionSocketFactory.createLayeredSocket(SSLConnectionSocketFactory.java:393)
       org.apache.http.conn.ssl.SSLConnectionSocketFactory.connectSocket(SSLConnectionSocketFactory.java:384)
       org.apache.http.impl.conn.DefaultHttpClientConnectionOperator.connect(...)
       ...
       org.eclipse.aether.transport.http.HttpTransporter.execute(...)
```

This is **not** an environment/network-reachability problem: `curl -sS
https://repo.maven.apache.org/maven2/com/google/code/gson/gson/2.10/gson-2.10.pom`
from the same host returns `HTTP 200` in ~60ms. The JVM-level
`SSLSocketFactory` object handed to Apache HttpClient's
`SSLConnectionSocketFactory` is missing its backing `SSLContext` before any
network I/O is attempted.

## Root cause (as previously diagnosed and fixed 2026-07-23/26)

`javax/net/ssl/SSLSocketFactory.getDefault()` (the static method,
`native-builtins/src/phases_late/ssl_security.rs`) used to allocate a bare
0-field synthetic `SSLSocketFactory` object, never wiring up field 0 (the
owning `SSLContext`) — unlike `SSLContext.getSocketFactory()`'s registration
a few lines above, which correctly stashes the context at field 0. Any
caller reaching the layered `createSocket(Socket,String,int,boolean)`
overload through a factory obtained via the static `getDefault()` hits that
overload's `ctx.get_field(factory, 0)` on a factory with no field 0 at all,
throwing `IllegalStateException("SSLSocketFactory has no owning
SSLContext")` instead of connecting.

## Regression note (2026-08-04)

Re-reading the current source
(`native-builtins/src/phases_late/ssl_security.rs`, `javax/net/ssl/
SSLSocketFactory` → `getDefault`, ~line 1518-1548) shows the 2026-07-23 fix
**is still present and looks correct**: it resolves/creates the runtime
default `SSLContext` via `t27_tls::get_runtime_default_ssl_context()` and
sets it at field 0 before returning the factory object, exactly as the
closed doc describes. So this is not a case of the specific fixed line being
reverted — either a different `SSLSocketFactory` acquisition path (not the
static `getDefault()`) is now the one Aether/HttpClient actually exercises,
or something else changed the runtime state `get_runtime_default_ssl_context()`
depends on. Not fully root-caused this session; flagging as a confirmed
regression by symptom (identical exception, identical mechanism, previously
verified fixed "VM-wide" per the 2026-07-26 closure note) rather than
guessing at the new cause. Worth checking on the next pass: other bare
0-field `SSLSocketFactory` allocations still exist elsewhere in the same
file family (`native-builtins/src/t27_tls.rs:4819` and `:4994`,
`javax/net/ssl/HttpsURLConnection.getDefaultSSLSocketFactory`/
`getSSLSocketFactory`) — if Aether's actual call path (Apache HttpClient
**4.x**, package `org.apache.http.conn.ssl`, per today's stack trace — note
the *2026-07-23 fix's own comment* describes the caller as "Apache
HttpClient**5**'s `SSLConnectionSocketFactory`", a different library/package
than what today's stack trace shows) resolves its `SSLSocketFactory` through
one of those, or through `SSLContexts.createDefault()` in a way that
bypasses `t27_tls::get_runtime_default_ssl_context()`'s cache, that would
explain why the fix's own coverage doesn't reach this call path.

Today's evidence (residual rerun `craton-residual32-20260804`, all 3 from
independent modules/shards, all sharing the identical stack trace shape
above):

| Module | Class | Failing method(s) | Maven coordinate that failed to resolve |
|---|---|---|---|
| `module/spring-boot-gson` | `org.springframework.boot.gson.autoconfigure.Gson210AutoConfigurationTests` | `gsonRegistration()` | `com.google.code.gson:gson:jar:2.10` |
| `core/spring-boot` | `org.springframework.boot.diagnostics.analyzer.NoSuchMethodFailureAnalyzerTests` | `parseOpenJ9ErrorMessage()`, `parseHotspotErrorMessage()`, `whenAMethodOnAClassIsMissingThenNoSuchMethodErrorIsAnalyzed()`, `whenAnInheritedMethodIsMissingThenNoSuchMethodErrorIsAnalyzed()` (4/4 tests in the class) | `org.springframework:spring-core:jar:5.3.12` |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests` | `customizerIsCompatibleWithTomcatVersionsWithoutMaxPartCountAndMaxPartHeaderSize()` (1/66 tests in the class) | `org.apache.tomcat.embed:tomcat-embed-core:jar:11.0.7` |

Logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s2/all-jit/logs/module_spring-boot-gson.org.springframework.boot.gson.autoconfigure.Gson210AutoConfigurationTests.{out,err}.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s3/all-jit/logs/core_spring-boot.org.springframework.boot.diagnostics.analyzer.NoSuchMethodFailureAnalyzerTests.{out,err}.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s2/all-jit/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests.{out,err}.log`

## Root cause found (2026-08-04, second pass, same day) — duplicate native registration clobbers the fix

Answers the "Worth checking on the next pass" question above. This is
**not** a bare 0-field allocation elsewhere, and not an Apache HttpClient
5-vs-4 call-path difference — it's a second, stale registration of the exact
same `(class, method, descriptor)` triple that overwrites the fixed one.

`javax/net/ssl/SSLSocketFactory.getDefault()` (`()Ljavax/net/SocketFactory;`)
is registered **twice**. CratonVM's native registry uses
last-registration-wins for a duplicate triple
(`native-api/src/registry.rs`, `pub fn register`: on a repeat key, the
existing slot is updated in place rather than the registration being
skipped or panicking).

1. `native-builtins/src/phases_late/ssl_security.rs:1519-1546`
   (`register_p68_ssl`) — the **correct**, already-fixed registration
   this doc describes above: sets field 0 to the runtime default
   `SSLContext`.
2. `native-builtins/src/net_phase_e.rs:12067-12074`
   (`register_re6_ssl_context`, called from `register_phase_e_networking`)
   — a **second, stale/broken** registration of the identical triple that
   allocates a factory but sets field 0 to `None`:
   ```rust
   let sf = "javax/net/ssl/SSLSocketFactory"; // line 11636
   ...
   r.register(sf, "getDefault", "()Ljavax/net/SocketFactory;", |ctx, _args| {
       let f = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 1);
       ctx.set_field(f, 0, Value::Object(None));
       Ok(Some(Value::Object(Some(f))))
   });
   ```

Boot order (`native-builtins/src/lib.rs`, inside
`register_essential_natives_with_shims`): `register_p68_ssl(registry)` runs
first (line 17504), then `net_phase_e::register_phase_e_networking(registry)`
(→ `register_re6_ssl_context`) runs later (line 17527) — so registration 2
overwrites registration 1's slot, and every caller of
`SSLSocketFactory.getDefault()` gets a factory whose field 0 is `None`.
When Apache HttpClient's `SSLConnectionSocketFactory.createLayeredSocket`
then calls `createSocket(Socket, String, int, boolean)` on that factory,
`ssl_security.rs`'s registration for that overload does:
```rust
let ssl_context = match ctx.get_field(factory, 0) {
    Value::Object(Some(context)) => context,
    _ => return Err(RuntimeError::IllegalStateException {
        message: "SSLSocketFactory has no owning SSLContext".into(),
    }.into()),
};
```
and `Value::Object(None)` hits the `_` arm, throwing exactly the observed
message.

Notably, `net_phase_e.rs` has a code comment a few lines above this
registration (next to the sibling `SSLContext.getDefault()` registration in
the same function) that explicitly documents and relies on
last-registered-wins ordering between `register_p68_ssl` and
`register_re6_ssl_context` for *that* method — so the ordering itself is
known/intentional for `SSLContext.getDefault()`. The
`SSLSocketFactory.getDefault()` duplicate a few lines below it was evidently
never updated to match when the 2026-07-23 fix landed in `ssl_security.rs`,
and still returns the pre-fix broken (field-0-`None`) shape — explaining why
a fix that "looked present and correct" in `ssl_security.rs` doesn't
actually take effect at runtime.

This also explains 3 more classes, seen in this same 2026-08-04 residual
batch (a different shard/assignment than the 3 above), failing on the
identical stack trace/message, via the same `@ClassPathOverrides` mechanism:

| Module | Class | Maven coordinate that failed to resolve |
|---|---|---|
| `module/spring-boot-flyway` | `org.springframework.boot.flyway.autoconfigure.Flyway110AutoConfigurationTests` | `org.flywaydb:flyway-core:11.0.0` |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.HikariDataSourceConfigurationTests` (3/13 methods) | `org.crac:crac:1.3.0` |
| `module/spring-boot-liquibase` | `org.springframework.boot.liquibase.autoconfigure.Liquibase423AutoConfigurationTests` | `org.liquibase:liquibase-core:4.23.1` |

Logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s1/all-jit/logs/module_spring-boot-flyway.org.springframework.boot.flyway.autoconfigure.Flyway110AutoConfigurationTests.{out,err}.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s1/all-jit/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.HikariDataSourceConfigurationTests.{out,err}.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s4/all-jit/logs/module_spring-boot-liquibase.org.springframework.boot.liquibase.autoconfigure.Liquibase423AutoConfigurationTests.{out,err}.log`

### Fix direction (not applied — investigation/doc only; source not modified)

Delete or align the `native-builtins/src/net_phase_e.rs:12067-12074`
`SSLSocketFactory.getDefault()` registration so it matches
`ssl_security.rs`'s (populate field 0 with the runtime default `SSLContext`,
or simply remove the duplicate and let `register_p68_ssl`'s registration —
which already runs first and already carries the fix — stand alone).

## More affected classes (2026-08-04, third pass — test-support itself)

Same residual batch (`craton-residual32-20260804-s4`), same exact stack
trace shape (`IllegalStateException: Resolution failed after 5 attempts` →
... → `IllegalStateException: SSLSocketFactory has no owning SSLContext`),
this time in `test-support/spring-boot-test-support` itself — i.e. the
`ModifiedClassPathExtension`/`ModifiedClassPathClassLoader` machinery's own
test suite, not just downstream consumers of `@ClassPathOverrides`:

| Module | Class | Failing method(s) | Maven coordinate that failed to resolve |
|---|---|---|---|
| `test-support/spring-boot-test-support` | `org.springframework.boot.testsupport.classpath.ModifiedClassPathExtensionOverridesParameterizedTests` | `classesAreLoadedFromParameterInArray(Object[])`, `classesAreLoadedFromParameter(Class)` (2/2) | `org.springframework:spring-context:jar:4.1.0.RELEASE` |
| `test-support/spring-boot-test-support` | `org.springframework.boot.testsupport.classpath.ModifiedClassPathExtensionOverridesTests` | `classesAreLoadedFromOverride()`, `classesAreLoadedFromTransitiveDependencyOfOverride()` (2/2) | `org.springframework:spring-context:jar:4.1.0.RELEASE` |

Logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s4/all-jit/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension-fe760ee1e07b.{out,err}.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-residual32-20260804-s4/all-jit/logs/test-support_spring-boot-test-support.org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension-d45d6d67b931.{out,err}.log`

No new investigation performed here — this is the same duplicate
`SSLSocketFactory.getDefault()` registration bug documented above (same
mechanism, same fix direction), just two more instances found while
triaging the 2026-08-04 residual rerun.

## Affected classes

- `module/spring-boot-gson` — `org.springframework.boot.gson.autoconfigure.Gson210AutoConfigurationTests`
- `core/spring-boot` — `org.springframework.boot.diagnostics.analyzer.NoSuchMethodFailureAnalyzerTests`
- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.autoconfigure.TomcatWebServerFactoryCustomizerTests`
- `module/spring-boot-flyway` — `org.springframework.boot.flyway.autoconfigure.Flyway110AutoConfigurationTests`
- `module/spring-boot-jdbc` — `org.springframework.boot.jdbc.autoconfigure.HikariDataSourceConfigurationTests`
- `module/spring-boot-liquibase` — `org.springframework.boot.liquibase.autoconfigure.Liquibase423AutoConfigurationTests`
- `test-support/spring-boot-test-support` — `org.springframework.boot.testsupport.classpath.ModifiedClassPathExtensionOverridesParameterizedTests`
- `test-support/spring-boot-test-support` — `org.springframework.boot.testsupport.classpath.ModifiedClassPathExtensionOverridesTests`

(Likely affects every other `ModifiedClassPathClassLoader`-based test across
every module, per the original fix's "VM-wide" scope claim.)

---

## STATUS 2026-08-04: FIXED AND CLOSED

Fixed on `fix/ssf-getdefault-dup-registration-20260804`, merged to `dev`.
The "Root cause found (second pass)" section above is correct in full — this
section records the runtime confirmation, the fix, the two residuals it also
closes, one finding that materially changes the blast radius, and the
verification.

### Runtime confirmation of the duplicate

Read from the source, the duplicate is only a strong inference; the registry
census proves it. `--dump-native-registry` emits one row per *registration*
(not per slot), in registration order, each with its `#[track_caller]` site,
so the surviving owner of a triple is the last row.

Baseline (`origin/dev`, commit `c7c63d8818`):

```
javax/net/ssl/SSLSocketFactory getDefault ()Ljavax/net/SocketFactory;
    by=native-builtins/src/phases_late/ssl_security.rs:1521  overwrote=None
javax/net/ssl/SSLSocketFactory getDefault ()Ljavax/net/SocketFactory;
    by=native-builtins/src/net_phase_e.rs:12069              overwrote=bridge
```

After the fix, one row, owned by `ssl_security.rs:1521`. The
`overwrote=bridge` on the second row is the registry telling us, in the
artefact we already emit, that a working implementation was replaced.

### The fix

1. **`net_phase_e.rs`** — the stale duplicate registration is deleted, with a
   comment explaining why the *sibling* `SSLContext.getDefault()` duplicate a
   few lines above is intentional and stays. Same bug shape as the
   `TimeZone.getDefault()` duplicate removed 2026-08-03.
2. **`t27_tls.rs`** — `default_ssl_context_or_create` and
   `default_ssl_socket_factory_obj`. Three natives were each minting a
   "default `SSLSocketFactory`" independently, which is three chances to get
   field 0 wrong; the idiom is converted rather than the sites.
3. **`t27_tls.rs`** — the two residuals this doc flagged for "the next pass"
   are closed. `HttpsURLConnection.getDefaultSSLSocketFactory` (unset-default
   fallback) and `HttpsURLConnection.getSSLSocketFactory` both minted bare
   0-field carriers and so threw the identical exception; both now return the
   wired carrier. Confirmed independently by `probes/SsfSurfaceProbe.java`,
   which fails **all three** entry points on the baseline binary and passes
   all three on the fixed one.

   *Follow-up, same day:* the "known remaining gap" this section originally
   left open — a per-connection `setSSLSocketFactory` not being readable back
   — is **also closed now**, and its stated reason was wrong. It claimed the
   fix needed "a new GC-rooted per-connection table (scan + post-move
   remap)". It did not: the factory belongs in the real JDK
   `sslSocketFactory` instance field, which is an ordinary object field and
   therefore already a GC root and already remapped by the moving collector.
   The counter-example was in the same file all along —
   `setHostnameVerifier`/`getHostnameVerifier` store into the real
   `hostnameVerifier` instance field for exactly that reason, and say so.
   See the `huc-per-connection-ssf-readback` commit and
   `probes/HucFactoryReadbackProbe.java`.
4. **`ssl_security.rs`** — the layered
   `createSocket(Socket,String,int,boolean)` overload no longer converts a
   lost field 0 into a hard `IllegalStateException`. There is no such thing
   as a contextless `SSLSocketFactory` in the JDK, so an empty field 0 always
   means one of our synthetic carriers lost it, and the JDK-faithful answer is
   the process default context — which `getDefault()` would have supplied
   anyway. It cannot weaken trust: caller anchors are resolved by factory
   identity (`p68_factory_trust_roots`), not through this context, and the
   default context validates against the platform store, which is strictly
   stricter than a permissive caller-installed `TrustManager`, never laxer.
   This is the third time one mis-wired carrier has been converted into an
   exception thrown before any network I/O, taking out a whole test family.

### The blast radius is Maven-cache-dependent — this is why it looked arbitrary

The single most useful finding of this pass, and the reason the affected-class
list above is **incomplete**.

`ModifiedClassPathClassLoader` resolves coordinates through Aether against
`System.getProperty("user.home") + "/.m2/repository"` first, and only reaches
the network on a miss. So an affected test **passes** whenever its coordinates
happen to already be in the local repository, and fails only on a cold one.
Nothing about the class distinguishes the two cases.

That has two consequences worth remembering:

* The 2026-08-04 residual rerun's 8 classes were not "the affected set" — they
  were the subset whose coordinates that host had not cached. On a fresh host
  the set is larger; on a fully warm one the bug is invisible.
* **A warm-cache A/B is worthless here, and silently so.** During this pass a
  83-class comparison came back `base: 83 PASS / fixed: 83 PASS`, which reads
  as "no bug" — only because the *fixed* arm, run an hour earlier, had
  downloaded the artifacts into the shared `~/.m2` that the baseline arm then
  hit. Every arm must get its own cold repository (`-Duser.home=<fresh dir>`)
  or the comparison measures download history, not the VM.

Re-running the full `@ClassPathOverrides`/`@ClassPathExclusions`/
`@ForkedClassPath` population (83 classes, found by grepping the Spring Boot
checkout for those annotations) with a **cold repository per arm** gives the
real picture:

| arm | result |
|---|---|
| baseline (`origin/dev`) | **13 FAIL**, 70 PASS |
| fixed | **83 PASS**, 0 FAIL |

All 13 baseline failures carry the exact
`IllegalStateException: SSLSocketFactory has no owning SSLContext`, and all 13
pass on the fixed binary. Five were never listed in this doc:

| Module | Class | Tests fixed |
|---|---|---|
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` | 61/61 |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.SpringProfileArbiterTests` | 7/7 |
| `core/spring-boot` | `org.springframework.boot.logging.logback.LogbackLoggingSystemTests` | 2/86 |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.condition.ConditionalOnCheckpointRestoreTests` | 1/2 |
| `core/spring-boot-test` | `org.springframework.boot.test.json.DuplicateJsonObjectContextCustomizerFactoryTests` | 1/1 |

`SpringProfileArbiterTests` is worth calling out: the 2026-07-23 fix in
`ssl_security.rs` is *named after it*
(`springprofilearbitertests-ssf-getdefault-no-context`). It failing on the
baseline binary is direct proof that the named fix had been inert since the day
the duplicate registration was authored — not that it regressed later.

### Verification

| Check | Baseline (`origin/dev`) | Fixed | Control |
|---|---|---|---|
| `probes/SsfDefaultProbe.java` (getDefault → layered createSocket → HTTPS GET) | `FAIL-NO-OWNING-CONTEXT` | PASS, TLS13_AES_256_GCM_SHA384, HTTP 200, 9787 body bytes | real JDK 21: PASS, same 9787 bytes |
| `probes/SsfSurfaceProbe.java` (7 checks over every factory-acquisition path) | 4 pass / **3 fail** | **7 pass / 0 fail** | real JDK 21: 7 pass / 0 fail |
| the 8 classes listed in this doc | 8 FAIL, all with the SSF signature | **8 PASS** | HotSpot: 8 PASS, test-for-test identical |
| all 83 `ModifiedClassPathClassLoader` classes, cold repo per arm | 13 FAIL / 70 PASS | **83 PASS** | — |
| 177 TLS/HTTP-adjacent Spring Boot classes | 171 PASS / 1 FAIL / 5 EMPTY | identical | — |
| `cargo test -p cratonvm-native-builtins -p cratonvm-native-api` | — | 3250 + 270 + 60 pass, 0 fail | — |

The one class that moved in the 177-class slice
(`NettyReactiveWebServerFactoryTests`, FAIL in both arms, failed 1 → 2) is a
load-induced flake in `whenARequestIsActiveAfterGracefulShutdownEndsThen
StopWillComplete`, not an SSL failure: three interleaved A/B repeats give
`failed=1` on both binaries every time. Its one real failure,
`whenSslBundleIsUpdatedThenSslIsReloaded`, is identical in both arms and is a
separate pre-existing endpoint-identification issue.

### Guards

Both in `native-builtins/tests/registry_contracts.rs`, and both verified
non-vacuous by injecting the defect and watching them fail:

* `ssl_entry_points_keep_their_documented_owning_registration` — pins the
  *surviving owner site* (not a registration count) for the three duplicated
  `javax.net.ssl` entry points, so a deliberately-intentional duplicate stays
  legal while a silent change of winner does not. Re-injecting the deleted
  registration fails it, naming both competing sites.
* `no_native_mints_a_field_less_ssl_socket_factory_carrier` — scans the four
  TLS source files for 0-field `SSLSocketFactory` allocations, with a
  found-count floor so a rename or a file split cannot make it pass vacuously.
  Injecting a 0-field allocation fails it.

### Note for the next duplicate-registration bug

`--dump-native-registry` already answers "who owns this triple, and did they
overwrite someone" for all ~11,900 registrations. Reading a file and finding
the fix present proves nothing about which registration wins at runtime; the
census does. That is the check to run first the next time a closed fix's
symptom returns unchanged.
