# Proposal: say WHY helper windows refuse, then make a blocked compiled peer stop being a window

*Filed 2026-09-28 by gcd d9/c (lane fallback9). Proposal, not a defect: the
fallback-young livelock (`../../internal/gc/gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928.md`)
is fixed by making the trigger and the futile verdict stop assuming a young
cycle can drain; this is the direction that would make the drain itself
available again.*

## Why

On a JIT-warm Generational run a REFUSING helper window costs a pause twice:
the young cycle cannot copy (`XT_HELPER_WINDOW`), and -- through
`gc_quiescence::unrewritable_peer_state` -- the non-moving sweep cannot
promote either, so young cannot drain at all. A peer that blocks for good
under compiled frames (a JDK daemon in `ReferenceQueue.remove`, a pool worker
in `park`) refuses EVERY pause for the rest of the run. That is the
precondition of the livelock above, and of the `[moving-young] fallback ...
xt-helper-window-conservative-scan` streams on the Tomcat and H2 pages.

A window refuses for one of four reasons, and the run records none of them:

1. its machine band is incomplete (`snapshot_parked_slot` /
   `snapshot_peer`: `complete = false`);
2. its shadow window is untrusted (never published, identity mismatch,
   torn `top`: `read_peer_shadow_window_ex` -> `Untrusted`);
3. its shadow window holds INDIRECT entries and the peer published no stack
   band (`BandlessIndirect`);
4. the peer never answered the signal (`STATE_CANCELLED`, Linux) or could not
   be read (`Snapshot::Failed`, Windows).

`[xt-jit-roots] ... complete= shadow_ok=` (under `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1`)
says it per window, but that flag is too loud to leave on for the runs that
fail one time in twelve.

## Step 1: a refusal census (cheap, no behaviour change)

Four per-reason counters beside `XT_HELPER_WINDOWS_REFUSED` in
`vm/src/jit/xt_root_scan.rs` (both OS arms), printed on the `[GC]
xt_peer_scan` shutdown line (`VmHeap::print_gc_summary`, owner of
`gc/src/vm_heap.rs`), plus the count of DISTINCT tids that refused (a
persistent refuser shows up as 1 tid and N refusals). Verify: a unit test per
reason on the pure classifier; one `GenR4W6JitOomRootProbe -Xmx64m` run,
where the line must account for every `xt-helper-window-conservative-scan`
fallback (`sum(reasons) >= fallbacks` on that reason).

## Step 2, by what step 1 finds

- **Reason 2 dominates** (a peer blocked under compiled frames with no
  published shadow window): find the entry door that runs compiled code
  without `set_jit_thread` -> `publish_self_shadow_addr_once`
  (`vm/src/jit/helpers.rs`, the JIT owner) and publish there. Fixes the
  refusal outright.
- **Reason 3 dominates**: publish the stack band at thread start rather than
  at the first JIT entry (`conservative_roots::publish_self_shadow_addr_once`
  publishes it only on the first publish).
- **Reasons 1 and 4**: nothing cheap; they are genuinely unreadable peers.
- **In every case, the larger lever:** d2/i's blocked-monitor PROOF
  (`CRATONVM_XT_BLOCKED_MONITOR_PROOF`), generalised from the compiled
  `monitorenter` helper to every blocking helper a compiled frame calls
  (`Object.wait`, `LockSupport.park`, `Thread.sleep`, blocking natives): the
  blocking deposit proves the JIT chain rewritable at a known call site with
  an oop map, so the peer is credited like a parked mutator and is no window
  at all. Blocked peers then stop gating promotion, and a JIT-warm
  Generational run gets its young drain back on every pause they used to
  refuse.

## Gate

Step 1: default on (a census). Step 2's proof generalisation: opt-in first,
with d2/i's own flip gate (the four rows on
`../../internal/gc/gcd-d1b-thread-exit-shape-livelocks-on-forced-young-cycles-FIXED-20260928.md`)
re-run per new blocking helper, and `sp_selective / sp_sweeps` (the
`[GC_OVERHEAD]` census) rising on `GenR4W6JitOomRootProbe` as the measured
benefit.
