# `TestHttpServletDoHeadInvalidWrite*ValidWrite*` — consolidated history and current status

| | |
|---|---|
| **Status** | OPEN, but understood and scoped. **Not a correctness bug.** Generational-only throughput collapse (`[moving-young] fallback`), reconfirmed on current `dev` 2026-08-12/13 at a 3000s budget (7.5x the 400s that already wasn't enough back on 08-10). G1 and ZGC are both clean. |
| **Scope** | The full parameterized family — up to 64 classes (`InvalidWrite{0,1,511,512,513,1023,1024,1025}ValidWrite{same 8}`), each ~156 JUnit sub-tests, a full Tomcat start+HTTP+stop cycle per class. |
| **Why this doc exists** | This family has accumulated **eleven** separate internal investigation docs since 2026-07-01, most already `-FIXED`/closed, chasing what turned out to be several distinct, now-resolved correctness bugs plus one structural, still-present GC/JIT interaction. Nobody reading any single one of those docs gets the full picture. This page is that picture, plus this session's fresh verification. |

## TL;DR

The DoHead family has been hit by at least **eight distinct, confirmed, now-fixed correctness bugs** since early July 2026 — GC-root gaps, a JIT frame-safety gap, native buffering mismatches, unpinned `ObjectRef`s across allocating native calls, a thread-identity aliasing bug, and an OSR/exception-table gap. All of those are closed. What's **left**, and has been left since at least 2026-08-10, is not a bug in the classic sense: under the **Generational** collector specifically, ~44 of the family's 64 classes fall back to the collector's slow, safety-first non-moving young sweep on essentially every cycle, and the resulting throughput collapse is severe enough that even a very generous per-class timeout doesn't let them finish. **G1 and ZGC don't have this failure mode at all** — G1 because it evacuates instead of falling back (a separate, now-fixed crash risk), ZGC because it's a non-moving collector by design and structurally cannot hit a moving-collector safety fallback.

## Part 1 — the correctness bugs (all fixed, chronological)

Every one of these was found, root-caused, and fixed against DoHead-family reproductions. None of them is the reason the family still fails today — they're kept here so nobody re-discovers them from scratch.

| Date | Doc (under `fixed-suite-bugs/tomcat/`) | Root cause | Fix |
|---|---|---|---|
| ~pre-07-01 → 07-02 | `tomcat-dohead-blocked-thread-gc-sweep-corruption.md` | `Class$Atomic.casReflectionData`/`newReflectionData` held live `ObjectRef`s in a GC-invisible side store; `AQS` `ConditionNode` held in a callee-saved JIT register across `park`/`awaitNanos`, invisible to `scan_active_jit_frames`. Addendum: even after the skip-list fix, the condition chain survived only in a register for the whole blocked window on the large-write sibling. | `2ea402b7` (root the side store into `collect_roots`/`update_all_roots`); skip-list additions for `awaitNanos`/`awaitUntil`/`awaitUninterruptibly`; `23388aba`→`71aef458` (pin `Thread.parkBlocker` across `park`). |
| 07-01 | `tomcat-dohead-stop-wait-latency.md` | Two accept paths (`ServerSocketChannel.accept()`, `sun.nio.ch.Net.accept()`) could block in OS `accept()` on a cloned/`Arc`'d listener handle after the registry entry was already removed, stalling stop. | Close-aware nonblocking poll loop, 10ms interval, inside the existing GC blocked region. |
| 07-02 (orig.) / 07-09 (regressed) / 07-13 (re-fixed) | `dohead-streamencoder-eager-flush-commit-threshold-FIXED.md` | The real-mode `sun.nio.cs.StreamEncoder` shim forwarded every `write()` straight through with no internal buffering, unlike HotSpot's 512-8192 byte batching — changed the byte-count granularity `NoBodyOutputStream.checkCommit()` observes. Regressed 07-09 when an ungated `OutputStreamWriter` native surface shadowed the shim entirely. | `1773d3df2`: a Rust-side pending-bytes buffer per encoder, flushed only when full. Re-fix: gate the OSW native surface under `synthetic-jdk` only. |
| 07-03 | `dohead-mainvm-array-helper-boundary-read.md` | `mark_young`'s young-mark BFS (`gen_heap.rs`, inlined `for_each_ref_slot`) capped a conservative-root candidate's `array_length` at `i32::MAX` but never cross-checked the resulting extent against the arena — a misparsed packed pointer walked ~59.3MB out of bounds. **Crash face fixed; the upstream producer of the corrupt bytes was left open** (most likely the register-invisibility residual below, or a `try_alloc_young` publish-race). | `8e64d9a5`: reject any candidate whose computed extent leaves the generation. |
| 07-09 (found) / 07-13 (fixed) | `dohead-jit-heap-corruption-register-invisibility-FIXED.md` | **Layer 2:** a stale register-held reference clobbered a freed slot's header to zero; sweep walkers then strided the zeroed span as a phantom object and re-freed it, crossing into live headers — UAF/double-serve. Also: three unrelated 07-09 regressions (SocketFactory producer/consumer split-brain, the same OSW shadowing above, a `String.size()I` NSME flood). **Layer 1 (register-invisibility) was explicitly left unfixed** — the real fix is precise oop maps/a shadow stack, not a policy patch. | `928cc5b3` (anchor-based resync + deferred zeroing + unwind-on-anomaly sweep hardening); `945e44920` (STW-takeover bracketing — killed the dominant flake family); `c73eeda7b`; `d94712f2a`. |
| 07-14 (found) / 07-15 (fixed) | `dohead-residual-http2-midrun-hang-FIXED.md` | **A:** `ThreadRegistry` resolved `Thread` mirrors by raw pointer; a recycled dead-thread address aliased a live new `Thread`, producing spurious `IllegalThreadStateException`/lost wakeups. **B:** `DirtyBufferGuard::drop` on thread exit discarded a dying thread's buffered old→young card-table edges — conflating "no live stack roots" with "no live heap edges the thread's writes created," corrupting the static `FastHttpDateFormat`→`ConcurrentLinkedQueue` chain used for HTTP Date headers. | **A:** identity re-keyed on `Thread.tid`. **B:** "retain-and-reap" — a dying thread's non-empty card buffer stays registered and gets drained by the next STW `flush_all` instead of being dropped. |
| 07-15 → 07-21 (many checkpoints, C17-C46) | `dohead-post-fix-sporadic-residuals-FIXED.md` | A long tail of distinct bugs surfaced only after the bigger ones above stopped masking them: zero-slot `HashMap$Node` allocations in `HttpURLConnection.getHeaderFields()`/`System.getenv()`; an HTTP/2 selector interest-change race (Linux); **an OSR/exception-table gap** — `compile_osr_artifact()` only bailed OSR on a direct `athrow`, not on any non-empty exception table, so OSR-compiled callers silently lost real try/catch for callee-thrown exceptions; unpinned `ObjectRef` locals in `native_map_remove_pinned` across a GC-capable `equals()` call, corrupting the response header map; **unpinned `this` in `native_bos_flush_locked`/`_write_locked`/`_write_bulk_locked`** (`BufferedOutputStream`) across `invoke_virtual` — identified as the root cause of most of what had been miscategorized as "environmental flakiness" since 07-15. | `eda677f45`→`62693f104` (RBC.6b OSR exception-table bail); `955031d30`→`bb266fb8e` (pin/refresh map chain nodes); `1360cd9ab` (pin `this` across the BOS native calls). |
| 07-21 → 07-23 | `dohead-environmental-transport-flake-FIXED.md` | The residual ~1-2% flakiness left after the above turned out to be more instances of the *same* unpinned-`ObjectRef`-across-allocation shape, scattered across native I/O receivers, JULI/`LogManager`, `MessageBytes`, `HttpURLConnection`, `HashSet`/`HashMap.remove` in `ThreadPoolExecutor.processWorkerExit`, a `ClassLoader` OOB field probe, `Objects.hash(Object[])`, and a stale pre-park `Reference`/`ReferenceQueue` snapshot. Also, separately: a JIT compiler defect in JUnit's own `TestClass.collectAnnotatedMethodValues` (a compiled enhanced-for iterator local going null). | Root every native-held `ObjectRef` across allocating/collecting boundaries; keep only the one JUnit iterator helper interpreted. 64/64 PASS, no-JIT and JIT, at closure. |
| 09-05 (found) / 09-06 (fixed) | `gen-atomicreference-getandset-null-receiver-doheadhttp2-FIXED-20260906.md` | `native_atomic_ref_get_and_set` opened `unsafe_obj(args, 0).unwrap()`, as did 14 sibling `Atomic{Long,Reference}` natives, so a receiver arriving as `Value::Object(None)` — or an `args` slice with no receiver slot — panicked inside the native instead of raising NPE. Reported on Azure as three `CRASH` classes; the process survived every one, and the harness's `panicked at` grep filed them beside real `rc=139` segfaults. The page's own hypothesis (a marshalling defect at the JIT call site) is REFUTED: every dispatch door raises NPE for a null receiver before any native runs, so the receiver was already null in the field it came from — i.e. one face of the live-reference loss in Parts 2/5, not a separate defect. | `atomic_receiver` on all 15 sites: raises the `NullPointerException` JVMS `invokevirtual` specifies and prints args, the Java stack and (under `RUST_BACKTRACE`) the Rust one. The unwrap no longer exists, so the reported crash cannot recur; a lost receiver is now REPORTED rather than fatal, and fired zero times across 65 Generational class-runs on two platforms. Harness fixed on Azure: `panic` is its own results.csv column and a JUnit summary decides `status`. |
| (unresolved) | `dohead1023-http2-index0-socketexception-likely-host-contention.md` | One single sub-test, `TestHttpServletDoHeadInvalidWrite1023ValidWrite1023`'s `testDoHeadHttp2[0]`: a `SocketException: Connection aborted (WSAECONNABORTED)` on the very first HTTP/2 preface read. Never confirmed as a CratonVM bug or refuted — best guess is Windows scheduler starvation under heavy shared-host load. **Left open, low-confidence, not chased further.** A sibling failure in the same original log (`testDoHeadHttp2[25]`, a `Thread.setPriority()` NPE) *was* a confirmed bug and was fixed separately (`populate_real_thread_holder`, `3e7f4a30`/`34faddd9`) — not the subject of this doc. | — |

**Net effect of Part 1:** the 09-06 row is a later addition — the eight rows above it closed by 2026-07-23, when the DoHead family was passing 64/64 clean, both JIT and no-JIT, 3,509 native-lib tests green. Every bug above is closed. None of them explains what's described in Part 2.

## Part 2 — the mechanism that's still open: `[moving-young] fallback`, Generational-only

Starting 2026-08-06/07, a separate line of investigation (unrelated Tomcat classes: `TestHostConfigAutomaticDeployment*`, `TestNonBlockingAPI`) chased a hypothesis that the Generational collector's young-generation "moving young falls back to a non-moving sweep whenever it can't prove a JIT frame is safely relocatable" mechanism was itself expensive. That specific investigation (`gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED.md`, closed 08-07) **disproved its own hypothesis for those classes** — nothing hung, and the fallback's own GC-side cost measured at 0.4% of wall time. But it correctly identified the actual mechanism and terminology, which is what shows up again at DoHead scale four days later.

**The architectural root**, from `moving-young-corruption-rootcause.md`: `moving_young_coverage_complete()`/`moving_young_safepoint_coverage_complete()` (`jit/src/x64.rs`) can only certify root coverage from the abstract interpreter's own locals and operand stack — but compiled frames also hold live oops in scalar-replacement field slots, LICM hoist slots, and the blind full-GPR spill area, none of which the shadow-stack publish covers. The fallback to non-moving is the GC's fail-closed safety response to that gap: if it can't prove a JIT frame is safe to move through, it doesn't move it, at the cost of a much slower, conservative sweep for that cycle.

**Whether the fallback actually costs a given workload anything is empirical per-workload**, not a fixed tax:

- For the Hibernate `OffsetDateTimeTest`/`ZonedDateTimeTest` family (`moving-young-inert-under-jit-throughput-tax-20260730-RETIRED.md`) and the `TestHostConfigAutomaticDeployment*`/`TestNonBlockingAPI` pair above, it turned out to cost ~nothing once other, unrelated bugs were fixed.
- For four Spring Boot classes (`moving-young-fallback-four-springboot-classes-RETIRED-20260818.md`), it was real and severe enough to turn tests red — one root cause (`innermost_frame_method` failing closed on indirect/virtual calls) was found and fixed (zeroed one class's fallback count entirely), but two of the four needed switching collectors (G1/ZGC) instead, because their dominant fallback reasons (`unregistered-jit-frame-on-stack`, `cross-thread-jit-peer`) have no equivalent fix — they're inherent to Tomcat/Spring Boot's thread-pool pattern under a moving young generation. **That page's own closing note: this mechanism "is not fixed — it is no longer on the default path." Anyone running `-XX:+UseGenerationalGC` explicitly still hits all of it.**
- For the DoHead family specifically (`gc-backend-3way-fullsuite-comparison-20260810.md`), it is real, severe, and — as of this doc — still unresolved:

  > Of 63 classes non-PASS under the default (Generational) collector but PASS under both G1 and ZGC, **44 are the `TestHttpServletDoHeadInvalidWrite*ValidWrite*` family** (44 of its 64 classes). 98% of those 63 classes (62/63) log `[moving-young] fallback` lines; **0/63 do so under G1 or ZGC.** Re-run on dev at a 400s budget (up from the original 300s): **44 HANG out of 44, unmoved.** Fallback reasons across the 63: `xt-helper-window-conservative-scan` (289 lines in one run), `unregistered-jit-frame-on-stack` (256), `innermost-rbp-belongs-to-unguarded-callee` (209), `compiled-frame-oop-not-published` (53), `active-safepoint-map-incomplete` (52), `compiled-frame-band-unbounded` (4), `cross-thread-jit-peer` (4). Characterized explicitly as **"Throughput, not deadlock"** — the logs show Tomcat genuinely starting, servicing, and stopping the whole time, just far too slowly to finish inside the budget.

  G1 doesn't have this failure mode because it evacuates instead of falling back (which used to crash — see `g1-sigsegv-unguarded-callee-jit-frame-FIXED.md`, an unaligned TLAB carve, now fixed and unrelated to JIT-frame coverage). ZGC structurally cannot hit it: it's a stop-the-world **non-moving** mark-sweep, `vm_init.rs` hard-codes `RELOCATION_REQUESTED = false`, and the fallback exists specifically to protect a *moving* collector's relocation step.

## Part 3 — this session's verification (2026-08-12/13)

Ran the complete 651-class Tomcat suite under all three GC backends in parallel (3 GCs x 2 shards, `-Parallel 2`, **3000s per-class timeout** — 7.5x the 400s budget that the 08-10 doc already found insufficient), on current `dev`, local Windows box.

| Arm | PASS | FAIL | HANG | NOSUMMARY |
|---|---|---|---|---|
| G1 shard1 | 300/326 (92%) | 25 | 0 | 1 |
| G1 shard2 | 311/325 (95.7%) | 10 | 2 | 2 |
| ZGC shard1 | 307/326 (94.2%) | 16 | 1 | 2 |
| ZGC shard2 | 314/325 (96.6%) | 8 | 3 | 0 |
| Generational shard2 | 301/325 (92.6%) | 11 | **11** | 2 |
| Generational shard1 | (dominated by the DoHead family; every `TestHttpServletDoHeadInvalidWrite*` class hit in this run's range HANGs at the full 3000s) | | | |

**The DoHead family still does not clear even a 3000s budget under Generational.** This is not new information about the mechanism — it's the same already-documented throughput collapse, now shown to survive a timeout nearly an order of magnitude larger than the one that already failed to clear it on 08-10. That's the useful new data point: this isn't a borderline "just needs a slightly bigger budget" situation. Whatever the per-class overhead is under sustained `xt-helper-window-conservative-scan`/`unregistered-jit-frame-on-stack` fallback pressure, it is severe enough that no reasonable timeout increase alone will clear it.

**Cross-GC confirmation, from a separate rerun:** built the union of every class that failed/hung/crashed under *any* of the 6 shards above (93 classes total) and reran that exact set under ZGC alone (`-Parallel 4`, same 3000s budget). **75 of 93 (80.6%) now PASS under ZGC**, including presumably most of the DoHead-family entries that were only in the union because of the Generational arm. The 4 that still HANG under ZGC (`TestNonBlockingAPI`, `TestHttp2Section_8_2`, `TestCharsetCachePerformance`, `TestMethodPerformance`) are exactly the same 4 ZGC's own original shards already hung on — fully reproducible, GC-independent, and unrelated to the DoHead mechanism. None of the 93 were DoHead-family classes among the still-failing 18, consistent with the family being Generational-specific as this doc's mechanism section predicts.

## Where this leaves things

- ~~**No action needed on the correctness front.**~~ **STALE — see Parts 4 and 5.** Part 1's bug classes are indeed closed, but this bullet generalised from them to the whole family and that no longer holds: on a fast host these classes FINISH and FAIL rather than hang, with reference fields reading null. Part 5 measures the correctness residual and what its disappearance is and is not worth.
- **The Generational-only throughput collapse is understood, not mysterious, and not new.** It's the same `[moving-young] fallback` mechanism already root-caused architecturally (`moving-young-corruption-rootcause.md`) and already observed at exactly this scale on 08-10. This page adds: it survives a 3000s budget too, and it is cleanly GC-specific (G1 and ZGC both pass the same classes without triggering the mechanism at all).
- **This is already mitigated at the product level**: ZGC has been the shipped default since 2026-08-10 specifically because it cannot exhibit this class of fallback. Anyone hitting this is doing so under an explicit `-XX:+UseGenerationalGC`.
- **A real fix, if ever undertaken, is the same one named throughout Part 2**: precise oop maps or a shadow stack for compiled frames, so `moving_young_coverage_complete()` can actually certify what it currently has to assume. That is a substantially larger undertaking than anything in Part 1, and nothing in this investigation's history suggests a smaller intervention (timeout increases, isolated fallback-reason fixes like `innermost_frame_method`) will clear the DoHead family specifically — the Spring Boot investigation already found that two of its four classes needed a collector switch, not a fix, for exactly this reason.
- **Not chased further here**: per-fallback-reason attribution specific to the DoHead family (which of the seven reasons dominates *this* class shape, the way the Spring Boot doc did for its four classes) would be the natural next step if someone wants to reduce fallback volume rather than switch collectors — not attempted in this session.

## Part 4 — 2026-09-06: today's full-suite CRASH does not reproduce at small scale, on current dev

A fresh complete 640-class, 3-GC-arm-concurrent Tomcat run on `dev` (worktree
built at `8d83c7585`, before today's later GC/JIT landings) scored the
DoHead family at **2 CRASH / 51 FAIL / 5 HANG of 64** under Generational,
against **0 FAIL / 1 FAIL / 0 FAIL** on G1/ZGC respectively — on its face a
reconfirmation of this page's mechanism. The two CRASH classes
(`TestHttpServletDoHeadInvalidWrite1ValidWrite1024`,
`...InvalidWrite512ValidWrite0`) both scored `rc=137` (SIGKILL) at
260–290 s, which is this run's harness cap acting on a class that didn't
finish — the same "throughput, not deadlock" shape as the rest of this page,
just classified `CRASH` instead of `HANG` by this particular harness's
rc-to-status mapping.

**But it does not reproduce on the current dev tip (`f45fe11d0`, after
merging 87 commits including the pin-discharge and moving-young-withdrawal
work from today) at smaller scale:**

| condition | binary | result |
|---|---|---|
| Standalone, 1 process, 3 classes (the 2 CRASH classes + one control) | `f45fe11d0` | **3/3 PASS in 13–15 s each.** `CRATONVM_GC_STATS`-style tracing on a direct repro of one class showed `[GC] decision: no collection has run yet` — the process never even ran a young collection, let alone a moving one. |
| 3-way concurrent (Generational + G1 + ZGC simultaneously, same 3 classes each — light contention, not the full 640-class scale) | `f45fe11d0` | **9/9 PASS**, 16–20 s each on all three arms, including Generational. |

Both checks used the identical launch path (`run-tomcat-suite-3gc-20260906.sh`
+ `EXTRA_VM_ARGS`) as the original run, so the GC selection itself is not in
question — a direct repro confirmed `moving_young_requested=true` under
`-XX:+UseGenerationalGC`. The only things that changed were (a) the dev
commit and (b) the scale/duration of concurrent host load.

**This does not overturn Parts 1–3.** The architectural mechanism there was
established with direct instrumentation (`[moving-young] fallback` line
counts, `CRATONVM_GC_STATS` decision histograms) on classes that were shown
to *actually enter* a moving cycle and fall back. Nothing here re-examined
that evidence, and it stands.

**What this does open is a real, unresolved question**: how much of
*today's* full-640-class 3-arm-concurrent CRASH/FAIL/HANG total for this
family is the moving-young-fallback mechanism specifically, versus sustained
multi-hour 3-way host contention (CPU + memory pressure from three complete
640-class sweeps running in parallel) pushing an otherwise-modest per-class
GC tax over a ~300 s cap — the same "shard count is part of the measurement"
effect `nonpassed-class-census.md` already documents for other Tomcat
classes on this exact suite. At the scale tested here (a 3-class burst,
either alone or under matching 3-way concurrency), the mechanism did not
engage at all — no young collection ran even once.

**Not settled, and not chased further today**: a real answer needs a
full-scale reproduction — the actual 640-class, hours-long, 3-arm-concurrent
condition, on `f45fe11d0` — with `[moving-young] fallback` line counts
captured per class, the way Part 3's original 08-10/08-12 sweeps did. That
was not attempted here (it is the same multi-hour cost as the original
measurement). Until that is run, treat today's specific 2-CRASH data point
as **unconfirmed at the current dev tip**, not as a fresh reproduction of
this page's mechanism.

**Note added while landing the above (same day, later merge)**: `dev` just
landed
`moving-young-fallback-was-residue-and-not-the-cost-FIXED-20260906.md`,
a different workload (Spring Boot's Kafka integration test) but a finding
that bears directly on this page's central assumption. Two things it
establishes there:

1. Most `[moving-young]` fallback triggers were a **false positive** in the
   A5 coverage probe (a stale return address left below a compiled frame's
   own `entry_sp`), now screened out by default
   (`CRATONVM_JIT_A5_RESIDUE_FILTER=1`). This page's own fallback-reason
   tallies (§7 of the netty page this history cites, and the DoHead
   fallback-reason breakdown in Part 2) predate that screen and may
   overcount for the same reason.
2. More importantly: **the non-moving sweep this page calls the "sound,
   fail-closed response" is not unconditionally safe.** On that workload it
   reclaimed a live object outright — `CRATONVM_DBG_SWEEP_ZERO=1` caught a
   `NativeThreadSet` zeroed by the non-moving sweep while still reachable
   through a register/native-stack root the marker missed — and that, not
   any throughput cost, was the test's real failure.

**This page's "Not a correctness bug" verdict for the DoHead family is not
re-examined by that finding** — nobody has run `CRATONVM_DBG_SWEEP_ZERO=1`
against a DoHead-family class to check for the same live-reclaim signature,
and the two workloads are different enough (this page's failures are all
timeouts, not `ClassCastException`/`NullPointerException`-shaped wrong
answers) that the same mechanism should not be assumed. But the general
premise this page's verdict leans on — "falling back to non-moving is
safety-first" — no longer holds unconditionally project-wide, and that is
worth knowing before treating any future DoHead-family symptom as "just
throughput" without checking.

## Part 5 — 2026-09-06: the CORRECTNESS residual, and what "does not reproduce" is worth

Part 4 above asks how much of today's full-suite total is the moving-young
mechanism versus host contention. This part answers the neighbouring question
for the CORRECTNESS half — the `ReentrantLock.sync` NPE, not the timeouts —
and it reaches Part 4's observation by a different route: Part 4 saw
`no collection has run yet`, i.e. relocation had stopped; the measurements
below quantify exactly that and then divide it out.

### The interleaved control, and why its raw run counts could not settle it

Interleaved control -- one box, the same minutes, 4-way parallelism, arms
alternated round by round, 900 s cap on both so a hang cannot pass as a pass:

| arm | runs | OK | non-OK | truncated | NPE lines |
|---|---:|---:|---:|---:|---:|
| the binary that measured 5/24 earlier today | 24 | 22 | **2** | 0 | 2 |
| current `dev` | 24 | **24** | **0** | 0 | 0 |

Both surviving failures on the old binary are this residual's exact signature
(`Http2TestBase$TestInput.fill:1094`). Mean completed-run duration is
equivalent -- 101.1 s old, 99.0 s new -- so the newer binary is not passing by
running slower or by dying early, and neither arm truncated. Counting an
earlier plain-vs-instrumented sweep on the same build, current `dev` is
**0 failures in 72 runs** — subject to the masking caveat below, which is
the first thing to read here.

**MASKED, NOT FIXED — and this is the load-bearing caveat.** This defect
REQUIRES young relocation (`CRATONVM_NO_MOVING_YOUNG=1` took it to 0/24). Young
relocation is currently being refused on this workload: every
`[moving-young]` line in BOTH arms of the run above is a `fallback` with
`reason=unregistered-jit-frame-on-stack` (217 old, 216 new), and a
`CRATONVM_GC_STATS=1` census shows the surviving moving cycles are not equal
between the arms either:

| arm | moving young cycles | non-moving |
|---|---:|---:|
| old binary | 7 and 3 (two runs) | 14, 23 |
| current dev | 1 and 3 (two runs) | 25, 22 |

Small sample (two runs per arm), but the direction is unambiguous and it is
confounded with the result: the arm that failed relocated ~2.5x more than the
arm that did not. **A relocation-gated defect not firing in the arm that
relocates less is not evidence of a repair.**

`docs/internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
reaches the same conclusion independently for its own 19 classes, and states the
consequence plainly: the refusal now suppressing relocation is the SAME
`[moving-young] fallback` this page's Part 2 documents as a *throughput*
problem, so **whoever repairs moving-young engagement re-exposes this
correctness bug**. The green is conditional on the collector declining to do its
job.

**Also a REPRODUCTION result, not a root cause.** Two further limits:

* The old binary's rate TODAY is 2/24 (~8%) against 5/24 (~21%) when this
  residual was characterised. The host is less sensitive than it was, so 72
  clean runs prove less than 72 runs at the earlier sensitivity would.
* **No commit is credited.** `dev` took many GC commits in the interval,
  including one that landed a moving-young root cause and was then WITHDRAWN by
  its author. Attributing the disappearance to any of them would be a guess.

**The shape to look for if it returns**, since the characterisation cost more
than the disappearance did: the victim is
`java.util.concurrent.locks.ReentrantLock.sync` -- null at `unlock()` and
non-null at `lock()`, so the slot is live entering the critical section and zero
leaving it, while the owning thread is blocked INSIDE it. Seen on two unrelated
instances and paths (`NioSocketImpl.readLock` on the HTTP/2 client read,
`LinkedBlockingQueue.takeLock` under `TaskQueue.take` on a worker). Gated on
young relocation, and Generational-specific rather than moving-young-generic:
G1 relocates young objects and never exhibited it.

**Instruments that make this findable again**, because the default symptom names
the wrong object: JUnit sees only a messageless NPE at `TestInput.fill:1094`,
which reads as a zeroed `private final InputStream` and is not.
`CRATONVM_DBG_NPE_NONE=1` shows the raise is Rust-side with `fill` as the
DEEPEST Java frame; `CRATONVM_DBG_STTRACE=1` recovers the compiled frames that
already left the stack, whose deepest is `ReentrantLock.unlock`; and
`ReentrantLock.unlock()` is `sync.release(1)`. Without the STTRACE snapshot the
six JDK frames holding the answer are invisible.

#### Resolved by normalising on relocation exposure

The masking caveat above was the right question and the wrong answer. Current
dev *does* relocate less — but that does not account for the green, and the way
to show it is to stop counting runs and count the gated event.

The defect fires only on a MOVING young collection, so failures per moving
cycle is the rate that means anything. `CRATONVM_GC_STATS=1` in both arms,
interleaved, 3 rounds, 8 classes:

| arm | runs | non-OK | moving cycles | failures per 100 moving cycles |
|---|---:|---:|---:|---:|
| the binary that showed the defect | 24 | **5** | 73 | **6.85** |
| current `dev` | 24 | **0** | 44 | **0.00** |

Current dev relocates at 0.60x the old binary's rate over identical runs — the
masking effect is REAL and is why the raw run counts could not settle this. But
pooling every measured current-dev moving cycle from this and the amplifier run
gives **0 failures in 137 moving cycles**, where the old binary's rate predicts
**9.4**. P(observing zero | rate unchanged) ≈ **8.4e-5**.

So the improvement is not explained by reduced relocation. Both things are true:
dev relocates less, AND its failure rate per relocation is genuinely lower.

**Still not a root cause, and still no commit credited.** The mechanism was
never found; dev took many GC commits in the window, including a moving-young
root cause that landed and was WITHDRAWN. This says the defect no longer fires
at a measurable rate per unit of the exposure it needs — nothing about why.

**The amplifier does not work on this workload**, which is worth recording so
nobody re-runs it: `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` is the netty page's lever
for forcing relocation (1 -> 14-24 cycles there), and here it measured 43 moving
cycles against a plain arm's 50. Exposure on these classes cannot be forced
level, only measured and divided out.
