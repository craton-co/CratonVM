# `AdaptiveByteBufAllocator*` and `SearchProcessorTest` — confirmed genuine (non-contention) throughput wall, not deadlocks

## Status
Restored here from the internal tree (stripped from public git history) on
2026-08-29 because these four classes keep reappearing as non-passing in
every full netty suite run and had no public-tree home. Original text
(2026-08-16/17) preserved below unedited; see "Reconfirmed 2026-08-29" at the
end for today's data point.

**Original status: RETIRED 2026-08-17 — the "genuine VM-wide throughput
ceiling" this page reconfirmed was, for the larger part, one cacheable call
shape, and that is now FIXED.**

This page's central claim was that the three `AdaptiveByteBufAllocator*`
classes cost what they cost because of "per-call/JIT-entry dispatch cost ×
hundreds of millions of calls", with "no VM-wide fix for the underlying
per-call cost" available. The call count was right; the attribution was not.
Every `ByteBuf` accessor runs `ensureAccessible()` → `RefCnt.isLiveNonVolatile`
→ **`VarHandle.get`**, and a signature-polymorphic call site cannot be found in
the native registry by its own descriptor — so the JIT's per-call-site native
cache had cached a permanent refusal for every one of them, and each call
re-ran `invoke_or_native`'s whole cascade plus a compile probe that could never
succeed. `VarHandle.get` on an `int` field cost **1 979 ns**; it now costs
**155 ns**. Full record (internal):
`fixed-suite-bugs/netty/varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`.

Re-measured on this page's own four classes, same host, same day, `-Xmx 1500m`,
no per-method cap (so the class completes instead of being clipped). The host
carried unrelated load of 8-35 throughout, so read the CPU column, not wall:

| class | before (wall / CPU) | after (wall / CPU) | ratio (CPU) |
| --- | ---: | ---: | ---: |
| `AdaptiveByteBufAllocatorTest` | 635–725 s / 638–727 s | **419–432 s / 421–435 s** | 1.51–1.67× |
| `AdaptiveByteBufAllocatorGrowthTest` | 1 081 s / 2 963 s | **645 s / 2 042 s** | 1.45× |
| `AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest` | 752 s / 754 s | **435 s / 432 s** | 1.75× |
| `search.SearchProcessorTest` | 153 s / 141 s | **131–139 s / 130–135 s** | ~1.07× |
| `BigEndianHeapByteBufTest` (control, not on this page) | 37.4 s / 56.5 s | 38.4 s / **48.5 s** | 1.17× |

**One reading in this table was nearly a cherry-pick, and naming it is the
point.** An un-paired run of `AdaptiveByteBufAllocatorTest` on the fixed binary
came back at **270 s**, which would have made that row read 2.6×. Re-run
interleaved against `base` — same class, same host, adjacent — it measured
**432 s** and **419 s** against a `base` of 725 s and 635 s, with two further
un-paired runs at 426 s and 475 s. The 270 s was this box, not this change, and
1.45-1.75x is where the family actually sits. **Interleave, or do not compare.**

All of them still pass every test they did before — `AdaptiveByteBufAllocatorTest`
127/127, `GrowthTest` 400/400, `UseCacheForNonEventLoopThreadsTest` 128/128,
`SearchProcessorTest` 15/15, `BigEndianHeapByteBufTest` 414/414 — which is the
half of this that a timing table cannot show.

**What this settles, and what it does not.**

* The **misattribution** is settled. Anyone arriving here should not read these
  classes as evidence for the size of the generic per-call cost; a 1.7× on the
  allocator class came out of one cache, not out of the dispatcher.
* The **wall cap** is not settled. `AdaptiveByteBufAllocatorTest` is 419-432 s
  against a 180 s cap — 2.4× over, down from 4×. The other two remain further
  over. Under the suite's 180 s cap all three still report HANG, and this page
  still answers why.
* `SearchProcessorTest` is unchanged in kind: it is the same load-margin case
  the fix doc it cites already recorded, now with ~15% more headroom. The
  quiet-box re-run this page asked for was not taken on a genuinely quiet box
  as of 2026-08-17 — see "Reconfirmed 2026-08-29" below for one now.

