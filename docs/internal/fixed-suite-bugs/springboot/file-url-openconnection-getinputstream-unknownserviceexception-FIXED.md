# FIXED — `file:` URL `openConnection().getInputStream()` and last-modified header

**Fixed:** 2026-07-17

## Symptom

Spring Boot's loader tests failed in their common cleanup path:

```text
java.net.UnknownServiceException: protocol doesn't support input
    java.net.URLConnection.getInputStream(URLConnection.java:857)
```

The affected tests were:

- `org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests`
- `org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests`

Both call `file.toURI().toURL().openConnection()`, then use
`getHeaderFieldDate("last-modified", 0)` and `getInputStream()`.

## Root cause and fix

`URL.openConnection()` handed non-HTTP/non-`jar:` URLs a synthetic object whose
runtime class was the abstract `java.net.URLConnection`. In real-JDK mode, its
real bytecode `getInputStream()` implementation took precedence over the base
native registration and threw `UnknownServiceException`.

The connection factory now uses the existing synthetic
`java.net.HttpURLConnection` carrier for every non-`jar:` scheme. Its
carrier-specific `getInputStream()` native dispatches non-HTTP URLs back to
`URL.openStream()`. The same carrier now exposes `file:` timestamps through
`getLastModified()` and `getHeaderFieldDate("last-modified", ...)`; the header
value is rounded down to whole seconds, matching `FileURLConnection`'s RFC-1123
date precision.

## Validation

Remote validation used `/data/wt-springboot-file-url-openconnection-20260717`,
Java 25, and the dedicated binary
`/data/cratonvm-springboot-fileurl-20260717`.

- A direct probe created a temporary `file:`, verified the bytes returned from
  `openConnection().getInputStream()`, and compared its last-modified header to
  the file timestamp. It passed with JIT enabled and with `--nojit`.
- `NestedUrlConnectionTests`: **11/11 passed** with JIT and `--nojit`.
- `JarUrlConnectionTests`: the documented failure is absent with JIT and
  `--nojit`; 44/47 tests pass. The three remaining failures are independent
  cached-jar stream-identity and `ZipFile$CleanableResource` cleanup defects,
  not `file:` URL connection behavior.
