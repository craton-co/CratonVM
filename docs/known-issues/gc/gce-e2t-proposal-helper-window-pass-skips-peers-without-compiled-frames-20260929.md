# Proposal: the helper-window pass should skip blocked peers that have never entered compiled code

> **STATUS (2026-09-29, gce e2/t): PROPOSAL, backed by a measurement: the helper-window pass costs ~470 us of every pause even when it finds nothing.**

- **Status:** PROPOSAL (gce e2/t).
- **Area:**
  - `vm/src/runtime/interpreter/gc_and_alloc.rs::stw_take_over_and_wait`
    (the `helper_window_pass` call);
  - `vm/src/jit/xt_root_scan.rs` (`helper_window_pass`, both OS arms).
- **Collectors:** all three.

## Measurement

Windows release binary (base 2026-09-27), running `GceE2tTakeoverProbe`
under `--verbose:gc`, 40 pauses per arm:

| Arm | median `xt_post_quota_us` | windows found |
|---|---|---|
| default | 473 | 0 |
| `CRATONVM_XT_HELPER_WINDOW_SCAN=0` | 3 | 0 |

The pass runs whenever `blocked_count() > 0 && any_thread_in_jit()`. In
practice that is every pause, because the Reference Handler, the Finalizer
and similar threads are always blocked. For each blocked peer it suspends
the thread (Windows), or signals it and parks it in the handler (Linux),
copies its used stack, and classifies every word. The measured probe found
no window in any pause.

## Proposal

Before the pass, drop every blocked tid whose published JIT depth proves it
has no compiled frame:

- **`Some(0)`**: the thread entered compiled code at some point but has left
  it.
- **`None`**, while `CRATONVM_XT_PINNED_PEER_DEPTH` is on (the default): the
  thread never pushed a JIT entry, so it never registered a depth slot.
  `conservative_roots::publish_self_jit_depth` registers the slot lazily, at
  the first chain push.

This is the same entry-chain argument as `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY`.
The stake is higher, though: the helper window is the ONLY root source for a
blocked peer's compiled frames. So:

1. Ship it opt-in (`CRATONVM_XT_HELPER_WINDOW_JIT_ONLY`).
2. With `CRATONVM_XT_ROOT_SCAN_AUDIT=1`, still scan the skipped peers, and
   print `[xt-jit-roots] HELPER-WINDOW JIT-ONLY MISS tid=..` for any of them
   whose band held a compiled frame (`has_jit`). This needs a
   per-tid `has_jit` report out of both OS arms' loops. Today they only count.
3. Flip the default after zero misses over the audited battery
   (`GenR4W6JitOomRootProbe`, `VthreadGcStress`, `MtChurnProbe`,
   `GceE2tTakeoverProbe`, on all three collectors).

The `None` case depends on the depth publication being on. If
`CRATONVM_XT_PINNED_PEER_DEPTH=0`, `None` must read "unknown", and the peer
must be scanned.

## Expected gain

About 0.45 ms off every pause that has a blocked system thread, on top of the
first-pass grace (`gce-e2t-takeover-first-pass-dominates-time-to-safepoint-FIXED-20260929.md`).
Together they cover ~1.2 ms of the ~1.25 ms of per-pause take-over overhead
measured above, on a pause that froze nobody.