The residual is the ordinary native-dispatch floor (~150-210 ns per registered
native call from compiled code) — general JIT work, not netty-scoped. This
page's one actionable finding (the `VarHandle.get` cache bug) has been found
and fixed; what remains below is a throughput ceiling, not a defect with a
known fix.

---

*Original 2026-08-16 text follows, unedited.*

**Status: OPEN**, re-measured 2026-08-16 on `dev` `3ef3eb7441c`. Three of the
four classes below are **not a new finding** — they restate and reconfirm a
throughput ceiling already fully characterised on 2026-08-12/13. The fourth
(`SearchProcessorTest`) already has a FIXED doc, and today's result is
flagged as an unresolved re-occurrence of that doc's own stated margin
caveat, not a confirmed reopening.

## What it is

Today's full 657-class, 3-collector-in-parallel suite run recorded all four of
these classes as HANG (180 s timeout) on generational, G1, **and** ZGC:

* `io.netty.buffer.AdaptiveByteBufAllocatorGrowthTest`
* `io.netty.buffer.AdaptiveByteBufAllocatorTest`
* `io.netty.buffer.AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest`
* `io.netty.buffer.search.SearchProcessorTest`

Hitting the cap on all three collectors at once is the shape a shared-host
contention artifact would produce, so the first job was to isolate it.

## Isolation: one class per process, no other target class contending — still HANG

```
cd apps/netty-suite-runner
printf '%s\n' \
  io.netty.buffer.AdaptiveByteBufAllocatorGrowthTest \
  io.netty.buffer.AdaptiveByteBufAllocatorTest \
  io.netty.buffer.AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest \
  io.netty.buffer.search.SearchProcessorTest > /tmp/mygroup.txt
./run-netty-suite.sh --list /tmp/mygroup.txt --gc g1 --shards 1 --out runs/agent-buffer-20260816
```

```
idx  class                                                                    status  found  ok  failed  ms    sig
0    AdaptiveByteBufAllocatorGrowthTest                                       HANG    0      0   0       0     process-died rc=124 timeout=180s
1    AdaptiveByteBufAllocatorTest                                             HANG    0      0   0       0     process-died rc=124 timeout=180s
2    AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest               HANG    0      0   0       0     process-died rc=124 timeout=180s
3    search.SearchProcessorTest                                               HANG    0      0   0       0     process-died rc=124 timeout=180s
```

Total wall for the 4-class run: 722 s — i.e. every class individually
consumed its full 180 s budget before being `timeout`-killed (`rc=124`), one
after another, with nothing else in this list running concurrently. This
rules out **inter-class** contention (racing the other 656 classes in a full
batch) as the explanation. `found=0`/`ok=0` on every row is not evidence the
run made zero progress — `CratonRunner` does not flush `@@RESULT` lines until
a class finishes, so a killed process always reads all-zero regardless of how
far it got.

The raw log shows each process is doing real work up to the point it goes
silent: class start times are exactly 180 s apart, each completes VM
bootstrap and JUnit discovery normally, and the fourth (`SearchProcessorTest`)
additionally logs a JIT compile of `AhoCorasicSearchProcessorFactory.buildTrie`
~12 s in — i.e. it has reached real test-body execution — before 168 more
silent seconds and the kill. Nothing in any of the four logs looks like a
crash, an exception, or a stuck native call.

## HotSpot cross-check: fast, clean, all pass

```
./run-netty-suite.sh --list /tmp/mygroup.txt --hotspot --shards 1 --out runs/agent-buffer-20260816
```

```
idx  class                                                        status  found  ok   ms
0    AdaptiveByteBufAllocatorGrowthTest                            PASS    400    400  11681
1    AdaptiveByteBufAllocatorTest                                  PASS    127    127  10776
2    AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest    PASS    128    128  13240
3    search.SearchProcessorTest                                    PASS    15     15   4934
```

All four classes complete on HotSpot 25 in 5–13 s, nowhere near any timeout.
So whatever is at fault is CratonVM-specific, not a shared classpath/harness
defect and not something HotSpot also struggles with.

## The three `AdaptiveByteBufAllocator*` classes: the known VM-wide per-call cost wall, reconfirmed unchanged

This is not new. It was already measured in detail on 2026-08-12/13:

