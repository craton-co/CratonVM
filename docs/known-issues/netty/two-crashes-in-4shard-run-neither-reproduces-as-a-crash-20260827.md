# Two CRASH classifications from the 2026-08-26 4-shard ZGC run — neither reproduces as a crash in isolation

## Status
**Not reproducible as CRASH.** Both investigated 2026-08-27 by rerunning the
exact class standalone; neither crashed. One is the already-known
`NativeImageHandlerMetadataTest` non-bug (see
`not-cratonvm-bugs-consolidated.md`), now confirmed to also just plain FAIL when
run alone rather than crash. The other, `BrotliIntegrationTest`, hung instead of
crashing on rerun — and the hang shape doesn't match this class's existing
closed investigation, so it's flagged separately below as possibly new.

## What was observed originally

The 2026-08-26 complete-suite run (4 parallel ZGC shards, `dev` HEAD
`3c09f9d93`) recorded two `CRASH` rows (`process-died rc=1`, zero bytes of
output on either stdout or stderr — no `@@RESULT`, no partial test progress, no
JIT/panic/allocation-failure log line, nothing):

```
io.netty.handler.NativeImageHandlerMetadataTest      CRASH  process-died rc=1 timeout=180s
io.netty.handler.codec.compression.BrotliIntegrationTest  CRASH  process-died rc=1 timeout=180s
```

Zero output on a CRASH row is itself unusual for this harness — every other
crash/hang signature seen this session (the KFusion OOM abort, timeout kills)
produces at least a startup banner and the VM's own boot-time WARN lines before
whatever failure follows. Producing literally nothing suggests the process was
killed or failed to even start correctly, which points more toward a transient
host/OS condition (both classes ran deep into a 76-class, 94-minute,
4-way-concurrent-shard batch) than a deterministic CratonVM defect.

## Rerun 1: `io.netty.handler.NativeImageHandlerMetadataTest` — FAILs cleanly, does not crash

```
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.NativeImageHandlerMetadataTest
=> @@RESULT ... found=1 started=1 ok=0 failed=1 aborted=0 skipped=0 ms=28439
=> System.exit(1) called, rc=1
```

This is the already-documented non-CratonVM-bug (`ChannelHandlerMetadataUtil
.generateMetadata` assertion failure comparing generated vs. checked-in
reflection metadata — see `not-cratonvm-bugs-consolidated.md`). `rc=1` here is
just the JUnit console launcher's normal exit code for "tests failed", not a
crash signal — the harness's own classifier only marks CRASH when it finds no
`@@RESULT` line in the captured output, which is what happened originally, but
evidently didn't happen this time.

## Rerun 2: `BrotliIntegrationTest` — hangs instead, and the hang doesn't match the known issue

```
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.codec.compression.BrotliIntegrationTest
=> timed out after 60s, zero output past class-init WARN lines (no JUnit test-start line at all)
```

This class already has a CLOSED investigation
(`docs/internal/fixed-suite-bugs/netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md`,
Azure Linux): 10 of 11 tests pass in 4.2s, only `testHugeDecompress` doesn't
finish, and the process is confirmed CPU-bound at 100% (a plain interpreter
throughput wall building a 256MB buffer one byte at a time — not a native
library load, not a real hang).

**This rerun's shape doesn't match that.** No test-start output was seen at
all before the timeout — not even the 10 quick passes the Linux investigation
found. Not yet determined whether this is: (a) the same known throughput wall,
just slower to reach the first test-start print on this Windows host/build; (b)
a genuinely different, earlier stall (e.g. in Brotli4j's native library load,
which the closed investigation explicitly ruled out on Linux but hasn't been
re-checked on Windows); or (c) another instance of the timing/contention noise
this run's other classes showed (see
`timing-margin-fails-under-4shard-zgc-self-contention-20260826.md`) — the
original run had 4 concurrent ZGC shards and a host at 0GB free RAM.

## Not yet done
- Did not let the isolated `BrotliIntegrationTest` rerun run past 60s to see if
  it eventually produces the same "10 pass, one slow" pattern the Linux doc
  found, just delayed.
- Did not check CPU usage during the isolated rerun's hang window (the closed
  Linux doc's strongest evidence was "100% of one core, R state" — that's the
  one measurement that would distinguish "still the known throughput wall" from
  "a new stall").
- Neither class was re-run multiple times to establish whether the CRASH
  classification itself is reproducible at all, or was a one-off host artifact
  from the original 4-shard run.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.codec.compression.BrotliIntegrationTest
```
