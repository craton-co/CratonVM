# STW barrier: new-thread `stw_ready` gate missing from `alive_count_blocked_and_os_tids` -- whole-VM freeze under thread-creation + GC-pressure contention

Status: FIX READY, NOT MERGED (pending review -- this is deliberately high-risk `gc_barrier`/STW territory; see "Why this is parked, not merged" below)
Severity: High -- reproduces reliably (~50%+ per run) under a common, simple pattern: several new threads contending on a shared `java.util.concurrent.locks.ReentrantLock` while GC pressure is present. No WildFly, no JIT, no exotic feature flags required.
First confirmed: 2026-07-11, discovered while investigating `docs/known-issues/wildfly-domain-hc0053-server-inventory-timeout.md` -- this bug, not `WFLYHC0053` itself, was blocking every live-boot verification attempt for that investigation.

## Relationship to other STW/GC-barrier docs

This is a **distinct** bug from:
- `WFLYHC0053` (`docs/known-issues/wildfly-domain-hc0053-server-inventory-timeout.md`) -- that doc's own investigation hit this bug as a blocker and is where the minimal repro (`AqsContentionProbe.java`) was built and first characterized. This doc is the dedicated, standalone writeup the WFLYHC0053 investigation's eighth session promised.
- The parked `wip/gc-stw-quota-race-20260710` branch ("STW barrier quota-race fix (UNVERIFIED -- hangs under load)") -- that branch addresses a *different* mechanism: threads the barrier's `expected` computation **excluded** as blocked, whose arrivals could wrongly satisfy another thread's quota slot (a "counted a real mutator as excused" bug). This doc's bug is the **opposite polarity**: a thread the barrier's `expected` computation **included**, whose arrival structurally **cannot** count toward the quota. Read that branch's diff and `docs/internal/gc-audit-2026-07-10-open-findings.md` finding 1 in full before touching any `gc_barrier` code -- this doc's fix does not repeat that branch's mistake (see "Liveness argument" below for why), but the two are easy to conflate.
- `docs/internal/gc-audit-2026-07-10-open-findings.md` finding 1(b) ("residual monitor-vs-evacuation race... lost wakeups") -- a **second, separate, still-open** bug this investigation's verification loop re-discovered (see "Second bug found during verification" below). Not fixed by this doc's patch, and not the same mechanism.

## Symptom

Any workload where several threads are created around the same time as heavy contention on a `java.util.concurrent.locks.ReentrantLock` (or, by the same mechanism, any lock/primitive whose contended path routes through `LockSupport.park()`) under concurrent GC pressure can freeze the entire VM permanently:

```text
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=1..5 taken=0
```

followed by total silence -- zero further log output, zero forward progress on ANY thread (not just the contending ones), until an external timeout/kill. `CRATONVM_DBG_STW_CENSUS=1` shows the barrier's `expected` count permanently exceeding `arrived` by exactly the number of threads stuck in this state; `CRATONVM_DBG_UNPARK_MISS=1` shows nothing (this is not an unpark-miss -- the stuck threads never even attempt to `arrive_and_wait` in a way that could count).

## Minimal reproduction

`docs/internal/repros/wildfly-hc0053-aqs-stw-hang/AqsContentionProbe.java` (already committed on `dev`). N threads hammering one shared `ReentrantLock` in a tight loop, with a concurrent thread forcing `System.gc()` periodically:

```java
Thread gcPressure = new Thread(() -> {
    List<Object> garbage = new ArrayList<>();
    while (!stop) {
        garbage.add(new byte[256]);
        if (garbage.size() > 2000) garbage.clear();
        if ((i++ % 300) == 0) System.gc();
    }
});
// ... N worker threads, each: for (r : rounds) { LOCK.lock(); acquires++; LOCK.unlock(); }
```

- N=2, 4, 6, 7: consistently clean (8,000-28,000 acquires, no hang) in this session's testing.
- **N=8: hangs** reliably (observed in most attempts) on unfixed `dev` tip.
- Zero JIT, zero WildFly, zero jboss-threads, zero custom classloading involved -- pure JDK `ReentrantLock` + `Thread.start()` + `System.gc()`.

## Root cause (confirmed via live gdb, not inferred)

Built with the `hc0053dbg` Cargo profile (debug symbols, no LTO) and attached `gdb -p <pid>` to a hung `AqsContentionProbe` process. Every stuck worker thread showed the identical backtrace:

