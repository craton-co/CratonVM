# Unbuilt Maven test-webapp submodule (`webapp-virtual-webapp`) — 1 class

**Not a CratonVM bug.** Fails identically on real JDK 25 (HotSpot) in the
same fixture.

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
