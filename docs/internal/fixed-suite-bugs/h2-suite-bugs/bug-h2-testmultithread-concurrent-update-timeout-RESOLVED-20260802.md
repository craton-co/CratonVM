# `TestMultiThread` — the class died in 3 seconds, then passed; the throughput gap is a separate page now

## Status
**RETIRED 2026-08-02.** Every defect this page named is closed, and the
measurement it was built on is corrected. What remains — a constant factor, and
a thread-scaling question this host turns out not to be able to answer — moved
to
`docs/known-issues/h2/h2-update-path-throughput-20260802.md`, stated as a
throughput number rather than as a bug.

Closed here:

| what | outcome |
| --- | --- |
| the class died in <3 s on dev tip (7 of 8 runs) | **fixed on `fix/h2-testdiskfull-livelock-20260802`** — two IR-tier JIT defects, found independently and concurrently with this branch; see the retired `unresumable-unconditional-trap-mvmap-FIXED-20260802` write-up, which is the authority on them |
| `LOCK_TIMEOUT` after 10 000 ms / `TimeoutException` from `job.get(5, MINUTES)` | **not a defect.** The class passes; H2's own timeouts were tripping on the slowness, in a different place each run, exactly as this page's own re-diagnosis said |
| `CloneNotSupportedException` on `COMMIT`, 2 runs in 10 | **moved** to `bug-h2-classid0-stale-address-family.md`, where its family lives, with the verdict instrument this page asked for now wired into the clone dispatch |
| "the invoke slow path takes a process-wide `ClassManager` read per call" — the named scaling target | **done**, and measured: ~10 % of CPU per update |

## The class runs to completion — and H2's own timeout is still load-sensitive

`org.h2.test.db.TestMultiThread`, `--Xmx 1g`, no flags, on the fixed build.
Every run reaches the end of the class; none dies in three seconds:

| run | wall | CPU (user+sys) | host | rc |
| --- | --- | --- | --- | --- |
| pre-merge binary | 1214 s | 1506 s | load 18-35 | **0** |
| merged binary, 1 | 699 s | 1303 s | load 17-31 | **0** |
| merged binary, 2 | 2097 s | 2722 s (**1662 of it SYS**) | load 57-156, 156 sessions | **1** |
| HotSpot jdk-25 | 6.4 s | 17.3 s | load 18 | 0 |