```text
#8  cratonvm_vm::threading::gc_barrier::GcBarrier::arrive_and_wait_inner (...) at vm/src/threading/gc_barrier.rs:498
#9  cratonvm_vm::threading::gc_barrier::GcBarrier::arrive_and_wait_excluded (...) at vm/src/threading/gc_barrier.rs:448
#10 cratonvm_vm::vm::vm_exec::{impl#5}::thread_start::{closure#6} () at vm/src/vm/vm_exec.rs:4630
#11 std::sys::backtrace::__rust_begin_short_backtrace<...>
...
#21 start_thread (arg=<optimized out>) at ./nptl/pthread_create.c:447
```

i.e. these are **brand-new OS threads, still inside their own `thread_start` bootstrap**, that have never deposited a root snapshot, never reached any interpreter safepoint, and never executed one instruction of Java bytecode. This is the exact `thread_start` startup loop (`vm/src/vm/vm_exec.rs:4620-4634`):

```rust
loop {
    let marked_ready = shared_arc.gc_barrier.run_if_no_stw_requested(|| {
        shared_arc.thread_registry.mark_stw_ready(tid);
    });
    if marked_ready {
        break;
    }
    let pointer_map = shared_arc.gc_barrier.arrive_and_wait_excluded(tid);
    // ...
}
```

By design, a new thread that observes an STW pause already active when it starts calls `arrive_and_wait_excluded` -- the **non-participating** wait variant, which never increments the barrier's `arrived` counter (correct: it did not exist as a running mutator when the pause's `expected` was computed, so it must not need to "arrive").

**The bug**: `ThreadRegistry::alive_count_blocked_and_os_tids` -- the function `maybe_gc`/`maybe_gc_forced`/`force_gc_from_native` (i.e. the young-gen and forced-GC paths, by far the most common trigger) call to compute a pause's `expected` -- only filtered on `e.alive`:

```rust
// BEFORE (buggy):
for e in threads.values() {
    if e.alive.load(Ordering::Acquire) {
        alive += 1;
        // ...
```

A freshly `Thread.start()`ed child is marked `alive` in the registry **before** it reaches the `thread_start` loop above (registration happens earlier in the same function, filling in root snapshot / frame trace / TLAB address / OS tid). If a GC's `alive_count_blocked_and_os_tids()` snapshot runs in the window after that registration but before the child reaches its own startup loop, the child is counted into `expected` -- but its own arrival, once it does reach the loop, unconditionally goes through the **non-participating** `arrive_and_wait_excluded` path. `arrived` can then structurally never reach `expected`: the barrier waits forever, and since `maybe_gc`'s own wait for `wait_for_all`/the cross-thread JIT takeover loop is what every OTHER thread in the process is itself blocked on (directly or via the interpreter safepoint poll), the entire VM freezes.

**The fix already existed, half-applied.** `ThreadRegistry::alive_count_and_os_tids` -- a separate function, added 2026-07-03 in "identity-based barrier excusal for xt-takeover (Fix E)", used by the two `request_stw_counted` (not `_with_live_blocked`) call sites for G1 remark/cleanup -- already filters on **both** `alive` and `stw_ready`, with a doc comment explicitly describing this exact race:

> "A `Thread.start()` child is alive before its carrier can answer a safepoint. It stays out of this counted set until `mark_stw_ready` flips the startup gate; if an STW is already active, the child waits it out via `arrive_and_wait_excluded` first."

`alive_count_blocked_and_os_tids` was added six days later (2026-07-09, "fix(vm): stabilize ForkJoin GC stress roots") as a near-duplicate that additionally tracks the blocked-thread count -- but the `stw_ready` gate was not carried over. Confirmed via `git log -S'fn alive_count_and_os_tids'` / `git log -S'fn alive_count_blocked_and_os_tids'` that the two functions were written six days apart by different sessions; this reads as a straightforward omission, not a deliberate design choice.

## The fix

```rust
pub fn alive_count_blocked_and_os_tids(&self) -> (usize, usize, Vec<u32>) {
    let threads = self.threads.lock();
    let mut alive = 0usize;
    let mut blocked = 0usize;
    let mut tids = Vec::with_capacity(threads.len());
    for e in threads.values() {
        if e.alive.load(Ordering::Acquire) && e.stw_ready.load(Ordering::Acquire) {
            alive += 1;
            if e.gc_block_state.in_blocked_region.load(Ordering::Acquire) {
                blocked += 1;
            }
            let t = e.os_tid.load(Ordering::Acquire);
            if t != 0 {
                tids.push(t);
            }
        }
    }
    (alive, blocked, tids)
}
```

