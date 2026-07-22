# `NettyReactiveWebServerFactoryTests.whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed` — FIXED

**Status: FIXED 2026-07-20**, branch `fix/h2c-hpack-headerblock-20260720`. Was
`docs/known-issues/springboot/h2c-priorknowledge-hpack-headerblock-decode-failure.md`.

## Root cause

`native-builtins/src/unsafe_natives.rs::native_unsafe_get_byte_at_address` —
the backing native for `sun.misc.Unsafe.getByte(long)` / `jdk.internal.misc.
Unsafe.getByte(long)` (the single-`long`-address overload, as opposed to the
`(Object, long)` overload) — returned the raw arena/pointer byte as `v as i32`
(`u8 as i32`), which **zero-extends** (`0x83` → `131`) instead of
**sign-extending** to the signed Java `byte` contract (`0x83` → `-125`). The
sibling `(Object, long)` overload (`native_unsafe_get_byte_mb`) already did
this correctly (`b[0] as i8 as i32`, with a comment noting the sign-extension
requirement explicitly) — the single-address overload was simply never
brought in line.

Netty's direct-buffer byte reads (`PlatformDependent0.getByte(long)` →
`UNSAFE.getByte(address)`) go through exactly this path. `HpackDecoder.
decode()`'s very first branch is `if (b < 0) { /* indexed header field */ }`
(RFC 7541 §6.1: top bit set ⇒ indexed representation) — for any header byte
≥ `0x80`, this comparison silently evaluated as `false` on CratonVM instead
of `true`, routing the byte into the wrong branch of HPACK's representation
state machine. From there the decoder's internal `index`/`length` bookkeeping
desyncs completely, and — for the specific byte sequence this test's request
headers happen to produce — it manifests several bytes later as `HpackDecoder.
notEnoughDataException`: `"decode only works with an entire header block!"`.

This explains every observation in the original doc:
- **Reproduced identically under `--nojit`**: sign-extension happens in the
  interpreted native call itself, independent of JIT.
- **Reproduced identically across `pooled`/`adaptive`/`unpooled` Netty
  allocators**: all three eventually read a direct buffer's bytes via
  `Unsafe.getByte(long)`.
- **Wire bytes were correct** (per `CRATONVM_SOCKET_CAPTURE`): the bytes on
  the wire, and even in the target `ByteBuf`'s backing memory, were never
  corrupted — only their *sign* was misinterpreted the moment Java code
  compared a byte value against zero.

## Fix

`native-builtins/src/unsafe_natives.rs`, both return paths of
`native_unsafe_get_byte_at_address` (the arena-hit branch and the
real-pointer branch): route the raw `u8`/`byte` through `as i8 as i32`
instead of `as i32`, matching the already-correct sibling overload.

## Verification

1. **Isolated, no sockets/Netty/HTTP2 at all**: `SignExtendProbe.java` reads
   a `0x83` byte back from a `ByteBuffer.allocateDirect`-backed `ByteBuf`
   (direct AND sliced) and compares `< 0` — was `false` (wrong), now `true`
   (matches a byte read from a literal or a heap array, and matches real
   HotSpot).
2. **`HpackDecoder` alone** (`HpackIsolatedProbe.java`, package
   `io.netty.handler.codec.http2` for constructor access, zero sockets):
   feeding the real, unmodified `HpackDecoder.decode()` a hand-built 17-byte
   HPACK payload containing an indexed-header byte (`0x83`) — was
   `IllegalArgumentException: decode only works with an entire header
   block!`, now decodes successfully to the expected headers.
3. **Standalone repro** (`docs/known-issues/repros/reactor-netty-h2c-hpack/
   H2cRepro.java`, plain `reactor.netty.http.server.HttpServer` + Jetty
   `HTTP2Client`, no Spring Boot): was `FAILED: ...DecryptError`-style
   stream-cancel, now `status=200 body=Hello World` — reproduced identical
   pass across `pooled`/`unpooled`/default (adaptive) allocators and under
   `--nojit`.
4. **Real Spring Boot suite**: `module/spring-boot-reactor-netty.org.
   springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests`
   went from `FAIL` (this test method's `HpackDecoder`/`Http2Exception`
   trace) to that specific method passing; the class's only remaining
   failure is the unrelated, separately-tracked
   `sslWithPemCertificates` (see
   `docs/known-issues/springboot/pemcertificates-clientauth-rustls-decrypterror.md`).
5. Full `module/spring-boot-reactor-netty` (5 classes) and a 300-class cross-
   module regression slice: no regressions attributable to this fix (a
   handful of pre-existing Docker/DB-dependent HANG/FAIL/CRASH results,
   unrelated to `Unsafe.getByte`).

## Why this was hard to find

The corruption is not in byte *content* — every buffer-content/slice/
readerIndex mechanic that could plausibly explain "decode fails on
well-formed input" checks out correctly on CratonVM (verified via dedicated
probes for raw NIO reads, `ByteBuf.slice()`/`readSlice()`, and explicit
`readerIndex(int)` sets). The bug is a **sign**, not a **value** — every
probe that inspected byte VALUES via masking (`& 0xFF`) for display, as the
prior investigation session's hand-decode did, would see the exact right
byte and never notice the interpretation was wrong. It only surfaces once
Java code performs a signed comparison (`b < 0`) against a value sourced
from this one specific native path.
