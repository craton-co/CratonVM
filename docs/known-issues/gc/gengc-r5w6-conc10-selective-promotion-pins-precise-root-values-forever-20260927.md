# Selective promotion pins every root value, so an object a precise VM root names never leaves young under the JIT

> **STATUS (2026-09-29, gce e1/x): KEEP -- no e1 row runs `CRATONVM_GEN_PRECISE_ROOT_PROMOTE`.** **Remaining:** its d5/r flip gate.

> **STATUS (2026-09-29, gce e1/c): the "pending" cross-lane line is ALREADY APPLIED -- only the flip gate is left.** `gc/src/gen_heap.rs::sweep_young_non_moving`, pin set (1), reads `tenuring::take_sealed_precise_root_values(roots.len())` (applied by d5/s; the d8/x block below still calls it "to do"). Gate item 5 is met in code; the d5/r gate's runs (with `prp_stale_refused=0`) and the Tomcat / Spring Boot engagement runs remain.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- opt-in (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`), OPEN as its flip gate.** No d7 row runs the flag; the sweep-side switch to `take_sealed_precise_root_values` (the cross-lane request of the d5/r block) and the Tomcat / Spring Boot engagement runs are still to do.

## STATUS (2026-09-28, gcd d5/r): opt-in stands; gate item 5 (the stale-table guard) LANDED on the scan side, the sweep's one-line switch is a cross-lane request; the FLIP GATE is restated below with it

- **Landed (unbuilt, inert until the sweep switches to it):**
  `gc/src/gen_heap_tenuring.rs` -- `seal_precise_root_values(final_len)`
  (per-thread `PRECISE_ROOT_SEALED_LEN`; `publish_` and `clear_` unseal) and
  `take_sealed_precise_root_values(roots_len)`, which hands the table over
  only when the publishing scan finished and the sweep's root list is at
  least that long (the gatherer appends frozen peers' words after
  `collect_roots`, never removes), else consumes it, exempts nothing and
  counts `prp_stale_refused` on `[GC] precise_root_promote:` (a sealed table
  longer than the list also warns, rate-limited: a gatherer dropped
  entries). `vm/src/memory/roots.rs::collect_roots` seals with its final
  length just before it returns (a no-op when nothing was published, i.e.
  the flag off). Re-exported from `gen_heap`. `take_precise_root_values()`
  is unchanged (its tests and the sweep still use it).
- **Cross-lane (young-copy lane, `gc/src/gen_heap.rs::sweep_young_non_moving`,
  pin set (1); not a build dependency):**
  ```diff
  -            let precise_only: FxHashSet<usize> = tenuring::take_precise_root_values()
  +            let precise_only: FxHashSet<usize> = tenuring::take_sealed_precise_root_values(roots.len())
  ```
  Flag off: byte-identical (nothing is ever published).
- **Tests:** `cargo test -j 5 -p cratonvm-gc --lib gcd_d5r_the_sealed_take_refuses_a_table_that_does_not_describe_the_list`
  -> 1 passed; `cargo test -j 5 -p cratonvm-gc --lib gcd_d2g_` and
  `cargo test -j 5 -p cratonvm-vm --lib gcd_d2g_precise_counts_cover_only_the_precise_sections`
  unchanged.
- **FLIP GATE** (all must hold before `CRATONVM_GEN_PRECISE_ROOT_PROMOTE`
  defaults on; `=0` must then restore byte-identical behaviour): items 1-4,
  6 and 7 of the d3/n block below, unchanged, PLUS item 5 now = the
  cross-lane line above applied, and in every gate run
  `[GC] precise_root_promote: ... prp_stale_refused=0` (a non-zero value is
  a foreign gatherer feeding the sweep: report it; it is safe, but it means
  the exemption silently switched itself off). The flip itself: the flag is
  read through `runtime_flag_on(PRECISE_ROOT_PROMOTE_FLAG)`
  (`precise_root_promote_enabled`); move it to `runtime_flag_default_on`
  and give its `types/src/flag_groups.rs` entry `off_word: Some("0")`.
  Owner: orchestrator (runs), young-copy lane (the one line).

