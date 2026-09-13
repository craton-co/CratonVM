# 4 classes `NoClassDefFoundError` on BouncyCastle/UnboundID — `cp.txt` pointed at Gradle module-cache paths that no longer existed — FIXED 2026-09-12

## Status
**FIXED (fixture), never a CratonVM bug.** Moved from
`docs/known-issues/tomcat/` 2026-09-12. All 4 classes PASS on HotSpot and on
CratonVM against the rebuilt classpath. The harness that produced the stale
file no longer can: `cp.txt` names harness-owned copies, and every run checks
the file before it starts.

| class | HotSpot | CratonVM |
|---|---|---|
| `org.apache.tomcat.security.TestSecurity2018` | OK (1) | OK (1) |
| `org.apache.tomcat.util.net.TestLargeClientHello` | OK (1) | OK (1) |
| `org.apache.tomcat.util.net.TestPQC` | OK (39) | OK (39) |
| `org.apache.catalina.realm.TestJNDIRealmIntegration` | OK (76) | OK (76) |

Measured 2026-09-12, local Windows box, real JDK 25, one class per process with
the suite's JVM arguments, CratonVM built from `dev@d292a7bc1` plus the
collection-dedup fix of the same day
(`c1-c2-single-arm-timing-sensitive-flakes-RESOLVED-20260912.md`).

## What was measured (the original report)

2026-09-12, `dev@c0bebbde5`, `-Parallel 2`, two JIT-tier arms
(`CRATONVM_C2_SUPERSEDE=0` and `CRATONVM_JIT_FORCE_C2=1`), 651-class suite
each. The 4 classes failed identically in both arms:

| class | fails calling | missing class |
|---|---|---|
| `TestSecurity2018` | `TesterKeystoreGenerator.generateKeystore` | `org/bouncycastle/asn1/x500/X500Name` |
| `TestLargeClientHello` | `TesterKeystoreGenerator.generateKeystore` | `org/bouncycastle/asn1/x500/X500Name` |
| `TestPQC` | `TesterKeystoreGenerator.generatePQCCertificate` | `org/bouncycastle/jce/provider/BouncyCastleProvider` |
| `TestJNDIRealmIntegration` | `createLDAP` | `com/unboundid/ldap/listener/InMemoryDirectoryServerConfig` |

`cp.txt` named those jars inside `~/.gradle/caches/modules-2/files-2.1/...`,
and the whole hashed version directories were gone. A dead classpath entry is
silent: the class loader reports the class it would have held as undefined,
not "jar not found".

## It was six dead entries, not three

Checking every entry, not just the three the failures named:

```
MISSING ...\net.bytebuddy\byte-buddy\1.14.12\...\byte-buddy-1.14.12.jar
MISSING ...\com.unboundid\unboundid-ldapsdk\7.0.4\...\unboundid-ldapsdk-7.0.4.jar
MISSING ...\org.bouncycastle\bcprov-jdk18on\1.84\...\bcprov-jdk18on-1.84.jar
MISSING ...\org.bouncycastle\bcpkix-jdk18on\1.84\...\bcpkix-jdk18on-1.84.jar
MISSING ...\org.bouncycastle\bcutil-jdk18on\1.84\...\bcutil-jdk18on-1.84.jar
MISSING ...\org.apache.ant\ant\1.10.11\...\ant-1.10.11.jar
```

The dead `ant-1.10.11.jar` would have broken `TestDeployTask`/`TestJspC` the
same way on the next run that reached them.

## Root cause

`Build-Classpath` (`apps/tomcat-suite-runner/run-tomcat-suite.ps1`) wrote the
absolute path of whatever jar a directory walk found, in place, inside
dependency caches other tools own and garbage-collect. Its miss detection ran
only when the file was regenerated, so an eviction between regenerations went
unnoticed until a test needed a class from the missing jar. The same walk
matched `byte-buddy-1.*.jar` by glob and took the first hit, which is how the
classpath carried a byte-buddy too old for EasyMock
(`easymock-bytebuddy-classpath-version-gap-FIXED-20260912.md`).

## Fix

`apps/` is gitignored, so the harness change lives in the local checkout only
(pre-change copies: `*.bak-20260912` next to each script).

- **Which jar.** Versions, Maven locations and checksums are read from
  Tomcat's own `build.properties.default` (`<lib>.jar`, `<lib>.loc`,
  `<lib>.checksum.*`, overridable by `build.properties`), matched by exact file
  name. A pinned version that is not on disk falls back to the highest version
  present and is written to `.suite/cp-substituted.txt`. The two libraries with
  no Tomcat pin (`ecj`, `ant`) take the highest version; `ecj` excludes the
  retired `org.eclipse.jdt.core.compiler:ecj` coordinates, whose 2016 `4.6.1`
  outranks the current `3.x` line on version number alone — the first run of
  the new resolver picked it.
- **Where it lives.** Every resolved jar is copied into
  `apps/tomcat/.suite/lib/pinned/` (provenance in `PROVENANCE.txt`) and
  `cp.txt` names the copy. The pinned directory is searched first, so a later
  cache eviction changes nothing.
- **Checked every run.** `run-tomcat-suite.ps1` checks every `cp.txt` entry
  before selecting classes and rebuilds the classpath if any is gone;
  `run-one.ps1` does the same through `-RefreshClasspath`; the Linux
  `run-tomcat-suite.sh` refuses to start and names each dead entry.
- **Fetching.** `-RefreshClasspath -FetchMissingLibs` downloads a pinned jar
  no root holds, from Tomcat's own `.loc`, and keeps it only if it matches
  Tomcat's own checksum. That is how `unboundid-ldapsdk-7.0.4.jar` (not present
  anywhere on this box) and `objenesis-3.5.jar` were sourced, both verified.

The resulting classpath: `junit-4.13.2`, `hamcrest-3.0`, `easymock-5.6.0`,
`objenesis-3.5`, `byte-buddy-1.18.8`, `unboundid-ldapsdk-7.0.4`,
`derby*-10.17.1.0`, `bc{prov,pkix,util}-jdk18on-1.84`, `ecj-3.45.0`,
`ant{,-launcher}-1.10.17`, every one a pinned copy.
`apps/tomcat-suite-runner/run-tomcat-suite.md` §6 describes the contract.

## Repro of the fix

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
powershell -ExecutionPolicy Bypass -File .\run-tomcat-suite.ps1 -RefreshClasspath -FetchMissingLibs
powershell -ExecutionPolicy Bypass -File .\run-one.ps1 -Vm hotspot -Class org.apache.tomcat.util.net.TestPQC
```
