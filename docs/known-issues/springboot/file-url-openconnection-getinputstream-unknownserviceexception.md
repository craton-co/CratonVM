# `file:` URL's `openConnection().getInputStream()` throws `UnknownServiceException`

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Failing test |
|---|---|
| `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests` | `getLastModifiedHeaderReturnsFileModifiedTime` |
| `org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests` | `getLastModifiedHeaderReturnsFileModifiedTime` |

Identically-named test method, identical failure, in two unrelated classes:

```
=> java.net.UnknownServiceException: protocol doesn't support input
   java.net.UnknownServiceException.<init>(UnknownServiceException.java:56)
   java.net.URLConnection.getInputStream(URLConnection.java:857)
   org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests.getLastModifiedHeaderReturnsFileModifiedTime(JarUrlConnectionTests.java:511)
```
```
=> java.net.UnknownServiceException: protocol doesn't support input
   java.net.UnknownServiceException.<init>(UnknownServiceException.java:56)
   java.net.URLConnection.getInputStream(URLConnection.java:857)
   org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests.getLastModifiedHeaderReturnsFileModifiedTime(NestedUrlConnectionTests.java:156)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests.out.log`
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests.out.log`

## Root cause

Both test bodies are otherwise unrelated (`JarUrlConnection`/`NestedUrlConnection`
against `jar:`/`nested:` URLs), but they share one identical `finally` block —
comparing the tested connection's `last-modified` header against a **plain
`file:` URL's** connection, opened directly:

```java
URLConnection fileConnection = this.file.toURI().toURL().openConnection();
try {
    assertThat(connection.getHeaderFieldDate("last-modified", 0))
        .isEqualTo(withoutNanos(this.file.lastModified()))
        .isEqualTo(fileConnection.getHeaderFieldDate("last-modified", 0));
}
finally {
    fileConnection.getInputStream().close();
}
```

The trap fires on the `finally` block's `fileConnection.getInputStream()` —
not on either loader-specific `jar:`/`nested:` connection under test. The
thrown exception is literally the **abstract base class's** default body
(`java.net.URLConnection.getInputStream`, `URLConnection.java:857`):

```java
public InputStream getInputStream() throws IOException {
    throw new UnknownServiceException("protocol doesn't support input");
}
```

On a real JDK, `new File(...).toURI().toURL().openConnection()` returns a
`sun.net.www.protocol.file.FileURLConnection`, which *overrides*
`getInputStream()` to return a working `FileInputStream`. The fact that the
plain base-class method fires here means CratonVM's `file:` protocol handler
does not produce (or does not correctly dispatch to) an equivalent override
for the `URLConnection` object returned by `openConnection()` on a `file:`
URL. `native-builtins/src/net_phase_e.rs` (~line 5397 onward) does have
substantial `file:` handling, but it is wired for the `URL.openStream()`
convenience path (reading bytes directly and handing back an
already-materialized stream) — not for a caller that first gets a `URLConnection`
object via `openConnection()` and then calls `getInputStream()` on it
separately, which is the exact shape both failing tests use. This was not
traced further to the specific dispatch site (e.g. whether `openConnection()`
returns a bare `java.net.URLConnection`-typed object, or a subclass that
never gets its `getInputStream()` native/override registered) — flagging as
the strongest evidence-backed hypothesis, not a confirmed file:line fix
target.

## Affected classes

| module | class |
|---|---|
| loader/spring-boot-loader | org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests |
| loader/spring-boot-loader | org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests |
