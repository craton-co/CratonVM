# `AdaptiveByteBufAllocator*` and `SearchProcessorTest` — confirmed genuine (non-contention) 180 s wall hits, not deadlocks

**Status: RETIRED 2026-08-17 — the "genuine VM-wide throughput ceiling" this
page reconfirmed was, for the larger part, one cacheable call shape, and that
is now FIXED.**

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
**155 ns**. Full record:
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
1.45-1.75x is where the family actually sits. The ancestor page said so in
advance
(`performance/vm-per-call-dispatch-cost-RETIRED-20260817.md` §6: the same class
measured 450, 551, 559 and 594 s on one binary): **interleave, or do not
compare.**

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
  quiet-box re-run this page asked for has still not been taken on a genuinely
  quiet box — every measurement above was made at load 8-35.

The residual is the ordinary native-dispatch floor (~150-210 ns per registered
native call from compiled code), which is
`performance/vm-per-call-dispatch-cost-RETIRED-20260817.md` — reopened on
2026-08-17 because these classes are what is still failing on it — and not a
netty matter. This page moves here because its one actionable finding has been found
and fixed and because its stated cause is now corrected.

---

*Original text follows, unedited.*

**Status: OPEN**, re-measured 2026-08-16 on `dev` `3ef3eb7441c` (checkout
`C:\craton\cratonvm`). Three of the four classes below are **not a new
finding** — they restate and reconfirm a throughput ceiling already fully
characterised on 2026-08-12/13. The fourth (`SearchProcessorTest`) already has
a FIXED doc, and today's result is flagged as an unresolved re-occurrence of
that doc's own stated margin caveat, not a confirmed reopening.

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
far it got (the same non-signal this doc set records for other harnesses that
buffer to end-of-run, e.g. SbRunner).

The raw log shows each process is doing real work up to the point it goes
silent: class start times are exactly 180 s apart (`02:59:29`, `03:02:29`,
`03:05:30`, `03:08:30` UTC), each completes VM bootstrap and JUnit discovery
normally, and the fourth (`SearchProcessorTest`) additionally logs a JIT
compile of `AhoCorasicSearchProcessorFactory.buildTrie` ~12 s in — i.e. it has
reached real test-body execution — before 168 more silent seconds and the
kill. Nothing in any of the four logs looks like a crash, an exception, or a
stuck native call; the silence itself is inconclusive by this harness's
design (see "Traps" below).

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

This is not new. It was already measured in detail on 2026-08-12/13 and the
conclusion has not changed on today's commit:

* [`netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md`](../../fixed-suite-bugs/netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md)
  first filed these three classes' wall-clock timeouts (no assertion
  failures), noting "All of them **pass when run solo**" — meaning given
  enough wall-clock room, not within the suite's 180 s cap — "the gap is 10×
  broadly and ~90× through `AdaptivePoolingAllocator`."
