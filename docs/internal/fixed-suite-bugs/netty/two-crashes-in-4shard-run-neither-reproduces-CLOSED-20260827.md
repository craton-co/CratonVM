# Two CRASH classifications from the 2026-08-26 4-shard ZGC run — CLOSED

**Status: CLOSED 2026-08-27.** Neither row is a CratonVM defect, and the
`CRASH` classification itself is not reproducible for either class. The three
items the open page listed as "not yet done" are all answered below, on the
Windows host, against a binary built from
`perf/netty-funnel-and-crash-residuals-20260827`.

Superseded page: `known-issues/netty/two-crashes-in-4shard-run-neither-reproduces-as-a-crash-20260827.md`.

## What was observed originally

The 2026-08-26 complete-suite run (4 parallel ZGC shards, `dev` HEAD
`3c09f9d93`, host at 0 GB free RAM) recorded two `CRASH` rows — `process-died
rc=1`, zero bytes on stdout and stderr, no `@@RESULT`, no partial progress, no
VM boot banner:

```
io.netty.handler.NativeImageHandlerMetadataTest           CRASH  process-died rc=1 timeout=180s
io.netty.handler.codec.compression.BrotliIntegrationTest  CRASH  process-died rc=1 timeout=180s
```

The harness classifies `CRASH` by the ABSENCE of an `@@RESULT` line, so a
process the OS killed before it printed anything lands here, and so does a
process that never started. Producing literally nothing — not even the boot-time
WARN lines every other run on this branch emits — is the shape of an external
kill, not of a VM defect.

## The three open items, answered

### 1. `NativeImageHandlerMetadataTest` — is the CRASH reproducible at all?

**No. Three isolated runs, three clean `@@RESULT` lines.**

```
cratonvm --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.NativeImageHandlerMetadataTest

run 1  found=1 started=1 ok=0 failed=1 aborted=0 skipped=0 ms=47162
run 2  found=1 started=1 ok=0 failed=1 aborted=0 skipped=0 ms=40947
run 3  found=1 started=1 ok=0 failed=1 aborted=0 skipped=0 ms=41482
```

`failed=1` is the already-documented **non-CratonVM bug**: the test computes a
resource path from Maven `groupId`/`artifactId` metadata this non-Maven harness
never populates, so the path is the literal `null/null` **on HotSpot too**
(`not-cratonvm-bugs-consolidated.md`, and `nativeimagehandlermetadatatest-
harness-module-scope-FIXED-20260819.md` for the module-scope half). The JUnit
console launcher's `rc=1` for "tests failed" is not a crash signal; the harness
only reads it as one when the `@@RESULT` line is also missing, which is what
happened once and has not happened since.

The runs above were taken on a deliberately BUSY host (a release build and the
Brotli run below were both in flight), which is the condition the original
4-shard run had — and it still did not crash.

Two more data points, from an unrelated 89-class sweep run on two binaries the
same day (this branch and one built from unmodified `origin/dev`): the class
answered `found=1 ok=0 failed=1` on BOTH, as did all fifteen of its siblings
across the other packages. Five clean `@@RESULT` lines for this class, zero
crashes, on two different builds.

### 2. `BrotliIntegrationTest` — 100 % of one core, or a stall?

**100 % of one core.** Sampled against the process's own CPU accounting while
it was in the window the open page called a hang:

```
pid=14196  cpu_before=12.84 s  cpu_after=32.63 s  over 20 s wall
        => 0.99 cores busy
```

