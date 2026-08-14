# ZGC compaction panics `compaction must not raise the cursor` on a netty TLS workload

**Status:** OPEN, bisected to the flag but not to the line (2026-08-14). Found on
Azure host 2 (Linux x86_64, JDK 25) at `origin/dev` @ `6134821f4`.

## Symptom

```
thread 'main-vm' panicked at gc/src/arena.rs:1884:9:
compaction must not raise the cursor: 15348137664 > 168782728
fatal runtime error: failed to initiate panic, error 5, aborting
#  SIGABRT at pc=…  jdk mode: real-jdk
```

Reproducible 3 of 3 runs. The panicking thread is `main-vm` or a netty
`multiThreadIoEventLoopGroup-*` worker — it is not thread-specific. The two
numbers differ by ~91×, so the cursor is not being nudged; it is landing
somewhere unrelated.

## Bisect

Using the kill switches `ad8ac62c6` ("flip parallel marking and compaction
default-ON for the gauntlet") shipped for exactly this purpose:

| run | result |
|---|---|
| `-XX:+UseZGC` (default) | **panic**, 3/3 |
| `-XX:+UseZGC CRATONVM_ZGC_RELOCATE=0` | 72/72 ok |
| `-XX:+UseZGC CRATONVM_ZGC_PARMARK=0` | **panic** |
| `-XX:+UseG1GC` | 72/72 ok |

So it is the **compaction** half of that flip, not parallel marking, and it is
ZGC-only. `CRATONVM_ZGC_RELOCATE=0` restores the non-moving sweep and the whole
class passes.

## Repro

```bash
cd apps/netty-suite-runner
echo io.netty.handler.ssl.SslErrorTest > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-zgc bash run-netty-suite.sh --list /tmp/one.txt --gc zgc --shards 1 --timeout 400 --out /tmp/repro
```

`common.args` must carry `netty-tcnative-boringssl-static-<ver>-<os>.jar`.
Without it `OpenSsl.isAvailable()` is false, `SslErrorTest.data()` (which is
OpenSSL-only) generates no parameters, the class reports `NOTESTS`, and nothing
allocates — which is why this crash could not be seen before 2026-08-13, not
because the GC changed under it. See the retired
`ssl-suite-test-discovery-undercounts` write-up.

`SslErrorTest` is 72 TLS handshakes over real sockets against a BoringSSL-backed
`SslContext`; that is the shape, and other netty classes with a similar profile
(`SniHandlerTest`, `ParameterizedSslHandlerTest`) run alongside it without
panicking, so a narrower reproducer is likely to exist. Two allocation-heavy but
non-TLS classes (`PooledByteBufAllocatorTest` 47 tests,
`UnpooledTest` 41) do **not** trip it, so raw allocation volume is not the
trigger on its own.

## Related

- `ad8ac62c6` — the flip, and the source of both kill switches.
- `docs/known-issues/netty/openssl-key-material-and-engine-residuals-20260813.md`
  — the other residuals the same newly-running OpenSSL half exposed.
