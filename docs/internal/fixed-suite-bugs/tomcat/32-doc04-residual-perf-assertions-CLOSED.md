# 32 — the four per-test performance residuals from known-issue 04

**Status:** ✅ **CLOSED 2026-07-31.** Retired from
`docs/known-issues/tomcat/32-doc04-residual-perf-assertions.md`.

**Read this first: "closed" here does not mean "all four tests pass."** Two
still fail. It means no residual is owned by *this* document any more — one item
was a real defect and is fixed, one is not a defect, and two are consumers of
VM-wide problems that now sit in the documents that own those problems, with
today's numbers attached. Carrying them here as well was duplicating an
open item in two places, which is how 32.1 came to be tracked against a
prediction nobody re-tested.

| item | disposition |
|---|---|
| 32.1 | Moved to [jit-raw-jit-to-jit-shadow-stack-overflow](../../../known-issues/jit-raw-jit-to-jit-shadow-stack-overflow-20260731.md) (and its context in [the retired moving-young gate doc](../../jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md)). Cause 1 fixed 07-30; the entire residual is the raw JIT-to-JIT direct-call edge, whose gate stays closed. No Tomcat work. |
| 32.2 | ✅ **Not a defect, and now confirmed on a LOADED host** — the harder condition, not the easier one. |
| 32.3 | ✅ **Real defect found and FIXED** (`9f7095ed9`): the bulk `ByteBuffer` natives copied one byte per accessor call. SEQ0 and SEQ1 close. The SEQ2 residual is general Java throughput, measured, and moves to [30](30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md). |
| 32.4 | Moved to [30](30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md), which the 07-30 revision already argued for and then did not act on. |

HotSpot reference for all four, same host, same fixture: all PASS.

## Every number in the 2026-07-30 revision is superseded

Not because the host changed — because **the lever that revision measured with
no longer runs.** `CRATONVM_NO_MOVING_YOUNG=1`, which produced its entire
cause-2 analysis, now crashes with an access violation 3 runs out of 3 on a
ten-second probe (`DateSymbolsProbe`), with `CRATONVM_JIT_DIRECT_CALLEE_CALLS`
on *or* off; the default lane is clean 3/3 on the same binary. That is a
separate defect and is filed separately. The consequence here is that none of
the 07-30 cause-2 table can be reproduced, so it is recorded as history rather
than as evidence.

Re-measured 2026-07-31 on dev `33281d948` plus this branch:

| item | test | 07-30 doc | 07-31 measured |
|---|---|---|---|
| 32.1 | `TestMapperPerformance` | easiest host 5 724 ms | easiest host **6 057 ms**, rerun 9 222 ms |
| 32.2 | `TestELParserPerformance` | PASS quiet, FAIL loaded | **PASS while LOADED**, 5.8 % margin |
| 32.3 | `TestAsyncMessagesPerformance` | SEQ1 494, SEQ2 162–248 | SEQ1 **86–143**, SEQ2 495–500 |
| 32.4 | `TestOneLineFormatterPerformance` | 44× short | **128× short** (loaded host) |

---

## The correction that matters most: this test has NINE hostnames

`TestMapperPerformance.testPerformance` iterates

```java
String[] requestedHostNames = new String[] { "xxxxxxxxxxx", "iowejoiejfoiew",
    "iowejoiejfoiex", "owefojiwefoi", "owefojiwefoix", "qwerty.net",
    "foo.net", "zzz.com", "abc.com" };
```

and asserts **each one** completes 10⁶ `recycle() + map()` calls in under
5 000 ms, with one automatic rerun to absorb a GC blip.

Every revision of this document measured **two** of those nine, and the 07-30
revision concluded "with both causes removed the test PASSES on both
hostnames." That sentence was never a claim about the test passing — seven
hostnames were never run. Today the class fails on the **first** hostname
(`xxxxxxxxxxx`, the easiest) at 6 057 ms, reruns at 9 222 ms, and never reaches
the other eight. Anyone re-testing this must run the class, not the probe.