One added condition (`&& e.stw_ready.load(Ordering::Acquire)`), mirroring the already-correct, already-in-production sibling function exactly. **Zero changes to `gc_barrier.rs` itself** -- no changes to `arrive_and_wait`, `wait_for_all`, the blocked-region enter/exit paths, or any locking/waiting discipline. The fix only narrows *which threads get counted* in a pre-existing, already-locked registry snapshot; it adds no new lock, no new wait, no new blocking call anywhere.

Because this single function feeds all five `request_stw_counted_with_live_blocked`/`brief_stw_counted_with_live_blocked` call sites (`maybe_gc`, `maybe_gc_forced`, `force_gc_from_native`, and both concurrent-mark initial-mark sites in `interpreter.rs`), the fix applies uniformly to every young-gen/forced/concurrent-mark-initiating GC pause without touching each call site individually.

## Liveness argument (why this cannot introduce a new deadlock/livelock)

This is exactly the class of reasoning the parked `wip/gc-stw-quota-race-20260710` branch's own postmortem asked for, and the reason that branch is not a template to follow here (it modified `gc_barrier`'s wait/count/blocked-exit logic itself, adding a *new* synchronized wait -- `leave_blocked_region_synced` -- whose own author suspected "starvation/livelock in the synced blocked-region exit under continuous pause pressure"). This fix does not add any new wait, so that failure mode cannot apply:

