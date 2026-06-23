# SBR-08 — jar-URL `openConnection()` returns abstract `java.net.JarURLConnection`

**Status:** 🟠 Open — object-identity cluster (deferred).
**Recommendation:** FIX — concrete-type gap in the `jar:` URL handler.

> Investigated 2026-06-22: part of the cluster (SBR-08/09/10/11/13) where
> CratonVM synthesizes a JDK object under its **abstract/public type** instead of
> the concrete `sun.*` impl. Each is its own subsystem fix (here: the `jar:`
> stream handler must return a `sun.net.www.protocol.jar.JarURLConnection`-typed
> object). Not a one-liner; no safe shared fix. Functionally the connection works.
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes

`KExactProbe`, `KUrlProbe`.

## Symptom

```
CratonVM: openConnection = java.net.JarURLConnection
HotSpot:  openConnection = sun.net.www.protocol.jar.JarURLConnection
```

`new URL("jar:...").openConnection().getClass()` yields the **abstract base**
`java.net.JarURLConnection` under CratonVM, but the **concrete** stream handler
`sun.net.www.protocol.jar.JarURLConnection` under HotSpot. `java.net.JarURLConnection`
is `abstract`, so CratonVM is reporting an abstract type as the runtime class of a
live object — internally it must be a synthetic concrete subclass whose
`getClass()` is mis-reported (or it is an instance of a CratonVM-internal class
mapped onto the abstract name).

## Root cause (hypothesis)

CratonVM's `jar:` protocol handler returns a connection object whose runtime
class identity is the abstract `java.net.JarURLConnection` rather than a
`sun.net.www.protocol.jar.JarURLConnection` (or a faithfully-named subclass).
Either the synthetic class is registered under the abstract name, or `getClass()`
resolves to the declared field type.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" KUrlProbe
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" KUrlProbe
```

## Impact

Code that `instanceof`-checks or reflects on `sun.net.www.protocol.jar.JarURLConnection`
(some resource-scanning libraries do) takes the wrong branch. Functionally the
connection may still read entries, but the type identity is wrong. Scoped to the
jar-URL connection factory.
