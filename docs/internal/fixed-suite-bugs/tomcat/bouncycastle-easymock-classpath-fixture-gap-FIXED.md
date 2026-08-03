# Fixture gap: BouncyCastle + EasyMock jars missing from the Windows suite classpath — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED** 2026-08-03 — retired from `docs/known-issues/tomcat/` |
| **Was** | `bouncycastle-jar-missing-classpath-fixture-gap.md` (3 classes) + `easymock-jar-missing-classpath-fixture-gap.md` (8 classes) — one root cause, one fix |
| **Verdict** | Fixture gap, NOT a CratonVM bug. With the classpath repaired, **CratonVM passes 13/13** of the affected classes, JIT and `--nojit`; **HotSpot passes 5/13** (see the EasyMock/JDK 25 note at the bottom) |

## Symptom

Two separately-filed known-issues docs, same shape — a third-party test
dependency was absent from `apps/tomcat/.suite/cp.txt`, so classes failed on
`NoClassDefFoundError` for a library that has nothing to do with the VM:

```
1) testClientMLDSAwithMLDSAServer[JSSE](org.apache.tomcat.util.net.TestPQC)
java.lang.NoClassDefFoundError: org/bouncycastle/jce/provider/BouncyCastleProvider

1) testLargeClientHelloWithSessionResumption(org.apache.tomcat.util.net.TestLargeClientHello)
java.lang.NoClassDefFoundError: org/bouncycastle/asn1/x500/X500Name

1) testPostRequestInvalidNonceAsParameterValidPath(org.apache.catalina.filters.TestRestCsrfPreventionFilter)
java.lang.NoClassDefFoundError: org/easymock/EasyMock
```

## Root cause

`Build-Classpath` in `apps/tomcat-suite-runner/run-tomcat-suite.ps1` resolved
each jar by **one** file-name pattern — the name Tomcat's own
`ant download-compile` writes into `${base.path}`:

```
bouncycastle-provider-1.84.jar      easymock-5.6.0.jar
```

`$LIB` (`C:\Users\Victor\tomcat-build-libs`) does not exist on this box, so the
only surviving root was the Gradle module cache — which stores artifacts under
their **upstream Maven** names:

```
bcprov-jdk18on-1.84.jar   bcpkix-jdk18on-1.84.jar   bcutil-jdk18on-1.84.jar
```

Nothing matched, and the miss was a `Write-Warning` that scrolled past in the
setup log; the `| Where-Object { $_ -ne $null }` filter then silently dropped
the entry. `cp.txt` had **14** parts where it should have had 19, and eleven
classes turned into `NoClassDefFoundError` "failures" that read like CratonVM
regressions. Same failure mode as `vm/build.rs`'s `cargo:warning=` swallowing
the javac transcript: a diagnostic that is technically emitted but practically
invisible is not a diagnostic.

## Fix

`apps/tomcat-suite-runner/run-tomcat-suite.ps1`

1. **Multi-name lookup.** Every jar entry is now a *list* of candidate
   patterns — Tomcat's renamed name first, then the Maven artifact name
   (`@('bouncycastle-provider-1.84.jar','bcprov-jdk18on-1.84.jar')`).
2. **More roots.** `$LIB`, the Gradle module cache, `~/.m2/repository`, and a
   new suite-local drop box `apps\tomcat\.suite\lib` (for jars that live in
   none of the caches).
3. **Fail loud, fail closed.** Misses are collected, printed in red with the
   roots that were searched, written to `.suite\cp-missing.txt`, and `Die` —
   unless the caller passes the new `-AllowMissingLibs`. A short classpath can
   no longer leave the harness silently.
4. **`-RefreshClasspath`.** Rebuilds `cp.txt` + `all-tests.txt` in seconds
   without the ~20 min `ant deploy && ant test-compile`, so a newly-supplied
   jar can be picked up immediately.

`apps/tomcat-suite-runner/run-one.ps1` — two defects found while re-running the
affected classes with it, both of which corrupt any single-class repro:

5. **It did not pass the suite's JVM arguments.** Its header claimed "exactly
   the suite's environment", but it only exported the four `CRATONVM_*` env
   vars: no `--add-opens`, no `-Dtomcat.test.*`. Every EasyMock-based class
   therefore failed under `run-one.ps1` while passing under the suite runner —
   a pure launch-environment artifact. The `$argv` list is now a copy of
   `Invoke-Mode`'s `$jvmArgs` (4 × `--add-opens`, the `tomcat.test.*`
   properties, `-Xint`/`--nojit` for `-NoJit`).
6. **Its exit code was always 0.** `Start-Process -PassThru` returns a
   `Process` whose `.ExitCode` reads back as `$null` once the child is gone
   unless `.Handle` was touched first, so the script printed `rc=` and then
   `exit $null` → **exit 0**. Every driver that trusted that code scored a
   failing class as a PASS. Fixed by caching `$p.Handle`, plus `rc=125` as a
   never-silently-zero fallback.

Jar provisioning: the three BouncyCastle 1.84 jars were already in the Gradle
cache at exactly the version `build.properties.default` pins. `easymock-5.6.0.jar`
was on no cache on this box; it was fetched from Maven Central into
`.suite\lib` and verified against the MD5 **and** SHA-1 that
`build.properties.default` pins (`2be7351f…` / `f8e15a47…`, both matched).
`cp.txt` is now 19 parts and `-RefreshClasspath` exits 0 with no misses.

