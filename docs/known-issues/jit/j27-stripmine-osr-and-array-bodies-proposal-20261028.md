> **j27 wave 8 (lane W8OSR, 2026-10-28): item 1's `long` inductions taken; the rest REVIEWED, PENDING HOST VERIFICATION.**

> **j27 triage (builder lane TRIB, 2026-10-28, Windows, release LTO-off build of the j27 tree at 20:34 = `C:\craton\j27-trib-bin\cvm-trib-1.exe`): wave-8 rows VERIFIED.** `J27W8OsrLongStripRows` 6 facts equal HotSpot in default and in `CRATONVM_JIT_IR_STRIP_MINE=1 CRATONVM_JIT_IR_STRIP_MINE_OSR=1 CRATONVM_JIT_IR_STRIP_MINE_LONG=1`, both modes. Tests `strip_mine` 24 and `osr_service_copy_window` 2 pass. Facts only, no timing taken.
> - **Review.** I re-read `strip_mine/osr.rs` against its exactness proof and found no defect. The proof admits only the `iv < lim` stay test (`Lt`, or `Gt` reversed) with a constant step in `1..=MAX_STEP`. The new invariants feeding `d` (`I2L(lim)`, constants) are within the OSR remat program's shapes if the scheduler hoists them in front of the landing. A remat refusal there costs the OSR entry, not soundness.
> - **What changed.** An OSR-form header with a `long` induction is now strip-mined under the existing `CRATONVM_JIT_IR_STRIP_MINE_LONG=1`, so it needs all of `STRIP_MINE`, `STRIP_MINE_OSR` and `STRIP_MINE_LONG`; no new switch. Before, it was refused with `a long induction at a header with a bci`.
>   - The distance is the select-free saturating `d = min(max(lim, q), min(q, MAX - span) + span) - q` (`long_strip_distance`).
>   - `lim - q` would wrap a `long`; this cannot. The value lies in `[0, span]` and equals `min(lim - q, span)` for `lim > q`, else `0`.
>   - The inner trip count `i - q` is `long` too. Proof in the function doc.
> - **Unit tests** (crate `cratonvm-jit`, `jit/src/ir_optimize/strip_mine/tests.rs`). Both fail on the old code, which refused the loop.
>   - `j27_w8osr_long_osr_entry_keeps_the_answer_and_the_trip_count`: mid-loop entries; `Long.MIN_VALUE` under `Long.MAX_VALUE`, where `lim - q` wraps; landing and wrapping strides at the top; zero trips; across `2^31`.
>   - `j27_w8osr_long_osr_distance_is_long_full_strips_poll_and_opt_in`.
> - **Probe:** `tools/probes/jit/j27/J27W8OsrLongStripRows.java`. Expected HotSpot 25 lines are in its header and below.
> - **Verify:**
>   - Run `cargo test -p cratonvm-jit -j 5 --lib strip_mine`.
>   - Run the probe under the default and under `CRATONVM_JIT_IR_STRIP_MINE=1 CRATONVM_JIT_IR_STRIP_MINE_OSR=1 CRATONVM_JIT_IR_STRIP_MINE_LONG=1`. Every fact must equal the lines below.
>   - With `CRATONVM_DBG_UNROLL=1`, the second arm must print `strip-mined at its bci (OSR form, ...)` for each loop method except `empty`.
>   - Decision: compare `-Dj27.time=true` medians, interleaved and P-core pinned, on vs off, with GC time-to-safepoint unchanged.
>
>   ```text
>   fact osrl-plain-0-3000000 8833698474470001505
>   fact osrl-top-1 8174176532021305185
>   fact osrl-top-step3 6029846543080957473
>   fact osrl-cross31 -4995494980812054687
>   fact osrl-min 8833698474470001505
>   fact osrl-empty 1
>   ```
> - **Still not built:** array reads in the OSR form, and the two back-to-back strips of a main/post OSR graph (at most `2N` trips; bounded, kept).