## STATUS (2026-09-27, gcd d3/n, superseded above): RE-CHECKED, no code change; opt-in stands; FLIP GATE written below. Open until the gate passes (orchestrator)

**Re-check of d2/g's opt-in (by reading, base `e352a1d35`).**
- The rule (`gc/src/gen_heap_tenuring.rs::precise_only_root_values`) exempts
  a young value only when its occurrences in the sweep's WHOLE `roots` slice
  equal the precise count published by the SAME thread's `collect_roots`
  (`vm/src/memory/roots.rs::publish_precise_root_counts`, sections 2/5/6/9).
  Any extra source in the slice (conservative frame words, peer snapshots,
  `xt_roots`, movable JIT slots, the native pin stack) raises the count and
  keeps the pin; an edited or rewritten entry lowers it and keeps the pin; a
  band word vetoes it. Pins that never enter the slice (finalizer values,
  card-gap words, late conservative bases, interior words resolved through
  `young_object_ranges`) are inserted independently and win (the pin set is a
  union), so an interior conservative word into a precise-only object still
  pins its base. Sound as far as reading goes.
- Static reference values are not baked into compiled code: the inline
  `getstatic` loads through the slot base, and `ldc` String/Class re-fetch
  (`ldc_string_cp` / `ldc_class_cp`), so a moved static value is re-read.
- The table is per thread and take-once, cleared at the top of every
  `collect_roots`. Every production gatherer that feeds a collection calls
  `collect_roots` on the initiating thread (the seven `gc_and_alloc.rs`
  sites, the JNI and JIT doors); a table published by a scan no sweep
  consumed (a heap dump, a concurrent pause, `CRATONVM_DBG_ROOT_REMAP_AUDIT`'s
  re-scan, a lost STW race) is cleared by that thread's next scan. The one
  shape that would consume a STALE table is a young sweep whose roots did not
  come from `collect_roots` on the same thread after the publish; none exists
  today. Gate item 5 below makes that an assertion instead of a reading.
- Diagnostic only: `PRECISE_ROOT_CENSUS` (the `[GC] precise_root_promote:`
  counters) is a process static, so in a multi-VM process each VM's line
  sums every VM's sweeps. It gates no decision.

**FLIP GATE (all must hold before `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` defaults
on; `=0` must then restore byte-identical behaviour):**
1. Unit: `cargo test -j 5 -p cratonvm-gc --lib gcd_d2g_` and
   `cargo test -j 5 -p cratonvm-vm --lib gcd_d2g_precise_counts_cover_only_the_precise_sections`
   pass, Linux and Windows.
2. Engagement: on the Tomcat fixture and the Spring Boot sample (exploded),
   JIT on, `CRATONVM_GEN_PRECISE_ROOT_PROMOTE=1 CRATONVM_DBG=gc-stats`: the
   `[GC] precise_root_promote:` line shows `prp_sweeps>=1` and
   `prp_exempt>=1` (else the arm never ran and nothing below proves anything).
3. Remap audit, same runs plus `CRATONVM_DBG_ROOT_REMAP_AUDIT=1`: zero
   root-remap findings, 3/3 each driver; and with `CRATONVM_GC_VERIFY_RSET=1`
   every `[rset-verify] site=` line `missing=0` (a promoted static value now
   holds old->young edges through cards, not through a root).
4. Parity, JIT on, WITHOUT `CRATONVM_GEN_YOUNG_MIRROR_DEFER` /
   `CRATONVM_GEN_Y2O_LIVE_SEED`, 3/3 each, stdout equal to
   `java -XX:+UseSerialGC -Xmx256m -cp tools/bench <probe>`:
   ```
   F="CRATONVM_GEN_PRECISE_ROOT_PROMOTE=1 CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_DBG=gc-stats"
   env $F cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5ConcUnloadProbe
   env $F cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5RemarkRefsProbe
   ```
   i.e. `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`
   and the RemarkRefs probe's HotSpot line; never an exception.