---

## 32.1 — cause 1 is fixed; the residual is one re-gated JIT edge

`MapperPerfProbe`, hostname `xxxxxxxxxxx`, 300k iterations:

| loop body | dev `b695d468f` (pre-regression, PASSED) | 07-30 after cause-1 fix | **07-31 today** | 07-30 with both causes removed |
|---|---|---|---|---|
| `MappingData.recycle()` | 0.73 µs | 2.48 µs | **3.46 µs** | 0.80 µs |
| `recycle()` + `map()` | 2.03 µs | — | **6.2–8.5 µs** | — |

Cause 1 (`jit_virtual_tierup` flipped default-OFF by `c28bdd687`) is fixed on
dev and stays fixed — we are at 3.46 µs, not the 24–30 µs that flip produced.

The remaining 4.3× against the pre-regression baseline is cause 2, and cause 2
turned out to be **two** gates, not one. The owning document scoped the
optimizing-tier gate open on 07-31 — that half landed. It then **re-gated**
`direct_jit_callee_calls_enabled` back to the bare `moving_young_enabled()`
flag the same day, after `BasicErrorControllerIntegrationTests` SIGSEGV'd 14/14
with it open. So the second penalty is still fully present, which is directly
visible in the compile trace:

```
[cratonvm-jitc] ir-direct-call MISSED java/util/Calendar.setTime(...)V
                @pc=5 ir_direct=false static=false special=false
```

`MappingData.recycle` → 4× `MessageBytes.recycle` → 2× `AbstractChunk.recycle`
per iteration is exactly the call-chain shape that gate penalises.

**This is why the 07-30 prediction — "32.1 should close when that branch
merges" — did not come true.** The branch merged; half of what it did was
reverted for a soundness reason four days later, and nothing re-tested the
prediction.

Reopening that edge is not a Tomcat decision, and as of dev `cac4cbac0` it has
its own document:
[jit-raw-jit-to-jit-shadow-stack-overflow-20260731](../../../known-issues/jit-raw-jit-to-jit-shadow-stack-overflow-20260731.md).
Note that document **supersedes the reclaimed-root hypothesis** this item was
originally reasoned about — measured, the root scan behaves correctly and
diverts to the non-moving sweep; what actually fails is a shadow-stack push
running off the end of the thread's 2 MiB buffer. Three defects found on the
way are fixed and shipped; the gate itself stays closed. 32.1 closes when it
reopens, and is tracked there.

---

## 32.2 — not a defect, confirmed under the harder condition

`el.parser.TestELParserPerformance.testParserInstanceReuse` asserts `ReInit` is
faster than `new ELParser()`.

| host state | result | `ReInit` | `new ELParser()` | margin |
|---|---|---|---|---|
| 07-30, quiet | PASS, 320.9 s | 3.87 s | 3.99 s | 3.0 % |
| 07-30, loaded | FAIL, 1022.9 s | 12.72 s | 12.83 s | 0.9 % |
| **07-31, loaded** | **PASS, 574.9 s** | **6.96 s** | **7.39 s** | **5.8 %** |

The 07-30 revision left this "expect intermittency on a busy host". It now
passes *while busy*, with a margin nearly double what the quiet run recorded,
across 40 measured rounds. That is a strictly stronger result than the one the
item was closed on, so the disposition stands with better evidence: a
warm-up-curve property of the test's shape, not a CratonVM defect. A lone
failure on a loaded host is still not evidence of a regression.

---

## 32.3 — the recorded root cause was wrong; the real one is fixed

The 07-30 revision concluded SEQ1's cost "is not the two thread handoffs and
cannot be recovered by more I/O work — it is per-completion processing (the
Rust→Java upcall plus WebSocket frame decode)". The first half is right and the
second half is wrong. It was neither the upcall nor frame decode. It was
`ByteBuffer`.