> **j27 wave 5b (lane STRIPOSR2, 2026-10-28): promoted field cells taken, PENDING HOST VERIFICATION.** Same switch.
> A promoted header (`counterInt`, `twoCells`: cells in `R`'s phis, in-loop stores deleted, exit write-back) is no
> longer refused: (1) `Graph::promoted_cell_writebacks` records move `R -> O` (`osr::rekey_cell_writebacks`), so
> `Lowerer::promoted_cell_poll_writebacks` gives O's poll the write-back exit; (2) its `current` reads an inner-header
> cell phi through `ir_optimize::strip_outer_twin` (one line in `ir_lower`); (3) `plan_promoted_pending` is never
> reached for such a loop: it only plans SILENT stores (a deopt point in the body), which refuse the strip mine
> (`header_tables_clear`; a store in the body also fails `scan_body`). The exit write-back stores, which named the
> header snapshot, now name an unrenamed copy of it (`osr::keep_exit_frame`): after the loop `R`'s phis are the exit
> state. `scan_body` (OSR form only) no longer counts nodes pinned outside the loop (exit code reading `R`'s phis).
> Renaming uses `Graph::set_safepoint_slot` (def-use lists). New tests:
> `j27_striposr2_promoted_cell_records_follow_the_outer_header`, `j27_striposr2_promoted_cell_state_at_the_poll_and_the_exit`
> (the pad's cell value at every outer poll equals the original poll's after the same iterations; same answer), and the
> refusal list now has the silent-store case. Host must confirm: `DBG_UNROLL` shows `strip-mined at its bci` for
> `osrCounterInt` / `osrTwoCells` too (or names a refusal, e.g. `a memory phi` if the deleted stores leave the header's
> memory phi unfolded -- then fold `phi(H, m, self)` first), `osr-counterInt` / `osr-twoCells` facts unchanged, and
> a debugger/mode-exit request during `osrCounterInt` still leaves with `COUNTER` written (the OSR-door exit census
> keeps `promoted-field-cells*` refusals at zero for these rows).
>
> **j27 wave 5 (lane STRIPOSR2, 2026-10-28): item 1 PARTLY (opt-in; locals-only loops, not promoted static cells), PENDING HOST VERIFICATION.** New
> opt-in sub-switch `CRATONVM_JIT_IR_STRIP_MINE_OSR=1` (under `CRATONVM_JIT_IR_STRIP_MINE=1`): in an OSR compile
> (`Graph::osr_compile`), after the main loops, a pure `int` counted loop whose header `R` carries bci `b` is
> strip-mined in a TRIP-DISTANCE form (`jit/src/ir_optimize/strip_mine/osr.rs`, design in "Item 1: what was built"
> below). The outer header `O` takes `b` (and the header phis' pcs); the one snapshot at `b` is renamed to name `O`'s
> phis, so the OSR stub lands at `O` and computes the strip distance `d` there, and the outer poll's mode exit reads
> each `q` from its edge input (`R`'s phi) exactly as `back_edge_mode_exit_state` does for any header; `R` has no bci
> (no second entry); no body node and no body snapshot changes. No post loop is needed (the wrap case that refused
> the earlier design is exact here). Refused, per-trip poll kept: promoted field cells at the header (so the parent
> page's `counterInt` / `twoCells` rows, whose static cells the OSR route promotes, are NOT taken yet -- see "Item
> 1: remaining"), `long` inductions, array reads, an opaque OSR arm, a claimed/second/chain header snapshot, any
> other snapshot naming a loop value at a bci a deopt or entry can consult. Verify: (1) `cargo test -p cratonvm-jit
> -j 5 --lib strip_mine` (`j27_striposr2_*` green, every older test unchanged); (2) `J27StripMine2Rows` (new `osr-*`
> rows, run FIRST while the methods are cold) in the four arms below plus `... CRATONVM_JIT_IR_STRIP_MINE_OSR=1`,
> every fact equal to the list below; (3) engagement: `CRATONVM_DBG_UNROLL=1` prints `strip-mined at its bci (OSR
> form, ...)` for `osrLocalMix` / `osrMixTop` / `osrMixStep3` / `osrWorkerMix`, and `OSR-form strip mine refused --
> promoted field cells at the header` for `osrCounterInt` / `osrTwoCells`; the OSR stub still emits for the bci
> (`CRATONVM_DBG_JITC` OSR-entry census: no new "block control carries no bci" / liveness refusal at `b`); (4) the
> `osr-safepoint-*` rows finish (a hang is an outer poll that never runs).
>
> **rx27 wave 2 (lane stripmine, 2026-10-28): not a strip-mine defect.** The facts battery's `J27StripMineRows
> --compatible` "hang" (no output, SIGTERM ignored, `j27-strip-worker` spinning) was every `--compatible` program
> printing nothing and failing `Shutdown.exit`, after dev's fast-boot native prune; fixed and recorded in
> `docs/internal/fixed-bugs/j27-compat-fast-boot-jit-runs-no-main-20261028-FIXED-20261029.md`. On the fixed binary both probes match
> HotSpot 25 in both modes with and without `CRATONVM_JIT_IR_STRIP_MINE=1`, and SIGTERM stops a spinning compiled
> counted loop (rc 143) with the switch on or off.

> **j27 wave 4 (lane STRIPOSR, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** Items 2 and 3 are built behind two
> opt-in sub-switches of the opt-in `CRATONVM_JIT_IR_STRIP_MINE`: `CRATONVM_JIT_IR_STRIP_MINE_ARRAYS=1` (item 2: the
> check elimination is asked BEFORE the strip mine, the strip mine is refused unless it proved every read of the body,
> and the proven reads ride `Graph::strip_mine_proven_accesses` into the lowering's `ir_check_elim::analyze`) and
> `CRATONVM_JIT_IR_STRIP_MINE_LONG=1` (item 3: a `long` induction with the select-free saturating strip end
> `slim = min(min(q, Long.MAX - N*S) + N*S, lim2)`). SOUNDREV's first risk is closed in code: the inner latch skips its
> poll only in a VALIDATED block layout (`schedule.layout.applied`), so `CRATONVM_JIT_IR_RPO_LAYOUT=0` or a refused
> layout polls every trip as before. Item 1 (OSR-entered loops) is NOT built: the design below ("Item 1: design")
> needs a second OSR landing node and an entry-state rule the frame builders do not have; ask GCMAP before building
> it. Code: `jit/src/ir_optimize/strip_mine.rs` (+ `strip_mine/tests.rs`), `ir_check_elim::mark_strip_mine_proven`,
> the `Graph::strip_mine_proven_accesses` field in `jit/src/ir.rs`. Probe: `tools/probes/jit/j27/J27StripMine2Rows.java`.
> Verify recipe: (1) `cargo test -p cratonvm-jit -j 5 --lib strip_mine` (all `j27_stripmine_*` and `j27_striposr_*`
> green); (2) the probe and `J27StripMineRows` in four arms -- default, `CRATONVM_JIT_IR_STRIP_MINE=1`,
> `CRATONVM_JIT_IR_STRIP_MINE=1 CRATONVM_JIT_IR_STRIP_MINE_ARRAYS=1 CRATONVM_JIT_IR_STRIP_MINE_LONG=1`, and that arm
> with `CRATONVM_JIT_IR_STRIP_MINE_ITER=1` -- every `fact` line equal to the expected lines below, the probe finishing
> (a hang in `safepoint-*` is a worker that never polls); (3) engagement: `CRATONVM_DBG_UNROLL=1` prints
> `strip-mined (Int, step 4, 4 array read(s), ...)` for `hashArr` and `(Long, ...)` for `lmix`, and names every
> refusal (`an array read whose check the range pass keeps` for `hashArrN`/`dot`, `a memory phi` if the builder
> leaves one on a load-only loop -- see risks); (4) decision (flip either sub-switch ON): `time hashArr-1e7` /
> `time lmix-1e8` with `-Dj27.time=1` on vs off, P-core pinned, interleaved medians, and the GC battery's
> time-to-safepoint unchanged.

# Proposal: strip-mine OSR-entered loops and loops with array reads

Status: PARTLY, PENDING HOST VERIFICATION (items 2, 3 built opt-in in j27 wave 4, lane STRIPOSR; item 1 built
opt-in in wave 5, lane STRIPOSR2, `CRATONVM_JIT_IR_STRIP_MINE_OSR`; promoted field cells since wave 5b). Proposed j27 wave 2, lane
STRIPMINE, 2026-10-28.
Area: `jit/src/ir_optimize/strip_mine.rs`, `jit/src/ir_check_elim.rs`, the OSR entry stubs (`ir_lower`'s
`emit_osr_entry_stubs`).
Parent: `j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md` (item 1 built for pure, pc-less main loops).

## What is left

1. **OSR-entered loops.** The OSR door enters the original header (the post loop of a main/post unroll, or the
   only loop of an OSR-door graph), so `counterInt(1_000_000)` / `twoCells(1_000_000)` (the parent page's OSR rows,
   4.3x / 3.8x HotSpot) still poll every trip. Strip-mining a header with a bci needs: the outer header to carry
   the original header's bci and frame state (the OSR stub lands there and recomputes `slim`; the outer poll's
   mode exit names the header state as `back_edge_mode_exit_state` does today), the inner header to carry none
   (no second OSR entry), and every body frame of the inner loop unchanged. Ask GCMAP before touching the frame
   builders. **Built in wave 5 (lane STRIPOSR2) in a trip-distance form that needs no frame-builder change; loops
   with promoted field cells (`counterInt`, `twoCells`) are taken since wave 5b -- see "Item 1: what was built" and "Item 1:
   remaining".**
2. **Bodies with array reads.** `ir_check_elim` proves `a[i + k]` in range from the main test `i < n - d`; the
   inner test `i < slim` hides `n - d` behind `min`. Teach the check elimination that `slim <= lim2 <= lim`
   (both are `MinI` / `MinL` of the bound) or strip-mine after the range-check decision, as C2 does
   (`PhaseIdealLoop::strip_mined_loop` runs after RCE). **Built (wave 4): the second, smaller route.**
3. **`long` inductions.** `q + N*S` can wrap a `long`; the cap is `lim2 = min(lim, Long.MAX - S + 1)` and the
   strip end needs a saturating add (`q > lim2 - N*S ? lim2 : q + N*S`, with `lim2 - N*S` checked for wrap).
   **Built (wave 4), without a select.**

## Item 2: what was built and why it is exact

Teaching `ir_check_elim` the `min` chain is not one rule but several: the upper bound (`idx < x` readings,
`length_margin`, `below_own_length`, the `at_most` facts), the lower bound (the induction now enters from the
outer phi `q = phi(i0, i)`, so `proven_non_negative` / the monotone φ walk would have to see through a second
phi) and the stride-wrap argument (`is_margin_stride_induction` reads the test dominating the back edge). The
alternative is C2's order: decide the range checks first.

`strip_mine::accesses_proven` runs `ir_check_elim::analyze` once per pass (on first need, under the switch) on
the graph as it stands, and takes a body only if every read in it -- a PRIMITIVE `ArrayLoad` (no reference
decode in a poll-free loop) or an `ArrayLength`, with no control user (no exception edge of its own) -- is
proven null-safe and, for an element read, in bounds. The reads are recorded in
`Graph::strip_mine_proven_accesses`; `ir_check_elim::mark_strip_mine_proven` (end of `analyze`, so only with the
analysis on) marks them elided again when the lowering asks.

Exact: an elision is a claim about the index and array at every run of the node. The strip-mined graph runs the
main body only on iterations the original main loop runs, with the same carried values (the module doc's "Why it
is exact"), and the body writes no memory (a store makes a memory phi, refused), so every run of a read now is a
run of it then, with the same index, array and length. With both checks gone the read is one `MOV`
(`emit_array_null_bounds_guards_for`): it cannot transfer or wait, so the poll-free inner loop stays poll-free in
fact. A verdict taken before an earlier loop's rewrite still holds after it (that rewrite is exact for every other
loop's runs).

Frames: the main/post DEOPT form (`CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT`, default ON) gives each copy of a
read-carrying body its own `copy_frame` snapshot naming loop values, which the wave-2 rule ("no snapshot may name
a loop value") refused. A snapshot is now accepted when it is a `copy_frame` (not post-call, not a chain) and
every live node naming it is an inner-loop node (`read_only_inside`): none of those can transfer, so nothing reads
it, and the strip mine leaves every snapshot as it was. A by-pc snapshot naming a loop value still refuses.

## Item 3: the saturating strip end

`lim2 = min(lim, Long.MAX - S + 1)`, `slim = min(min(q, Long.MAX - N*S) + N*S, lim2)`. The add cannot wrap; `slim <=
lim2` always; for `q < lim2`, `q < slim <= q + N*S` (if `q > Long.MAX - N*S` the sum is `Long.MAX` and `slim = lim2`,
`lim2 - q < N*S`); for `q >= lim2` (a first strip entered at or past the cap) `slim <= q`, the inner test fails at
once, the outer test exits and the post loop runs the iterations, wrap included, with a poll each. No select and no
`lim2 - N*S` wrap check are needed.

## Item 1: what was built (wave 5, lane STRIPOSR2)

```text
O: q = phi(i0, i)                          // bci b, the header snapshot's phis; polls
   d = (int) min(max((long) lim - (long) q, 0), min(N*S, 2^30))
   R: i = phi(q, i + S); if (i - q < d) { BODY; goto R }   // no bci, no poll
   X: if (i < lim) goto O                  // the ORIGINAL compare node
```

Exact (module doc of `strip_mine/osr.rs`): on trip `k`, `kS < 2^30 + 2^16`, so the `int` difference `i - q` is `kS`
whether or not `i` wrapped. While `kS < lim - q` nothing has wrapped and the body runs iff the original test holds
and the strip is not used up; once `kS >= lim - q` the inner test fails and the outer test is the original compare on
the same `i` (a wrapped `i` re-enters `O` as the original re-enters its body: the javac loop that never ends still
polls once per strip). `q >= lim` gives `d = 0` (zero trips); `q < lim` gives `d >= 1` (progress). At most `N` inner
trips per poll. This removes the need for a post loop (the first fact of the design below), and the landing needs
no second snapshot (fact 2): the header's own snapshot is renamed, since only the OSR stub, the header poll's mode
exit and by-bci deopts (none: the loop is pure and the snapshot must be unclaimed) read it.

Frames: a body snapshot naming loop values is accepted only when no outside node names it and its bci is no
`Merge` / `Region` bci, no bci of an outside node that may deopt, no `Guard` bci (`consulted_bcis`, the rule of
`ea_ir_bridge::ir_consumable_snapshot_bcis`).

Tests (`jit/src/ir_optimize/strip_mine/tests.rs`, crate `cratonvm-jit`): `j27_striposr2_osr_entry_mid_loop_keeps_the_answer_and_the_trip_count`
(entry at the header with a mid-loop `i`, zero trips left, `lim = MAX` with landing and wrapping strides, one trip),
`j27_striposr2_osr_entry_polls_once_per_strip`, `j27_striposr2_the_outer_header_takes_the_bci_and_the_header_snapshot`,
`j27_striposr2_refusals_leave_the_graph_alone`, `j27_striposr2_is_idempotent_and_opt_in`,
`j27_striposr2_only_the_inner_latch_skips_the_poll`, `j27_striposr2_an_osr_main_post_graph_mines_the_main_and_the_post_loop`.

## Item 1: remaining

* **Promoted field cells: DONE in wave 5b** (status block at the top). The original note: the lowering keys the cells' poll write-back by the header
  NODE (`Graph::promoted_cell_writebacks`, `Lowerer::promoted_cell_poll_writebacks`, whose `current` resolves only
  the header's own phis) and `plan_promoted_pending` plans the silent stores per header region. Taking such a loop
  needs: re-key the records `R -> O`, let `current` map an inner-header phi through the outer edge, and check the
  pending-store plan covers the outer blocks (the outer poll sits outside `R`'s loop). Without it the outer poll's
  mode exit would refuse (`promoted-field-cells-unrecorded-header`), so the strip mine refuses instead.
* `long` inductions (the trip distance needs a 128-bit-safe bound: `lim - q` can overflow a `long`), array reads
  (the carried proof of item 2 relies on a main-loop range test), entries at an inner header of a nest (taken; not
  separately tested on the host).
* Two strips may run back to back without a poll in a main/post OSR graph (the main loop's last, the post loop's
  first: the main exit does not poll): at most `2N` trips.

## Item 1: design (superseded by the build above; kept for the record)

Three facts shape it:

* A strip-mined loop stops early at `lim2` and relies on a POST loop for the rest (the wrap case). A main loop has
  one; an ordinary counted loop or the post loop itself has none. Strip-mining such a header alone would need a
  polling remainder loop -- a second copy of the body with its own frames (`install_copy_frames`). Without it the
  only fall-back (`slim' = max(slim, q + 1)`) runs a wrapping trip in the poll-free inner loop, about 2^32 / S trips
  without a poll in a `javac` loop that never ends: unbounded time to safepoint. Refused.
* The OSR door lands at the header bci `b` through the by-bci snapshot `S_b` (whose locals name the header phis) and
  `emit_osr_entry_stubs` seeds those phis (remat programs for unnamed invariants such as `lim`).
* Every main/post loop's post loop IS the original header `R` (bci `b`), and `main_post_unroll_loop` already
  proves that the licence + main loop + join path is exact for ANY entry values.

So the smaller design is **OSR entry migration**, not a strip-mined OSR header: let an OSR-entered run reach the
(strip-mined) main loop.

1. In an OSR compile (`ctx.osr_route` / `graph.osr_compile`), after `main_post_unroll_loop` and the strip mine, add a
   landing merge `P` in front of the licence: `P`'s phis take the OSR stub's values for `R`'s carried phis, the
   licence's entry inputs become `P`'s phis. `P` is an OSR landing for `b`; `R` stops being one. `R` keeps its bci
   and its snapshot for its own poll's mode exit (`back_edge_mode_exit_state` at `R` is unchanged).
2. Entry state: the stub writes the interpreter's locals at `b` into the nodes `S_b` names. Landing at `P` needs a
   snapshot `S'_b` at the same bci naming `P`'s phis in the slots where `S_b` names `R`'s, and every other slot
   equal. Two snapshots at one bci are what `ir_verify::check_frame_states` rejects unless each is claimed: `S'_b`
   is claimed by `P` (a new `Graph` field `osr_landing: Option<(usize /* bci */, NodeId /* P */, u32 /* S'_b */)>`
   that `emit_osr_entry_stubs` consults before the header lookup by bci). This is the part to clear with GCMAP:
   the stub's liveness seed (`LiveModel` live-in at the landing block), the remat of `lim` / `lim2` (computed from
   `n`, a local, before the licence: no remat needed when `P` sits before them), and the opaque-arm table keyed by
   header bci.
3. Exactness: entering at `P` with the state at `b` runs licence -> main (strip-mined) -> join -> `R`, which is the
   main/post equivalent of entering `R` with that state. The outer poll has no mode exit (its header has no bci),
   as today; an interpreter-mode request is honoured at the post loop's poll or at the outer poll's safepoint, at
   most `N * F` iterations later. Frames inside the main loop are unchanged (pc-less, or per-copy frames under the
   DEOPT form).
4. Refusals: an OSR graph whose door is not a main/post post loop (an OSR-door body with no main loop, an
   un-unrolled counted loop) keeps today's per-trip poll; so does an OSR entry at an INNER loop header of a nest.

Expected effect: `counterInt(1_000_000)` / `twoCells(1_000_000)` (OSR rows) take the strip-mined, unrolled main loop
instead of the per-trip-polling post loop.

## Risks and things a host run must confirm

* **A memory phi on a load-only body.** `header_phis` refuses a memory phi. If the builder leaves a trivial memory
  phi at a loop header whose body only reads, item 2 never engages (`DBG_UNROLL` names `a memory phi`); then fold
  a memory phi whose back value is itself before the strip mine (a separate, small change).
* **Where `slim` is scheduled** (SOUNDREV's third risk): `slim` and `lim2` are pure nodes of `q`; a late placement
  into the inner test's block computes them every trip (correct, slower). Check `CRATONVM_DBG_IR_SLOTS` / the
  disassembly of `hashArr` that the `MinL` sits in the outer block.
* **The extra `analyze`** runs once per compile that has a strip-mine candidate with reads (under the switch), and
  its `schedule` bumps `RPO_LAYOUTS_APPLIED` once more (a census counter only).
* **The OSR outer header's mode exit** (SOUNDREV's second risk) is item 1's; nothing built here gives `O` a bci.

## Expected HotSpot 25 output of `J27StripMine2Rows` (modelled from Java int/long arithmetic)

The wave-5 `osr-*` rows print FIRST (modelled from Java int arithmetic; `osr-localMix-3000000` equals
`lmix-0-3000000` below, a cross-check):

```text
fact osr-counterInt-1000000 1783293664
fact osr-twoCells-1000000 21000000
fact osr-localMix-3000000 -1481520287
fact osr-mixTop-2144483647 1249304417
fact osr-mixStep3-3000001 1417533311
fact osr-safepoint-gc-returned true
fact osr-safepoint-stack-walked true
fact osr-safepoint-worker-finished true
fact osr-safepoint-worker-answers true
```

then:

```text
fact warm 3319074157584434055
fact hashArr-0 1
fact hashArr-1 32
fact hashArr-3 31015
fact hashArr-4 961487
fact hashArr-5 29806126
fact hashArr-1000 -1085958355
fact hashArr-1000003 -235814649
fact hashArrN-ok none 578127237
fact hashArrN-oob java.lang.ArrayIndexOutOfBoundsException: Index 3000 out of bounds for length 3000
fact hashArr-null true
fact hashLongs-0 1
fact hashLongs-5 -6250086458812333559
fact hashLongs-1000 -460848449314872315
fact hashLongs-100003 2891200404722507428
fact hashBytes-0 1
fact hashBytes-5 29042681
fact hashBytes-1000 -1608184475
fact hashBytes-100003 -1519605732
fact pairs-0 0
fact pairs-1 0
fact pairs-2 8
fact pairs-5 1096
fact pairs-1000 -888516178
fact pairs-100003 -42745792
fact dot-ok none -864033684
fact dot-oob java.lang.ArrayIndexOutOfBoundsException: Index 999 out of bounds for length 999
fact lmix-0-0 1
fact lmix-0-1 31
fact lmix-0-1000 133786869
fact lmix-2147483547-2147488647 -657848649
fact lmix--1099511627776--1099511527776 1226182481
fact lmix-9223372036854774807-9223372036854775807 413006453
fact lmix--9223372036854775808--9223372036854774808 133786869
fact lmix-9223372036854775807-9223372036854775807 1
fact lmix-5-3 1
fact lmix-0-3000000 -1481520287
fact lmixStep3-0-3001 376456827
fact lmixStep3-9223372036854774808-9223372036854775807 1563172884
fact lmixStep3-9223372036854772807-9223372036854775807 -2052558555
fact lmixStep3--9223372036854775808--9223372036854745808 281774233
fact lmixStep3-2147483347-2147486647 2017016627
fact lsumSq-0 0
fact lsumSq-1000 332833500
fact lsumSq-1000000 333332833333500000
fact lsumSq-3000000 8999995500000500000
fact safepoint-gc-returned true
fact safepoint-stack-walked true
fact safepoint-worker-finished true
fact safepoint-worker-answers true
fact safepoint-hashArr-4000000 -1071247487
```

Cross-checks against the parent page's HotSpot-modelled `J27StripMineRows` lines: `lmix-0-1000` equals `mix-0-1000`,
`lmixStep3-0-3001` equals `mixStep3-0-3001`, `lsumSq-1000` / `-1000000` equal `sumSq-1000` / `-1000000`. Confirm the
whole list against a HotSpot 25 run on the host before relying on it.

## Decision rule

The parent page's icount OSR rows against HotSpot; the GC battery's time-to-safepoint unchanged;
`J27StripMineRows` facts unchanged; `J27StripMine2Rows` facts equal to the lines above in every arm.
