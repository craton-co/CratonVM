# The three gcj j2/jt2 barrier switches: what stands between them and a default flip

> **STATUS (2026-10-19, gcj j2/jt2): OPEN -- the switches are built, opt-in, unrun (no-build rule).** Generational only. Switches: `CRATONVM_JIT_POST_ONLY_CARD_MARK`, `CRATONVM_JIT_CARD_BYTE_MAP_BASE`, `CRATONVM_JIT_IR_FRESH_STORE_NO_BARRIER` (`docs/gc-tuning.md`, "JIT <-> GC barrier switches"). Rows: `docs/internal/gc-round-gcj-20261019/j2-jt2-report.md` blocks P, S, B, F.

## What closes it

1. **Rows.** Every row of the report's blocks P (perf, interleaved CPU A/B), S (soundness) and F
   (fresh store) on the Windows build, and the Linux take-over arm once a Linux host is available:
   the Linux half of the completion (`TakenOver::note_frozen_registers` from the parked slot's
   `regs`) is written by reading only, and Windows is the only platform the round runs.
2. **Engagement before verdicts.** `gen_jit_card_post_only_sites=` must be non-zero on every `_po`
   row (the switch reached a compile), and `gen_jit_card_post_only_completed=` non-zero on the
   multi-threaded S rows (`Gcj2Jt2PostOnlyStoreProbe 4 60 4096 8`); a zero there with
   `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` showing take-overs means the completion never saw an old register word,
   and the row proves nothing about the window.
3. **The window is rare by construction.** A take-over must freeze a peer between a store and its
   card byte, and the pause must have cleaned that card, for a missing completion to lose an edge.
   A green S row is evidence, not proof. A deterministic test needs a freeze at a chosen pc: an
   executed unit test that single-steps a compiled store site (the shape the PRE/POST pair's own
   proof used, `gci_i23lc_the_lean_post_keeps_every_card_and_spares_the_second_check`, simulates the
   pause between halves), run against the completion instead -- not written.
4. **`CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD`.** The post-only switch refuses itself beside it
   (`x64::objects::jit_post_only_card_mark_enabled`): a peer frozen after an element store but before
   the element address is re-formed holds only the array header in a register, and that scan trusts
   element cards over a dirty header card. Flipping both needs the completion to dirty the element
   card too (the index is in RCX on both tiers at that point; the completion would have to decode the
   site), or the element address kept in a register through the window.
5. **The optimizing tier's PRE trace.** `CRATONVM_DBG_IR_REF_STORE_TRACE=1` counts the PRE card
   compare (`JitCardView::trace[2..4]`); a post-only arm has none, so `gen_jit_card_ir_dirty/clean`
   read zero on `_po` rows. Not a defect; read the post-only counters instead.
6. **Fresh store.** The skipped gate is the SATB one only; the remaining young test is HotSpot's
   `ReduceInitialCardMarks` minus the card. The full form needs a deferred card mark on every
   old-returning JIT allocation and a completion that covers a fresh object held only in a stack slot
   (`gcj-j1jt-proposal-stores-into-a-fresh-allocation-skip-the-post-barrier-20261019.md`, STATUS).
