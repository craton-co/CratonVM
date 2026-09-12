# 4 classes `NoClassDefFoundError` on BouncyCastle/UnboundID — `cp.txt` points at Gradle module-cache paths that no longer exist

## Status
Confirmed fixture rot, **not a CratonVM bug**. The classpath was correct when
`Build-Classpath` generated `apps/tomcat/.suite/cp.txt`; the referenced jars
have since been evicted from the Gradle module cache by something outside
this repo (a `--refresh-dependencies`, a cache GC, a Gradle version bump —
not determined). Would fail identically on any JVM: the classes these
`NoClassDefFoundError`s name are simply not resolvable from any file that
still exists.

## Measured
2026-09-12, `dev@c0bebbde5`, local Windows box, real JDK 25, `-Parallel 2`,
two JIT-tier arms (`CRATONVM_C2_SUPERSEDE=0` and `CRATONVM_JIT_FORCE_C2=1`,
651-class complete suite each). All 4 classes below FAIL identically in both
arms — consistent with a classpath-load-time failure that has nothing to do
with JIT tiering.

## The classes and their missing class

| class | fails calling | missing class | jar `cp.txt` names |
|---|---|---|---|
| `org.apache.tomcat.security.TestSecurity2018` | `TesterKeystoreGenerator.generateKeystore` | `org/bouncycastle/asn1/x500/X500Name` | `bcprov-jdk18on-1.84.jar` |
| `org.apache.tomcat.util.net.TestLargeClientHello` | `TesterKeystoreGenerator.generateKeystore` | `org/bouncycastle/asn1/x500/X500Name` | `bcprov-jdk18on-1.84.jar` |
| `org.apache.tomcat.util.net.TestPQC` | `TesterKeystoreGenerator.generatePQCCertificate` | `org/bouncycastle/jce/provider/BouncyCastleProvider` | `bcprov-jdk18on-1.84.jar` (9 of 9 sub-tests, all the same trace) |
| `org.apache.catalina.realm.TestJNDIRealmIntegration` | `createLDAP` | `com/unboundid/ldap/listener/InMemoryDirectoryServerConfig` | `unboundid-ldapsdk-7.0.4.jar` |

## Confirmed: the referenced files are simply gone

`cp.txt` names these exact paths:
```
C:\Users\Victor\.gradle\caches\modules-2\files-2.1\org.bouncycastle\bcprov-jdk18on\1.84\2d5651789941d2f8ae9b8771f23356de6b61e96b\bcprov-jdk18on-1.84.jar
C:\Users\Victor\.gradle\caches\modules-2\files-2.1\org.bouncycastle\bcpkix-jdk18on\1.84\dab889a3259e27caec6e6c2f3bde94af036b2fcc\bcpkix-jdk18on-1.84.jar
C:\Users\Victor\.gradle\caches\modules-2\files-2.1\com.unboundid\unboundid-ldapsdk\7.0.4\2fe2d5461a87a58aee97f836e3af63ef8ce7b29e\unboundid-ldapsdk-7.0.4.jar
```
None of the three exist on disk — not just the leaf jar, the whole hashed
version directory is absent (`.../bcprov-jdk18on/1.84/` doesn't exist at all).
`jar tf` on any of them fails with `NoSuchFileException`, which is exactly the
shape a stale classpath entry produces at class-load time: the jar is a dead
byte string in `cp.txt`, so the classloader reports the class it would have
contained as simply undefined, not "jar not found."

This is the same failure mode `run-tomcat-suite.md` §6 already documents for
a *first-time* miss (renamed-vs-upstream jar names silently dropping
BouncyCastle/EasyMock off the classpath for weeks) — except this fixture's
`cp.txt` was correct once, and the jars disappeared underneath it later. The
"-Setup"/"-RefreshClasspath" miss-detection in that harness runs at
regeneration time, not at every suite run, so a cache eviction between
regenerations goes undetected until a test actually calls a class from the
missing jar.

## What would fix it, and what wouldn't

Re-resolving against currently-live caches:

| jar | still findable? | where |
|---|---|---|
| `bcprov-jdk18on-1.84.jar` | yes | `~/.m2/repository/org/bouncycastle/bcprov-jdk18on/1.84/` |
| `bcpkix-jdk18on-1.84.jar` | yes | `~/.m2/repository/org/bouncycastle/bcpkix-jdk18on/1.84/` |
| `unboundid-ldapsdk-7.0.4.jar` | **no** — searched the full Gradle module cache, `~/.m2`, and `apps/tomcat/.suite/lib`; not present anywhere on this box | — |

So `apps\tomcat-suite-runner\run-tomcat-suite.ps1 -RefreshClasspath` (which
re-runs `Build-Classpath`'s multi-root jar search: `tomcat-build-libs` →
Gradle module cache → `~/.m2` → `.suite/lib`, per `run-tomcat-suite.md` §6)
should recover the BouncyCastle-dependent 3 of 4 classes by falling through
to the `.m2` copies, but **not** `TestJNDIRealmIntegration` — UnboundID LDAP
SDK 7.0.4 needs to be fetched or dropped into `apps/tomcat/.suite/lib` by
hand first. Not attempted in this session; the two-line grep above (does the
jar exist at the path `cp.txt` names) is enough to characterize all 4
classes, and re-running the full 651-class suite to confirm the fix costs
another ~100 minutes per arm.

## Not yet done
- `-RefreshClasspath` + rerun of these 4 classes to confirm the fix.
- Sourcing an `unboundid-ldapsdk-7.0.4.jar` (or the version Tomcat's own
  `ant.properties`/Ivy build actually pins) into `apps/tomcat/.suite/lib`.
