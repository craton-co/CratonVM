# ZGC: relocation and uncommit review residuals after zt t1/rl

> **STATUS (2026-10-19, zt t5/pg3): OPEN (the relocation hub).** This page now carries every row-gated relocation / uncommit flip: 1 (`zgc_relocate_refuses_raw_jni_locals`, gc/src/gc_quiescence.rs:4360: RL-6 + RL-12; from zr1rl), 2 (`CRATONVM_ZGC_POINTER_MAP_SORTED`: RL-2; from zr3fp), 3 (`zgc_relocations_ceiling_for`, gc/src/zgc.rs:21400: RL-3; from zr3fp), 4 (`zgc_uncommitter_refused_on_this_platform`, gc/src/zgc.rs:23263: RL-8 on the Windows box, the zr4rv4-footprint battery with it; from zr5rv5 / zr4un), plus item 7 (OnceLock-latched switches) and, carried from `zgc-zt3-items-*` item 1, rows SL9 / R1-R2 (relocations-table size over many cycles, `CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED` / `CRATONVM_ZGC_FORWARD_CHAIN_BY_EPOCH` default vs `=0`). Items 8-10 are now decided or proposals (zs3cr, zs2sl2 retired); item 5 is the G1 / Generational owners' (the `evac_pool` statics, an AGENTS.md process-global violation).

> **STATUS (2026-10-19, zt t3/rl): PARTLY.** Item 1: what the opt-in needs to become default is decided (raw-JNI page, this date): RL-6 plus the JNI cost row; the indirect-locals dispatch no longer clones the ledger `Arc` when the count is kept (vm/src/native/jni.rs `RawLocalsDispatch::enter`, 7de2caa855), and t2/x1 latched the switch (5e574ac27d, reviewed: sound; the vm crate's tests keep their own per-thread override). Items 2-4: unchanged, waiting for their rows. Item 5 unchanged (G1 / generational owners). Item 6: unchanged. Item 7: unchanged. Item 8: zs3cr items 2, 3, 5 decided (see that page); 7 open. Item 10: unchanged (proposal). Rows in docs/internal/zgc-round-zt-20261018/t3-rl-report.md.

> **STATUS (2026-10-19, zt t1/rl): OPEN (found by reading; none reproduced).** What the t1
> relocation lane reviewed and did not close. The fixed items are in
> `docs/internal/zgc-round-zt-20261018/t1-rl-report.md` section 2. Line numbers are against branch
> `zt1-rl`.

## Default decisions this lane built but could not take (no build, no host)

1. **`CRATONVM_ZGC_RELOCATE_REFUSES_RAW_JNI_LOCALS` is opt-in.** The refusal is the ZGC half of
   z2-r2 hunk X2 (`relocate_refusal`, the `RAW_JNI_LOCALS` arm after the retained-TLAB refusal,
   gc/src/zgc.rs). Its population is the `CRATONVM_JNI_INDIRECT_LOCALS=0` arm plus raw escapes, so
   the default arm cannot lose a slide to it except through an escape. What would close it: row
   RL-6 (`Zr2R2JniRawLocal` under `INDIRECT_LOCALS=0`: `upcall ok N/N` with the switch, the known
   FAIL without it), then a default-ON flip with `=0` kill. The flip also turns on the count in
   `raw_jni_locals_matter()` for every dispatch (one ledger `Arc` clone and one TLS bump on an
   indirect-locals dispatch), so it needs the JNI cost row (`Gcd1JniCostProbe`) inside 1.05x.
2. **`CRATONVM_ZGC_POINTER_MAP_SORTED` is opt-in.** A lookup becomes a binary search. The
   consumers named by the proposal look up per monitor, per discovered reference, per root and per
   JNI local. Not audited: whether any VM consumer looks up per HEAP object (an identity-hash or
   tag side table keyed by address). If one does, the CPU row RL-2 shows it.
3. **`CRATONVM_ZGC_RELOCATIONS_CEILING_SCALED` is opt-in.** It only lowers the bound (1 M entries
   at `-Xmx256m`). A clear degrades `forwarded_after_slide` for older moves to the stamped-word
   path. What would close it: RL-3 with `CRATONVM_DBG_VACATED_FRAMES=1` on the H2 / netty suites
   (no new unrepaired stale reads) and `fp_relocations_peak` / maxrss down on `-Xmx256m`.
4. **`CRATONVM_ZGC_UNCOMMITTER_WINDOWS_PAUSE_ONLY` is opt-in.** The fault window it closes has no
   named reader (zr5rv5). What would close it: the Windows stress row RL-8; if it faults, flip ON
   on Windows only.

## Diagnostics

5. **The process-wide `[GC] evac_pool:` census still mixes G1 and the generational heap.** ZGC's
   pool left it (per-heap census, `EvacPool::per_heap`), but `EvacPool::new` / `with_name` still
   feed the statics (gc/src/evac_pool.rs, the statics above `PoolCensus`). Moving those two owners
   to `per_heap` and printing each pool's own census is a G1 / generational change, out of this
   round's scope. AGENTS.md lists these statics as a violation to remove.
6. **The quarantine dead-check walks the whole registry.** `quarantine_stale_slots` is debug-only
   and O(registered slots) per release; a slot holding a primitive the class declares as a
   reference (the W7-84 family) can read as a hit. The positive control is a unit test only: no
   Java-level arm can plant a stale slot.
7. **Several relocation switches are latched in process `OnceLock`s**
   (`zgc_unrewritable_peer_state_refuses`, `zgc_relocations_prune_amortized`,
   `zgc_forward_chain_by_epoch`, gc/src/zgc.rs). A unit test that toggles one of them with
   `with_thread_overrides` sees whatever the first reader latched. Not compatibility state, so not
   an AGENTS.md violation, but a test hazard; the new zt switches read per call.

## Carried forward (unchanged)

8. zs3cr items 2 (unsizable survivor booked at 0 bytes in the evacuation headroom), 3 (in-pause
   zeroing of vacated bytes), 5 (mark / sweep closures not audited for worker-thread flag reads), 7
   (forwarding through the quarantine).
9. zs2sl2 items 2 and 4-6 (host decisions), 8-9 (module debt, selector parity).
10. zr4un gap 1: no uncommitter in decommit mode. Proposal
    `docs/internal/proposals/gc/zgc-zt1rl-proposal-give-back-safepoint-for-decommit-mode-20261019.md`.

## What would close it

Items 1-4: their rows, then a flip or a refusal recorded here. Item 5: a G1 / generational round.
Items 6-7: as written. Items 8-10: their pages.
