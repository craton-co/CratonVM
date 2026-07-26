# KC26-PIC.1 / KC26-RX.1 — blocked by a classloading bug, NOT a missing fixture

**Status: still banned, blocked by a separate boot-time bug found this session, not by "no fixture" (a prior doc's claim that Keycloak has no fixture on this host was wrong — see below).**

## Correction to a prior finding

`docs/known-issues/full-ban-inventory-status-20260726.md` (this same
session, written before this investigation) listed KC26-PIC.1/KC26-RX.1 as
blocked because "needs a real Keycloak 26.2.4 checkout; not present on this
host." **That was incorrect** — a full real Keycloak fixture exists on this
host at `/home/victor/.m2/repository/org/keycloak/` (Maven repo with
Keycloak 26.6.1 and a `999.0.0-SNAPSHOT` master build, including a
bootable `keycloak-quarkus-dist-26.6.1.tar.gz` server distribution and
several `keycloak-tests-*` test jars). It was found this session only
after being told to search harder than the initial `find -iname
'*keycloak*'` sweep, which missed it because the previous searches only
checked shallow depth / obvious paths and the repo-scripts directory
(`apps/keycloak-suite-runner`, which is just the *runner*, not the
checkout it expects at `apps/keycloak`).

## What was tried

Extracted the real distribution
(`/data/tmp/kc-dist/keycloak-26.6.1/`), wrapped the CratonVM binary as
`bin/java` (matching this host's established WildFly-boot pattern —
`JAVA_HOME=<fake-dir-with-cratonvm-as-bin/java> CRATONVM_JAVA_HOME=<real
JDK25>`), and ran:

```bash
cd /data/tmp/kc-dist/keycloak-26.6.1
JAVA_HOME=/data/tmp/kc-javahome CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  timeout 90 bash bin/kc.sh start-dev --http-enabled=true --hostname-strict=false
```

## Result: boot fails immediately, before reaching KC26-PIC.1/RX.1's code paths at all

```
Exception in thread "main" java/lang/ExceptionInInitializerError
	at io/quarkus/bootstrap/runner/QuarkusEntryPoint.main(QuarkusEntryPoint.java:37)
	at io/quarkus/bootstrap/runner/QuarkusEntryPoint.doRun(QuarkusEntryPoint.java:86)
	at org/keycloak/quarkus/runtime/KeycloakMain.main(KeycloakMain.java:68)
Caused by: java/lang/NullPointerException: Cannot invoke "String.toLowerCase()"
because "org.keycloak.common.Version.VERSION" is null
	at org/keycloak/common/Version.<clinit>(Version.java:43)
```

`Version.<clinit>` (decompiled from
`keycloak-common-26.6.1.jar!/org/keycloak/common/Version.class`) does:

```java
InputStream is = Version.class.getResourceAsStream("/keycloak-version.properties");
Properties p = new Properties();
p.load(is);
VERSION = p.getProperty("version");   // null if the resource wasn't found or lacks "version"
```

`VERSION` ends up `null`, and the next line calls `.toLowerCase()` on it,
NPEing. This means `getResourceAsStream("/keycloak-version.properties")`
either returned `null` (resource not found) or returned a stream that
didn't parse to a Properties object containing a `version` key.

**Not yet root-caused whether this is:**
1. A genuine CratonVM classloader resource-lookup gap specific to
   Quarkus's "fast-jar" runner layout (`QuarkusEntryPoint` uses a custom
   classloader over a `lib/main/` + `app/` + `quarkus/` directory
   structure, not a single flat jar — a layout this host's WildFly-boot
   precedent didn't need to handle), or
2. Something specific to how `keycloak-version.properties` is packaged
   (it may live in a different jar than `Version.class` itself, requiring
   correct classpath-wide resource search across the runner's multiple
   `lib/main/*.jar` entries).

This is **not a JIT bug** — it happens during class initialization before
any JIT-relevant code executes, and blocks reaching the actual KC26-PIC.1
(Picocli command reflection / SmallRye config timeout) and KC26-RX.1
(RxJava3 Infinispan stream hang) code paths entirely, so neither ban could
be re-tested this session.

## Recommendation

A future session should either (a) root-cause and fix the
`getResourceAsStream` gap for Quarkus's fast-jar runner layout (which would
also unblock testing potentially many other Quarkus-based JIT bans in this
codebase, not just these two), or (b) find/build a Keycloak test harness
that runs individual test classes directly via a JUnit launcher (like the
`hib-suite-runner`/`CratonRunner` pattern already established for
Hibernate on this host) rather than booting the full Quarkus server, which
would sidestep this specific boot blocker and let KC26-PIC.1/RX.1 be
tested against their actual named test classes (`PicocliTest`,
`RealmModelTest`) directly. The `keycloak-tests-*` jars in the m2 repo
(Keycloak 26+'s newer test-framework module) may already contain compiled
test classes suitable for this — not yet explored this session due to time.