That is the one measurement the closed Linux investigation called decisive
(`netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md`: "100% of one
core, R state"), and it answers the open page's hypothesis (b): this is **not**
a stall in Brotli4j's native library load, on Windows any more than on Linux.

That also answers item 3 for this class: **the `CRASH` classification is not
reproducible for `BrotliIntegrationTest` either.** Three runs, three
`@@RESULT` lines.

### 3. Why the rerun saw no test-start output at all

Because the stock `CratonRunner` prints nothing until the class ends. That is
the whole of it, and the fixture already carries the instrument that separates
the two readings — `ProgressRunner`, which prints `@@START`/`@@FINISH` per
test, and which the closed Linux page's own "how to re-check this class"
section says to use. The 2026-08-27 rerun used `CratonRunner`, so "no test-start
line at all before the timeout" was a property of the runner, not of the run.

With `ProgressRunner`, the class produces exactly the shape the Linux page
recorded — and then goes one step further than that page could:

```
@@START  testLargeRandom      t=...166839
@@FINISH testLargeRandom      SUCCESSFUL
... eight more ...
@@FINISH testCompressible     SUCCESSFUL   t=...170400      <- 10 of 11 in 3.6 s
@@START  testHugeDecompress   t=...170419
@@FINISH testHugeDecompress   FAILED       t=...576986      <- 406 s later
@@RESULT ... found=11 ok=10 failed=1 ms=410783
```

**It finishes.** The Linux investigation could only say `testHugeDecompress`
"never finishes" inside the wall it had; given 20 minutes it completes in 406 s
and then FAILS — on the fixture's own
`-Djunit.jupiter.execution.timeout.default=120s`, which in JUnit 5's default
`SAME_THREAD` timeout mode does not interrupt the test but fails it afterwards
for having exceeded the bound. So the failure is the fixture's timeout, and the
406 s underneath it is the per-byte `ByteBuf.writeByte` wall the closed page
priced at 2 500–2 800 ns/byte over 268 435 456 bytes.

HotSpot 25 on this same host, same classpath, for scale:
`found=11 started=11 ok=11 failed=0 ms=11197`.

Three CratonVM runs, three clean `@@RESULT` lines, no crash in any of them:

```
ProgressRunner  found=11 ok=10 failed=1 ms=410783   (host busy: a build and another suite in flight)
CratonRunner    found=11 ok=10 failed=1 ms=241181
CratonRunner    found=11 ok=10 failed=1 ms=234087
```

The 406 s -> 234 s spread is host load, not variance in the class: the first
was taken while a release build and a second suite were running. Every run
reaches `@@RESULT`, which is the thing the `CRASH` classification asserts did
not happen.

## Verdict

| row | what it is |
|---|---|
| `NativeImageHandlerMetadataTest` | a known non-CratonVM bug that FAILS cleanly, three times out of three. The `CRASH` classification is not reproducible. |
| `BrotliIntegrationTest` | the known, already-owned per-byte `ByteBuf` throughput wall. Not a hang, not a native library load, and not a new stall — 10 of 11 tests pass in 3.6 s and the eleventh is CPU-bound at 0.99 cores for 406 s. |

Neither is a crash and neither is new. The `CRASH` rows themselves are best
explained by what the run's own conditions say: 4 concurrent ZGC shards, a host
at 0 GB free RAM, 76 classes into a 94-minute batch, and a process that
produced zero bytes on either stream — the signature of an external kill, which
the harness cannot distinguish from a VM crash because its only discriminator
is the absence of `@@RESULT`.

## Residual

None for these two classes. The throughput wall behind `testHugeDecompress`
is not this page's and was never netty-specific — it belongs to the ByteBuf
per-call throughput workstream (`performance/netty-per-call-throughput-20260813.md`),
and `netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md` is where its
measurements live.

One harness observation is worth carrying, because it cost this investigation
two reruns: **a `CRASH` row with zero bytes of output should be re-run with
`ProgressRunner`, not `CratonRunner`.** The stock runner prints nothing until
the class ends, so "no output" from it is not evidence about where the process
got to — and both of this page's classes were misread that way once.

## Repro

```bash
cd apps/netty-suite-runner
# per-test progress; the stock runner cannot tell you WHICH test it died in
cratonvm --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  ProgressRunner io.netty.handler.codec.compression.BrotliIntegrationTest
# expect: 10 FINISH ... SUCCESSFUL by ~4 s, then START testHugeDecompress
```