5. Stale-table assertion (a one-line code change to land WITH the flip, in
   `gen_heap.rs`'s pin loop, lane m's region): publish the root slice's
   length with the counts (`publish_precise_root_counts` sees only the
   prefix up to step 9, so publish `collect_roots`'s FINAL length from its
   end instead) and have the sweep take the table only when
   `roots.len() >= published_len`; a debug assertion on a mismatch. Until
   then gate item 3 is the only guard against a foreign gatherer.
6. No-regression under stress, flag ON vs OFF in ONE binary, interleaved:
   `GenR4W4SteadyPromotionProbe` (JIT on and `--nojit`) and
   `BinTreesClassic 18` -- identical stdout/checksums;
   `CRATONVM_DBG=gc-stress=250000` on `GenR4W4SteadyPromotionProbe` 5/5 with
   the flag on (no SIGSEGV, no `[root-remap]` finding).
7. Mode invariance: the flag changes object placement only; run 4 in
   `--compatible` and in the default `jdk-only` launcher mode, both equal to
   HotSpot.

## STATUS (2026-09-27, gcd d2/g, superseded above): IMPLEMENTED OPT-IN (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, default OFF), unbuilt; open until the audit runs below pass

**What landed** (d1c's design, with the P \ C test done by COUNTING, so every
root source outside `collect_roots` is covered without being enumerated):
- `vm/src/memory/roots.rs::collect_roots`: clears the table at the top (beside
  the movable set), records the `[start, end)` of steps 2 (statics), 5
  (interned strings), 6 (class mirrors) and 9 (JNI globals), and right after
  step 9 publishes, per address, how many entries those four sections hold
  (`publish_precise_root_counts`; licence `precise_root_promote_licensed`:
  Generational + the flag). A range that no longer describes the list
  publishes nothing.
- `gc/src/gen_heap_tenuring.rs` (new section): the per-thread, take-once table
  (`publish_precise_root_values` / `clear_precise_root_values` /
  `take_precise_root_values`), the rule `precise_only_root_values`, and the
  census line `[GC] precise_root_promote: prp_sweeps= prp_exempt=
  prp_kept_pinned=` (printed after the `[GC] conc_driver:` block, under the
  flag only; `concurrent_mark.rs::driver_census_line`).
- `gc/src/gen_heap.rs::sweep_young_non_moving`, pin set (1): a young root value
  is left out of the pin set only when EVERY occurrence of it in the `roots`
  slice the sweep was handed is one of the counted precise ones, and no band
  word vetoed it (`is_unrewritable_jit_root`). Any other occurrence -- a
  conservative frame word (steps 1, 14, 14a5), a peer's deposited snapshot, a
  frozen peer's `xt_roots`, a movable JIT slot, a JNI local -- raises the
  count and the value stays pinned exactly as before. Finalizer values,
  card-gap words, the late conservative bases and interior words resolved
  through `young_object_ranges` still pin their base independently.
- The tables are rewritten by `memory::gc::update_all_roots` (steps 2 via
  `STATIC_REF_SLOTS`, 5, 6 + the reverse map, 9) on the same map the non-moving
  path returns (`evac_map`); the same-cycle old sweep seeds every promotion
  destination (`sweep_old_gen_non_moving_impl`), so an evacuated root value is
  marked there too. The JIT re-fetches `ldc` String/Class constants
  (`ldc_string_cp` / `ldc_class_cp`), it never bakes them.

**Still open (orchestrator):** the audits below, then a default decision. The
prerequisite loader edge is in (d1c), and this wave closes its unparseable-
stretch residual (`gcd-d1c-young-instance-loader-lost-in-an-unparseable-young-stretch`).

**Verify.**
- `cargo test -j 5 -p cratonvm-gc --lib gcd_d2g_` (includes
  `gcd_d2g_only_values_named_by_precise_tables_alone_are_exempt`,
  `gcd_d2g_precise_root_values_are_taken_once`) and
  `cargo test -j 5 -p cratonvm-vm --lib gcd_d2g_precise_counts_cover_only_the_precise_sections`.
