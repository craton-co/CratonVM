# Proposal: a blocked thread's young-pin deposit stands until it wakes

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 33
> of 54).** Not built. The take-over arm it would make cheaper now engages: d7
> takeover_1 (`CRATONVM_GEN_PINNED_YOUNG_COPY=1
> CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER=1 Gcd1PinnedTakeoverYoungProbe`)
> `ptko_candidates=39 ptko_taken=39`, control takeover_ctl_1 0/0. Still
> opt-in-only value. **Gate:** the page's sizing run (helper-window band lines
> and `cjdiv_ledger_words_sum`, take-over arm on vs off) before building;
> after, one deposit per blocking episode and no more pinned pages. **Size:**
> M.

*Filed 2026-09-27 by gcd d3/m (lane young3). A direction for triage, not a
defect.*

- **Status:** PROPOSAL. *gcd d4/m (2026-09-28): the over-pin risk it names
  is bounded without it, and was not narrowed further.*
  - A band that would pin more than 1/8 of from-space's pages makes the
    pinned copy decline to the sweep, i.e. the flag-off behaviour. So does
    one that overflows `PEER_BAND_PIN_WORDS_CAP`.
  - The cost is therefore at most a lost pinned cycle, or one cycle's
    retention of the objects on the pinned pages.
  - Narrowing the band itself without a standing deposit was examined and
    rejected:
    - dropping the words the helper-window probe does not resolve loses
      the base-minus-offset derived pointers `PIN_WORD_SLACK_ABOVE` exists
      for;
    - dropping the idle-semispace words saves ledger space but no pinned
      page;
    - dropping the frames above the outermost JIT entry is what gen
      r5w2/roots6 showed to be unsound.
- **Backend:** Generational (the young pin ledger is its only consumer).
- **Owners:** `vm/src/jit/conservative_roots.rs` (the deposit),
  `vm/src/vm/vm_exec.rs` (`deposit_root_snapshot_inner`),
  `gc/src/gc_quiescence.rs` (the ledger), `vm/src/jit/xt_root_scan.rs`
  (the pass that reads bands today).

## Where things stand after gcd d3/m

A blocked thread with compiled frames on its stack enters a pause's young pin
ledger only as a BAND. The helper-window pass reads it from outside, once per
pause, and it is recorded as `YoungPinLedger::note_peer_band`:

- helper windows, under `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`;
- blocked `monitorenter` peers credited by proof, under
  `CRATONVM_XT_BLOCKED_MONITOR_PROOF` with the pinned copy or option B.

Each read costs a suspend (Windows) or a signal round-trip into a helper
slot (Linux), per blocked peer, per pause. The band is the whole stack
`[rsp, top)`, so it also pins every stale young word that deep Rust frames
left behind. That is over-pinning, and the pinned copy's 1/8 page bound
can decline the pause for it.

## Proposed change

Have the blocked thread deposit for ITSELF, once, at its flag-raising
deposit (`deposit_root_snapshot_inner(raise_blocked_flag = true)`), the way a
parked peer deposits at its park (`deposit_pause_young_pin_words`). The
deposit would be marked STANDING: valid for every pause that opens while
the thread stays in its blocked region, and withdrawn by its wake
(`leave_blocked_region*`), which cannot run during a pause.

- **Words:** exactly what a parked peer's deposit holds — its registers,
  the young words of its JIT band that no oop-map slot or shadow home names,
  and the layout-free (Rust) part of the band. That is not the whole stack,
  so it pins far less than a band read from outside.
- **Validity across pauses:** a standing word stays valid for as long as
  every pause that could move its object either pinned it or did not move.
  Each such pause is a pinned in-place cycle, which read the standing slot
  and pinned the word, or a sweep. A Cheney cycle cannot run over a standing
  deposit that holds a young word, because option B's `words == 0` is the
  only way past term 4 without the pinned copy.
- **Ledger:** `YoungPinSlot` gains a `standing` bit; `YoungPinLedger::read`
  counts a standing slot whatever its stamp, while the bit is set.
- **The pass:** no longer reads a credited blocked-monitor peer's band. A
  helper window still needs its band, because an unproven compiled frame
  has no map to screen by.

## What it would buy

- One deposit per blocking episode instead of one signal per pause per
  blocked peer. That matters on a Linux host with thousands of parked pool
  workers (`common-c-linux-takeover-signals-every-thread-FIXED-20260929`).
- Fewer pinned pages. This is the lever for `GenR4W4EvacThroughputProbe`
  (evac7 page) if its helper-window cycles turn out to decline on the page
  bound under the take-over arm.

## Risks to settle first

- The withdraw must happen before the thread can touch the heap after
  waking. `leave_blocked_region_flagged` is that point today.
- The blocking thread may be inside a JNI native (raw local refs). The
  proof arm is already never set with a JNI local frame open; a standing
  deposit needs the same rule.
- A thread that blocks, is woken spuriously and re-blocks without a new
  flag-raising deposit keeps its old standing words. Check every re-block
  path for a deposit.

## How to size it before building

With the current code, count the pauses that read a band and the band sizes:
`CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` prints the helper-window lines, and the
option-B census's `cjdiv_ledger_words_sum` / `_native_sum` rises by the band
words. Compare a helper-window-heavy run
(`GenR4W6JitOomRootProbe`'s `oome-thread-exit`,
`GenR4W4EvacThroughputProbe`) with the take-over arm on and off.