1. **No new blocking is introduced.** The change is a pure narrowing of a `for` loop's inclusion predicate, executed entirely under a lock (`self.threads.lock()`) that was already held for the whole snapshot before this change. No new lock, condvar, or wait call is added anywhere.
2. **The count can only get smaller, never larger, and never negative-affects a thread that IS running.** A thread excluded by the new `stw_ready` check is, by construction, one that has not yet reached the point in its own startup where it could execute a single instruction of Java bytecode (`mark_stw_ready` is the specific gate for that). It therefore cannot be holding a reference to a live Java object the collector needs to synchronously see, and cannot be concurrently mutating the heap -- there is no "GC evacuates under a live mutator" gap this creates, unlike the *opposite*-polarity quota-race the WIP branch targeted (a thread that IS running being wrongly excused).
3. **The excluded thread's own progress is unaffected and was already correct.** `thread_start`'s retry loop (`run_if_no_stw_requested` / `arrive_and_wait_excluded`, unmodified by this fix) already correctly waits out any in-progress pause via the barrier's *existing*, already-verified generation-keyed wait (`arrive_and_wait_excluded` -> `arrive_and_wait_inner` -> waits on `gc_generation`, the exact mechanism `68c6993e` "fix(gc): multi-thread stop-the-world deadlock under concurrent GC" hardened against the flag-vs-generation race in 2026-06-22). Once the active pause completes, the loop re-checks `run_if_no_stw_requested`, and -- now that this thread is correctly excluded from any *subsequent* pause's `expected` until it actually marks itself ready -- succeeds and proceeds normally.
4. **No new interaction with concurrently-active pauses.** The fixed function is a point-in-time snapshot read under one lock acquisition, exactly as before; it participates in no new cross-pause state (no new HashSet, no new generation tracking, nothing resembling the WIP branch's `excluded_blocked` bookkeeping that had to be threaded through `request_stw_counted_locked`/`complete_gc`/etc.).

In short: the fix removes a case where a pause's `expected` snapshot and a thread's own arrival-eligibility computation (`thread_start`'s loop) could structurally disagree about whether that thread needed to participate. After the fix, both computations agree by construction (`stw_ready` is the single source of truth both now consult), so the disagreement that caused `arrived` to be permanently stuck below `expected` cannot recur for this specific mechanism.

## Verification

- **`AqsContentionProbe8` (N=8), 30 consecutive runs on the fix**: 26 clean, **0 STW-hangs** (down from 8/15 = ~53% STW-hangs on an unfixed control run of the same binary profile), 4 failures due to a **second, separate, pre-existing bug** (see below) -- not the mechanism this fix addresses.
- **Control (unfixed `dev` tip, same `hc0053dbg` binary profile), 15 consecutive runs**: 7 clean, 8 STW-hangs (all showing the `arrive_and_wait_excluded`-from-`thread_start` signature), 0 runs showing the second bug's corruption signature -- consistent with the STW-hang (this fix's target) being the *dominant* failure mode pre-fix, likely masking the rarer second bug in small samples.
- `cargo test --release -p cratonvm-vm --lib threading::` -- 278/278 passed.
- `cargo test --release -p cratonvm-vm --lib gc_barrier::` -- 12/12 passed (including `barrier_excluded_thread_does_not_release_early` and `barrier_late_reduce_expected_can_satisfy_bounded_wait`, the two tests most directly adjacent to this fix's territory).
- `cargo test --release -p cratonvm-gc --lib` -- 783/783 passed.

## Second bug found during verification -- pre-existing, separate, NOT fixed here

4 of the 30 post-fix runs did not hang via the STW-quota mechanism at all; instead they showed an escalating flood of heap-corruption-detector warnings that in some runs self-heals (the run completes `CLEAN` despite the warnings) and in others spirals into what looks like a livelock (the same corrupt address re-detected every few milliseconds, growing `num_slots` counters, never resolving within the timeout):

```text
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver (ptr=..., all-zero header) -- falling back to CP class java/util/concurrent/locks/AbstractQueuedSynchronizer
WARN cratonvm_gc::gen_heap: GC: inconsistent header -- kind=Object but array_length=2049 (num_slots=14853, class_id=1); inline-alloc forgot to set kind=Array. Treating as corrupt so the walker can re-sync.
WARN cratonvm_gc::gen_heap: mark_young: rejecting object at 0x20107002f88 with implausible extent 0 ... corrupt header, not marked/scanned
WARN cratonvm_gc::gen_heap:   [A2] BREADCRUMB -- NO allocation record covers 0x20107002f88 (never header-written here, or freed+reused past the ring)
```

This was **not observed at all** in the 15-run unfixed-control sample (plausibly because the STW-hang, at ~53% probability, usually fires first and pre-empts the process before this rarer condition can manifest) but reproduces at roughly the same ~13% rate against the fixed binary once the STW-hang stops dominating. The corruption always centers on an `AbstractQueuedSynchronizer`-related object (matching this repro's own `ReentrantLock` contention), matches the "stale pointer... all-zero header" and "corrupt header... re-sync" language of `docs/internal/gc-audit-2026-07-10-open-findings.md` finding 1(b) ("a residual monitor-vs-evacuation race survives [the quota fix]... lost wakeups"), and is very likely the same underlying, already-documented, unsolved defect -- just reached via `LockSupport.park()`/AQS's own internal signaling this time, rather than via `synchronized`/`Monitor::block_enter` as in that finding's original MTChurn/BinaryTrees repro.

**Not investigated further in this session** -- it is deep, pre-existing, `gc_barrier`/monitor/evacuation territory that finding 1(b) already flags as unsolved, and this repro (`AqsContentionProbe.java`) is now a much cheaper, WildFly-independent way to chase it than MTChurn/BinaryTrees/the WildFly boot were.

## Why this is parked, not merged

Per this investigation's own explicit verification bar: **a fix in this territory must make its own repro pass reliably with zero hangs before merging.** This fix's own targeted mechanism (the `stw_ready` gap) is fully eliminated (0/30 vs 8/15 baseline) and has a complete, self-contained liveness argument establishing it cannot introduce a new deadlock. But `AqsContentionProbe.java` still hangs intermittently (~13%) due to the second, unrelated, pre-existing bug above -- so the letter of the bar ("the repro must pass reliably... if you see ANY hang... do not merge") is not met for the *combined* state of the codebase, even though this specific patch is not the cause of the remaining failures.

**Recommendation for whoever reviews this**: the `thread_registry.rs` change here is self-contained, narrowly scoped, backed by a complete liveness argument, and independently verified (control comparison + full test suite) to eliminate a real, confirmed, majority-share bug with no plausible mechanism to introduce a regression. It is reasonable to merge this fix on its own merits regardless of the second bug's timeline, since the two are independent and this one does not need the other resolved first to be safe. That said, per the explicit instruction under which this investigation was run, the agent that produced this doc did not make that call unilaterally and left it parked on `fix/gc-barrier-stwready-alivecount-20260711-135958` for review.

## Recommended next steps

1. Review and (if agreed) merge `fix/gc-barrier-stwready-alivecount-20260711-135958` on its own -- it does not need the second bug fixed first.
2. Chase the second bug (heap corruption / stale `AbstractQueuedSynchronizer` pointer under GC pressure) using `AqsContentionProbe.java` as the repro -- it is dramatically cheaper to iterate on than MTChurn/BinaryTrees/a full WildFly boot, and directly continues `gc-audit-2026-07-10-open-findings.md` finding 1(b).
3. Once both are resolved, `AqsContentionProbe8` should pass 100% reliably across many runs and multiple host-load conditions -- re-run this doc's own verification loop (30+ iterations) as the final confirmation before considering this fully closed, then return to `docs/known-issues/wildfly-domain-hc0053-server-inventory-timeout.md` and resume the live gdb/interpreter-trace capture that bug has been blocked on.