## Verification — no CratonVM defect underneath

Binary `cratonvm-bccp-20260803.exe` (release, branch
`fix/tomcat-bc-classpath-fixture-20260803` off `dev` `36f1157ad`), one process
per class via the fixed `run-one.ps1`, status taken from the JUnit banner (not
the exit code).

| Class | HotSpot 25.0.3 | CratonVM JIT | CratonVM `--nojit` |
|---|---|---|---|
| `tomcat.util.net.TestPQC` | PASS `OK (39)` | PASS `OK (39)` | PASS `OK (39)` |
| `tomcat.util.net.TestLargeClientHello` | PASS `OK (1)` | PASS `OK (1)` | PASS `OK (1)` |
| `tomcat.security.TestSecurity2018` | PASS `OK (1)` | PASS `OK (1)` | PASS `OK (1)` |
| `catalina.core.TestAsyncContextImpl` | PASS `OK (70)` | PASS `OK (70)` | PASS `OK (70)` |
| `catalina.filters.TestRestCsrfPreventionFilter` | PASS `OK (23)` | PASS `OK (23)` | PASS `OK (23)` |
| `catalina.realm.TestJNDIRealm` | FAIL ×3 † | PASS `OK (4)` | PASS `OK (4)` |
| `catalina.session.TestPersistentManager` | FAIL ×1 † | PASS `OK (2)` | PASS `OK (2)` |
| `catalina.startup.TestWebappServiceLoader` | FAIL ×7 † | PASS `OK (7)` | PASS `OK (7)` |
| `catalina.valves.TestCrawlerSessionManagerValve` | FAIL ×5 † | PASS `OK (5)` | PASS `OK (5)` |
| `catalina.valves.TestLoadBalancerDrainingValve` | FAIL ×192 † | PASS `OK (192)` | PASS `OK (192)` |
| `catalina.valves.TestSSLValve` | FAIL ×19 † | PASS `OK (19)` | PASS `OK (19)` |
| `coyote.TestRequest` | FAIL ×4 † | PASS `OK (4)` | PASS `OK (4)` |
| `jasper.servlet.TestTldScanner` | FAIL ×1 † | PASS `OK (3)` | PASS `OK (3)` |

The three classes the BouncyCastle doc named go green everywhere, with
identical test counts on both VMs. Nothing was hiding under the classpath gap.

## † The residual is HotSpot's, not CratonVM's: EasyMock 5.6.0 cannot mock classes on JDK 25

The 8 daggered rows fail **on stock HotSpot**, with Tomcat's own `<junit>`
jvmargs, with the jar present:

```
java.lang.RuntimeException: Failed to mock class org.apache.catalina.connector.Connector
Caused by: java.lang.IllegalArgumentException:
  org.easymock.mocks.Connector$$$EasyMock$2 must be defined in the same package
  as org.easymock.internal.ClassProxyFactory
```

`ClassProxyFactory.classLoadingStrategy()` (5.6.0, verified by `javap`) is:

```java
if (ClassInjector.UsingUnsafe.isAvailable()) return new ClassLoadingStrategy.ForUnsafeInjection();
return ClassLoadingStrategy.UsingLookup.of(MethodHandles.lookup());   // <- ClassProxyFactory's OWN lookup
```

On JDK 25 `UsingUnsafe.isAvailable()` is **false** (byte-buddy 1.18.8's
`UsingUnsafe.Factory.resolve` now requires an `Instrumentation`; the
`sun.misc.Unsafe` define-class path is gone), so it falls back to a lookup
rooted in `org.easymock.internal` — and `Lookup.defineClass` can only define
into its own package. Measured with a byte-buddy probe on this JDK:

| flags | `UsingUnsafe` | `UsingReflection` | `UsingLookup` |
|---|---|---|---|
| none | false | false | true |
| `--add-opens java.base/java.lang=ALL-UNNAMED` | false | **true** | true |
| … + `--add-{opens,exports} java.base/jdk.internal.misc` | false | true | true |

No flag combination brings `UsingUnsafe` back, so this is not fixable from the
harness — it is an upstream EasyMock-5.6.0-vs-JDK-25 incompatibility. CratonVM
passes all 8 because its `Unsafe` still offers the define-class path byte-buddy
looks for, i.e. CratonVM is *more* permissive here, not less.

**Consequence for baselines:** a HotSpot control run on JDK 25 will show these
8 classes red forever. They must not be counted as CratonVM regressions, and a
future "CratonVM-only PASS" diff on them is expected, not suspicious.

## Out of scope (separate, already-filed issues)

* `TestSSLValveWithProxy01/02` — need a real `httpd` binary, see
  `docs/known-issues/tomcat/httpd-proxy-integration-windows-connection-refused.md`.
* `TestOcspEnabled`, `TestSsl` — pre-existing flake noted in
  `serversocket-bind-socketaddress-noop-localport-zero-FIXED.md`, not this gap.
* `objenesis` resolves to 3.3 from the Gradle cache where
  `build.properties.default` pins 3.5. Harmless for these classes (the failure
  above is in class definition, before objenesis is reached) and left alone
  rather than pinned, since the entry deliberately globs `objenesis-3.*.jar`.