- Remap audit, flag ON, tomcat fixture and the Spring Boot sample (exploded):
  `CRATONVM_GEN_PRECISE_ROOT_PROMOTE=1 CRATONVM_DBG_ROOT_REMAP_AUDIT=1 CRATONVM_DBG=gc-stats ...`
  -- expected: no root-remap finding, and `[GC] precise_root_promote:` with
  `prp_exempt>=1` (else the arm never engaged and the run proves nothing).
- Probes, JIT on, WITHOUT `CRATONVM_GEN_YOUNG_MIRROR_DEFER` /
  `CRATONVM_GEN_Y2O_LIVE_SEED`, 3 runs each:
  ```
  F="CRATONVM_GEN_PRECISE_ROOT_PROMOTE=1 CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_DBG=gc-stats"
  env $F cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5ConcUnloadProbe
  env $F cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5RemarkRefsProbe
  ```
  Expected stdout: HotSpot's (`java -XX:+UseSerialGC -Xmx256m -cp tools/bench <probe>`),
  i.e. `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`;
  never an exception. Stderr: `prp_exempt>=1`. A run that still differs from
  HotSpot with `prp_exempt>=1` means the anchor is named by a second root
  source too (`prp_kept_pinned` counts those); read `CRATONVM_DBG_MIRRORPIN_WHY`.
