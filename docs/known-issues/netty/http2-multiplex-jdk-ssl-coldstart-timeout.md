# Netty `Http2MultiplexTransportTest.testFireChannelReadAfterHandshakeSuccess_JDK()` 5 s `@Timeout`

Measured 2026-11-16 on Linux (`origin/dev` `a09d2d7e8f`, release `--real-jdk`). Split from the retired `netty-4.2-speed-bound-residuals` tracker after 10 of 11 test classes passed and `EmptyHttpHeadersInitializationTest` was confirmed as a non-bug (JLS 12.4.2 two-class initialization cycle).

## Symptom

- `io.netty.handler.codec.http2.Http2MultiplexTransportTest`: 10 of 11 started tests pass in `~1.6 s` total (`found=13 started=11 ok=10 failed=1 aborted=0 skipped=2 ms=6634` under `-XX:+UseG1GC`).
- `testFireChannelReadAfterHandshakeSuccess_JDK()` fails `@Timeout(value = 5000L, unit = MILLISECONDS)` while waiting on `ServerBootstrap.bind(...).sync()` (`AbstractBootstrap$PendingRegistrationPromise` incomplete at 5000 ms).
- Under default GC (ZGC), the class intermittently hit a pre-existing `SIGSEGV` in `ZgcRealHeap::alloc_raw_quiet` (`DATA WRITE of a NOT-PRESENT page`), whereas `-XX:+UseG1GC` completed without crashing.

## Measurements & Cause

`testFireChannelReadAfterHandshakeSuccess_JDK()` runs cold-start `CertificateBuilder().buildSelfSigned()`, `SslContextBuilder.forServer(...).sslProvider(SslProvider.JDK)...build()`, `ServerBootstrap.bind(...).sync()`, client `SslContextBuilder`, and TLS/HTTP2 handshake inside a single 5000 ms `@Timeout`.

Phase probes (`Http2Probe`, `SslBuildBreakdown`, `NettyBuildOnly`) vs HotSpot 25:
- `Http2Probe` (default `--real-jdk`): `group=672.0 ms`, `cert=585.5 ms`, `serverSsl=3546.0 ms`, `bind=255.6 ms`, `clientSsl=9.6 ms`, `total=5068.7 ms` (HotSpot: `1688.0 ms`, `serverSsl=928.7 ms`).
- `SslBuildBreakdown` (after pre-warming `SSLContext`, `KeyStore`, `KeyManagerFactory`, and `SSLEngine`): `nettyBuild` (`SslContextBuilder.forServer(...).build()`) still took `3500.7 ms` vs HotSpot `499.5 ms`.
- `NettyBuildOnly`: `iter=0 build=4065.3 ms`, `iter=1 build=81.2 ms`, `iter=2 build=81.3 ms`.
- `-XX:CompileThreshold` sweep on `Http2Probe`: `50` -> `5723.0 ms`, `500` (default) -> `5068.7 ms`, `1000` -> `5173.5 ms`, `2000` -> `4897.2 ms`.
- `perf` profile (`perf_http2_zgc.data`): `83.42%` of total CPU is in `cratonvm` (flatly spread across the Rust interpreter, classloader, JIT bridge, and background compiler, with the hottest single symbol `execute_frame_from_index_loop` at `2.47%–3.02%`) and `4.67%` in `[JIT]`.

Closing the `~3.5–4.1 s` vs `499.5 ms` cold-start gap requires broader cold-start interpreter/Tier-1 JIT or pre-compiled JDK classloading improvements rather than a localized fix.
