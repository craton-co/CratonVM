# Reactor Netty HTTP client: outbound request bytes get corrupted, inserting garbage between the method token and the request line, causing Tomcat to reject with 400

**Status: OPEN — found 2026-08-05**

## Symptom

`ReactorClientHttpRequestFactoryBuilderTests` — 4/33 methods fail, all
against a local embedded Tomcat, all producing a 400 from the *server*
instead of the expected status (HotSpot: 33/33 pass,
`hotspot-baseline-latest.tsv`):

- `redirectDontFollow(String="POST")`: expected 302 FOUND, got 400 BAD_REQUEST
- `redirectDefault(String="GET")` and `redirectDefault(String="PATCH")`: expected 200 OK, got 400 BAD_REQUEST
- `connectWithSslBundle(String="POST")`: body assertion fails because the
  response body is Tomcat's own 400 error page, not the echoed request

The last one shows the smoking gun — Tomcat's error page embeds the exact
malformed request line it received:

```
Message: Invalid character found in method name [POST0xe1&#47;0x00HTTP&#47;1.10x0d0x0aaccept-encoding: ].
HTTP method names must be tokens
	at org.apache.coyote.http11.Http11InputBuffer.parseRequestLine(Http11InputBuffer.java:391)
```

Decoded, Tomcat received (as raw bytes on the wire) something like:
`POST` + `0xe1` + `0x00` + `HTTP/1.1` + `0x0d 0x0a` + `accept-encoding: ...`
— i.e. the ` /path ` portion of the request line (and the `\r\n` that
should separate it from `HTTP/1.1`) has been replaced/overwritten with two
garbage bytes (`0xe1 0x00`), while everything after (`HTTP/1.1\r\naccept-encoding: `)
is intact. This is raw corruption of the outbound buffer, not a logic
error in what Reactor Netty intended to send — the method token and the
headers are both correct, only the middle of the request line is
scrambled. The other 3 failures (redirect status mismatches, no visible
body) are consistent with the same corruption hitting different byte
offsets of their request lines and being rejected/mishandled before Tomcat
could even produce a matching error body every time.

A `Selector.select() returned prematurely 512 times in a row; rebuilding
Selector` warning from Netty's NIO layer appears in the same log around the
same time — Netty's own defensive workaround for a spinning selector, not
itself the bug, but consistent with something unusual happening in
CratonVM's underlying NIO/socket layer during this run.

## Root cause

Not yet pinned — **needs further investigation**. The corruption pattern
(two garbage bytes precisely where the path + line terminator should be,
with the exact bytes on either side preserved) points at a buffer
management bug in whatever backs Reactor Netty's outbound
`ByteBuf`/direct-buffer writes under CratonVM (native `DirectByteBuffer`
handling, `Unsafe` memory ops, or a `sun.nio.ch` socket-write native), not
in Reactor Netty's own Java-level request-line assembly (which is identical
bytecode to what passes on HotSpot). Candidates to check:

```
grep -rn "DirectByteBuffer\|native_write\|socket_write\|sun_nio_ch" native-builtins/src/*.rs
```

A useful next repro step: a minimal Reactor Netty client POST against a
plain (non-SSL) local server with a fixed short body, run in a loop, to see
whether the corruption is timing/load-dependent (consistent with a buffer
reused/overwritten before flush completes) or deterministic for a given
path length.

## Affected classes
- `spring-boot-http-client` — `org.springframework.boot.http.client.ReactorClientHttpRequestFactoryBuilderTests` (4/33 failed: `redirectDontFollow`, `redirectDefault` x2, `connectWithSslBundle`)

## See also
`jdk-httpclient-sslbundle-tls-handshake-eintr-20260805.md` — the JDK
`HttpClient` variant of the *same test method* (`connectWithSslBundle`)
fails too, with a different symptom (an EINTR during the TLS handshake
read rather than corrupted plaintext bytes). Different client stack,
different immediate symptom, but worth keeping in mind as a possible
shared lower-level cause (both exercise CratonVM's socket/TLS I/O under
the same embedded-Tomcat SSL bundle scenario).
