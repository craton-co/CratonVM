# `NettyReactiveWebServerFactoryTests.whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed` — Netty HPACK decode fails on a well-formed HEADERS frame

**Status: OPEN — found 2026-07-20 (residual of the reactor-netty hang fix; see
[`reactor-netty-server-startup-hang-FIXED.md`](../../internal/springboot/reactor-netty-server-startup-hang-FIXED.md)).
Root cause not identified; wire-level evidence gathered and documented below.**

## Symptom

A Jetty `HTTP2Client`-backed `HttpClient` (`HttpClientTransportOverHTTP2`,
prior-knowledge h2c, no TLS) POSTs to an embedded `reactor.netty.http.server.
HttpServer` configured with `.protocol(HttpProtocol.HTTP11, HttpProtocol.H2C)`
(exactly what `NettyReactiveWebServerFactory.listProtocols()` produces when
HTTP/2 is enabled without SSL). The server-side Netty pipeline throws while
decoding the client's very first HEADERS frame:

```
io.netty.handler.codec.http2.Http2Exception: Error decoding headers: decode only works with an entire header block!
UnpooledSlicedByteBuf(ridx: 5, widx: 49, cap: 49/49, unwrapped: AdaptivePoolingAllocator$AdaptiveByteBuf(ridx: 58, widx: 78, cap: 2048))
Caused by: java.lang.IllegalArgumentException: decode only works with an entire header block!
	at io.netty.handler.codec.http2.HpackDecoder.notEnoughDataException(HpackDecoder.java:460)
	at io.netty.handler.codec.http2.HpackDecoder.decode(HpackDecoder.java:299)
```

The client then sees the connection reset mid-stream:
`java.io.IOException: cancel_stream_error/input_shutdown`
(`org.eclipse.jetty.http2.HTTP2Session.onShutdown`).

**Reproduces in complete isolation** — a ~40-line standalone Java program
([`repros/reactor-netty-h2c-hpack/H2cRepro.java`](../repros/reactor-netty-h2c-hpack/H2cRepro.java))
using plain `reactor.netty.http.server.HttpServer`/`org.eclipse.jetty.client.HttpClient`
with no Spring Boot involved reproduces the identical error, byte-for-byte
identical buffer state (`ridx: 5, widx: 49, cap: 49/49`), every run. Compile
and run against the module's own classpath, e.g.:
```
javac -cp <spring-boot-reactor-netty module test classpath> H2cRepro.java
cratonvm -cp .;<same classpath> H2cRepro
```

## What's been ruled out (verified this session)

- **Not a JIT bug.** Reproduces identically under `--nojit` (interpreter
  only) — same exact `ridx`/`widx`/`cap` values.
- **Not specific to Netty's `AdaptivePoolingAllocator`.** Reproduces
  identically with `-Dio.netty.allocator.type=pooled` (Netty falls back to
  `PooledUnsafeDirectByteBuf`) — same `ridx: 5, widx: 49, cap: 49/49`,
  just a different buffer class name in the message.
- **The bytes actually on the wire are well-formed HTTP/2 framing.**
  Captured the exact bytes both peers exchanged with the existing
  `CRATONVM_SOCKET_CAPTURE=<prefix>` debug facility
  (`native-io/src/net.rs`) and hand-decoded every frame:
  - Write 1 (58 bytes): connection preface (24B) + SETTINGS (9B hdr + 12B
    payload: `MAX_CONCURRENT_STREAMS=32`, `INITIAL_WINDOW_SIZE=8388608`) +
    WINDOW_UPDATE (9B hdr + 4B payload).
  - Write 2 (78 bytes, delivered to the server in **one single `read()`
    call** — confirmed via the same capture, so this is not a
    multi-read/fragmentation issue): HEADERS frame (9B header declaring
    `length=49, type=HEADERS, flags=END_HEADERS, stream=1`) + exactly 49
    bytes of HPACK payload, immediately followed by a DATA frame (9B
    header declaring `length=11, flags=END_STREAM` + `"Hello World"`,
    11 bytes). `9+49+9+11 = 78` — the frame boundaries in the raw
    bytes account for every byte with nothing left over and nothing
    missing.
  - Hand-decoded the 49-byte HPACK payload against RFC 7541: byte 0
    (`0x3f`) starts a Dynamic Table Size Update integer (continuation
    bytes `0xe1 0x1f` → value 4096, 3 bytes total); byte 3 (`0x83`) is an
    Indexed Header Field, static-table index 3 (`:method: POST` — correct
    for this request); byte 4 (`0x41`) starts a Literal Header Field with
    Incremental Indexing, static-table name index 1 (`:authority`); byte 5
    (`0x8b`) is that value's Huffman-flagged length prefix, declaring 11
    Huffman-coded octets, well within the 44 bytes still available in the
    49-byte payload at that point. Nothing in the first 5 decoded bytes
    (exactly where `HpackDecoder` reports `ridx: 5` at failure) looks
    malformed or under-length by this reading.

Given the wire bytes parse cleanly by hand up to (and past) the exact point
where Netty's own decoder gives up, the discrepancy is either in a detail of
Netty's `HpackDecoder`/`DefaultHttp2FrameReader` state machine not captured
by a byte-level RFC 7541 reading (plausible — HPACK decoder state is more
than "read the next field"), or in something CratonVM does that affects
Netty's *in-memory view* of an otherwise-correctly-received buffer (e.g. a
gap in how a pooled/direct `ByteBuffer`'s backing memory is populated by the
native socket-read path) without corrupting the bytes this investigation's
capture hook sees (which taps the raw `&[u8]` at the Rust I/O layer, before
any copy into Java-visible memory) — **not distinguished this session.**

## Next steps for whoever picks this up

1. Reviewed `native-io/src/socket_channel.rs`'s `buffer_access`/
   `buffer_write_bytes` (how bytes read off the OS socket land in a Java
   direct `ByteBuffer`'s backing memory via `Unsafe`-style `address +
   position` addressing) and found nothing obviously wrong — and this
   exact code path is shared by every other passing Netty-based test in
   this suite, so a generic bug there seems unlikely but was not
   conclusively ruled out for this specific case (pooled/adaptive-allocator
   buffer *slicing* specifically, as opposed to a plain top-level direct
   buffer, was not independently tested).
2. A live debugger/memory-inspection session (attach at the
   `HpackDecoder.decode()` call site, or add a temporary
   `eprintln!`/`System.err.println` dump of the slice's actual backing
   memory bytes at `ridx=5` to compare against this doc's hand-decoded
   expectation) would settle whether the in-memory bytes at that point
   genuinely differ from what was captured on the wire.
3. Try the same repro against Netty's `UnpooledByteBufAllocator` (not yet
   tested; `pooled` and `adaptive` both reproduce identically, which argues
   against an allocator-specific bug, but `unpooled` — no arena/slicing at
   all, plain JVM-heap-adjacent direct buffers — would be a useful third
   data point).
4. Try forcing HTTP/1.1-only (`.protocol(HttpProtocol.HTTP11)`) with the
   *same* Jetty client sending equivalent headers over h1.1, to check
   whether the underlying byte-delivery mechanism is fine outside the
   HTTP/2 codec specifically (h1.1 traffic passes extensively elsewhere in
   this suite, so this is expected to work, but confirming with the exact
   same header content would narrow whether this is HTTP/2-frame-specific
   or a general large-write artifact).

## Affected classes

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-reactor-netty` | `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` | `whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed` |
