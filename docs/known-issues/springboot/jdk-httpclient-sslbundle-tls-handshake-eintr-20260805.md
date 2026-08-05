# `java.net.http.HttpClient` TLS handshake read fails with EINTR (os error 4) against an SSL-bundle-configured embedded Tomcat

**Status: OPEN — found 2026-08-05**

## Symptom

`JdkClientHttpRequestFactoryBuilderTests.connectWithSslBundle(String="POST")`
— 1/32 methods fail (HotSpot: 32/32 pass, `hotspot-baseline-latest.tsv`):

```
java.io.IOException: HttpClient request failed: TLS handshake read: Interrupted system call (os error 4)
	at org.springframework.http.client.JdkClientHttpRequest.executeInternal(JdkClientHttpRequest.java:118)
	at org.springframework.http.client.AbstractStreamingClientHttpRequest.executeInternal(AbstractStreamingClientHttpRequest.java:87)
	at org.springframework.boot.http.client.AbstractClientHttpRequestFactoryBuilderTests.connectWithSslBundle(AbstractClientHttpRequestFactoryBuilderTests.java:119)
```

`os error 4` is `EINTR` — a blocking read syscall was interrupted by a
signal and returned early instead of completing or being transparently
retried. This surfaces as a hard `IOException` from CratonVM's TLS
implementation instead of the read being retried, which is what any
correct blocking-I/O implementation must do for `EINTR` (POSIX read/recv is
never treated as a real error for this case; HotSpot's own socket/TLS code
retries internally and this test passes 32/32 there).

## Root cause

Not yet pinned — **needs further investigation**. This looks like a missing
EINTR-retry loop around a blocking read syscall somewhere in CratonVM's
native TLS/socket read path (used by the JDK `HttpClient`'s TLS engine),
surfacing only under this test's specific timing (an embedded Tomcat with
an `SslBundle`-configured connector, `-jit` build). Candidates:

```
grep -rn "Interrupted system call\|EINTR\|ErrorKind::Interrupted" native-builtins/src/*.rs vm/src --include=*.rs
```

Rust's own `std::io::Read`/`Write` impls already retry on
`io::ErrorKind::Interrupted` in most cases, so this is more likely a raw
`libc::read`/`recv` call in CratonVM's own native socket or TLS glue that
bypasses that retry (e.g. a direct FFI call not going through
`std::net`/`rustls`'s usual wrappers).

## Affected classes
- `spring-boot-http-client` — `org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests` (1/32 failed: `connectWithSslBundle`)

## See also
`reactor-netty-outbound-request-line-corruption-20260805.md` — the Reactor
Netty variant of the *same test method* (`connectWithSslBundle`) also
fails, with corrupted outbound request bytes rather than an EINTR. Worth
investigating together in case both trace back to the same underlying
socket/TLS I/O primitive under interrupt/contention.
