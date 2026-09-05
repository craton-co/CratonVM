# Two NullPointerExceptions in Spring Boot's own jar/nested URL-protocol handlers

## Status

**OPEN, weakly clustered.** Two classes in Spring Boot's own `loader` module
fail with an NPE on an internal field/resource that should have been
initialized. Plausibly related (same module, same shape: an internal
resource object is null where the code assumes it is set), but **not
confirmed as one root cause** — could equally be two independent gaps.

## The two failures

Full Spring Boot suite run, 2026-09-04/05 (`full_g1_mysession_20260904_041104`,
`full_zgc_mysession_20260904_041104` — both showed clean `FAIL`; Generational's
own run hit the now-fixed GC crash on both these classes instead, masking
whatever it would have shown):

```
org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests
  getContentTypeWhenNotKnownInStreamButKnownNameReturnsDeducedType()
  java.lang.NullPointerException: Cannot read field "zsrc" because "<local3>.res" is null
	at java.util.zip.ZipFile.getInputStream(ZipFile.java:327)

org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests
  getContentLengthWhenContentLengthMoreThanMaxIntReturnsMinusOne()
  java.lang.NullPointerException: Cannot invoke
    "org.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionResources.connect()"
    because "this.resources" is null
	at org.springframework.boot.loader.net.protocol.nested.NestedUrlConnection.connect(NestedUrlConnection.java:195)
```

Both are Spring Boot's own custom `URLStreamHandler`/`URLConnection`
implementations for its `jar:nested:...` executable-jar layout — code this
project authors and tests directly, not a third-party dependency.

## Why these two are grouped, and why only tentatively

Shared shape: in both cases, an internal object the connection expects to
already hold (a `ZipFile`'s `Source` handle in one, an owning
`NestedUrlConnectionResources` in the other) reads back `null` at the point
of use, despite the surrounding code's own contract assuming it was set
earlier (by `connect()`, by construction, or by a cached-connection reuse
path). That is consistent with a single underlying pattern — some
CratonVM-specific field-initialization or object-identity issue that drops a
reference during construction or across a connection-caching boundary — but:

- The two exceptions are in **unrelated classes** (`java.util.zip.ZipFile`,
  a JDK class, vs. Spring Boot's own `NestedUrlConnection`) with **unrelated
  fields** (`zsrc` vs. `resources`).
- Neither has been isolated to confirm it reproduces standalone, independent
  of suite-run ordering or shared state from an earlier class in the same
  shard.
- Not cross-checked against HotSpot on this harness.

## What is NOT claimed

- That these share a root cause — that is a hypothesis based on shape alone,
  not a traced mechanism.
- That either is deterministic — neither has been re-run in isolation yet.

## Repro

```bash
cd apps/spring-boot-suite-runner
CV_BIN=<binary> ./run-spring-boot-suite.sh -Category all \
  -ClassList <(printf 'org.springframework.boot.loader.net.protocol.jar.JarUrlConnectionTests\norg.springframework.boot.loader.net.protocol.nested.NestedUrlConnectionTests\n')
```

Next step before treating this as one defect: reproduce each in isolation
(single class, single shard, no other test in the same JVM) to rule out
suite-ordering/shared-state effects, and check both against HotSpot on the
same harness.