* [`internal/performance/netty-per-call-throughput-20260813.md`](../../performance/netty-per-call-throughput-20260813.md)
  (RETIRED, kept as a measurement record) sized it exactly:
  `AdaptiveByteBufAllocatorTest` alone executes **826,764,658** `jit_entries`
  (JIT call-boundary crossings) across its 127 tests, at ~667 ns/entry, for a
  wall time of **450–594 s** measured directly (§3.2/§3.3) — 2.5–3.3× the
  180 s cap, independent of host load. `perf record` attributes ~30% of CPU
  to call-transfer bookkeeping (`push_entry_full`/`pop_jit_entry`,
  MIC/dispatch machinery, the native-registry probe) and only 3.3% to the
  interpreter itself. It explicitly refutes an `AdaptivePoolingAllocator`-
  specific defect: the non-adaptive baseline class makes almost the same
  number of entries per test (§1's `AdaptiveBigEndianHeapByteBufTest` row),
  and the cost is "the same per call; its test class simply makes 23× more
  calls per test."
* That doc's own §8 states plainly: **"None of these classes hang... they are
  recorded as HANG only because they cross the suite's wall cap."**

Today's isolated, non-contended run reconfirms this unchanged: all three
classes still hit the 180 s cap solo, consistent with the previously measured
450–594 s wall times, which exceed the cap by 2.5–3.3× regardless of host
load. This is a genuine VM-wide throughput ceiling (per-call/JIT-entry
dispatch cost × hundreds of millions of calls), not a deadlock and not an
allocator-specific defect. No VM-wide fix for the underlying per-call cost
has landed as of this commit (the throughput doc's own §6 "what would
actually move it" — the `push_entry_full`/`pop_jit_entry` per-entry cost and
the native-registry probe — remain open, general JIT work, not netty-scoped).

**Recommendation, unchanged from the parallel `FastThreadLocalTest` case**
(see [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)):
leave these as a recorded throughput gap. A `class-overrides.tsv` entry could
raise the cap to ~600–700 s per class and convert the HANG into a real (and
passing) result, but that is not a fix, just an acknowledgement of the
current floor.

## `SearchProcessorTest`: the FIXED doc's own margin caveat reproducing, not a confirmed reopening

This class **does** have current FIXED-tagged coverage:

* [`arraylist-native-overhead-and-view-carrier-FIXED-20260813.md`](../../fixed-suite-bugs/netty/arraylist-native-overhead-and-view-carrier-FIXED-20260813.md)
  fixed the `ArrayList`-native-call-tax defect behind this class's failure and
  reports it **15/15 at 95–105 s wall** on "a reasonably quiet box" — but adds
  its own explicit warning: *"the fix moves the class from ~1.3x OVER the
  120 s per-method cap to ~0.8x of it... it clears the cap on a reasonably
  quiet box and does not on a busy one. This host was at load 20-29
  throughout this branch's runs."* At that load it measured **14/15** with a
  `TimeoutException` on `testUniqueLen64Substrings`.
* [`misc-non-tls-residuals-CLOSED-20260813.md`](../../fixed-suite-bugs/netty/misc-non-tls-residuals-CLOSED-20260813.md)
  independently confirms the same fix and repeats the identical caveat
  verbatim.

Today's isolated solo run (no other target class contending) still hit the
180 s process-level cap with zero results flushed. But the host was
measurably busy throughout this run: `loadavg` read **11.60** during/after the
run, and `ps aux` showed an unrelated `cratonvm` process (from the
differently-cased `C:\craton\CratonVM\target\release\cratonvm` checkout — a
different session's build) actively running the entire time. This is exactly
the "busy box" condition the FIXED doc's own caveat already names as
insufficient to clear the cap.

We could not establish from this run whether it made it past
`testUniqueLen64Substrings` (the one test the FIXED doc's own busy-box
measurement failed on) or stalled earlier — as noted above, `CratonRunner`
only emits results at completion, so a killed process reads `found=0` either
way.

**Conclusion: not confidently a reopening.** It is consistent with, and most
plausibly explained by, the same load-margin caveat the fix's own writeup
already recorded three days ago, now observed again under similarly heavy
host load. Whoever picks this up next should re-run it solo on a quiet box
(`loadavg` near 0, no other sessions' `cratonvm` processes live) before
treating it as a regression. If it still fails to clear 180 s on a genuinely
quiet box, that would be new information this doc does not have.

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

## Related

* [`netty-per-call-throughput-20260813.md`](../../performance/netty-per-call-throughput-20260813.md) — RETIRED, the full per-call-cost measurement behind the three `AdaptiveByteBufAllocator*` classes.
* [`netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md`](../../fixed-suite-bugs/netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md) — first filed these three classes as wall-clock-only timeouts.
* [`arraylist-native-overhead-and-view-carrier-FIXED-20260813.md`](../../fixed-suite-bugs/netty/arraylist-native-overhead-and-view-carrier-FIXED-20260813.md) and
  [`misc-non-tls-residuals-CLOSED-20260813.md`](../../fixed-suite-bugs/netty/misc-non-tls-residuals-CLOSED-20260813.md) — the `SearchProcessorTest` fix and its quiet-box-only caveat.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md) — sibling case: a different class hitting the same class of VM-wide per-call-cost ceiling.