### What it actually was

The five bulk `java/nio/ByteBuffer` natives — `get([B)`, `get([BII)`,
`put([B)`, `put([BII)` and `put(Ljava/nio/ByteBuffer;)` — moved their heap↔heap
payload **one element at a time** through `get_array_element` /
`set_array_element`, two dynamic accessor calls per byte. The WebSocket client
pushes ~32 KiB through those five methods for every 8 KiB chunk it delivers:
socket → `response` → `inputBuffer` → `messageBufferBinary` → the defensive
`copy` handed to `onMessage`.

`probes/WsChunkProbe`, 8 KiB per op, interleaved A/B, two reps:

| 8 KiB op | before | after | HotSpot |
|---|---|---|---|
| `put(ByteBuffer)` heap→heap | 293–295 µs | **3.8–4.6 µs** | 0.31 µs |
| `get(byte[8192])` | 204–242 µs | **2.8–2.9 µs** | 0.26 µs |
| full per-chunk sequence | 441–482 µs | **20–27 µs** | 1.19 µs |
| `System.arraycopy` (control) | 2.7 µs | 1.5–2.1 µs | 0.39 µs |

The control is flat. Note the VM already reached memcpy speed for
`System.arraycopy` on the identical 8 KiB — `write_byte_array_from` /
`read_byte_array_into` existed, with a doc comment naming these exact call
sites as the migration they were written for. The machinery was there and
unused. Fixed in `9f7095ed9`.

**Why the previous revision missed it:** it A/B'd the *AIO* fast path, which is
real and did help, and then attributed the entire remainder to "upcall and
frame decode" without measuring either. The buffer copies were never on the
list of candidates.

### Effect on the test, interleaved base vs fix, two reps

| lane | SEQ0 (tol 1) | SEQ1 (tol 10) | SEQ1 med gap | SEQ2 (tol 100) | SEQ2 med gap |
|---|---|---|---|---|---|
| base rep1 | 4 | 500 | 1.289 ms | 500 | 1.477 ms |
| fix rep1 | 1 | **143** | **0.599 ms** | 500 | 1.415 ms |
| base rep2 | 1 | 500 | 0.970 ms | 500 | 1.241 ms |
| fix rep2 | 1 | **86** | **0.578 ms** | 495 | 1.333 ms |

SEQ0 closes. SEQ1 goes from saturated to 86–143 and its gap halves. SEQ2 does
not move, and is now the binding constraint.

### SEQ2, measured rather than guessed

`CRATONVM_DBG_AIO_INLINE` now reports the split (`9bef50216`). n=1000
not-ready reads:

```
wait buckets: <1ms=493  1-10ms=6  >10ms=501  (sub-10ms mean=444us)
queue_mean=35us   deliver_mean=46us
```

The 501 long waits are the reads sitting out the server's deliberate 50 ms
pause — SEQ0's reads, and SEQ0 passes. For SEQ2's reads, of the ~1.3 ms gap:

* **35 µs** queueing the job until a worker picks it up — ours,
* **46 µs** worker → dispatcher → Java handler — ours,
* **444 µs** the worker genuinely blocked waiting for the peer to send,
* the remaining **~775 µs** is client-side Java between the two `onMessage`
  callbacks.

**Our I/O plumbing is 81 µs, about 6 % of the gap.** Two other candidates are
ruled out by measurement, not argument: thread wake-up is 20.5 µs for a
Semaphore round-trip against HotSpot's 10.5 µs, and `park`/`unpark` is *faster*
than HotSpot (8.1 vs 10.3 µs) — `probes/ParkPingPongProbe`. Both the 444 µs and
the 775 µs are Tomcat's own Java executing inside this VM (the test runs the
embedded server and the client in one process, so the server's send turnaround
is ours too).

So SEQ2 is general compiled-code throughput, and **no further AIO or buffer
work will close it.** It joins 30.

