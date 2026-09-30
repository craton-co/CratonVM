# Proposal: the Linux take-over should signal every candidate first, then collect the answers

> **STATUS (2026-09-29, gce e1/t): PROPOSAL -- a time-to-safepoint (TTSP) direction for the Linux take-over, not a defect. Filed by reading. Needs a measurement before it is built.**

- **Status:** PROPOSAL (gce e1/t). Not a work item until the orchestrator
  triages it.
- **Area:** `vm/src/jit/xt_root_scan.rs`, Linux `imp::take_over_pass`,
  `imp::scan_slot`, `imp::readable_regions`, and the Linux
  `helper_window_pass`.
- **Collectors:** all three. Linux x86-64 only.

## What the pass does today

`take_over_pass` walks its candidates (the roster by default) one at a time:

```
for tid in candidates {
    arm_slot(tid); send_takeover_signal(tid);
    answer = wait_for_response_retrying(slot, tid);   // waits for THIS peer
    if PARKED { scan_slot(slot, ..) }                   // reads /proc/self/maps, then the band
}
```

So a pass costs the sum, over its candidates, of three things:

1. the time for the signal to be delivered;
2. the time for the peer to be scheduled and answer;
3. for each peer that parks, a fresh `/proc/self/maps` read and parse.
   `scan_slot` calls `readable_regions()` once per parked peer, by design:
   "This snapshot must be taken PER PARKED PEER, never hoisted".

Every candidate is signalled, not only the ones in compiled code: a thread
parked at the barrier or in the interpreter still answers `STATE_NOT_JIT`
after it is scheduled. `stw_take_over_and_wait` runs a pass on every barrier
round of the first 20 (`stw_takeover_should_scan`). So with R roster threads,
a pause pays up to about `20 * R` serial scheduler round-trips, plus one maps
parse per frozen peer per pass. A JVM process has thousands of mappings (the
code cache, arenas, each thread's stack), so each parse is a noticeable cost.

The Windows arm is also serial, but a `SuspendThread` / `GetThreadContext`
pair costs about 15 us and needs no scheduling of the target.

## Proposed shape

1. **Arm and signal every candidate, then wait for all of them against one
   deadline.** A slot is already per-tid state. The handler already copes
   with redundant deliveries and late arrivals (CAS-guarded
   `ARMED -> PARKED`), and the retry loop can re-signal every slot still
   `ARMED` each round. The pass then costs about one round-trip, not R.
   The 512-slot table bounds how many can be in flight; past that, fall back
   to batches.
2. **Read `/proc/self/maps` once per pass, after every answer is in.** Then
   scan each parked slot against that one snapshot. The per-peer rule exists
   because the walk used to dereference memory that the snapshot said was
   readable. Since gen r5w1 the band is read through `read_self_memory`, a
   kernel-mediated read that fails cleanly on an unmapped page, and it stops
   at the peer's published stack top (`peer_band_bound`). Two things must be
   checked before hoisting:
   - that the snapshot is still needed only for the band's END;
   - that every short read is already counted as `stack_incomplete`
     (`note_takeover_stack_incomplete`).

   If both hold, a stale snapshot costs at most an incomplete verdict, never a
   fault.
3. **Keep the order that matters.** Answers must still be judged per slot
   (`ROSTER HOLE`, `JIT-ONLY SIGNAL MISS`, unclassified). The pass's
   `ACTIVE_SESSIONS` count must still cover every signal it sends.

This combines well with the gce e1/t yield in the handler
(`gce-e1t-linux-parked-takeover-peers-spin-a-core-each-FIXED-20260929.md`).
Parallel signalling makes many peers park at once, and without the yield
they would all spin at the same time.

## Why not now

There is no measurement of the serial pass's share of TTSP yet.
`CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` prints one `linux pass:` line per pass but
no pass duration. The per-pause take-over time is in the ledger
(`gc_quiescence::xt_pass_ns`), but it is not split per peer.

## How to measure first

On the Linux host, run `GenR4W5ConcMarkJitGateProbe` (8 threads in compiled
loops) and `MtChurnProbe 8 40 64` with `CRATONVM_GC_STATS=1`. Read `xt_pass_ns`
and the TTSP from the `[GC]` exit lines.

Then add one line to the `linux pass:` debug print: the pass's elapsed time,
and the part of it spent in `readable_regions`. If the pass takes more than
about 20 % of TTSP, this proposal is worth building. The expected result
after building it: TTSP down roughly in proportion to the roster size, with
no new `unclassified_peers` and no new `ROSTER HOLE`.