Run 2's failure is `JdbcSQLTimeoutException: Timeout trying to lock table
"TEST"` at `TestMultiThread$ConcurrentUpdate2.run(TestMultiThread.java:414)` —
H2's own 10 s `LOCK_TIMEOUT`, which is **wall-clock**. It trips whenever the
VM's per-operation cost plus the host's contention pushes one row lock past ten
seconds, and that run's system time exceeding its user time by 60 % says the
host was thrashing, not that the VM misbehaved.

So the honest statement is the one this page's own re-diagnosis made and then
overstated in the other direction: **the class is not deterministically broken
and is not deterministically green.** The timeouts are a consequence of the
throughput gap, they are not a defect with a fix, and they will keep flapping on
a shared host until the gap closes. What IS fixed is the class dying in under
three seconds, which happened in 7 of 8 runs on dev tip regardless of load.

Wall clock is otherwise recorded and not compared: this host ran 15-156 sessions
on 16 cores over this session, and the three rows this page used to carry
(348 / 727 / 784 s) were the same binary at different loads.

## The measurement this page got wrong, and the corrected one

The claim was: *"cratonvm's CPU per update doubles from 4 to 25 threads
(3.6 → 7.8) while HotSpot's falls (0.41 → 0.15)"*, from a 4-thread × 200-update
arm and a 25-thread × 1000-update arm.

**The 4-thread arm could not resolve that.** 4 × 200 is 800 updates — about 3-6
CPU-s of actual work — subtracted from a VM-start-plus-`MERGE`-seed baseline of
~24 CPU-s then, ~40 CPU-s now, whose own run-to-run spread on this host is
±15 CPU-s. The conclusion rested on a signal smaller than the noise in the
number being subtracted from it.

Re-measured with **both shapes doing 10 000 updates**, 0-update baselines
interleaved as ordinary arms rather than taken once up front, arms round-robin.
Four campaigns, loads 8-43, n = 6 runs at 4 threads and 9 at 25:

| shape (10 000 updates) | HotSpot | cratonvm before | cratonvm after | before ÷ HotSpot |
| --- | --- | --- | --- | --- |
| 4 threads × 2500 | 0.17-0.22, med **0.21** | 3.8-9.3, med **6.6** | 5.8 | ~**31x** |
| 25 threads × 400 | 0.15-0.19, med **0.17** | 7.5-10.6, med **9.5** | 8.3 | ~**58x** |

**The 4-thread ratio is ~31x, not 9x** — which puts it back in line with the
retired insert page's "roughly constant 25-30x" instead of contradicting it.
That is the one correction this re-measurement supports outright.

**The slope claim does not survive, and neither does its replacement.** The old
page said CPU per update doubles from 4 to 25 threads (3.6 → 7.8). My own first
answer — 1.5x rather than 2.2x, from a median of three — was equally
overconfident: across four campaigns the per-rep 25t ÷ 4t ratio came out 0.84,
0.98, 1.35, 1.47, 1.56, 1.67, 1.78, 2.27, and the two bands overlap outright.

The reason is structural. At equal total work a 4-thread run takes ~10 minutes
of wall clock and a 25-thread run ~1, so running them back-to-back inside a rep
does not make them see the same load on a host shared with 15-44 sessions.
Pairing removes a level shift; it cannot remove two arms sampling different load
windows. Resolving it needs a 4-thread arm doing ~100 000 updates — a 16:1
work-to-baseline ratio instead of 1.5:1 — on a host under 10 % load. That is
recorded as the open experiment on the successor page rather than guessed at
here.

## The `ClassManager` read: A/B

`invoke_on_class_shared_inner` asked the `ClassManager` for a `class_id`'s name
under one `read()` guard and for its synthetic-stub/interface flags under
another, on every call — both reading the same `Class`.
`parking_lot::RwLock::lock_shared` is a `compare_exchange_weak` on one shared
word, and a CAS that concurrent readers lose falls into `lock_shared_slow`, the
symbol the profile named at 1.47 %. The two guards are now one.

Paired A/B, each pair run back-to-back within a rep so a load excursion hits
both, work term = shape minus that rep's own 0-update run:

| thread count | pairs | median after ÷ before | range |
| --- | --- | --- | --- |
| 4 | 3 | **0.90** | 0.83 - 1.25 |
| 25 | 7 | **0.89** | 0.74 - 1.08 |

**~10 % of CPU per update, and no more than that.** The honest caveat is the
range: a single pair on this host is uninformative, and two of the ten pairs
came out the wrong way. The effect appears at 4 threads as well as 25, which
says it is mostly one fewer atomic RMW per invoke rather than contention relief
alone.

## What is ruled out (carried to the successor page)

* **There is no `org/h2/` JIT package ban to lift.** Measured 2026-08-02 with
  `CRATONVM_DBG_JIT_COMPILED=1`: **27** `org/h2/…` methods JIT-compile on the
  default build, **26** with `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/`. The flag is a
  no-op here, so the retired insert page's "lifting the ban made it ~9 % worse"
  was a **null A/B** and is withdrawn.
* **Not heap pressure** (`--Xmx` 1g/2g/4g/8g: no trend), **not the young-GC
  livelock**, **not the STW cross-thread takeover**
  (`CRATONVM_XT_PEER_DEADLINE_MS` 1/20/200: no effect), **not the JIT-root path**
  (`--nojit` scales identically).
* **`jit_activation`'s global `Mutex` is gone** (per-thread tables since
  2026-07-31).
* **`Math.random()` is not a contention point** — a thread-local `Cell` seed
  (`native-builtins/src/lang_math.rs`), not a shared `Random`. Worth recording
  because `testConcurrentUpdate` calls it twice per update and a shared LCG is
  the obvious suspect.

## The `CloneNotSupportedException` residual, before it moved

Ten runs on `dev` @ `5a18a9db1c`: 8 passes at ~360 s, **2 failures with
`General error: "java.lang.CloneNotSupportedException"` on `COMMIT` in
`testConcurrentUpdate:382`**, at 77 s and 112 s.

What that exception can be on this VM is now pinned down, and it is not a clone
bug. `Object.clone()` is served by `native_object_clone`, which copies fields
and never throws it. It can therefore only come from a `clone()` body that
throws by construction — `java.lang.Enum.clone()` and `java.lang.Thread.clone()`
are the two in the JDK — and reaching either means virtual dispatch was driven
by a receiver whose class is not the one the call site named. Both are
`protected final`, so javac refuses to compile a call to either. H2's frame is
`Page.copy()` → `Page.clone()`, which catches the exception and rethrows it
wrapped, which is why it surfaces as a bare "General error".

That is the stale/reclaimed-`ObjectRef` family, and identical in shape to the
already-fixed `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame`
write-up. `memory::reclaim_guard::report_reclaimed_receiver` is now wired into
that dispatch, so the next occurrence produces a verdict — including after the
allocator has re-served the block, the case where the receiver reads back as a
*valid* object of an unrelated class and the `ClassId(0)` gate misses it
entirely.

## Lessons this page is worth keeping for

1. **Size the shape so the work term dominates the baseline.** The whole
   contention story here rested on an arm whose signal was under a quarter of
   the noise in its own baseline.
2. **Interleave the baseline as an ordinary arm.** Taken once up front, it
   carries that minute's load into every derived number. Four runs of the
   identical 0-update shape, minutes apart, came back 29 / 36 / 43 / 50 CPU-s.
3. **A profile that sums to 39 % is not a bug list.** Removing every named
   cluster is under 2x against a gap of 87x. Framing the gap as "the bug" is
   what kept this page open across three sessions.
4. **An error message names the frame that reported it, not the frame that
   produced it** — and a field filled by `unwrap_or` is not evidence. Both are
   why the JIT regression underneath this class was diagnosed into the wrong
   backend first.

## Related

* `h2-update-path-throughput-20260802.md` — the successor, carrying the residual
  numbers and the next targets.
* the retired `unresumable-unconditional-trap-mvmap-FIXED-20260802` write-up — the two
  IR-tier defects that stopped this class running at all.
* `bug-h2-classid0-stale-address-family.md` — the memory-safety family
  found in this class, and where the `CloneNotSupportedException` residual now
  lives.
* the retired `bug-h2-testmultithread-concurrent-insert-throughput-RESOLVED-20260801`
  write-up — the INSERT half.
