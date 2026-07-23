# Unbuilt Maven test-webapp submodule (`webapp-virtual-webapp`) — 1 class

> ⚠️ **ROOT-CAUSE THEORY WAS WRONG — corrected 2026-07-23.** There is no
> `pom.xml` anywhere under `test/webapp-virtual-webapp/` — it isn't a Maven
> submodule at all, just a `src/main/...`-shaped resource fixture tree
> (properties files, TLDs, JSPs, no `.java` sources). Reading
> `TestVirtualContext.java` directly showed `target/classes` and the
> sibling `test/webapp-virtual-library/target/WEB-INF/classes` just need to
> **exist as directories** (`StandardRoot.createWebResourceSet` throws
> `IllegalArgumentException` on a missing directory, regardless of
> content) — for the specific method that was failing
> (`testAdditionalWebInfClassesPaths`), no Maven build of any kind was ever
> needed. **Fix:** `mkdir -p
> test/webapp-virtual-webapp/target/classes/rsrc` (+ a placeholder
> `.properties` file, since a *different* method does enumerate
> `target/classes/rsrc/*`) and `mkdir -p
> test/webapp-virtual-library/target/WEB-INF/classes`. That specific method
> now passes on both VMs. **A different method in the same class,
> `testVirtualClassLoader`, still fails on both VMs** (previously
> `expected:<200> but was:<404>` on HotSpot, unrelated to the two
> directories above) — HotSpot still 404s, and **CratonVM now returns 500**
> instead of 404, a smaller regression than the class's original blanket
> failure. See
> [regressions-revealed-by-fixture-completion-20260723.md](regressions-revealed-by-fixture-completion-20260723.md).

**Historical, WRONG root-cause theory below (there is no Maven submodule to
build) — kept for reference. See the correction note above for the actual
fix.**

## Symptom

```
java.lang.IllegalArgumentException: Unable to create WebResourceSet from [/data/data/tomcat-dohead-fixture-20260717/test/webapp-virtual-webapp/target/classes]
	at org.apache.catalina.webresources.StandardRoot.createWebResourceSet(StandardRoot.java:432)
	at org.apache.catalina.loader.TestVirtualContext.testAdditionalWebInfClassesPaths(TestVirtualContext.java:209)
```

## Affected classes

- `org.apache.catalina.loader.TestVirtualContext`

## Root cause

`target/classes` is a Maven build-output convention (not an Ant one — the
rest of the Tomcat build uses Ant/`output/`). `test/webapp-virtual-webapp/`
is a small standalone Maven submodule Tomcat's test suite uses purely as a
"pre-built classes directory to point a virtual WebResourceSet at" fixture;
it was never `mvn compile`d in this checkout.

## Fix

```sh
cd /data/data/apps/tomcat/test/webapp-virtual-webapp
mvn compile     # or: mvn -q compile if a local Maven + JDK is available
```
Verify `target/classes` exists and is non-empty afterward. If Maven isn't
installed on the Azure host, `sudo apt-get install maven` first (needs a
real JDK — reuse `/home/victor/jdk25` via `JAVA_HOME`).

## Verify

Rerun `org.apache.catalina.loader.TestVirtualContext` under HotSpot; expect
its `testAdditionalWebInfClassesPaths` case (and the rest of the class) to
PASS once `target/classes` is populated.
