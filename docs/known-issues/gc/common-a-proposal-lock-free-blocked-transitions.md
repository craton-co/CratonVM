# Proposal: take the global barrier mutex off every blocking-native transition

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 37
> of 54).** Not built (no `CRATONVM_STW_LOCKFREE_BLOCKED`). gcd d5/f landed
> the same Dekker shape for leaf windows (`GcBarrier::try_open_leaf_window` /
> `drain_leaf_windows`: store, SeqCst fence, load), which is the precedent
> this design needs. **Gate:** `probes/SharedLine.java` and a 32-thread
> executor profile of `parking_lot::raw_mutex::lock_slow`, then the opt-in
> with `ConcurrencyUnderGcSweep`, `MtChurnProbe`, `FinalizeOnceProbe`.
> **Size:** M.

> **Design refresh, gc-common w6-a (2026-09-24): still accurate, not
> advanced. The request path gained one step, and it composes with this
> design.**
>
> `request_stw_opening_cycle` now takes the process coverage slot
> (`gc_barrier.rs` `COVERAGE_SLOT_OWNER`) BEFORE `inner.lock()`, and
> `complete_gc` releases it under `inner`. Three consequences for this
> design:
>
> * The Dekker reorder still has one request core
>   (`request_stw_counted_locked`). The slot does not move the reorder's
>   window, because the slot is taken before the reset and before the
>   `stw_requested` store.
> * The flagged exit path (`leave_blocked_region_flagged`) and the plain
>   exits never touch the slot, so "exit keeps the lock only when
>   `stw_requested` is seen set" is unaffected.
> * `take_coverage_slot` stops waiting as soon as its own barrier's
>   `stw_requested` is set. A lock-free entry therefore cannot be kept out
>   of its own VM's pause by another VM's pause.
>
> Preconditions unchanged: a default-OFF flag, and the gauntlet named below.

> **Re-verified gc-common w5-a (2026-09-23): still accurate, not advanced.**
> The request core is unchanged (`request_stw_counted_locked`, one production
> entry `request_stw_opening_cycle`), and so are the three `inner`
> acquisitions per park/unpark hand-off.
>
> w5-a moved `gc_generation` off the line those transitions write
> (`PaddedU64`). That removes the one coherence cost the lock itself does
> not cause: JIT memo probes reading the generation. What remains is the
> lock's own serialisation, which is what this proposal addresses.
>
> Still behind the same two preconditions: a default-OFF flag, and a
> gauntlet a no-build lane cannot run.


> **Re-verified gc-common w4-a (2026-09-23): still accurate, not advanced.**
> Every production pause now requests through `request_stw_opening_cycle`
> (the non-collection pauses too, `request_non_collection_pause`), so the
> "flag before census" reorder has exactly one request core to change —
> `request_stw_counted_locked` — and no legacy entry to keep in step. The
> no-build constraint that kept it out of waves 1-3 still holds.

Status: PROPOSAL (filed gc-common round 2026-09-23, wave 1, lane A)
Area: safepoints / STW protocol — performance

> **Re-verified gc-common w3-a (2026-09-23): still accurate, not advanced.**
> The three lock acquisitions per park/unpark hand-off are unchanged
> (`enter_blocked` / `mark_blocked_region_enter`, `BlockedGuard::drop` /
> `mark_blocked_region_leave_after`, `leave_blocked_region_flagged`). Two
> things moved that the design must now respect: `request_stw_opening_cycle`
> (w2-a) runs the census and opens the coverage cycle under the same lock hold
> as `expected`, so "flag before census" has to keep the cycle open strictly
> before the flag store; and `run_collection_pause` now requests the barrier
> even for one thread, so the entry/exit fast paths are also on every
> single-threaded blocking call. Not landed because it reorders the request
> protocol and needs the default-OFF flag plus the gauntlet named below, which
> a no-build lane cannot run; measure `probes/SharedLine.java` first.

## Today

Every blocking operation (`Object.wait`, `LockSupport.park`, `Thread.sleep`,
`Thread.join`, contended `monitorenter`, selector `select`, JNI host-native
regions) goes through `vm/src/threading/gc_barrier.rs`:

- entry: `enter_blocked` / `mark_blocked_region_enter` — `inner.lock()`,
  `threads_blocked.fetch_add`, load `stw_requested`, unlock;
- exit: `BlockedGuard::drop` / `mark_blocked_region_leave_after` —
  `inner.lock()`, `wait_out_pause_locked`, `fetch_sub`, unlock; and for
  flagged regions `leave_blocked_region_flagged` — `inner.lock()` again.

So a park/unpark hand-off costs three acquisitions of ONE process-wide mutex
(also taken by every STW arrival and request). `CacheLineFlag`'s own doc
measured ~29,000 hand-offs/s on a synthetic ping-pong; a thread-pool server
with many workers parking and unparking serialises all of them on that line.

## Why the lock is there

It serialises the blocked transition with the census that computes
`expected` (`request_stw_counted_locked`), so a thread is either excluded and
told `pre_stw = false`, or counted and told `pre_stw = true` — never both. The
production census is the IDENTITY census (`in_blocked_region`), and the
anonymous `threads_blocked` counter is diagnostics-only on that path.

## Proposal

A Dekker-style handshake on the two flags that actually decide the answer:

- mutator entry: `in_blocked_region.store(true, SeqCst)` (already done by the
  deposit) then `pre_stw = stw_requested.load(SeqCst)`;
- initiator: `stw_requested.store(true, SeqCst)` FIRST, then run the census
  (`in_blocked_region.load(SeqCst)` per thread), then publish
  `expected`/`excluded_blocked` under `inner` as today.

With both sides store-then-load under `SeqCst`, at least one side sees the
other: a thread that read `pre_stw = false` is guaranteed to be seen blocked by
the census; a thread that read `true` arrives via `arrive_and_wait_auto`, which
already resolves counted/excluded from the identity set under the lock.
The anonymous counter becomes a relaxed diagnostic. The exit side keeps the
lock only when `stw_requested` is observed set (the rare path).

Risk: this reorders `request_stw_counted_locked` (flag before census), which
every waiter's generation-keyed loop tolerates but which must be re-reviewed
against `run_if_no_stw_requested` (startup) and `finish_after` (termination).
Must ship behind a default-OFF flag (e.g. `CRATONVM_STW_LOCKFREE_BLOCKED=1`)
with `ConcurrencyUnderGcSweep`, `MtChurnProbe`, `FinalizeOnceProbe` gauntlets.

## Measure first

`probes/SharedLine.java` (the ping-pong) plus `perf`/`vsampler` on
`parking_lot::raw_mutex::lock_slow` under a 32-thread executor benchmark.
