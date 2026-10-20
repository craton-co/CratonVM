# Young pin ledger and JIT band scan: review findings left open by gcj j1/pn

> **STATUS (2026-10-19, gcj j2/pn2): ITEM 1 FIXED IN CODE OPT-IN (`CRATONVM_GEN_BAND_DEAD_SPILL_BOUNDED`, Generational only), PENDING BUILD/PROBE; items 2-4, 6-8 open (re-read, notes below).** Item 1: `band_dead_spill_excuses` (pure) is the excuse; bounded, it covers exactly `layout_is_operand_spill` words, as `spill_slot_is_dead_above_cursor` does. In effect only while `published_young_regions_are_live()` (the Generational table; G1 / ZGC coverage keeps the unbounded excuse), and the switch is read only for a word the two rules disagree on (no latch: the vm statics ratchet). Test `w15_l5_layout_class_tests::gcj2pn2_the_bounded_dead_spill_excuse_stays_inside_the_spill_range`. Flip rows: block BS of `j2-pn2-report.md` (the cost is more refused coverage proofs: `[GC] decision histogram` `moving` vs `nonmoving-*` counts on `GenR4W6JitOomRootProbe` and the gauntlet, def vs on). Item 2, re-read: `JIT_REGION_BOUNDS` is also the JIT's inline young-containment table, so the screen cannot be narrowed there; the fix needs a second published pair (from-space base and its allocation top at the pause) written by `store_region_bounds_locked`'s caller at the pause and read by `YoungPinRange::published` -- not built (it changes what every deposit drops, so it needs its own canary rows). Item 8: unchanged (diagnostic).**

> **STATUS (2026-10-19, gcj j1/pn): OPEN -- found by reading; one soundness gap fixed in code this round (item 0), one perf nit fixed (item 5), the rest open.** Files: `vm/src/jit/conservative_roots.rs`, `gc/src/gc_quiescence.rs` (the ledger). Line numbers: today's tree, branch `gcj1-pn`.

## Fixed this round

0. **A licensing deposit that could not sweep to its stack top read as complete.**
   `young_pin_sweep_hi` falls back to the outermost entry when `own_stack_top()` is 0 (a foreign
   SP; every target but Windows / Linux), and `young_pin_scan_own_chain` committed the depth
   without `mark_incomplete`, so the Rust frames above the entry -- the ones gen r5w2/roots6 showed
   hold raw addresses no channel rewrites -- went unswept while the ledger licensed moves. Same for
   a non-empty chain with no band at all. Fixed: `young_pin_sweep_falls_short` ->
   `dep.mark_incomplete()` (`CRATONVM_GEN_YOUNG_PIN_SHORT_SWEEP_INCOMPLETE`, default ON, `=0`
   kills). Tests `gcj1pn_a_licensing_sweep_short_of_the_stack_top_is_short`,
   `gcj1pn_the_short_sweep_marking_is_default_on_with_a_kill_switch`.
5. `CRATONVM_DBG_JIT_ROOTSCAN` was read through the declared-flag lookup once per published
   unrewritable band word (the `[bandword]` print in the band publish loop); now the latched
   `dbg_jit_rootscan()`. The print may still duplicate `publish_validated_band_word`'s own line;
   check and delete one.

## Open

1. **Coverage soundness risk: the dead-spill excuse in `band_slot_is_verifiable_with_map` has no
   upper bound** (~7437): `off >= layout.spill_lo && off >= hi` excuses every offset at or above
   `max(spill_lo, live_hi)`, past `spill_hi`, including the typed ranges (`licm-ref-hoist`,
   `licm-arith`, `scalar-replaced-field`) and the locals tail when they overlap or follow the spill
   range, and (with `callee_saved_shallow`, AArch64) the outgoing reserve. The sibling
   `spill_slot_is_dead_above_cursor` uses `layout_is_operand_spill`, which excludes all of those and
   is bounded by `spill_hi`. In the moving-young coverage verifier "not verifiable" means "not
   checked for shadow publication", so a live typed-range reference could pass unchecked. Fix:
   replace the arm with `spill_slot_is_dead_above_cursor(layout, off, live_hi)` (or add
   `&& layout_is_operand_spill(layout, off)`). Shared code (G1 / ZGC coverage use it): gate on the
   Generational backend or run the G1 / ZGC rows too. Not changed here: the tightening refuses
   more coverage proofs, and the cost must be measured (`moving-young` decision histogram on
   `GenR4W6JitOomRootProbe` and javac, on vs off).
2. **Over-pin: the ledger's range screens the idle semispace and both regions' whole capacity.**
   `YoungPinRange::published()` (`gc_quiescence.rs` ~7303) takes `JIT_REGION_BOUNDS` words 0..4 =
   `[from base, from base + cap]` and `[to base, to base + cap]`, inclusive of the capacity end.
   At a pause to-space holds no young object (often uncommitted by `uncommit_evacuated_young`), and
   from-space words above the bump top name nothing. Every such stale stack word counts: option B
   needs `read.words == 0`, so one stale to-space word blocks the clear, and the pinned copy runs
   in place instead of as a Cheney copy for nothing (the plan then holds no object and falls back,
   but the decision census and the 1/8 bound are charged). Fix: screen from-space only, against
   `[base - LOW_SLACK, allocation top]`; at least drop the to-space span.
3. **Over-pin: the initiator sweeps the collector's own frames.** With moves licensed the deposit
   inside `collect_roots` sweeps to the stack top, i.e. through `collect_garbage_inner` and the
   frames below the GC entry, which hold arena bases / ends / tops (`base_ptr()`, `base + cap`) --
   all inside the range of item 2. The comment above `young_pin_capture_registers` ("keeps the
   ledger's own young-range bounds out") protects only frames below `lo`. Fix with item 2, plus:
   drop words that equal a published region bound other than from-space's base.
4. **Thread-local movable / unrewritable inserts from a peer's own publish** (`note_movable_band_root`
   / `note_unrewritable_band_root`, ~1686): only the collector reads its own sets; a parking peer's
   `update_root_snapshot` inserts into its own thread-local sets, which nobody reads, and they grow
   until that peer next collects (if the "update_root_snapshot still inserts" comment is accurate;
   trace it). Fix: insert only on the pause's requester.
6. **G1 trusts a deadness claim the ledger refuses.** `publish_validated_band_word` publishes a
   map-dead verifiable word as MOVABLE ("a dead word needs no rewrite"); the ledger, when it
   licenses moves, refuses exactly that claim (gen r5w4/pin8). Document why G1 is exempt or gate
   the arm the same way.
7. **Perf nits.** `YoungPinDeposit::push` dedups by a linear scan of up to 256 cells per word, and
   the sweep now reaches the stack top, so it runs far more often; `note_peer_band` re-sorts the
   whole vector per band (sort the incoming slice and merge).
8. `CRATONVM_DBG_YOUNG_PIN_SWEEP_ENTRY_ONLY` (UNSOUND, diagnostic) does not mark the deposit
   incomplete, so it can clear option B; acceptable for a diagnostic, but say so in its doc.

## Checked and found correct

The deposit seqlock and stamp checks (no stale-pause word is counted); every cap
(`YoungPinDeposit` overflow, `PEER_REG`, `PEER_BAND`) marks the pause incomplete; frame band bounds
in `young_pin_frame_words`; the named-slot excuse matches `remap_one_jit_frame`.
