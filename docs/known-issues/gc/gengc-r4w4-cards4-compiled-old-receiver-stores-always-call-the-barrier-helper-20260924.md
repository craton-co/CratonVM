# Every compiled reference store into an OLD receiver calls the barrier helper

> **STATUS (2026-09-29, gce ve2): OPEN -- keep off: correctness clean, B-arm gain 1.2 % is inside A's spread, and the post-barrier-needed engagement line is absent on both arms.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/y): OPEN (the flip) -- the gate made decisive.** `GenR4W4CardBarrierBenchProbe` printed `store_ms=` on STDOUT, so no battery row could ever be SAME. Stdout is now only HotSpot's line (`slots=65536 holders=4096 rounds=200 mismatches=0 checksum=456695737600`). The A/B number is one STDERR line, `[store-timing] rounds=200 store_ms=T median_round_us=M steady_median_round_us=S`. The rows are in section 2 of `docs/internal/gc-design-perf-round-20260929/e2-y-report.md`, each with its pass bound:
> - bench A/B x5, plus the single-pass-only arm;
> - StoreForms stress + audit;
> - BinT and HashMapOnly regression;
> - IR engagement, read from `ir ref-store bail post-barrier-needed`.
>
> No other code change was needed. The WildFly audit campaign stays a host item.

> **STATUS (2026-09-29, gce e1/x): KEEP -- the blocker is fixed (unit test passes); the flip gate was not run.** `gce_e1f_a_dirty_card_spares_the_putfield_helper` passes (e1 Windows suite). **Remaining:** flip gate item 1 with the IR tier on (`GenR4W4CardBarrierBenchProbe`, A B A B x5, medians) and the stress / audit runs, then the flip of `CRATONVM_JIT_INLINE_CARD_MARK`.

> **STATUS (2026-09-29, gce e1/f): the d5/u BLOCKER is FIXED IN CODE
> (opt-in, under the page's own flag); the flip itself stays the
> orchestrator's.** Of the two IR gated stores the blocker named, `aastore`
> had been taught the inline card check by JIT round 13 wave 2
> (`emit_ir_aastore_card_arm`, lane irhash); the reference `putfield` had
> not. Now `jit/src/ir_lower.rs` `emit_gated_ir_ref_putfield` runs
> `emit_ir_putfield_card_arm` on its "neither gate ruled the barrier out"
> edge when `ir_inline_card_view()` answers (i.e. `CRATONVM_JIT_INLINE_CARD_MARK=1`
> and `CRATONVM_JIT_IR_INLINE_CARD_CHECK` not `0`, and a generational card view
> is published): a null value or an old value stores inline with no card; a
> young value stores inline only when the receiver's HEADER card (the mark
> the helper dirties for a field store; `mark == holder` as in the
> single-pass `op_field.rs`) is already dirty and the SATB byte is still zero,
> in the object's own shape (compact cell, or the legacy 16-byte cell after a
> `num_slots` bounds test that sends an out-of-range index to the helper),
> then re-checks the card (POST) and takes `jit_putfield_object` if a pause
> cleaned it; everything else is the helper, as before. The soundness
> argument is the `aastore` arm's (compiled code never writes a card byte).
> Flag off (the default): no emitted byte changes -- the legacy-cell
> displacements are now computed once (`IrLegacyRefCell`) and shared by both
> inline stores, which keeps `layout_const_inventory`'s `ir_lower.rs` row
> unchanged. G1 and ZGC publish no gate plan, so this arm is never reached
> there. **Test:** `cargo test -j 5 -p cratonvm-jit --lib gce_e1f_a_dirty_card_spares_the_putfield_helper`
> (executed against a real `CardTable`: clean card -> helper, dirty card ->
> inline, old value -> inline and the card stays clean, null -> inline, armed
> SATB -> helper); the existing `the_gated_arm_*` and `r13w2_irhash_*` tests
> must stay green. **Then:** the page's flip gate, item 1 with the IR tier on
> (it can now measure a loop the IR tier compiles), items 2-4 unchanged.
> `RUST_LOG=cratonvm_jit=debug` shows no IR engagement line for this arm;
> use the `CRATONVM_DBG_IR_REF_STORE_TRACE=1` counters
> (`IR_REF_STORE_INLINE_TAKEN` vs `IR_REF_STORE_BAIL[3]`) to see it engage
> (the card arm's inline stores do not bump `INLINE_TAKEN`; a drop in
> `BAIL[3]` under the flag is the engagement signal).

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- OPEN (perf flip); the d5/u blocker stands.** The optimizing tier's gated stores (`ir_lower.rs::emit_gated_ir_ref_putfield`, `emit_gated_ir_aastore`) still ignore `CRATONVM_JIT_INLINE_CARD_MARK`; no d7 row measured the flag. Remaining: teach the IR tier the inline card mark, then the page's A/B (five criteria) and the JIT WildFly `CRATONVM_DBG_RSET_AUDIT=1` campaign.