- Flag OFF: byte-identical (the table is cleared every scan and never
  published; the sweep's `take` finds `None` and exempts nothing).

## STATUS (2026-09-27, gcd d1/c, superseded above): re-verified OPEN on 6d39e8dcc; NARROWED to an exact two-lane design; no code change (it moves live objects out from under roots, so it needs a build-and-audit lane)

**Re-verified.** `gc/src/gen_heap.rs::sweep_young_non_moving`, pin set (1):
every value of `roots` that lands in young is pinned (`pin_base_of`), except a
value published as a movable JIT root (`gc_quiescence::is_movable_jit_root`)
on a cycle whose coverage proof held (`honour_movable`) and that no band word
vetoed (`is_unrewritable_jit_root`). `collect_roots` returns one flat list, so
a static's value, a mirror, an interned string and a JNI global are pinned
like a conservative word.

**Why the existing movable channel cannot simply carry them.** The movable
set is keyed by ADDRESS, and the only veto is the band scan's
`add_unrewritable_jit_root`. The same `roots` list also carries conservative
words the veto never sees: `Frame::scan_locals_conservative` /
`scan_object_refs_conservative` (step 1 under `conservative_locals`), the A5
`conservative_frame_pass` (step 14a5), the frozen peers' `xt_roots` and every
peer's deposited snapshot. Publishing a static's value as movable would move
it out from under any of those words that also holds it. That is the whole
hazard of this page, and why it was not done in-session.

**The exact design (two halves, opt-in first).**
1. `vm/src/memory/roots.rs` (lane c): during the scan, collect the values of
   the PRECISE tables the post-collection fix-up rewrites -- step 2 statics
   (already recorded as `STATIC_REF_SLOTS`), step 5 interned strings, step 6
   mirrors, the JNI global table -- into a set `P`, and every value pushed by
   a conservative source (the four above) into `C`. Publish `P \ C` for the
   cycle, per thread (the pin decision runs on the initiator) -- a new
   `gc_quiescence` table beside the movable set, cleared with it at the top of
   `collect_roots`.
2. `gc/src/gen_heap.rs::sweep_young_non_moving` pin loop (lane e's region):
   skip a root value in that table exactly as a movable one is skipped, and
   keep pinning it if any peer snapshot or `card_gap_roots`/finalizer value
   names it (those are gathered outside `collect_roots`).
Before any default: `CRATONVM_DBG_ROOT_REMAP_AUDIT=1` on the tomcat and
spring-boot drivers must report no root naming a vacated address (it re-runs
the scan after the collection), and the four tables' remaps through
`evac_map` must each be checked on the non-moving path.

**Related, fixed this round:** the mirror special case of this page
(`CRATONVM_GEN_YOUNG_MIRROR_DEFER`) lost a live class under the JIT because a
promoted mirror exposed a missing young-instance → loader edge in the STW old
collection; see the STATUS of
`gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929.md`. Any
change that lets precise roots promote exposes the same edge, so that fix is a
prerequisite of this one.

**Owner:** lane c (roots.rs half) and lane e (pin loop half), next wave.

*Filed 2026-09-27 by gen round 5, wave 6, lane `conc10`. Found by reading
while explaining the concurrent class unload's JIT-only failure; not
measured on its own. Owner: the non-moving young sweep
(`gc/src/gen_heap.rs::sweep_young_non_moving`, its selective-promotion pin
set — lane pin10's young-copy region).*

- **Severity:** retention / policy, not memory safety. Makes several
  HotSpot-parity results depend on `--nojit`: an object whose DIRECT holder is
  a VM root (a static field value, a user-class mirror rooted by
  `collect_roots` step 6, an interned string, a JNI global) stays young for as
  long as the JIT keeps the young collection on the non-moving sweep, i.e.
  for the life of a JIT-warm process.
- **Status:** OPEN. The two consequences this lane met are handled opt-in
  elsewhere: young user mirrors (`CRATONVM_GEN_YOUNG_MIRROR_DEFER`,
  `gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929.md`) and
  young `Reference`s' referents (`CRATONVM_GEN_Y2O_LIVE_SEED`,
  `gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md`).

## What is wrong

`sweep_young_non_moving`'s selective promotion evacuates "exactly those marked
survivors NOT pinned by any root value", and its pin set is "every young
address that appears as a root or finalizer value — which INCLUDES
conservative JIT false-positives, because we pin by the raw slot value". The
rule is right for CONSERVATIVE roots (a word that may be a `long` cannot be
rewritten) and needlessly strong for PRECISE ones: a static slot, the
class-mirror table, the intern table and the JNI global table are rewritten
through the pointer map after every collection that moves (the same map the
moving young cycle and `evac_map` already feed). Because `collect_roots`
returns one flat list, the sweep cannot tell the two apart, so it pins both.

Consequences measured or read this round:
- a user-class mirror young when the JIT starts is never promoted, so the
  concurrent cycle (which never collects young) can never unload its loader
  (`GenR5W5ConcUnloadProbe`: JIT fails, `--nojit` passes);
- a `WeakReference` / `SoftReference` / `PhantomReference` held in a static
  stays young, and its referent is then a young→old seed at every concurrent
  pause (`GenR5W5RemarkRefsProbe`'s `weakToChild`);
- a finalizable held through a static array stays young and is finalized by a
  young collection, which treats old referents as live (the third reading on
  the mark2 page).

## Proposed fix

Let `collect_roots` publish, per pause, the index range(s) of its output that
are PRECISE and REWRITTEN post-GC (statics — `STATIC_REF_SLOTS` already
records them for the remap —, the class-mirror table, interned strings, JNI
globals), and let the pin set skip the values in those ranges unless the same
value also appears in a conservative range. The remap of those tables through
`evac_map` must then be verified complete (the `CRATONVM_DBG_ROOT_REMAP_AUDIT`
verifier re-runs the scan after the collection and reports any root still
naming a vacated address — exactly the check). Opt-in first.

## How to verify

`CRATONVM_DBG_ROOT_REMAP_AUDIT=1` with the change on the tomcat / spring-boot
drivers (no finding), then `GenR5W5ConcUnloadProbe` and
`GenR5W5RemarkRefsProbe` with the JIT and WITHOUT
`CRATONVM_GEN_YOUNG_MIRROR_DEFER` / `CRATONVM_GEN_Y2O_LIVE_SEED`: HotSpot's
lines once the objects age out of young.