* `AdaptiveByteBufAllocatorTest` alone executes **826,764,658** `jit_entries`
  (JIT call-boundary crossings) across its 127 tests, at ~667 ns/entry, for a
  wall time of **450–594 s** measured directly — 2.5–3.3× the 180 s cap,
  independent of host load. `perf record` attributes ~30% of CPU to
  call-transfer bookkeeping (`push_entry_full`/`pop_jit_entry`, MIC/dispatch
  machinery, the native-registry probe) and only 3.3% to the interpreter
  itself. This explicitly refutes an `AdaptivePoolingAllocator`-specific
  defect: the non-adaptive baseline class makes almost the same number of
  entries per test, and the cost is "the same per call; its test class simply
  makes 23× more calls per test."
* **None of these classes hang** — they are recorded as HANG only because
  they cross the suite's wall cap.

This is a genuine VM-wide throughput ceiling (per-call/JIT-entry dispatch cost
× hundreds of millions of calls), not a deadlock and not an allocator-specific
defect. No VM-wide fix for the underlying per-call cost has landed (the
`push_entry_full`/`pop_jit_entry` per-entry cost and the native-registry probe
remain open, general JIT work, not netty-scoped).

**Recommendation:** leave these as a recorded throughput gap. A
`class-overrides.tsv` entry could raise the cap to ~600–700 s per class and
convert the HANG into a real (and passing) result, but that is not a fix,
just an acknowledgement of the current floor.

## `SearchProcessorTest`: the FIXED doc's own margin caveat reproducing, not a confirmed reopening

This class **does** have current FIXED-tagged coverage (internal
`arraylist-native-overhead-and-view-carrier-FIXED-20260813.md`) — fixed the
`ArrayList`-native-call-tax defect behind this class's failure and reports it
**15/15 at 95–105 s wall** on "a reasonably quiet box" — but adds its own
explicit warning: *"the fix moves the class from ~1.3x OVER the 120 s
per-method cap to ~0.8x of it... it clears the cap on a reasonably quiet box
and does not on a busy one."* At load 20-29 it measured **14/15** with a
`TimeoutException` on `testUniqueLen64Substrings`.

On 2026-08-16, an isolated solo run (no other target class contending) still
hit the 180 s process-level cap with zero results flushed, at `loadavg`
**11.60** with an unrelated `cratonvm` process from a different session
actively running the entire time — exactly the "busy box" condition the FIXED
doc's own caveat already names as insufficient to clear the cap.

**Conclusion (as of 2026-08-16): not confidently a reopening.** Consistent
with, and most plausibly explained by, the same load-margin caveat the fix's
own writeup already recorded, now observed again under similarly heavy host
load.

## Reconfirmed 2026-08-29

Fresh full 657-class suite run, single shard, ZGC. All four classes still
non-passing:

| class | status | detail |
|---|---|---|
| `AdaptiveByteBufAllocatorGrowthTest` | HANG | 180s cap |
| `AdaptiveByteBufAllocatorTest` | HANG | 180s cap |
| `AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest` | HANG | 180s cap |
| `search.SearchProcessorTest` | **FAIL** (not HANG this time) | `found=15 ok=14 failed=1 ms=160689` — completed under the 180s process cap, but `testUniqueLen64Substrings` still individually timed out: `TimeoutException: ... timed out after 120 seconds` |

`SearchProcessorTest` clearing the 180s process-level cap this time (160.7s
vs. the 180s+ full timeout seen on 2026-08-16) while still failing the same
individual per-method 120s timeout is exactly the load-margin pattern this
doc already predicted — closer to the boundary, not past it, on whatever this
host's load happened to be during this run. Still not re-run on a
`loadavg`-near-0, single-session-only host to get a clean verdict either way.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' \
  io.netty.buffer.AdaptiveByteBufAllocatorGrowthTest \
  io.netty.buffer.AdaptiveByteBufAllocatorTest \
  io.netty.buffer.AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest \
  io.netty.buffer.search.SearchProcessorTest > /tmp/mygroup.txt

# CratonVM, isolated, one class per process
./run-netty-suite.sh --list /tmp/mygroup.txt --gc g1 --shards 1 --out /tmp/repro

# HotSpot oracle, same list
./run-netty-suite.sh --list /tmp/mygroup.txt --hotspot --shards 1 --out /tmp/repro

# check host load before trusting a SearchProcessorTest result either way
cat /proc/loadavg; ps aux | grep -i cratonvm
```