> **Trap worth keeping.** The first version of this diagnostic reported `wait`
> as a *mean* and got 25.8 ms — a value no read ever experienced, because the
> population is bimodal (a 50 ms pause and a 0.4 ms turnaround, roughly half
> each). It looked like "the peer is enormously slow" and would have sent the
> next round of work in the wrong direction. Bucket a latency before averaging
> it.

---

## 32.4 — moved to 30, which the previous revision already argued for

`juli.TestOneLineFormatterPerformance.testDateFormat` asserts `DateFormatCache`
beats `String.format`, 10⁶ iterations each. End to end on 07-31:

```
StringFormatImpl      4 730 855 700 ns
DateFormatCacheImpl 606 794 187 200 ns
```

**128× short.** The test feeds `System.nanoTime()` to a formatter whose cache is
keyed on `time / 1000`, so it misses on essentially every call and the miss path
— a bare `SimpleDateFormat.format(Date)` — is what is measured. Confirmed
against `DateFormatCache.getFormat` directly.

### The mechanism the previous revision misstated

32.4 was filed alongside doc 30's "hot methods never compile" story. For this
workload that is **not** what happens. `CRATONVM_DBG_JITC` shows both hot
methods compiling:

```
full-compile java/text/SimpleDateFormat.format(...)  entry=... len=1708
full-compile java/text/SimpleDateFormat.subFormat(...) entry=... len=64359
```

64 KB of code for `subFormat`. They compile; the compiled output is simply
~100× off HotSpot:

| operation | HotSpot | CratonVM |
|---|---|---|
| `SimpleDateFormat.format` (same Date) | 2.3–2.5 µs | 250–300 µs |
| `DateFormatSymbols.getInstance(Locale.US)` | 1.1–1.2 µs | 50–62 µs |
| `new DateFormatSymbols(Locale.US)` | 0.5–1.1 µs | 28–34 µs |

Passing needs `SimpleDateFormat.format` at ≲ 4.7 µs — where `String.format`
lands, because on CratonVM *that* side is a Rust intrinsic. So the assertion is
effectively "compiled Java must match a Rust intrinsic", and closing it is the
standing codegen-quality gap, not a Tomcat item. **Optimising `String.format`
makes this test harder**, which is worth knowing before anyone does.

One named defect sits here and is worth fixing on its own merits, but **not for
this test**: `DateFormatSymbols.getProviderInstance` fails codegen
(`compile-bail … backend_attempted=true`, `tier_fail_count=3`), which the VM
classifies as "not policy — these are bugs". It is reached ~2× per
`String.format` call, i.e. it slows the **fast** side of the assertion. Filed
separately.

---

## Reproduction

```
apps\tomcat-suite-runner\run-doc04-residuals.ps1 -Exe <exe> -Tag <tag> `
  -Only org.apache.catalina.mapper.TestMapperPerformance
apps\tomcat-suite-runner\run-doc04-residuals.ps1 -Vm hotspot -Tag hotspot

REM 32.3 — the buffer copies, and the AIO latency split
cratonvm.exe -Xmx2g -cp <probes-out> WsChunkProbe 2000
set CRATONVM_DBG_AIO_INLINE=1
REM 32.3 — the wake-up latency this ruled out
cratonvm.exe -Xmx2g -cp <probes-out> ParkPingPongProbe 2000
REM 32.1 — split by loop body (full | recycle | tochars)
run-one.ps1 -Main MapperPerfProbe -Args2 @('xxxxxxxxxxx','300000','2','recycle') ^
            -ExtraCp <probes-out> -Exe <exe>
REM 32.4
cratonvm.exe -Xmx2g -cp <probes-out> DateFmtProbe
cratonvm.exe -Xmx2g -cp <probes-out> DateSymbolsProbe 1000
```

Do **not** reach for `CRATONVM_NO_MOVING_YOUNG=1` — it crashes (see the top of
this document).