> **STATUS (2026-09-28, gcd d5/u): FLIP GATE WRITTEN; one BLOCKER found by
> reading: the optimizing tier never reads the flag. No emitted byte
> changed; nothing flipped.**
>
> - **The blocker.** `CRATONVM_JIT_INLINE_CARD_MARK` is consulted only by the
>   single-pass emitters (`jit/src/x64/objects.rs`
>   `inline_card_mark_available`, its callers in `objects.rs`, `op_field.rs`,
>   `op_array.rs`). The IR tier's two gated stores,
>   `ir_lower.rs::emit_gated_ir_ref_putfield` and `emit_gated_ir_aastore`,
>   send every store whose receiver fails the young-receiver gate to the
>   helper (`jit_putfield_object` / `jit_aastore`) under BOTH arms (their
>   comments said the inline barrier was hard-`false`; corrected this wave).
>   Hot loops -- and, since JIT r11 wave 11, OSR bodies with calls -- are
>   compiled by the IR tier, so gate item 2 below (the `store_ms` gain) can
>   measure nothing on a loop the IR tier owns, and a WildFly campaign
>   exercises the inline barrier only in single-pass bodies.
> - **Flip gate (all must hold, in this order):**
>   1. The A/B and stress items 1-5 of the r5w2 block below, run as written,
>      PLUS item 2 repeated with `CRATONVM_JIT_OSR_OPTIMIZING=0
>      CRATONVM_C2_ACCEPT=never` (so the single-pass body is the one measured;
>      `ir_evidence.rs`), and `RUST_LOG=cratonvm_jit=debug` showing `inline generational
>      card barrier emitted` for that method. If only the IR-off arm shows the
>      gain, the flip buys the single-pass tier only: decide on that basis, or
>      land the IR port first (proposal
>      `gcd-d5u-proposal-ir-tier-inline-card-check-20260928.md`).
>   2. `GenR4W4StoreFormsProbe` under `gc-stress=250000` with
>      `CRATONVM_GC_VERIFY_RSET=1 CRATONVM_DBG_RSET_AUDIT=1`, arm B, on Linux AND
>      Windows: 21 `form=<name> ok`, `forms=21 slots=2048 rounds=30 failures=0
>      checksum=1257738060`, no `[rset-miss]`, 3/3.
>   3. A JIT WildFly boot campaign under `CRATONVM_DBG_RSET_AUDIT=1`, arm B:
>      zero `[rset-miss]` (the July witness).
>   4. `jit/tests/r4w4_cards4_inline_card_barrier.rs` and
>      `cargo test -j 5 -p cratonvm-jit --lib r4w4_cards4` green.
>   Then flip `JitFlags::inline_card_mark` (`types/src/flags.rs`) with a
>   `=0` switch kept, and retire this page.

