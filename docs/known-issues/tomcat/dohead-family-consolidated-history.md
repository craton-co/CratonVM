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
| (unresolved) | `dohead1023-http2-index0-socketexception-likely-host-contention.md` | One single sub-test, `TestHttpServletDoHeadInvalidWrite1023ValidWrite1023`'s `testDoHeadHttp2[0]`: a `SocketException: Connection aborted (WSAECONNABORTED)` on the very first HTTP/2 preface read. Never confirmed as a CratonVM bug or refuted — best guess is Windows scheduler starvation under heavy shared-host load. **Left open, low-confidence, not chased further.** A sibling failure in the same original log (`testDoHeadHttp2[25]`, a `Thread.setPriority()` NPE) *was* a confirmed bug and was fixed separately (`populate_real_thread_holder`, `3e7f4a30`/`34faddd9`) — not the subject of this doc. | — |

**Net effect of Part 1:** by 2026-07-23, the DoHead family was passing 64/64 clean, both JIT and no-JIT, 3,509 native-lib tests green. Every bug above is closed. None of them explains what's described in Part 2.

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

- **No action needed on the correctness front.** Part 1's eight bug classes are all closed and don't need revisiting.
- **The Generational-only throughput collapse is understood, not mysterious, and not new.** It's the same `[moving-young] fallback` mechanism already root-caused architecturally (`moving-young-corruption-rootcause.md`) and already observed at exactly this scale on 08-10. This page adds: it survives a 3000s budget too, and it is cleanly GC-specific (G1 and ZGC both pass the same classes without triggering the mechanism at all).
- **This is already mitigated at the product level**: ZGC has been the shipped default since 2026-08-10 specifically because it cannot exhibit this class of fallback. Anyone hitting this is doing so under an explicit `-XX:+UseGenerationalGC`.
- **A real fix, if ever undertaken, is the same one named throughout Part 2**: precise oop maps or a shadow stack for compiled frames, so `moving_young_coverage_complete()` can actually certify what it currently has to assume. That is a substantially larger undertaking than anything in Part 1, and nothing in this investigation's history suggests a smaller intervention (timeout increases, isolated fallback-reason fixes like `innermost_frame_method`) will clear the DoHead family specifically — the Spring Boot investigation already found that two of its four classes needed a collector switch, not a fix, for exactly this reason.
- **Not chased further here**: per-fallback-reason attribution specific to the DoHead family (which of the seven reasons dominates *this* class shape, the way the Spring Boot doc did for its four classes) would be the natural next step if someone wants to reduce fallback volume rather than switch collectors — not attempted in this session.

### 2026-09-06 (later the same day): the residual is MASKED, not shown fixed

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

`docs/known-issues/netty/bytebuf-multiplethreads-npe-generational-moving-young-20260906.md`
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