> **Earlier status (2026-09-26, gen r5w2/alloc6): FIX LANDED (opt-in
> `CRATONVM_JIT_INLINE_CARD_MARK=1`), re-read at `4838666e1` and found
> CORRECT; no emitted byte changed this wave; awaiting the A/B below.**
>
> * **Re-read** (`jit/src/x64/objects.rs`: `gen_card_view_of`,
>   `emit_gen_card_check`, `emit_gen_card_barrier`,
>   `emit_aastore_write_barrier_args`; `gc/src/card_table.rs`:
>   `JitCardView`, `mark_dirty_lockfree`). Every edge that is not "card byte
>   already dirty" or the published mask's own young-receiver skip goes to the
>   call; the card index is computed only below `old_end`; `mark & !7` stays
>   in the same 512-byte card because the view's base is 8-aligned; the slow
>   arm reaches `mark_dirty_lockfree` (summary byte before AND after the card
>   byte), which is what makes skipping on a dirty card byte sound; the
>   operand copies through R10/R11 cannot clobber an ABI source. The PRE/POST
>   pair's three freeze cases hold (a card byte is cleared only in a pause,
>   `take_dirty_cards` / `clear_all`).
> * **One lying comment fixed** (`card_table.rs`, `mark_dirty_lockfree`'s
>   duplicate arm): it said the inline store "has never been enabled". Under
>   the flag a compiled store that hits an already-dirty card never enters the
>   Rust barrier, so `dup_barrier_over_card_marks=`
>   (`CRATONVM_GC_CARD_METRICS=1`) is NOT comparable across the two arms —
>   do not read the A/B off it.
> * **What the A/B must show** (commands in "How to verify" below, one
>   binary, A B A B, 5 reps each, medians):
>   1. both arms print exactly
>      `slots=65536 holders=4096 rounds=200 mismatches=0 checksum=456695737600`
>      (HotSpot's line) and no `[rset-verify]` line other than `missing=0`;
>   2. arm B's `store_ms=` median is below arm A's by more than the host's
>      noise band (take the A-vs-A spread of the same 5 reps as the band; ~3x
>      swings are known on this host, so interleave);
>   3. engagement: `RUST_LOG=cratonvm_jit=debug` shows `inline generational
>      card barrier emitted` lines on arm B only;
>   4. stress, arm B: `GenR4W4StoreFormsProbe` under `gc-stress=250000` +
>      `CRATONVM_GC_VERIFY_RSET=1` prints 21 `form=<name> ok` lines and
>      `forms=21 slots=2048 rounds=30 failures=0 checksum=1257738060`; with
>      `CRATONVM_DBG_RSET_AUDIT=1` no `[rset-miss]` line;
>   5. no regression on `BinT 14 -Xmx256m` (`sum=327670`) or `HashMapOnly`
>      wall medians.
>   Flip `JitFlags::inline_card_mark` (`types/src/flags.rs`) only when 1–5 hold
>   AND a JIT WildFly boot campaign under `CRATONVM_DBG_RSET_AUDIT=1` reports
>   zero `[rset-miss]` (the July witness).

> **2026-09-24 round 4 wave 5 (`review5`, adversarial review).** Status
> **unchanged: FIX LANDED (opt-in), awaiting the A/B and the stress runs
> below.** Nothing `cards4` claimed is closed without a run, and none is
> recorded.
>
> * **Protocol re-derived:**
>   * **Freeze window.** With the check before AND after the store, a single
>     takeover freeze cannot leave "edge in heap, card clean": the three
>     cases on the page hold. Card bytes are cleared only in pauses
>     (`clear_all`, `take_dirty_cards`); no `concurrent_mark.rs` path touches
>     the card table.
>   * **PRE slow arm and SATB.** The PRE slow arm is a Rust call. A thread
>     inside it is resumed by the takeover, not frozen, and reaches its next
>     poll AFTER the store, so it opens no new SATB window.
>   * **Slow-arm reloads.** They come from frame or callee-saved slots
>     (debug-asserted).
>   * **The `done` edges trust nothing but the card byte:** the young-receiver
>     test is the published mask's own, and an out-of-view holder takes the
>     call.
>   * **Card index bound.** `emit_gen_card_check` indexes the card map only for
>     `mark < old_end`, so the load stays inside the map. The map is a fully
>     allocated `Vec`, never lazily committed.
> * **Residual, as the page says:** two freezes of one peer inside one
>   ~30-instruction window, with a complete collection between them. Still
>   rooted and non-moving, as in July.
> * **Hardened by `review5`:** `gen_card_view_of` (`jit/src/x64/objects.rs`)
>   read the view's `flags` word, 32 bytes in, BEFORE it compared the magic
>   that decides whether the address is a view at all. It now reads word 0
>   first and returns `None` on a mismatch before touching word 4. New test:
>   `r4w4_cards4_tests::a_non_view_is_declined_on_its_first_word`. No emitted
>   byte changes.
> * **`card_table.rs`'s `refreshing_the_view_is_idempotent_for_a_fixed_table`
>   asserted nothing a deleted `refresh_jit_card_view` body would fail.** It
>   now clobbers the three run-time words and requires the refresh to restore
>   the table's geometry.

*Filed 2026-09-24 by generational GC round 4, wave 4, lane `cards4`.*

- **Status:** FIX LANDED (opt-in, `CRATONVM_JIT_INLINE_CARD_MARK=1`, default
  OFF), awaiting probe — commands and expected lines below. Flip the default
  only after the A/B and the stress runs pass.
- **Severity:** performance. Every path is correct today.
- **Code:** `jit/src/x64/objects.rs` (`inline_card_mark_available`,
  `emit_gen_card_check`, `emit_gen_card_barrier`, `emit_ref_element_address`),
  the six store arms that call them (`objects.rs` ×3 — inline body, fresh
  constructor, gated — `op_field.rs` ×2 — compact and uniform — and
  `op_array.rs`'s `aastore`), `gc/src/card_table.rs`
  (`JitCardView`), `vm/src/jit/helpers.rs::build_helpers_opt`.

## What is slow

Under `-XX:+UseGenerationalGC` a compiled reference store into an old object
is a call: `jit_putfield_object` or `jit_write_barrier` (`aastore`,
the gated `putfield` arm). Young receivers skip it on the published mask, so
the cost falls on exactly the workloads with a large tenured graph that is
mutated in a hot loop — caches, maps, queues, object pools. The inline card
mark that would avoid the call had been off since 2026-07-30 (`494aa83b3`)
after a WildFly boot audit found an old `org/jboss/modules/Module` holding a
young child on a CLEAN card.

## What landed

A new inline barrier, root-caused and redesigned
(`docs/internal/reviews/gengc-round4-w4-cards4-20260924.md`):

- **Check, don't store.** Compiled code reads the card byte through the card
  table's read-only `JitCardView` and returns if it is already dirty; a clean
  card is a call to the collector's own barrier (`CardTable::mark_dirty_lockfree`
  through `jit_write_barrier`). No compiled code writes a card byte, so the
  scan bound and the summary map stay armed.
- **Before AND after the store.** Closes the window the WildFly audit saw: a
  peer frozen by the BUG-03 takeover between the reference store and its card
  store.
- **Element cards** for `aastore` on a precise table, **header cards**
  otherwise; the young-receiver skip is the published mask's own test; the
  geometry is loaded at run time from THIS VM's heap.

## How to verify (the orchestrator's A/B and stress)

Build once; switch the arm with the flag only. Interleave A B A B, 5 reps
each, medians (host noise is ~3x).

```
javac -d tools/bench tools/bench/GenR4W4CardBarrierBenchProbe.java tools/bench/GenR4W4StoreFormsProbe.java

# A (helper barrier)            / B (inline barrier)
CRATONVM_GC_VERIFY_RSET=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -c tools/bench GenR4W4CardBarrierBenchProbe
CRATONVM_JIT_INLINE_CARD_MARK=1 CRATONVM_GC_VERIFY_RSET=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -c tools/bench GenR4W4CardBarrierBenchProbe
```

Expected in both arms, exactly (HotSpot prints the same line):

```
slots=65536 holders=4096 rounds=200 mismatches=0 checksum=456695737600
```

and no `[rset-verify]` line with `missing=` other than `missing=0`. The A/B
number is the `store_ms=` line: B should be lower. Engagement is visible with
`RUST_LOG=cratonvm_jit=debug` (`inline generational card barrier emitted`).

Stress, every store form, both arms:

```
CRATONVM_DBG=gc-stress=250000 CRATONVM_GC_VERIFY_RSET=1 CRATONVM_JIT_INLINE_CARD_MARK=1 \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -c tools/bench GenR4W4StoreFormsProbe
```

Expected: 21 `form=<name> ok` lines, then exactly

```
forms=21 slots=2048 rounds=30 failures=0 checksum=1257738060
```

Also with `CRATONVM_DBG_RSET_AUDIT=1`: no `[rset-miss]` line (the audit now
reads element cards —
`docs/internal/gc/gengc-r4w3-cards3-dbg-rset-audit-reads-only-header-cards-FIXED-20260924.md`).

If all of that holds on a JIT WildFly boot campaign as well (the original
witness: zero `[rset-miss]`), flip the default in `types/src/flags.rs`
(`JitFlags::inline_card_mark`) and retire this page.
