# The regex rows are still 7-17x HotSpot on n23-w4 (per-call cost; the `String.matches` / `replaceAll` rows and the Matcher rows)

> **Round rx27 close (2026-10-29, orchestrator): STILL OPEN; the remaining gap is code quality inside the big
> spliced bodies, not splice, clear or String decisions.** Linux host, `tools/probes/icount2.sh` (main-thread
> instructions per op, median of 5, quiesced), binary `cvm-rx27b19` (`fac6921ae7`) = `jit-regex-r27` with every rx27 flip;
> facts (`R24RegexSplitRows facts`) and the 92-probe battery equal HotSpot 25 in both modes wherever they did on
> the base, and `--compatible` went from printing nothing (dev's fast-boot native prune) to 83/89 probes equal.
>
> | row | base `df86fe90ba` | rx27 close | HotSpot 25 | ratio | gate (3.0x) |
> |---|---:|---:|---:|---:|---|
> | `regex-matches` | 16 476 | 14 859 | 3 793 | 3.92x | open |
> | `regex-find-email` | 44 932 | 35 139 | 9 566 | 3.67x | open |
> | `regex-find-ci` | 26 921 | 23 828 | 7 330 | 3.25x | open |
> | `regex-split` | 135 936 | 87 805 | 31 667 | 2.77x | **passes** |
> | `regex-replaceall` | - | 64 120 | 28 100 | 2.28x | **passes** |
> | `regex-groups` | 26 962 | 21 672 | 5 804 | 3.73x | open |
> | `regex-string-matches` | - | 33 389 | 12 881 | 2.59x | **passes** |
> | `matchonly` | 28 017 | 25 206 (25.2-27.9k) | 7 019 | 3.59x | open |
>
> What rx27 landed on these rows (default ON unless noted): the keep-loop string-prefix / rare-row rules (the
> compile-order race that picked the mode, -25 % on `cached`/`split-ws`/`regex-split`);
> `SEQ_CHARAT_OUTER_HOIST` (find-ci 0.91x, find-email 0.93x); the chain switches `NESTED_IN_LOOP_HOT`,
> `CHAIN_BIMORPHIC_PREDICATES`, `SPLICE_COLD_HANDLER_ARM`, `CHAIN_BIMORPHIC_LINKS` and `CHAIN_DEEP_SPINE`
> (regex-matches -12 %; deep spine splices the per-character `ASCII.isType` call); `FRAME_BLOCK_DESCRIPTOR` (one
> shadow-stack record per activation); the regalloc four `AND_ZERO_TEST_BRANCH` / `AND_ZERO_TEST_GUARD` /
> `OPERAND_IN_PLACE` / `PUBLISH_RELOAD_ELIDE` (find-ci -2.8 %, find-email -4.8 % over the chain set).
> `CHAIN_ENTRY_SPLICE` stays opt-in (it costs `cached` +19 %, `split-ws` +14 %). Switches that never paid were
> removed (GUARDED_HOT_BY_RECEIVERS, HOME_STORE_ELIDE, PHI_SOURCE_COALESCE, LS_CALLER_SAVED_EBB, three loop-entry
> clear switches, the four guards switches, ONCE_WALK_CHAIN_SPLICE and CALL_WRITE_SUMMARY).
>
> Why the rest does not close by more waves: the per-row breakdown
> (`rx27-w4-per-row-instruction-breakdown-20261028.md`) and the lane close-outs
> (`rx27-regalloc-frame-slot-traffic-under-the-accumulator-lowering-20261028.md`,
> `rx27-bimodal-per-activation-cost-of-large-ir-bodies-20261028.md`,
> `rx27-guards-compact-field-test-residual-20261028.md`) put 20-33 % of every row in frame-slot store/reload traffic
> of the optimizing tier's accumulator lowering (every value through RAX and its slot; a register only swaps a
> reload for a move) and 4-5 % in the per-object compact-header / layout-epoch field test. Closing to 3x needs
> an operand-in-place, register-allocating instruction selector for the IR tier, which is a project of its own,
> not a switch. Open sub-pages: `rx27-bimodal-matchonly-and-groups-spread-20261028.md` (matchonly bimodality),
> `rx27-chain-matcher-entry-group-and-predicate-pairs-20261028.md`, `rx27-find-email-reg-moves-are-residency-copies-20261029.md`.

> **j27 hand-off: IN PROGRESS (lane STRRX), out of scope for j27 if not retired by STRRX; then it moves to a separate session.** Round j27 lane STRRX worked the
> String-side of this page and found no String intrinsic, admission or pin decision left on the Matcher rows'
> path (every `String` / `CharSequence` member they call is a prefix kind or an expanded accessor; the reviewed
> arms answer HotSpot's values). Its one code change, the `String.indexOf(int, int)` prefix
> (`CRATONVM_JIT_IR_STRING_INDEXOF_FROM_PREFIX`, default ON, details on
> `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`), moves the split fast path only;
> the rows below are unchanged by it.
>
> Measured j27 (Windows, dev tree of 2026-10-28 + lane STRRX, P-cores `affinity FFFF`, 5 interleaved processes
> per arm, median of per-process minima, wall ns/op, `tools/probes/R24RegexSplitRows.java`; facts
> (`R24RegexSplitRows facts`, 14 lines) equal HotSpot 25 in both modes):
>
> | row | HotSpot 25 | cvm `--jdk-only` | ratio | cvm `--compatible` |
> |---|---:|---:|---:|---:|
> | `regex-matches` | 202 | 1 070 | 5.3x | 1 151 |
> | `regex-find-email` | 455 | 2 506 | 5.5x | 2 943 |
> | `regex-find-ci` | 279 | 1 755 | 6.3x | 1 898 |
> | `regex-split` | 1 496 | 8 794 | 5.9x | 11 404 |
> | `regex-groups` | 303 | 1 848 | 6.1x | 1 802 |
> | `split-ws` | 757 | 5 197 | 6.9x | 4 379 |
> | `matchonly` | 511 | 1 917 | 3.8x | 1 938 |
>
> (Wall time on Windows reads higher ratios than the Linux host's instruction counts of round 25, 3.5-5.5x.)
>
> What remains (none of it in the String area): (1) the spliced `Pattern$Start.match` scan loop's per-character
> code -- the `predicate` reload behind the layout check, the lambda class test materialised to 0/1, the
> CharSequence class / coder tests of each `charAt` prefix (a per-call prefix cannot be hoisted; a loop-level
> String receiver proof would be the fix, owned by the loop-residency / charAt-node-cost work); (2)
> `Matcher.<init>` (`CRATONVM_JIT_IR_SPLICE_ANEWARRAY`, default ON since the j27 user flip of 2026-10-28,
> together with `_LOOP_CALLEES`, which makes wave 13's loop-callee rule inert by default; see "Round rx27 wave 1
> (lane R)"); (3)
> `Pattern.matcher` stays a call because its `synchronized` compile guard is a catch-all handler row
> (`callee-exception-table`), which needs a never-taken monitor region to splice as a trap; (4) `matchonly`'s
> receiver-resolution chain move (W13-6). Recipe: the table above (`cmd //c start "" //b //wait //affinity FFFF
> <exe> -cp <cls> R24RegexSplitRows <row> 1 3`), or `tools/probes/icount2.sh` on the Linux host; per row
> `CRATONVM_DBG=jit-compiled,tierup-decline,ir-bailout` and `CRATONVM_DBG_JITC=1` first.

> **Round 25 (2026-10-24/25): the Matcher rows are 3.5-5.5x HotSpot by instructions (from 5.0-9.3x); see
> "Round 25 waves 8-12", "Round 25 result", "Round 25 wave 6" and "Round 25 wave 7". Still open.**
>
> **Measured on n23-w4 (a380a3bc69), JIT r23 wave 5, lane L6, 2026-10-21 -- not retired.** `R23ArraysEqRegex`
> `--jdk-only` n23-w4 / HotSpot (ns/op): `regex-matches` 2 055 / 295, `regex-find-email` 3 686 / 373, `regex-find-ci`
> 2 290 / 226, `regex-split` 17 349 / 1 604, `regex-replaceall` 6 018 / 1 019, `regex-groups` 5 784 / 331. Facts equal
> HotSpot in both modes (battery). The Matcher-row fix landed, but those rows are still 10-17x, so the page stays open
> for both groups.

(Former title: The regex rows are 5-80x HotSpot, and the gap is per-call cost, not exact-subset coverage.)

## Round rx27 wave 4b (lane chain, 2026-10-28; pending build): chain entry, `Matcher.group`, predicate pairs

Three opt-in switches for the profiler's chain items (`rx27-w4-per-row-instruction-breakdown-20261028.md`). Each
lets a nested call that the stricter nested rules refused splice like a hot top-level call:

* `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`: `Matcher.match`'s `matchRoot.match` into `BmpCharPropertyGreedy`
  (`regex-matches`);
* `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`: an in-loop nested call is hot (`Matcher.group` on `regex-groups`;
  `Matcher.<init>` under `Pattern.matcher`);
* `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES`: two predicate lambdas at a greedy node's shared `predicate.is`
  site (`regex-find-email`, `regex-matches`).

Evidence, census, `icount2` arms (`def ent loop pred all allc hs`) and tests:
`rx27-chain-matcher-entry-group-and-predicate-pairs-20261028.md`.

## Round rx27 wave 4 (lane chain, 2026-10-28; pending build)

**What changed: `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE`** (opt-in; inert without `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`).
This builds what `rx27-chain-regex-groups-spine-is-deeper-than-the-builder-frame-cap-20261028.md` said it would
take, so that `regex-groups`' second `Greedy` can be spliced with its whole `\w` chain:

* the builder's splice-frame cap goes from 8 to 12 (in chain trees, the unset nest depth goes to 11 plus the
  leaf tier);
* the inline resume budget goes from 9 to 13, on both the jit side and the VM side (`deopt_resume.rs`, a
  minimal edit in lane matcher's file);
* a transitive chain link rule: a link whose in-loop row keeps a call in its own tree is refused
  (`half-spliced row`).

Details and limits are on that page, "Wave 4".

**Engagement.**

```
CRATONVM_JIT_IR_CHAIN_DEEP_SPINE=1 CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=650 CRATONVM_DBG_JITC=1 \
  /root/r25-bin/cvm-$T -cp /root/rx27/p R24RegexSplitRows count regex-groups 20000 20000 2>&1 \
  | grep -E 'chain-deep-spine splice|half-spliced row|ir-chain-tree .*Matcher\.search|nest-static java/util/regex/ASCII\.getType'
```

What to expect:

* `chain-deep-spine splice at pc=.. depth=9..12` lines;
* an `ir-chain-tree ... in java/util/regex/Matcher.search` line with `max_depth` above 6;
* `getType` read as `SPLICED`;
* any `half-spliced row` refusal names the link the transitive rule kept as a call.

Without the switch, none of the `chain-deep-spine` lines print.

**A/B (`icount2.sh`).**

```
DIR=/root/rx27/p REPS=5 tools/probes/icount2.sh rx27f4 "regex-groups regex-find-ci regex-find-email regex-matches matchonly" \
  def:$T:CRATONVM_JIT_IR_CHAIN_DEEP_SPINE=0 \
  ds:$T:CRATONVM_JIT_IR_CHAIN_DEEP_SPINE=1 \
  ds650:$T:CRATONVM_JIT_IR_CHAIN_DEEP_SPINE=1,CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=650 \
  ds650c:$T:CRATONVM_JIT_IR_CHAIN_DEEP_SPINE=1,CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=650,CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS=1 \
  t650:$T:CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=650 \
  hs:hs:
```

**Expected, by reading.**

* `ds` alone: about flat. The 325-byte tree cap still holds the second group, and the transitive rule may
  refuse some links it used to admit.
* `ds650` / `ds650c`: this is the arm that splices the second group. `regex-groups` should gain if the links
  are frequent enough (the frequency and frame rules still judge them).
* `t650` isolates the tree cap from the depth.
* Watch `regex-find-ci`: `SliceI`'s case-folding helper may now read as half-spliced. If it loses, the
  `half-spliced row` lines on that row name the link.

**Batteries.**

* `R24RegexSplitRows facts` in both modes, with `ds650c`.
* The jit lib suite (three new tests, named on the page), and the vm lib suite (`deopt_resume.rs` changed).
* A deopt-heavy probe with the switch on: the deeper resume budget is new runtime territory. For example
  `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE=1` on the 66-probe facts battery, then grep `resume budget` in the
  stderr: it must stay silent.

## Round rx27 wave 3 (lane chain, 2026-10-28; pending build)

**Wave-1 verdicts (rx27b2, orchestrator).**

* `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE`: flat on every row (`regex-split` median 133.5k against 135.9k).
  It stays opt-in.
* The global `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE`: `regex-groups` 0.92x and `regex-matches` 0.96x. That
  is below the 15 % bar, and the switch opens a pair at every nested virtual site of every compile.

**What changed: `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS`** (opt-in; inert without
`CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`). It gives the SPLICE19-4 nested two-class row to dispatch-chain sites
only (`GroupTail.match` pc 70, `LastNode` / `BmpCharProperty` 50 / 49 %), without the global switch. The
global switch was read in four places, and each now asks `r25_w7b_chain_splice::nested_pair_admitted(chain_site)`
(the builder asks `nested_pair_switch_on`):

* the VM resolver's `pair` arm (`jit_bridge.rs`, `chain_site`);
* the planner strip (`ir_splice_strip_unadmitted_exact_nested`, `row_is_chain`);
* the nested append (`append_ir_inline_site`);
* the builder's nested second-class gate (`IrBuilder::bimorphic_second_class`). The planner has already
  dropped every non-chain pair by then.

Rows, guards and miss edges are SPLICE19-4's, unchanged. The chain link, frequency and frame rules still
judge each body of the pair.

**Engagement.**

```
CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS=1 CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-$T -cp /root/rx27/p \
  R24RegexSplitRows count regex-groups 20000 20000 2>&1 \
  | grep -E 'nest-profiled .*callee_pc=70 .*guard on classes|ir-nested-bimorphic-splice|ir-chain-tree .*Matcher\.search'
```

Expect a `guard on classes A+B` line at `callee_pc=70` and `ir-nested-bimorphic-splice` lines. Without the
switch there is no `guard on classes` line at pc 70. With the global switch on, other compiles print pairs
too; with only this switch, only chain sites do.

**A/B.**

```
DIR=/root/rx27/p REPS=5 tools/probes/icount2.sh rx27f3 "regex-groups regex-find-ci regex-find-email regex-matches regex-split" \
  def:$T:CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS=0 \
  cbl:$T:CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS=1 \
  nbs:$T:CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1 \
  cbl650:$T:CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS=1,CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=650 \
  hs:hs:
```

**Expected.** `cbl` should give about `nbs`'s `regex-groups` gain (0.92x), since the pair at pc 70 is the only
chain pair on that row. On other rows and workloads it should be flatter than `nbs`, which also opens
non-chain pairs. `cbl650` also lets the second group's `BmpCharProperty` / `GroupHead` links in past the tree
cap. Neither is likely to clear the 15 % bar alone. Batteries: `R24RegexSplitRows facts` in both modes with
`cbl`; the jit lib suite (new test
`r25_w7b_chain_splice::tests::rx27_chain_a_chain_sites_pair_is_kept_without_the_global_switch`).

**Not changed, by design: the builder frame cap.** Raising `ir::MAX_IR_SPLICE_DEPTH` does not help
`regex-groups`. See `rx27-chain-regex-groups-spine-is-deeper-than-the-builder-frame-cap-20261028.md`: one
more frame would splice the second `Greedy` with a per-character lambda call. Enough frames would pass
`deopt::MAX_OSR_INLINE_RESUME_DEPTH` (9, coupled to the VM's `deopt_resume`).

## Round rx27 wave 2 (lane bimodal, 2026-10-28; built and measured as `cvm-bimodal-3`)

**Finding.** On rx27b0 / rx27b2 every Matcher/split row ran in one of two instruction levels per process
(same binary, same env). The deciding call is the IR planner's profile-guarded splice of
`Pattern$BmpCharPropertyGreedy.match` (class 931) into `Pattern$Start.match` at pc 44. The
keep-loop-splice-caller rule (`CRATONVM_JIT_IR_KEEP_LOOP_SPLICE_CALLER`, default ON since the j27 flip)
refuses it when the callee's *published* artifact has an in-loop guarded row the copy does not reproduce.
Which body is published when `Start.match` compiles is a compile-order race: the C1 body has no IR rows
(splice: low mode), the C2 body has rows at pc 21 (`charAt`, String) and pc 70 (`next.match`, class 922)
(refuse: high mode, `Start.match` = 5008 bytes, the scan loop pays a call per position). Neither row is
really lost in the copy: pc 21 is answered by the inline String prefix (`chain-prefix-kept`), and pc 70 runs
on ~190 per mille of the body's entries (the nested chain rule's own `rare site` verdict). Not a thin
profile: the high run reads `ir-profile-guard-miss ... pc=44 ... obs=2675 types=1 top=931:2675`.

**Change** (`jit_bridge.rs`, `resolve_inline_site_from`'s keep-loop block): a lost row no longer counts when
it is a `java/lang/String` row at a `chain-prefix-kept` pc (`CRATONVM_JIT_IR_KEEP_LOOP_STRING_PREFIX`), or
when its site is rare by `chain_frequency_verdict` over the callee's receiver samples / invocations
(`CRATONVM_JIT_IR_KEEP_LOOP_RARE_ROW`). Both default ON, `=0` kills. Census (`jit/src/lib.rs`): one
`ir-profile-guard-miss` line per virtual site without a one-class guarded splice.

Engagement: `CRATONVM_DBG_JITC=1 cvm-bimodal-3 ... R24RegexSplitRowsQ countq cached ... | grep -E
'keep-loop-(string-prefix|rare-row)|ir-profile-guard .*Start.match'` -- both lines name
`BmpCharPropertyGreedy.match` pc 21 / pc 70, and `Start.match` gets `ir-profile-guard ... class id 931`.

icount2 (`REPS=5`, main/op, arms `fix:bimodal-3 off:bimodal-3:CRATONVM_JIT_IR_KEEP_LOOP_STRING_PREFIX=0,
CRATONVM_JIT_IR_KEEP_LOOP_RARE_ROW=0 base:rx27b2`; `ic2-bim-ab2.log`):

| row | fix (5/5 values) | off | base rx27b2 |
|---|---|---|---|
| `cached` | 45 226 (42.8-46.6k) | 61 027 (one 45k) | 62 186 (one 46.9k) |
| `split-ws` | 49 562 (49.5-50.4k) | 66 180 (one 49.6k) | 65 893 |
| `regex-split` | 99 606 (99.0-100.0k) | 136 231 (one 101k) | 135 595 (one 103k) |
| `regex-find-email` | 37 955 (37.9-39.2k) | 46 297 (one 38.1k) | 45 086 |
| `matchonly` | 28 959 (27.1-31.3k) | 28 959 | 28 959 |

Every row the race decided is now in its low mode on 5/5 processes (-16 to -27 % against base). Other rows,
`REPS=3`, fix / base: regex-matches 16 425 / 16 538, regex-find-ci 26 907 / 26 907, regex-replaceall
64 195 / 64 276, regex-string-matches 33 773 / 33 773, split-comma 6 557 / 6 689, split-comma-10 6 306 /
6 307, compile 4 394 / 4 386, list 3 827 / 3 815, regex-groups 27 896 / 27 200 (both arms 25.5-29.2k, a
separate spread). `matchonly` keeps three levels in every arm: another decision, see
`rx27-bimodal-matchonly-and-groups-spread-20261028.md`. Batteries still needed: the R24 facts in both modes,
the jit / types / vm suites (types: regenerate the flag docs).

## Round rx27 wave 1 (lane R, 2026-10-28; pending build)

Review of the code serving this page's and r23-w5's rows at `df86fe90ba`. No row-moving change.

* **STRRX `indexOf(int, int)` prefix** (`emit_str_index_of_char_from_arm` and the shared scan helpers): matches
  JDK 25 for both coders, the `[0, len]` clamp, negative / invalid / supplementary needles and lone
  surrogates; the `indexOf(int)` arm is byte-equivalent to dev's (only the pair scan's scratch moved from R11 to
  EAX). Registered in `flag_groups.rs` and both flag docs (stale "to be registered" claims fixed).
* **Fixed:** the W11-4 bundle (`CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS`, default ON since the j27 flip) honoured only
  `=0` as a kill of a bundled key; `=false` / `=off` / `=no` read ON. Test
  `rx27_the_bundle_yields_to_every_off_word`.
* **Filed:** `rx27-R-anewarray-loop-callees-flip-makes-w13-6-inert-20261028.md` (the j27 flip of
  `_SPLICE_ANEWARRAY_LOOP_CALLEES` undoes W13-6, last measured `cached` +12.6 %) and
  `rx27-R-effective-defaults-differ-from-inventory-20261028.md` (six keys listed opt-in that are ON through a
  parent).
* Round-25 flips reviewed without a provable defect: `NESTED_LAMBDA_IFACE` (exact class guard, the interface
  call on the miss edge), `LAMBDA_ADAPTER_SUPERSEDE` (thunks keyed by entry and artifact id, withdrawn under
  `mutation` after the `superseded` flag, two-check pairing at build), `SPLICE_ARRAYCOPY_ROW_BOUND` (gated on the
  inline copy). The splice-chain switches were read for their gates only, not re-proved.

Batteries: jit lib (`r25_w11_4_chain_loop_proofs::tests`), types flag tests (no row change).

**Triage of the opt-in switches named on the two pages** (still `runtime_flag_on` at `df86fe90ba`):

| switch | default read at | last measured (page) | verdict may have moved? | `icount2.sh` arm |
|---|---|---|---|---|
| `CRATONVM_JIT_IR_REG_ALLOC` | `ir_lower.rs` `ir_reg_alloc_enabled` | W8-4/W9-2: "neutral" | yes: `LS_CALLER_SAVED`, `SPLICE_SLOT_SHARE` now ON (j27 flip) | `ra:TAG:CRATONVM_JIT_IR_REG_ALLOC=1` |
| `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS` | `ir_optimize.rs` `r25_w12_4::inner_loop_proofs_enabled` | W12-4: not measured | already ON via the bundle; arm is the kill | `noinner:TAG:CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS=0` |
| `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` | `ir_optimize/charat_trap.rs` `seq_charat_hoist_enabled` | W13-4: "the hoist is the loss" (W11-4 bundle +1-6 %) | yes: lane A's scan-loop hoisting | `hoist:TAG:CRATONVM_JIT_IR_SEQ_CHARAT_HOIST=1` |
| `CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND` | `ir_optimize/seq_charat_predicate.rs` `w7d::entry_bound_enabled` | left out with the hoist (W13-4) | as above | `bound:TAG:CRATONVM_JIT_IR_SEQ_CHARAT_HOIST=1,CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND=1` |
| `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_PROFILE` | same file, `w7d::profile_enabled` | wave 7 (W7-D): "inside the repeat band on every row" | no | `prof:TAG:CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_PROFILE=1` |
| `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE` | `ir.rs` `ir_nested_bimorphic_splice_enabled` | W7-B "nested pairs": `regex-matches` 22 432 vs 18 157 (loss) | yes: chain cost rules W8-5/W10-3 since | `pair:TAG:CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1` |
| `CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE` | `ir.rs` `ir_nested_guarded_interface_enabled` | not measured on these rows (`NESTED_LAMBDA_IFACE` took the lambda half) | yes | `iface:TAG:CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE=1` |
| `CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE` | `ir.rs` `ir_polymorphic_guarded_splice_enabled` | n23-w6 side observation, not flipped | yes (chain splice ON) | `poly:TAG:CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE=1` |
| `CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP` | `ir.rs` `ir_splice_nested_profile_trap_enabled` | wave 12: opt-in, unmeasured here | yes | `ptrap:TAG:CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1` |
| `CRATONVM_JIT_IR_SPLICE_CALLEE_PROFILE_SEED` | `lib.rs` `ir_splice_callee_profile_seed_enabled` | W7-B round 3: off, not measured | yes (rare-link rule) | `seed:TAG:CRATONVM_JIT_IR_SPLICE_CALLEE_PROFILE_SEED=1` |
| `CRATONVM_JIT_IR_LAZY_REF_HOMES` | `ir_lower.rs` `ir_lazy_ref_homes_enabled` | round 25: "no measured contribution" | yes: `ELIDE_UNNAMED_HOMES` ON since round 26 | `lazy:TAG:CRATONVM_JIT_IR_LAZY_REF_HOMES=1` |
| `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR` | `ir_lower.rs` `ir_shadow_frame_block_lazy_clear_enabled` | round 25: no measured contribution; the clear's page retired by count | no | `lclear:TAG:CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR=1` |
| `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED` + `_SAVE_BANDS_SIZED` | `ir_lower.rs` `ir_deopt_image_sized_enabled`, `ir_save_bands_sized_enabled` | wave 12 (w11a, `matchonly` ns/op): 2765 / 2972 vs default 2791 / 2550, "none is a clear lever" | no | `sized:TAG:CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1,CRATONVM_JIT_IR_SAVE_BANDS_SIZED=1` |
| `CRATONVM_JIT_IR_SB_PREFIX` | `lib.rs` `ir_sb_prefix_enabled` | r23-w5: releases the throw-arm pin only | yes (the cold-arm skip is ON through ROW_BOUND) | `sb:TAG:CRATONVM_JIT_IR_SB_PREFIX=1` |
| `CRATONVM_JIT_IR_STRING_VALUE_OF_CALL` | `lib.rs` `ir_string_value_of_call_admits` | r23-w5 wave 6: opt-in, `STRING_CALL_ADMIT` took the gain | no | `vof:TAG:CRATONVM_JIT_IR_STRING_VALUE_OF_CALL=1` |
| `CRATONVM_JIT_SP_CODE_POINT_SCALAR` | `lib.rs` `sp_code_point_scalar_enabled` | r23-w5: "only a different timing" (`compile`) | no | `spcp:TAG:CRATONVM_JIT_SP_CODE_POINT_SCALAR=1` (`compile` row) |
| `CRATONVM_REGEX_PATTERN_COMPILE_CACHE` | `native-builtins/src/regex_matcher.rs` `pattern_compile_cache_enabled` | never a candidate: breaks `Pattern.compile` identity vs HotSpot | n/a | none (parity) |

`CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY`, `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS`,
`CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE` / `_SPLICED` / `_FIELD_CLASS_PREDICATE` read opt-in but are ON by default
(page above); their arm is `=0`. Suggested one-host run: `icount2.sh rx27-triage all hs:hs: def:TAG: <arms>`,
`REPS=5`, with the new arms interleaved per the host notes.
## Round rx27 wave 1 (lane scanloop, 2026-10-28; pending build)

Item (1) of the j27 box: the spliced `Pattern$Start.match` / `BmpCharPropertyGreedy` scan loop, about 75
instructions per character.

**What the loop is, per character.** On the `\s+` rows the inner scan loop runs about one iteration per text
position (a non-space ends it). So the per-character code is the OUTER position loop's body, and anything planted
at the inner loop's entry runs once per character too. That is why W12-4's inner field-class predicate (P2) is a
small win only, and why rx27 builds no inner-loop hoist. The outer loop's invariants, items (a) and (b), are blocked
by two writers. One is the predicate's miss call, a second-level key P2 plans only in a later round. The other is
the back-off `next.match` that the chain rule keeps as a call. Both are on
`rx27-scanloop-outer-position-loop-invariants-20261028.md` (OPEN).

**Changed.**

* `CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING` (opt-in), item (c). A trapping `charAt` whose receiver node is the
  operand of a licensed loop-entry `Guard(ExactClassIs(recv, String))` drops the prefix's per-call null, kind-tag
  and class-id tests: `TEST/JZ`, `TEST/JNE`, `CMP/JNE`, 6 instructions. It adds no value, live range or guard.
  This is what separates it from the W11-4 hoist, which lost 3-8 %.
  * Code: `ir_optimize::charat_trap::seq_charat_receiver_proven_string`, read by `ir_lower`
    `try_emit_ir_string_search_prefix`.
  * Not applied when either OSR predicate refusal is off.
  * Tests: `rx27_a_guarded_charat_receiver_is_proven_a_string`,
    `rx27_the_receiver_is_not_proven_without_its_premises`.
* Census lines, no behaviour change:
  * `ir-scan-loop`: one line per `charAt` loop, with spliced, calls, trapping, receiver proven, elided, entry
    guards, class-test branches and ref field reads left;
  * `ir-seq-charat-proven-string`: one line per trapping call, elided or kept;
  * `ir-fused-class-test`: one line per lowering. It counts the `If`s on an `ExactClassIs` that are fused, or
    refused by the switch, another block, a second use or frame name, or a clobbered input. This is item (b)'s
    existing fix, `CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH` with `CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH`
    (round 18, both opt-in), never measured on these rows.
* Probe `tools/probes/Rx27ScanLoopSubjects.java`. It warms `\s+` / `\w+` on Strings, then runs StringBuilder,
  CharBuffer, a user CharSequence and UTF16 subjects. Its facts must equal HotSpot's.

**Engagement** (`B=/root/r25-bin/cvm-<tag> P=/root/rx27/p`):

```
for a in 0 1; do CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING=$a CRATONVM_DBG_JITC=1 \
  $B --jdk-only -cp $P R24RegexSplitRows count split-ws 250000 750000 2>&1 \
  | grep -E 'ir-(scan-loop|seq-charat-proven-string|fused-class-test) java/util/regex/Pattern\$(Start|BmpCharPropertyGreedy)' \
  | sort | uniq -c; done
```

Expected with `=1`:

* `ir-seq-charat-proven-string ... elided` for `Start.match`, and no `kept` line there;
* `ir-scan-loop ... receiver proven=1 elided=1` on both of its loops;
* with `=0`, the same `receiver proven=1` and `elided=0`.

A `receiver proven=0` names a loop without the String guard: read the `ir-seq-charat-predicate` line. Repeat with
`CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH=1 CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH=1`: `fused=` should move
off 0, or the refusal counts name the reason.

**A/B** (`DIR=/root/rx27/p tools/probes/icount2.sh rx27-scanloop split-ws,cached,matchonly,regex-split,regex-groups,regex-matches,regex-find-email,regex-find-ci <arms>`):

```
base:<tag>:CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING=0
ps:<tag>:CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING=1
fct:<tag>:CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH=1,CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH=1
both:<tag>:CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING=1,CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH=1,CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH=1
hs:hs:
```

**Expected.**

* `ps`: about 6 instructions less per `charAt` executed in a proven loop. That is about -700 per op on `split-ws`
  (about 120 calls), about 1 %, and similar small shares on `cached` and `matchonly`. A row whose loop is not
  proven (`regex-groups` if its loop is still withheld) does not move.
* `fct`: a few instructions per fused class test, and only where the census shows `fused` > 0.

Neither closes the 3x gate. The outer-loop page is the larger lever.

**Batteries.**

* jit lib (`charat_trap` tests), types tests (flag surface).
* `R24RegexSplitRows facts` and `Rx27ScanLoopSubjects` in both modes, `ps=1`, equal to HotSpot.
* The 66-probe facts battery with `ps=1`.
## Round rx27 wave 1 (lane alloc, 2026-10-28; pending build): allocation on the Matcher rows

By reading tip, nothing on these rows should call Rust per allocation on ZGC. `new Matcher`, its `int[]` arrays
and its `IntHashSet[]` take the inline bump with the start-bit store when the class is a compact post-init
no-op. The wave-13 `jit_new_object` / `jit_post_alloc_init` share of `matchonly` came from an older build. A
per-site census now names any site that still calls: `CRATONVM_DBG_JITC=1` prints
`ir new-site|newarray-site ... call=<reason> method=<...>`. Host commands, expected lines and the per-row
estimate (allocation is about 5-8 % of the CratonVM instruction count, not the lever for the 3x gate) are in
`r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md` "Round rx27 wave 1". The residual
per-object cost is on `rx27-alloc-zgc-compiled-allocation-is-30-instructions-over-hotspot-20261028.md`. There is
no switch and no A/B.
## Round rx27 wave 1 (lane chain, 2026-10-28; pending build)

Lane F of round rx27: the chain / planner side of the Matcher rows. Not built or measured in the lane.

### What changed

* **`CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE`** (opt-in, `jit/src/lib.rs` `mod rx27_loop_root`, wired in the
  splice planner around `append_ir_inline_site`). The W13-6 premise as a rule, not as the `anewarray` trigger:
  a top-level splice tree whose rows under a loop (a row whose call is in a loop of its enclosing body, and
  every row below it) the append could not hold is rolled back, so the loop-rooted callee stays a call and keeps
  its own compile. The planner counts those rows three times:
  * `resolved`: as the resolver built them, less the String-prefix carve-out and the append's static
    refusals (both bind the callee's own compile too);
  * `planned`: after this compile's nested strips;
  * `spliced`: read back from the site table the append wrote.

  It keeps the call when `spliced < planned`: the caller's budget (or the nested cold reserve) ran out inside
  the loop. That is the W13-6 `cached` case (`Pattern.split`'s loop moved up to `left=181`, and
  `Matcher.find` / `subSequence` became calls).
* **`CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS`** (opt-in, inert alone): also keep the call when
  `spliced < resolved`, that is, when a nested strip of the caller's compile dropped an in-loop row. This is
  the reading for W13-6 `matchonly` if its "3 pruned branches" at `Matcher.search` pc 121 are in
  `Start.match`'s loop. The census tells.
* **`CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES`** (knob, default 325 = `FreqInlineSize`, clamped to 1 300):
  `strip_chain_tree`'s per-tree cap, for the `regex-groups` A/B (W7B-2, below).

Bimorphic and polymorphic top-level sites, synchronized bodies and self-recursion plans are not judged. The rollback is the one a
refused bimorphic second body already takes, so the rule can only keep a call.

### Census (`CRATONVM_DBG_JITC=1`)

```
[cratonvm-jitc] ir-loop-root-own-compile <callee> at pc=P in <root>: in-loop rows resolved=R planned=N spliced=S budget_left=B -> kept as a call (compiled as its own root) | spliced (this compile holds its in-loop rows)
```

One line per judged site (a tree with at least one row under a loop). Host check (`$T` = the build's tag):

```
cd /root/rx27/p
for row in cached matchonly; do for s in 0 1; do
  CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1 CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES=1 \
  CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE=1 CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS=$s CRATONVM_DBG_JITC=1 \
  /root/r25-bin/cvm-$T -cp . R24RegexSplitRows count $row 20000 20000 2>&1 \
  | grep -E 'ir-loop-root-own-compile|ir-chain-tree .*Matcher\.search|inline-plan java/util/regex/Pattern\.split' \
  > lr-$row-$s.txt
done; done
```

* `cached`, `lr-cached-0.txt`: a `kept as a call` line for `Pattern.split(Ljava/lang/CharSequence;IZ)` in
  one of `Pattern.split(CharSequence)`, the row's lambda or `spin`, with a small `budget_left`. The
  `inline-plan` line for `Pattern.split(...IZ)` is back with its own sites.
* `matchonly`: the line for `Pattern$Start.match` at `pc=121 in java/util/regex/Matcher.search`.
  * `spliced` with `resolved > planned` in `lr-matchonly-0.txt`, and `kept as a call` in `-1`: W13-6's
    matchonly cost is in-loop rows that the caller's nested strips dropped, and the STRIPS arm is the fix.
  * `resolved == planned == spliced`: the cost is not lost rows, and this rule does not reach it.
* The rule's arithmetic is covered by the unit tests below.

### A/B (`icount2.sh`)

```
DIR=/root/rx27/p REPS=5 tools/probes/icount2.sh rx27f "cached matchonly regex-matches split-ws regex-split" \
  anew:$T:CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1 \
  lr:$T:CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1,CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES=1,CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE=1 \
  lrs:$T:CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1,CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES=1,CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE=1,CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS=1 \
  def:$T:CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE=0 \
  lrdef:$T:CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE=1 \
  hs:hs:
DIR=/root/rx27/p REPS=5 tools/probes/icount2.sh rx27f-groups "regex-groups regex-find-ci regex-find-email" \
  def:$T:CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=325 \
  tree650:$T:CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=650 \
  pair:$T:CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1 \
  pair650:$T:CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1,CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES=650 \
  hs:hs:
```

Expected, by reading:

* `lr` against `anew`:
  * `cached` returns to `def` (W13-6's `anew` cost +12.6 %, which lane B's `anewarray` rule also recovers);
  * `regex-matches` keeps `anew`'s 0.96x (`Matcher.<init>` has no loop).
* `matchonly`: unchanged under `lr`. Under `lrs` it returns to `def` only if the census shows in-loop strips.
* `lrdef` against `def`: flat on these rows. The default build has no `anewarray` loop root to move, and the
  rule fires only where an in-loop row was lost. The battery row below checks that the rule is not too broad
  elsewhere.
* `tree650` and `pair` on `regex-groups`: small at best (see W7B-2 and W7B-3 below). `regex-find-ci` must keep
  its chain win.

If `lr` holds `cached` at `def` and `regex-matches` at `anew`, lane B's `anewarray` loop-callee refusal can be
replaced by this rule (page `rx27-chain-loop-root-rule-can-replace-the-anewarray-loop-refusal-20261028.md`).

### W7B-2, W7B-3 and `Pattern.split` self (read, not fixed)

* **W7B-2: the nest budget on `regex-groups`' second group.** Not charging static leaf intrinsics a level
  would no longer move the row:
  * since W10-3b, the chain's `charAt` is a kept prefix call, so there is no `StringLatin1.charAt` or
    `checkIndex` in the tree;
  * since W10-3, the depth in a chain tree is the builder's frame cap (7, plus the leaf tier).

  The bounds now are, in order:
  1. the 325-byte tree cap (the knob above);
  2. the spine itself: `Start` 0, `GroupHead` 1, `Greedy` 2, `GroupTail` 3, `BmpCharProperty` 4,
     `GroupHead` 5, `Greedy` 6. The second `Greedy`'s `predicate.is` forwarder at depth 7 is not a static
     leaf, so that `Greedy` keeps a non-link call and is refused as a link.

  Page `rx27-chain-regex-groups-spine-is-deeper-than-the-builder-frame-cap-20261028.md`.
* **W7B-3: `GroupTail` pc 70 (50 / 49 %).** Nothing blocks it but the opt-in
  `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE`.
  * With the switch on, the resolver resolves both classes through the chain link rule and the frequency rule
    (`jit_bridge.rs`, the `pair` arm), and the own-artifact refusal stops, because pc 70 then has a row.
  * It has not been measured alone since c9, where `B2` also set the budget and depth knobs. Use the `pair`
    arm above.
  * The switch is global (resolver, planner x2, builder), so scoping it to chain sites needs a builder change.

  Page `rx27-chain-grouptail-pair-is-gated-by-the-global-nested-bimorphic-switch-20261028.md`.
* **`Pattern.split` self, 29 % of `cached`.** Page `rx27-chain-pattern-split-self-per-element-work-20261028.md`
  lists what the compiled body still does per element (from the JDK 25 source), with the `perf annotate`
  command that settles it.

### Tests (`jit` lib)

* `rx27_loop_root::tests`:
  * `rx27_rows_under_a_loop_are_the_in_loop_row_and_everything_below_it`;
  * `rx27_the_spliced_count_reads_back_what_the_append_wrote`;
  * `rx27_a_loop_root_that_lost_an_in_loop_row_keeps_its_call`;
  * `rx27_the_loop_root_switches_are_opt_in`;
  * `rx27_a_tree_with_no_row_under_a_loop_is_not_judged`.
* `r25_w7b_chain_splice::tests::rx27_chain_the_tree_cap_is_a_knob_for_the_ab`.

### Batteries

* `R24RegexSplitRows facts`, in both modes, for every arm with the switch on.
* The `types` integration tests (three new flag rows).
* The jit lib suite.
* A broad compile row with `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE=1` (Spring Boot or
  `R16AllocPremium hashmap-cached`), counting the `kept as a call` lines. The rule judges every loop-rooted
  tree, not only regex trees.
## Round rx27 wave 1 (lane matcher, 2026-10-28; pending build)

Two changes on the per-`Matcher` fixed cost (j27 box items (2) and (3)). Nothing built or run here.

### 1. `Pattern.matcher` splices: its compile guard becomes a deopt at the branch

`CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1` (opt-in, token `ir-splice-cold-handler-arm`).

`Pattern.matcher` was refused `callee-exception-table (handlers: catch-all)`. JDK 25's body (javac):

```
 0 aload_0; 1 getfield compiled; 4 ifne 32
 7 aload_0; dup; astore_2; 10 monitorenter
11 aload_0; getfield compiled; ifne 22; aload_0; invokevirtual compile
22 aload_2; monitorexit; 24 goto 32
27 astore_3; aload_2; monitorexit; aload_3; 31 athrow        rows [11,24)->27 any, [27,30)->27 any
32 new Matcher; dup; aload_0; aload_1; invokespecial <init>; astore_2; aload_2; areturn
```

Every row and its handler lie in the fall-through arm `[7, 32)` of the `ifne` at 4. `compiled` is true after
the constructor, so the profile says the arm never ran.

* **Resolver** (`resolve_inline_site_from`, `jit_bridge.rs`). Only for a table with a catch-all row (the handler
  splice refuses those outright, so no body it admits changes route), IR tier, not a synchronized method.
  `cratonvm_jit::ir::cold_handler_arm_mask` picks the innermost conditional branch whose fall-through arm holds
  every row's range and handler and is entered only by that fall-through. It masks the spliced copy's arm to
  `aconst_null; nop..; athrow`. That is a cold throw arm, so the existing cold-throw pass takes it, and no walk
  sees `monitorenter`, `monitorexit`, `compile()` or a row. After `drop(cm)` the profile must show the branch
  jumped at least 8 times and never fell into the arm (`splice_cold_handler_arm_unexecuted`, the same bar and
  code-length stamp as `splice_typecheck_arm_unexecuted`). Otherwise the body is refused.
* **Builder.** Nothing new. `IrBuilder::prune_cold_throw_arm` turns the branch into `Op::Guard` (it fails exactly
  when the branch would fall into the arm) and walks the masked arm as dead. If it cannot (no chain snapshot, a
  synchronized splice window, a rotated body), the walk reaches the masked `athrow` and refuses the splice,
  which keeps the call. It never throws `null`.
* **Refusals in the shape**, each a `splice-cold-handler-arm REFUSED` census line: `jsr`/`ret`; no such arm; a
  jump into the arm; another `athrow` that is not a cold throw arm; a back edge outside the arm (the W13-6 rule:
  a loop root keeps its call); a handler that reads a local which the liveness without rows calls dead at a pc
  outside the arm (see below).

**Soundness.**

* *JVMS monitors (2.11.10; 6.5 `monitorenter` / `monitorexit`).* Compiled code executes no instruction of the
  arm. The guard deopts at the branch (bci 4), before `monitorenter`, so at the deopt point the callee frame
  owns no monitor and nothing has been locked or unlocked. The interpreter re-executes the `ifne` with the
  `compiled` value from the snapshot, falls into the arm and runs `monitorenter`, the body, and either the
  normal `monitorexit` or the handler's `monitorexit; athrow`. Every monitor the arm enters is exited by the
  same interpreter frame, so structured locking holds. A synchronized method is refused as before (its monitor
  spans the whole body), and the guard is never planted inside a synchronized splice window.
* *Deopt state.* The guard resumes through the chain snapshot at the top of bci 4: the callee at its own bci,
  operand stack `[compiled]`, over every caller parked mid-`invoke` (the cold throw arm's mechanism; with
  anything committed before it, the guard requires the chain). The chain snapshot's locals are narrowed by the
  spliced body's liveness over its UNMASKED code with the ADMITTED handler rows (`lib.rs`
  `spliced_body_codes`), and this body admits none. So `cold_handler_arm_mask` computes both liveness tables
  (`regalloc::live_locals_per_pc_all`, with and without the rows) and refuses unless, at every pc outside the
  arm, each local live with the rows is live without them. In `Pattern.matcher` the handler reads locals 2 and
  3, both stored inside the arm.
* *Exceptions.* The interpreter runs the method's own code and exception table, so a `compile()` that throws
  unlocks and propagates exactly as on HotSpot. No row of the callee covers compiled code.
* *A wrong "never".* A pattern whose `compiled` is false (deserialized) traps once at the branch. The
  interpreter's profile then counts the fall-through and the recompile refuses the arm: a call, as before.

A known adjacent defect, not new here: after a JVMTI redefinition of the callee's class the VM resumes a chain
scope from the spliced copy, which is the wrong code for any masked body
(`rx27-matcher-redefined-chain-resumes-a-masked-splice-copy-20261028.md`).

Expected: on `regex-matches` (eight `matcher(s).matches()` per op) the `Pattern.matcher` call and frame go.
`Matcher.<init>` / `reset()` (spliced into `Pattern.matcher` since W10-6, default ON at j27) now splice into the
row's lambda next to `matches()`, where the `Matcher` is a fresh `Op::New`. Estimate 3-8 % fewer instructions
per op there; about 1 % on the one-`matcher()` rows (`regex-find-*`, `regex-groups`, `matchonly`); nothing on
the split rows (their `matcher()` site is inside `Pattern.split`'s own loop-root compile). Risk: with the
`Matcher` fresh in the lambda, `parentPattern.root` may resolve and a `Node.match` chain (a loop root) may move
into the lambda. That is the `matchonly` mechanism of W13-6
(`rx27-matcher-loop-root-moves-into-a-caller-by-receiver-resolution-20261028.md`). Read the census below on
both arms before reading the numbers.

### 2. `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES` back to opt-in (the `cached` loss)

`CRATONVM_JIT_IR_SPLICE_ANEWARRAY` is default ON at j27 and stays so. The review found nothing to fix in it:
the resolver rows come from the access-checked `resolve_jit_new_site`; the rebase checks that every `anewarray`
has its row; masked arms and stripped copies are scanned consistently; the builder's `0xbd` arm and replay
fences are unchanged; a synchronized splice window admits no `anewarray`.

But the j27 mass flip (`WARNING j27-user-flip-20261028`) also turned `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES`
ON. That switch is W13-6's A/B arm that restores wave 10's admission of a looping `anewarray` callee, which is
the measured `cached` loss (+12.6 %). With it ON the W13-6 refusal never fires. It is opt-in again
(`runtime_flag_on`, registry `off_word: None`). `Matcher.<init>` and `ArrayList.grow` have no back edge and keep
splicing. The `matchonly` loss in the same table is a different mechanism (receiver resolution), which this
rule does not reach; see the page named above.

### Confirm on the host

```
B=/root/r25-bin/cvm-<tag>; P=/root/rx27/p
# 1. Pattern.matcher admitted, the guard planted
CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1 $B -cp $P R24RegexSplitRows regex-matches 3 1 2>&1 \
  | grep -E 'splice-cold-handler-arm|inline-resolve REFUSED java/util/regex/Pattern.matcher|cold throw arm TRAPPED.*callee bci 4,'
#   want: splice-cold-handler-arm java/util/regex/Pattern.matcher(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;
#         branch_pc=4 arm=7..32 ... and no `REFUSED ... Pattern.matcher ... callee-exception-table`.
#   default arm: REFUSED ... Pattern.matcher ... callee-exception-table (handlers: catch-all)
#   the same run prints `[ir] cold throw arm TRAPPED at combined pc ... (callee bci 4, arm 7..=31) in <lambda>`
# 2. the loop callee kept as a call on `cached` (default arm)
CRATONVM_DBG_JITC=1 $B -cp $P R24RegexSplitRows cached 3 1 2>&1 \
  | grep -E 'ir-splice-anewarray-loop-callee|inline-plan java/util/regex/Pattern.split\(Ljava/lang/CharSequence;IZ\)'
# 3. the guard must not keep firing (a handful at most during warm-up)
CRATONVM_DBG_DEOPT=1 CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1 $B -cp $P R24RegexSplitRows regex-matches 3 1 2>&1 \
  | grep -c 'Pattern.matcher'
# 4. the matchonly-style move, both arms: which compile carries the root.match chain
CRATONVM_DBG_JITC=1 [CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1] $B -cp $P R24RegexSplitRows <row> 3 1 2>&1 \
  | grep -E 'inline-plan|ir-splice-nested|chain' | grep -E 'Matcher\.(search|match)|Pattern\$Start\.match|lambda\$row\$'
```

A/B: `DIR=/root/rx27/p tools/probes/icount2.sh rx27m "regex-matches regex-find-email regex-find-ci regex-split
regex-groups split-ws cached matchonly" <arms>` with

* `j27:<tag>:CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES=1` (the j27 default before this wave)
* `def:<tag>:CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1` (this wave's default; expect `cached` about 11 % lower than `j27`)
* `arm:<tag>:CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1`
* `hs:hs:`

Before a flip of `ir-splice-cold-handler-arm`: the R24 facts lines (14) equal HotSpot in `--jdk-only` and
`--compatible` with the switch on; the jit lib suite with the switch exported; the types integration tests (new
key in `types/tests/flag-surface.txt`); the deopt and monitor batteries the cold throw arms run. Tests added in
`jit/src/ir.rs`, `mod rx27_cold_handler_arm_tests`: the JDK 25 `Pattern.matcher` bytes mask to one cold throw arm
with no monitor byte; the profile rule; a row outside the arm, a jump into it, a loop and a handler-read local
each refuse; the builder splices a masked monitor arm as a `Guard` at its branch, with no `MonitorEnter`,
`MonitorExit`, `Throw` or `If`. Updated: `r25w13_the_loop_callee_switch_is_opt_in`,
`r25w13_only_a_looping_anewarray_callee_stays_a_call`.

### Wave 3 verdict (lane matcher, 2026-10-28): the cold handler arm engages and saves nothing; withdrawn as a lever

Host (rx27b2, icount2, 4 reps): the switch engages (`splice-cold-handler-arm ... branch_pc=4 arm=7..32 ... masked`)
and `regex-matches` is flat (16 496 with it, 16 476 default). Why, read from the code:

* The cost the page attributed to `Matcher.<init>` (a 720-byte frame, its clear, the non-fresh field tests, ZGC
  gates) was already removed by W10-6, default ON at j27: `Matcher.<init>` and `reset()` splice into
  `Pattern.matcher`, where `new Matcher` is the same fresh `Op::New` the lambda would see. Moving that body one
  level up changes no field test, no store gate and no allocation.
* What the arm removes is one compiled call per `matcher()`: the call, a small prologue / epilogue and the return,
  about 20-30 instructions, eight per op, so 1-1.5 % of 16.5k: inside the spread of four reps. The guard adds back
  a field load and a compare per `matcher()`.
* Nothing downstream got cheaper: `matches()` re-reads the `Matcher`'s fields after its own calls, so knowing the
  object is fresh in the lambda buys no fold there.

So the switch stays opt-in and is NOT a flip candidate. The code stays (it is sound, and a lazily-initialized
`synchronized` guard is a common JDK shape); drop commits `2d354e564f` and `1492b0a907` if an unused switch is not
wanted. One host check separates "built and worthless" from "bailed": with the switch on, in the same run,
`CRATONVM_DBG_JITC=1 ... regex-matches 3 1 2>&1 | grep -E 'cold throw arm TRAPPED.*callee bci 4,|ir-bail|ir_build_bail'`
should show the TRAPPED line in the lambda compile and no bail at the masked `athrow`.

## Round 25 waves 8-12 (2026-10-25, orchestrator): two structural flips; rows 3.5-5.5x HotSpot; still OPEN

**Measurement changed.** Waves 8-12 measure with `tools/probes/icount2.sh` and `R24RegexSplitRowsQ` (W9-7,
`docs/internal/jit-proposals/r25-w9-deterministic-icount-20261025.md`): one process warms, waits until the compile
clock stops moving, then counts `instructions:u` on the Java main thread only for N and 3N ops, 3 interleaved reps,
medians. The same arm now repeats within 0.4-4 %. The old two-process count moved up to 26 % between repeats of
the default arm, because background compiles and their timing were counted.

**Flipped default ON** (each with a `=0` kill switch):

| switch | lane | what it does |
|---|---|---|
| `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE` | W10-4 | an interface call inside a spliced lambda body (`CharPredicate.union`'s `is(ch) \|\| p.is(ch)`) takes the profiled nested row |
| `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE` | W7-B, W8-5, W10-3, W10-3b | `Node.match` chains splice; links pay for the frame they add (W8-5); the nest cap no longer pushes a chain's callees out (W10-3, `_NESTED_CHAIN_DEPTH`); a chain body keeps the String-prefix `charAt` call (W10-3b, `_CHAIN_STRING_PREFIX_KEPT`); a loop-rooted tree stays hot (`_CHAIN_LOOP_ROOTED_HOT`) |
| `CRATONVM_JIT_LAMBDA_ADAPTER_SUPERSEDE` | W10-5 | fix: a lambda-adapter thunk in an inline cache is re-pointed when its implementation body is superseded (callers kept entering the C1 body through its not-entrant stub) |

Instructions per op, host build c28 (both flips default) against the same build with both killed, and HotSpot 25:

| row | HotSpot | both `=0` | default (c28) | ratio to HotSpot |
|---|---|---|---|---|
| `regex-matches` | 3 798 | 17 784 | 17 568 (c29) | 4.6x |
| `regex-find-email` | 9 679 | 57 431 | 37 524 | 3.9x |
| `regex-find-ci` | 7 330 | 51 172 | 26 248 | 3.6x |
| `regex-split` | 31 668 | 112 781 | 111 920 | 3.5x |
| `regex-groups` | 5 765 | 31 618 | 31 266 | 5.4x |
| `split-ws` | 12 594 | 55 554 | 55 909 | 4.4x |
| `cached` | 11 771 | 50 131 | 50 053 | 4.3x |
| `matchonly` | 7 019 | 31 000 | 30 734 | 4.4x |

Batteries with both flips (c28): every `R25*` probe prints HotSpot's facts in both modes under ZGC, G1 and
generational; the 66-probe facts battery equals the default arm's (only `R22W10G1Ladder`'s HotSpot-side timing fact
differs, in both arms); the GC battery is clean; `NativeGrowthReclaimProbe` on G1 passes 5 of 5 on each arm. All
four suites pass on c30 (types, jit statics ratchet, vm lib, jit lib 6 448 / 0).

**Kept opt-in** (measured, no flip): `CRATONVM_JIT_IR_REG_ALLOC` with increment 2 and direct ALU destinations
(W8-4, W9-2; neutral), `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE` / `_ALL_REFS` (W8-6, W9-4; neutral: the chain rule
already ranges splice-local values), `CRATONVM_JIT_IR_SPLICE_ANEWARRAY` (W10-6; `cached` +12.6 %),
`CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY` with the String-pin release (W10-2; neutral),
`CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS` (W11-4; +1-6 %, the hoist costs more than it saves),
`CRATONVM_JIT_IR_OSR_RECHECK_CLASS_PREDICATES` (W9-5; neutral). `CRATONVM_JIT_IR_SPLICE_ARRAYCOPY_ROW_BOUND`
(W11-2) and `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS` (W12-4) were not yet measured when this was written; see
the sections below for their host commands.

**What the profiles say is left** (`perf record -e instructions:u` with `CRATONVM_JIT_PERF_MAP=1`, c22):
`Pattern$Start.match` and the spliced scan loop (about 75 instructions per character against HotSpot's 8-12:
per-character `predicate` reload behind the layout check, the lambda class test materialised to 0/1, the String
class and coder tests), `Matcher.<init>` (17 % of `regex-matches`), and single-pass bodies still running in steady
state (`Arrays.copyOfRange` pinned by its String-intrinsic, `String.checkBoundsBeginEnd`,
`Preconditions.checkFromToIndex`, `ConcurrentHashMap.addCount`;
`docs/internal/fixed-bugs/r25-w10-steady-state-single-pass-bodies-FIXED-20261027.md`). The 3x gate is not reached.
(2026-10-27: most of those steady-state `[c1]` readings were a perf-map artifact, where a freed
single-pass body's line named the body that reused its range. The `copyOfRange` splice,
`CRATONVM_JIT_IR_SPLICE_ARRAYCOPY_ROW_BOUND`, is now default ON. See that page's "Closure".)

## Round 25 wave 13 (W13-6, 2026-10-25; pending build): a loop-rooted `anewarray` callee stays a call

### Measured (orchestrator, c26, instructions per op, default / `SPLICE_ANEWARRAY=1`)

| row | default | switch on | change |
|---|---|---|---|
| `regex-matches` | 17 713 | 17 057 | 0.96x |
| `split` | 116 499 | 119 198 | +2.3 % |
| `split-ws` | 55 947 | 57 573 | +2.9 % |
| `cached` | 50 302 | 56 664 | +12.6 % |
| `matchonly` | 31 000 | 33 579 | +8.3 % |

The other rows were inside the noise.

### `cached`: the split loop moved up into a caller whose budget it had already spent

The host capture (c31, `CRATONVM_DBG_JITC=1`) shows this:

* **Default arm.** `Pattern.split(CharSequence,int,boolean)` is refused for splicing
  (`new/anewarray/multianewarray`: its tail is `new String[resultSize]`). It compiles as its own
  optimizing-tier root and plans 21 of its 30 sites (20 in-loop) in its own 1 300-byte budget:
  * `Matcher.find` 250 bytes;
  * `subSequence` 145;
  * `ArrayList.add` 59 three times;
  * the rest, up to 1 181 bytes.
* **Switch arm.** The same body becomes spliceable, so it moves up into its callers:
  * into `Pattern.split(CharSequence)` (1 111 bytes);
  * then into the row's lambda (1 119 bytes, `left=181`);
  * then through `Row.run` into `spin` (1 134 bytes, 49 bodies).

  The loop's own sites no longer fit under the budget the caller has left. `Matcher.find` (13.3 %) and
  `String.subSequence` (9.8 %) become calls, and the `Pattern.split` symbol (26 %) disappears.

Hypotheses 1-3 of W13-6's first answer were wrong. The cost is not `ArrayList.grow` and not the fresh
receiver: a loop root lost its own compile.

### Fix: `splice_anewarray_refuses_loop_callee` (under `CRATONVM_JIT_IR_SPLICE_ANEWARRAY`)

The VM resolver keeps an `anewarray`-bearing callee as a call when its body has a back edge
(`bytecode_analysis::back_edges`). Such a callee keeps its standalone compile, with the whole budget
for its in-loop sites. The refusal prints as:

```
[cratonvm-jitc] inline-resolve REFUSED <callee> depth=D: ir-splice-anewarray-loop-callee (N anewarray site(s), E back edge(s); kept as a call, compiled as its own root) [tier=ir ...]
```

`Matcher.<init>` and `ArrayList.grow` have no back edge and stay admitted, so `regex-matches` keeps its
gain. The rule only ever refuses, and only under the switch.
`CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES=1` (opt-in, inert alone) restores wave 10's admission for
an A/B on one binary.

Tests: `jit/src/ir.rs`, `mod r25w13_splice_anewarray_loop_callee_tests`. They check that the switch is
opt-in, that the looping fixture has its one back edge, and that only a looping `anewarray` body is kept
as a call (not a straight one, not a body without `anewarray`, not under the opt-in).

To check on the host, use the same capture as before with `SPLICE_ANEWARRAY=1`:

* `grep ir-splice-anewarray-loop-callee` names `Pattern.split(Ljava/lang/CharSequence;IZ)`;
* `inline-plan java/util/regex/Pattern.split(Ljava/lang/CharSequence;IZ)` is back with its 21 sites;
* `cached` is back to the default arm.

### `matchonly`: a different mechanism, not fixed here

`matchonly` has no `anewarray` callee with a loop. The captured change is in `Matcher.search`'s
`root.match` site (pc 121):

* default: 27 bytes, 1 body;
* switch on: 244 bytes, 7 bodies, a profile guard on class 592, a chain tree and 3 pruned branches.

`Start.match`'s share rises from 56 % to 64 %, and spin OSR's falls from 25 % to 18 %. The likely cause
is that with `Matcher.<init>` spliced, the `parentPattern` the constructor stored is visible to the root,
so the receiver of `parentPattern.root.match` resolves. That admits a larger chain splice of `Start.match`
(itself a loop root) at that site. This is a loop root moving into a caller again, but through
receiver resolution, not through the `anewarray` admission, so the rule above does not reach it.

Lines needed to confirm before any fix, from both arms of `matchonly`, with `CRATONVM_DBG_JITC=1` (add
`CRATONVM_DBG_DEOPT=1` in a second run):

```
grep -E 'inline-plan|ir-splice-bytes|ir-splice-refused|inline-resolve REFUSED|ir-splice-nested|chain|pruned|guard' \
  | grep -E 'Matcher\.search|Pattern\$Start\.match|Pattern\.matcher|Matcher\.<init>|lambda\$row\$'
```

The `in <root>` suffix of the `pc=121` lines names which compile now carries the chain. The deopt lines
show whether its profile guard misses.

## Round 25 wave 10 (W10-6, 2026-10-25; pending build): `Matcher.<init>` is a call because it allocates `IntHashSet[]`

On c22 (default switches, `perf record -e instructions:u`), `regex-matches` attributes 17.0 % of JIT
instructions to `java/util/regex/Matcher.<init>(Pattern,CharSequence)` as its own compiled symbol. That is
about 3 000 instructions per op, or about 375 per `Matcher`, since an op runs eight `matcher(s).matches()`.
HotSpot spends about 3 800 instructions on the whole op.

### What the body is, and why it is a body at all

JDK 25 `Matcher(Pattern, CharSequence)` stores ten field initialisers and `parentPattern` / `text`. It then
allocates `new int[parent.capturingGroupCount * 2]`, `new int[parent.localCount]` and
`new IntHashSet[parent.localTCNCount]`, and calls `reset()`. `reset()` runs three `-1` fill loops (2, 0 and
0 iterations for `[a-z]+\d*`), seven field stores and `to = getTextLength()` (`text.length()`). There is
no loop worth the name and nothing intrinsic. The cost is a full compiled activation's worth of fixed
cost, plus a dynamic shape test at every field access, because `this` arrives from another body:

* prologue, 720-byte frame and its fresh-frame clear, callee-saved saves, frame record publication;
* a layout check per field site on the non-fresh receiver;
* the ZGC store gate on each of the five reference stores. A store into an object the same body
  allocated owes none (`zgc_fresh_receiver_stores`), but here the allocation is in the caller;
* three inline TLAB bumps, each with the ZGC start-bit announce;
* the return.

Evidence (a STALE local binary, `C:\craton\cratonvm\target\release\cratonvm.exe` built 2026-10-03, run on
the Windows box with `CRATONVM_DBG_JIT_DISASM='java/util/regex/Matcher.<init>'` and `CRATONVM_DBG_JITC=1`):

* The IR body is 11 497 bytes, with `sub rsp,2D0h` and a looping clear.
* 27 field sites each open with `cmp dword [rel epoch],0` and `test byte [rax+7],4`, with a legacy arm and
  a helper call behind them. Item 4's proven-receiver fields (default ON in round 25) take most of the
  per-site test out at tip, but not the call, the frame or the ZGC gates.
* Every array allocation is a bump plus a 30-instruction start-bit sequence.

The refusal that makes it a call is the same in that binary and at tip. In the optimizing compile of
`Pattern.matcher`:

```
[cratonvm-jitc] inline-resolve REFUSED java/util/regex/Matcher.<init>(Ljava/util/regex/Pattern;Ljava/lang/CharSequence;)V depth=0: new/anewarray/multianewarray
```

The VM's splice resolver (`resolve_inline_site_from`, `vm/src/runtime/interpreter/jit_bridge.rs`) admits
`new` for the optimizing tier but refuses `anewarray` and `multianewarray` for both tiers. Primitive
`newarray` was never refused. `IrBuilder`'s `0xbd` arm has handled a spliced `anewarray` since round 11
(`note_splice_local_alloc`; the `a_spliced_aastore_of_a_caller_reference_taints_the_local_array` test
builds one). What was missing is only the row plumbing from the resolver to the builder. The same refusal
keeps `ArrayList.grow` out of `add` (r23-w5 page) and every constructor that sizes an `Object[]`.

### What W10-6 builds: `CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1` (opt-in)

* **Resolver.** Under the switch and in IR mode, each `anewarray` of the callee is collected and its
  component class resolved with `resolve_jit_new_site`, the resolver the caller's own `anewarray` rows use
  (`cp_new_resolver`). An unresolved component refuses the body (`ir-splice-anewarray-unresolved`).
  `anewarray` initialises nothing (JVMS 6.5: it only resolves the component class), so the `new` rows'
  class-init and finalizer rules do not apply. The rows travel as `InlineSite::ir_anewarray_info`.
* **Rebase.** `append_ir_inline_site` rebases the rows into `IrInlineTables::anewarray_info` (combined
  pcs), and `merge_ir_inline_tables` / `IrBuilder::apply_inline_tables` merge them into the builder's
  `anewarray_info` after the caller's own (disjoint pcs). `ir_splice_anewarray_rows_ok` refuses the body
  (it keeps its call) when the rows are not exactly the body's `anewarray`s with non-zero class ids, or
  when the switch is off. A missing row would bail the whole METHOD at the builder's `0xbd` arm.
* Nothing else changes. The lowering of a spliced `Op::NewArray { element_type: 0 }` is the caller's
  own (`anewarray_object` stub or the inline bump). The replay fences already count `0xbd` as a
  trapping opcode (`ir_splice_opcode_cannot_trap`), and the builder stays the soundness gate for a trap
  after a committed store. With the chain frame states on (default), a trap inside the splice resumes the
  callee at its own bci.

Expected effect on `regex-matches`: `Matcher.<init>` and `reset()` splice into `Pattern.matcher`, which
removes the activation and its frame clear. Its field stores then go to the caller's fresh `Op::New`
(no null test, no layout test; the fresh-receiver ZGC store rule where no GC point intervenes), and the
`Matcher.<init>` perf symbol disappears. `Pattern.matcher` itself stays a call out of the row's lambda
(`callee-exception-table`: its `synchronized (this)` compile guard).

### Confirm in one host run

```
CRATONVM_DBG_JITC=1 [CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1] cratonvm -cp tools/probes R24RegexSplitRows regex-matches 3 1 2>&1 \
  | grep -E 'inline-resolve REFUSED java/util/regex/Matcher.<init>|inline-plan java/util/regex/Pattern.matcher|spliced .* into java/util/regex/Pattern.matcher|ir-splice-anewarray'
```

Default arm: the `REFUSED ... new/anewarray/multianewarray [tier=ir ...]` line. Switch arm: no such line,
and Pattern.matcher's `inline-plan` names the spliced bodies. Any `ir-splice-anewarray-*` reason names what
refused. Then compare instructions per op (`count regex-matches N N` vs `N 3N` under `perf stat -e
instructions:u`) for both arms, and run the `perf record` again: `Matcher.<init>` should be gone from the
symbol list.

For the residual inside whichever body keeps the code, read it per instruction:

```
CRATONVM_JIT_JITDUMP=1 perf record -k 1 -e instructions:u -o m.data -- cratonvm ... regex-matches 3 1
perf inject --jit -i m.data -o m.jit.data
perf annotate -i m.jit.data --stdio -s 'java/util/regex/Pattern.matcher(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;'
```

The dump goes to `$JITDUMPDIR/jit-<pid>.dump` (default `/tmp`). Use
`CRATONVM_DBG_JIT_DISASM='java/util/regex/Pattern.matcher'` for the static listing.

Before any flip: the facts battery in both modes, the jit lib suite with the switch exported, and the GC
battery. A spliced `anewarray` moves an allocation site into the caller, which matters for the ZGC
start-bit and armed-site clears.

### Tests

`jit/src/lib.rs`, `mod r25w10_splice_anewarray_tests`:

* the switch is opt-in;
* a resolved row is rebased to the combined pc;
* a row on another opcode, a missing row, a duplicate or a class id of 0 refuses the body, and so does
  any row with the switch off. No table gains a row;
* a body without `anewarray` needs no switch;
* the builder end to end: a spliced `anewarray` with its row in the inline tables builds one reference
  `Op::NewArray` of that component class, and without the row the build bails.

### What is left on the Matcher rows after it

* `Pattern.matcher` stays a call (its `synchronized` compile guard is an exception table). Splicing it
  needs the `callee-exception-table` refusal lifted for a monitor region on a never-taken arm: `compiled`
  is true after the first call, so the profile says the arm never ran.
* `Pattern.lambda$Range$0` and `CharPredicates.lambda$ASCII_DIGIT$0` (lane W10-4) and the C1-stuck
  `ConcurrentHashMap.addCount` (another lane) are the next symbols on the profile.

## Round 26 (2026-10-24): the branch and unnamed-home pages retired

`r25-constant-and-redundant-branches` and `r25-elide-unnamed-homes` are FIXED and moved to
`docs/internal/jit-proposals/` (`...-FIXED-20261024.md`): the four branch folds, the OSR flag fold (with
its store), the two home-elision switches and the new split-if extensions (partly constant phis, value
phis, decided-across-join branches) are default ON. Instructions per op against dev `5925c9543e`: the
Matcher rows 0.90-0.95x (`matchonly` 0.90, `regex-groups` 0.92, `regex-find-email` 0.93), `split-ws` 0.92,
the native-regex rows unchanged. Still OPEN here: the rows are still above 3x HotSpot; the remaining
cost is the frame anatomy and register-allocation pages listed below.

## Round 25 result (2026-10-24, orchestrator): 11 switches default ON, rows 0.68-0.85x of round 24 by instructions; still OPEN

Build: `jit-r25-arch` merged with dev `3076317260`, release with LTO off, on the Linux host (16 cores,
load 10-25). Probe: `tools/probes/R24RegexSplitRows.java`, user-mode instructions per op as
(I(3N) - I(N)) / 2N under `perf stat -e instructions:u` (repeats within 5-8 %). `base` is dev `c89d11b473`.
Facts equal HotSpot 25 in both modes (the 66-probe battery: 114 SAME, and the same 18 HotSpot-side DIFF
lines as the base, with the round-25 switches on and with all of them at `=0`).

| row | HotSpot | base | round 25 | ratio to HotSpot (base -> r25) |
|---|---|---|---|---|
| `regex-matches` | 3 806 | 26 293 | 17 997 | 6.9x -> 4.7x |
| `regex-find-email` | 9 977 | 79 644 | 62 739 | 8.0x -> 6.3x |
| `regex-find-ci` | 7 334 | 68 191 | 55 488 | 9.3x -> 7.6x |
| `regex-split` | 31 558 | 157 809 | 118 083 | 5.0x -> 3.7x |
| `regex-replaceall` | 28 183 | 64 933 | 65 204 | 2.3x -> 2.3x |
| `regex-groups` | 7 195 | 38 781 | 33 024 | 5.4x -> 4.6x |
| `regex-string-matches` | 13 037 | 36 889 | 36 477 | 2.8x -> 2.8x |

`regex-replaceall` and `regex-string-matches` run the native Rust regex (`string_regex_exact`) and are
already inside 3x; no round-25 change reaches them.

**What landed default ON (R25-FLIP, each `=0` kills).** Redesign item 2: `CRATONVM_JIT_IR_SHADOW_LIVE_ACROSS_CALL`,
`_SHADOW_LIVE_RECEIVER`, `_SHADOW_FRAME_BLOCK_PRECISE` (the frame block under precise liveness, the largest
single contribution). Item 3: `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`. Item 4:
`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS` (+ `_SPLICED`, `_DOMINANCE`). Regex-rows lane:
`CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS`, `CRATONVM_JIT_IR_SPLICE_PAST_ARRAYCOPY`,
`CRATONVM_JIT_PUBLISH_OVER_SUPERSEDED_CALLEE`, `CRATONVM_JIT_PIN_BAKED_CALLEES_WHILE_COMPILING`. With all eleven
at `=0` every row returns to the base (0.90-1.09x). Evidence and the not-flipped table: `docs/internal/jit-proposals/r25-optin-flip-triage-20261024.md`.

**Not flipped (no measured contribution, kept opt-in).** Item 1a `CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES`, item 1b
`CRATONVM_JIT_IR_LS_CALLER_SAVED`, the frame-anatomy claims, the four branch folds and
`CRATONVM_JIT_IR_OSR_FLAG_IF_FOLD`, `CRATONVM_JIT_IR_LAZY_REF_HOMES`, `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR`.
Item 2's remaining half (register-named roots across real calls) stopped at analysis: without an unwinder a
Rust frame between the caller and the collection hides the register.

**Where the time is now** (`regex-groups`, perf samples over JIT code): home stores 23 %, the fresh-frame
clear 22.6 %, branches 12 %, shadow traffic 2.4 %. The clear's share did not survive a direct count
(2026-10-25): emitting the clear twice adds 0.4-4.6 % instructions and 0-9 % wall time per op on every row, so
the 22.6 % was sample attribution, and the clear's page is retired by measurement
(`docs/feature-designs/ir-frame-anatomy.md`). Retired: `r25-elide-unnamed-homes` and
`r25-constant-and-redundant-branches` (round 26), item 4's residuals
(`docs/internal/fixed-bugs/r25-item4-proven-receiver-fields-open-work-FIXED-20261024.md`), and, by measurement on
2026-10-24, item 1b (`docs/internal/jit-proposals/r25-ls-caller-saved-gprs-RETIRED-20261024.md`: the caller-saved
file is never full and 90-93 % of the home reloads are references) and item 2's residuals
(`docs/internal/jit-proposals/r25-shadow-publication-residuals-after-item-2-RETIRED-20261024.md`).
The 3x gate is still not reached on the Matcher rows; the page stays open.

## Round 25 wave 7 (2026-10-25, orchestrator): the structural levers; one real gain, kept opt-in

Wave 6 showed no local lever moves the rows, so wave 7 took the structural ones named above, each behind its own
opt-in switch:

| lane | switch | page / section |
|---|---|---|
| W7-A, IR register allocation (increment 1 of 7) | `CRATONVM_JIT_IR_REG_ALLOC` | `docs/internal/jit-proposals/r25-w7-ir-register-allocator-RETIRED-20261025.md` |
| W7-B, `Node.match` chains spliced | `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE` | "Wave 7 (W7-B)" and "W7-B round 2/3" on this page |
| W7-C, frame-record republish after tail calls | `CRATONVM_JIT_IR_TAIL_CALL_REPUBLISH_SINK` | "Wave 7 (W7-C)" on `docs/internal/jit-proposals/r25-shadow-publication-residuals-after-item-2-RETIRED-20261024.md` |
| W7-D, charAt predicate in spliced loops, field-class and entry-bound guards | `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED`, `_PROFILE`, `CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE`, `CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND` | "Wave 7 (W7-D)" on `docs/internal/jit-proposals/r25-constant-and-redundant-branches-FIXED-20261024.md` |

Each lane went back to its agent at least once after a host build (test fixes, two real bugs in W7-A's admission,
three rounds of W7-B's link rules). Instructions per op on the last build, c12 (method as in "Round 25 result"):

| row | HotSpot | default | W7-A | W7-B | W7-B + nested pairs | W7-A + W7-B |
|---|---|---|---|---|---|---|
| `regex-matches` | 4 551 | 18 069 | 19 911 | 18 157 | 22 432 | 19 880 |
| `regex-find-email` | 9 381 | 61 510 | 60 809 | 61 813 | 59 392 | 64 347 |
| `regex-find-ci` | 8 996 | 59 065 | 55 844 | **38 220** | **33 713** | **33 246** |
| `regex-split` | 29 381 | 120 246 | 119 196 | 128 450 | 135 328 | 125 004 |
| `regex-groups` | 4 217 | 32 684 | 36 120 | 34 427 | 31 916 | 32 202 |
| `split-ws` | 11 401 | 56 862 | 58 959 | 64 912 | 63 705 | 63 105 |
| `cached` | 10 674 | 49 697 | 49 966 | 60 401 | 59 438 | 55 718 |
| `matchonly` | 7 074 | 31 019 | 33 116 | 36 507 | 36 549 | 33 587 |

* **W7-B is the one real gain of the round:** `regex-find-ci` 0.57-0.65x (its `Start` -> `Branch` -> `SliceI` chain is
  rooted by two loops). It also costs `cached` 1.22x, `matchonly` 1.18x and `split-ws` 1.14x, so it is not flipped. Three
  rounds of link rules (loop rooting, a 325-byte tree cap, a 25 % site-frequency floor, refusing a link whose own artifact
  splices its next call) removed the `Pattern.compile` / `study` chains from the split rows, but not the losses.
* **W7-A engages** (7-11 values site-named and 12-15 of 32-49 definition stores dropped on `BmpCharPropertyGreedy.match`
  and `Start.match`), and the rows do not move. Removing stores alone does not buy instructions on these bodies; what is
  left is call results (`op(Call)`), the frame clear and the calls themselves.
* **W7-C and W7-D** stay inside the repeat band on every row.

With every wave-6 and wave-7 switch on, the jit lib suite has only the three failures dev has, the 66-probe facts
battery matches the default build line for line, and the GC battery is clean.

**Where this leaves the page.** The regex rows are 3.5-8x HotSpot by instructions and the 3x gate is not reached.
The data of waves 6 and 7 says the remaining cost is the call tree itself: every `Node.match` link is a compiled
call with a full frame (setup, clear, publication, write-through homes). Splicing removes the call but grows the root's
frame for every position, so the trade only pays when every spliced link is hot, which `find-ci` has and the others do
not. Closing the gap needs the root frame to stop paying for cold spliced links: frame slots shared between disjoint
splices (colour reuse across inlined bodies), splices outlined back to calls on their cold paths, and the register
allocator's later increments (call results and references in registers). Those are the open items on the W7-A and
W7-B pages.

Wave 8 (W8-6) builds the first of these, opt-in: `CRATONVM_JIT_IR_SPLICE_SLOT_SHARE` shares the frame words of disjoint splices,
references included. It also assesses cold-splice outlining. Both are in `docs/feature-designs/ir-frame-anatomy.md`.

## Round 25 wave 6 (2026-10-24, orchestrator): five more levers, none moves the rows

Wave 6 sent one lane at each remaining cost named above, plus a bug lane and a review lane. Each lane's
design, soundness argument and residuals are on its page. Every switch below is opt-in, and every lane's
debug census confirmed that it engages on the regex bodies (`CRATONVM_DBG_JITC=1`, `regex-groups` and
`split-ws`).

| lane | switch(es) | page | engagement on the regex rows |
|---|---|---|---|
| W6-A, fresh-frame clear | `CRATONVM_JIT_IR_FRESH_CLEAR_COVER` | `docs/feature-designs/ir-frame-anatomy.md` | `Matcher.search` 18 -> 17 instructions per entry |
| W6-B, home stores | `CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES` (with both item-1a switches) | `docs/internal/jit-proposals/r25-elide-unnamed-homes-FIXED-20261024.md` | `ir-home-why` census on every body |
| W6-C, branches | `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE` (+ `_SEQ_CHARAT_HOIST`) | `docs/internal/jit-proposals/r25-constant-and-redundant-branches-FIXED-20261024.md` | one loop planted (`Start.match`) on `split-ws`, none on `regex-groups` |
| W6-D, per-call prologue | `CRATONVM_JIT_IR_LEAF_ENTRY_POLL_ELIDE`, `CRATONVM_JIT_IR_LEAF_FRAME_ELIDE` | `docs/internal/jit-proposals/r25-shadow-publication-residuals-after-item-2-RETIRED-20261024.md` | 19 entry polls elided and 5 leaf frames on `regex-groups`; no refusal |
| W6-E, field proofs | `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_COESTABLISH` (`_OSR_RECHECK` was later dropped: dev's item-4 residual `_PREHEADER` subsumes it) | `docs/internal/fixed-bugs/r25-item4-proven-receiver-fields-open-work-FIXED-20261024.md` | `coestablished=1` on 5 bodies, `osr_owed` on 4 |

Instructions per op on build c3 (`jit-r25-arch` with every lane merged), same method as "Round 25 result".
Each lane arm turns on only that lane's switches; `all` turns on every one of them.

| row | HotSpot | default | W6-C | W6-A | W6-D | W6-E | W6-B | all |
|---|---|---|---|---|---|---|---|---|
| `regex-matches` | 3 871 | 17 487 | 18 978 | 17 682 | 17 112 | 18 703 | 17 104 | 18 624 |
| `regex-find-email` | 9 090 | 62 423 | 63 225 | 61 751 | 65 143 | 62 423 | 61 399 | 60 791 |
| `regex-find-ci` | 7 321 | 56 358 | 55 488 | 54 464 | 53 585 | 55 452 | 55 128 | 58 764 |
| `regex-split` | 31 656 | 119 576 | 119 576 | 113 800 | 120 013 | 117 093 | 119 166 | 122 373 |
| `regex-groups` | 5 489 | 32 471 | 32 666 | 32 508 | 33 502 | 33 834 | 31 262 | 32 339 |
| `split-ws` | 12 418 | 58 438 | 58 082 | 57 814 | 58 778 | 59 036 | 59 337 | 59 407 |
| `split-comma` | 2 831 | 9 963 | 9 954 | 10 624 | 10 310 | 10 087 | 10 322 | 9 929 |
| `compile` | 808 | 5 554 | 5 354 | 5 178 | 5 243 | 5 541 | 5 415 | 5 193 |
| `cached` | 11 445 | 51 143 | 54 653 | 52 431 | 51 361 | 52 050 | 52 978 | 53 808 |
| `matchonly` | 7 019 | 32 317 | 33 945 | 34 047 | 33 855 | 34 133 | 32 272 | 38 517 |

No arm moves any row by the 15 % the flip rule needs. Every difference is inside the method's 5-8 % repeat
band except `all` on `matchonly` (1.19x, a loss). Nothing was flipped. With every wave-6 switch on, the
66-probe facts battery matches the default build line for line, and the GC battery is clean.

**What wave 6 shows.** The lanes each removed what they targeted: a few clear stores, some phi and header
stores, the guards of one charAt loop, an entry poll, a repeated receiver proof. The rows did not move. Per
call these bodies execute few instructions of each kind, spread over many short calls (`BmpCharPropertyGreedy`
scans 1-5 characters per call). The cost is the sum of every write-through home, every per-activation frame
setup and every guarded call, not one dominant category. No local lever left on these pages reaches the 3x
gate. Closing it needs the redesign this page's round-24 section named, done as one piece: values kept in
registers across the whole body (a real register allocator over the IR, with homes written only at deopt
and GC points), and splices deep enough that `Node.match` chains stop being calls.

**Bugs found and fixed in wave 6** (default behaviour, no switch):
* `ThreadMXBean.getThreadInfo` reported a parked thread's state from a later `Thread.getState()` call, not
  the instant its stack was read. HotSpot never pairs the state and stack that way (W6-F,
  `docs/internal/fixed-bugs/r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024.md`).
* A lambda proxy answered `instanceof` / `checkcast` by interface name across class loaders (W6-R,
  `docs/internal/jit-proposals/r25-w6-review-flipped-set-20261024.md`).
* `superseded_callee_still_callable` admitted a callee compiled before a full cache flush (W6-R, same page).
* `plan_site_named_homes` admitted phis with `CRATONVM_JIT_IR_DROP_PHI_HOME=0` (W6-B, found by the unit
  suite run with the switches exported).

## Wave 7 (W7-B, round 25, 2026-10-24; pending build): `Pattern$Node.match` chains splice as dispatch-chain links

Lane W7-B of round 25 wave 7: make the `next.match` chain splice in the optimizing tier where HotSpot's C2
inlines it. Switch `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE` (token `ir-nested-chain-splice`; opt-in when
written, **default ON since the 2026-10-25 wave-10 flip, `=0` kills it**, so the `=1` arms below are the
default build). Not
built or measured in the lane (the orchestrator builds); every claim below about which call stays a call
comes from reading the resolver and planner against JDK 25's `java.util.regex.Pattern` bytecode and the
earlier `CRATONVM_DBG_JITC` traces on this page (waves 8, 9, 12, round 24). The census lines below confirm
or correct it on the first build.

### The per-call inventory: which call stays a call, and why

The chains, per row (one pattern per process under `R24RegexSplitRows count <row>`, so the per-site
receiver profiles are monomorphic except where noted):

* `\s+` (`split-ws`, `cached`, `matchonly`, `regex-split`): `Matcher.search` -> `Start.match` ->
  per text position `BmpCharPropertyGreedy.match` (`is`, `charAt` per character) -> `LastNode.match`.
* `(\w+)=(\w+)` (`regex-groups`): `Start` -> `GroupHead` -> `Greedy` -> `GroupTail` -> `BmpCharProperty`
  (`=`) -> `GroupHead` -> `Greedy` -> `GroupTail` -> `LastNode`.
* `regex-find-email`, `regex-matches`: `Start` (or `Matcher.match`'s root) -> `Greedy` / `CharProperty` /
  `Curly` links; `regex-find-ci`: `Start` -> `Branch` (a loop over `conds[n].match`) -> `SliceI`.

| call (site) | runs | why it was a real call | under the switch |
|---|---|---|---|
| `Matcher.search` pc 121 -> `Start.match` | per `find` | spliced already (top-level profile guard) | unchanged |
| `Start.match` pc 44 `next.match` (in the position loop) | per text position | inside the `search` splice the nest-exact row for the constant-pool class `Pattern$Node` (27-byte body that never arrives) pre-empts the profiled route; reached, `Greedy` (89 bytes) is over the cold cap and not a leaf, so the nested leaf rule refuses it (round 24, wave 12) | contradicted exact row dropped; `Greedy` admitted as a chain link (site in a loop) |
| `Greedy.match` `predicate.is` / `seq.charAt` (scan loop) | per character | in `Greedy`'s own artifact spliced by top-level guards; one level down they are `invokeinterface` with no nested route (`CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE` opt-in, `nest-lambda-guarded` only at depth 0). This is why round 24's `NESTED_GUARDED_HOT_CAP_LEAF=0` arm lost (36.7k -> 54.8k on `matchonly`): splicing `Greedy` without them trades one call per position for two per character | a chain body's interface sites take the profiled nested row (`nest-profiled-iface`, the lambda forwarder / `String.charAt` behind a class test) |
| `Greedy.match` `next.match` (back-off loop) | per successful scan | as `Start` pc 44 | `LastNode` (45 bytes, leaf) admitted as a link (site in a loop) |
| `GroupHead.match` pc 28 `next.match` | per group attempt | nest-exact pre-emption; profiled `Greedy` non-leaf | link (entry-prefix site) |
| `BmpCharProperty.match` pc 36 `next.match` | per `=` match | nest-exact pre-emption; 185 samples < 256, non-leaf `GroupHead` | link if the row has 100 samples (HotSpot's `InlineFrequencyCount`) |
| `GroupTail.match` pc 70 `next.match` | per group end | bimorphic (`LastNode` / `BmpCharProperty`, 50 / 49 %); nested pairs only under the opt-in `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE` | the pair's bodies take the link rule with that switch on too |
| `Branch.match` `conds[n].match` (loop) | per position (`regex-find-ci`) | as `Start` pc 44 | link if the arm classes are one class (`SliceI` both arms) |
| any link at the nest budget | -- | `CRATONVM_JIT_IR_INLINE_MAX_NEST` (6) and the 1300-byte splice budget | unchanged: the link stays a call |

Not a cause here: the depth cap below the budget, recursion guards (a link reappearing, `GroupHead` twice
in `regex-groups`, is bounded by the nest budget as HotSpot bounds it by its inlining depth), and `Node.next`
not being constant-folded per receiver (the field is not final; HotSpot does not fold it either -- the
class test on the loaded receiver is what both tiers pay).

### Design (`CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`)

A **chain site** is a virtual or interface call inside a spliced body whose selector (name and descriptor)
is the body's own (`next.match(..)` inside `X.match(..)`); a **chain body** is one that holds a chain site.
Code: `jit/src/lib.rs` `mod r25_w7b_chain_splice` (rule), `vm/src/runtime/interpreter/jit_bridge.rs`
`resolve_inline_site_from` (nested resolver), `jit/src/lib.rs` planner admission
(`ir_splice_nested_exact_admitted_in`, `ir_splice_strip_unadmitted_exact_nested`, the nested hot-cap slots).

1. **Exact row yields to the profile at a chain site.** The nest-exact row of the constant-pool class is
   dropped when the callee's receiver evidence names another class (`ir_nested_receiver_evidence_contradicts`,
   the drop that was only under `CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_TRAP`), so the `nest-profiled` route plans
   the class the profile names.
2. **Chain-link admission** (`chain_link_verdict`), OR-ed with the plain nested rule: an instance body within
   the multi-body hot cap (325 bytes for one body), at a hot site (in a loop of the enclosing body, on its entry
   prefix, or 100 receiver samples), and **every call of the body is either spliced in turn (has a nested row)
   or is the chain's next link**. The last clause is the cost rule: a spliced link removes the call to it and
   keeps only the next-link call it made anyway, never one it had spliced in its own artifact (the round-24 and
   wave-12 failure). The VM resolver and the planner ask the same function of the same body and receiver row,
   so they agree; the planner's strip re-asks it after stripping a link's own rows (a link whose predicate row
   was stripped goes too, `withdrawn_after_strip`).
3. **Interface sites of a chain body take the profiled nested row** (what `CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE`
   does globally, here only inside chain bodies), so `predicate.is` (the lambda forwarder + implementation via
   the nested lambda route) and `seq.charAt` (`String.charAt`) are spliced inside a spliced `Greedy`.
4. **Slots.** The builder holds a nested guarded body to 35 bytes unless the planner registers its pc as a hot
   site, and the planner had four hot slots per compile shared with top-level sites. Under the switch the nested
   rows get 8 more (`CHAIN_NESTED_HOT_EXTRA_SITES`); the byte budget still bounds the tree.

### Soundness

Nothing new is emitted. Every link is an existing nested guarded row (`NestedInlineSite` with a guard class,
registered by the planner with `add_exact_receiver_site` + `add_nested_guarded_receiver_site`):

* an exact class-id test on the loaded receiver;
* the body resolved by JVMS selection from the guard class (`resolve_inline_site_from(Some(class))`, default
  methods included for an interface site);
* the miss edge is the site's unchanged call with its inline cache;
* the deopt state is the chain snapshot of every spliced scope. The rows exist only under chain fences, as
  `ir_splice_nested_exact_admitted_in` requires;
* a redefinition of a guard class withdraws the compile (`note_bytecode_copied`, as for every nested profiled
  row).

The rule changes which bodies are admitted and how many hot slots a compile has, never what a body or guard
means.

### Census (`CRATONVM_DBG_JITC=1`)

* `nest-chain <cp>.match... at callee_pc=P depth=D in <body> -> class C SPLICED (link body B bytes behind a class
  test)` or `-> class C refused: <why>` (switched off, static body, over the chain cap N, cold site, keeps a call
  at body pc Q that is neither spliced nor the next link): one line per chain site whose body the plain rule
  refused.
* `nest-exact Pattern$Node.match... -> class <Pattern$Node id> dropped (the receiver evidence names another class)`: step 1.
* `nest-profiled-iface Pattern$CharPredicate.is(I)Z ... -> guard on class N`: step 3.
* `ir-nested-guarded-hot-cap at combined pc=...`: step 4. If `ir-nested-hot-cap-cold-site` prints for
  `Matcher.search` pc 121, the top-level site was not hot and every link over 35 bytes was dropped.

Positive control: `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE=1 CRATONVM_DBG_JITC=1 java --jdk-only -cp tools/probes
R24RegexSplitRows count matchonly 20000 20000 2>&1 | grep -E 'nest-chain .*SPLICED|ir-nested-guarded-splice'`
must print a `nest-chain ... BmpCharPropertyGreedy ... SPLICED` line inside `Matcher.search`'s (or `find`'s)
compile. Negative control: the same with the switch unset prints no `nest-chain` line.

### A/B recipe

Instructions per op, as in "Round 25 result": `perf stat -e instructions:u` of `java --jdk-only -cp
tools/probes R24RegexSplitRows count <row> N N` and `... N 3N`, (I(3N) - I(N)) / 2N, for every row
(`regex-matches regex-find-email regex-find-ci regex-split regex-groups split-ws split-comma compile cached
matchonly`), interleaved, 3 reps. Arms:

* `off`: the switch unset;
* `on`: `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE=1`;
* `on+pair`: also `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1` (the `GroupTail` pc 70 pair);
* `on+budget`: also `CRATONVM_JIT_IR_INLINE_BUDGET=2600` and `CRATONVM_JIT_IR_INLINE_MAX_NEST=8` (does the
  `regex-groups` tree hit the 1300-byte or depth-6 bound?).

Facts must equal HotSpot in both modes (`R24RegexSplitRows facts`, and the 66-probe battery with the switch
set). The rule engages on any self-selector call (filter / interceptor chains, visitors, `equals` delegating
to a field's `equals`), so also run a broad battery row (Spring Boot, `R16AllocPremium hashmap-cached`,
`R19Mega5Nested`) for compile-time and code-size changes.

### Residuals and proposals

* **W7B-1, the frame clear grows with the tree.** Each spliced level widens the compile's frame, and the
  fresh-frame clear (22.6 % of `regex-groups`) is paid per call of the compile root. That root is
  `Matcher.search` (once per `find`), so the gain from removing per-position calls should dominate. If a row
  loses, compare `ir-frame` sizes for `search` between the arms first (W7-C owns the prologue).
* **W7B-2, the nest budget.** `search` -> `Start` -> `GroupHead` -> `Greedy` -> `charAt` -> `StringLatin1.charAt`
  is six levels, so the second group's links in `regex-groups` stay calls at depth 6. A proposal: do not
  charge a static leaf intrinsic (`StringLatin1.charAt`, `checkIndex`) a nest level.
* **W7B-3, `GroupTail` pc 70** is a 50 / 49 % pair. It needs the nested bimorphic splice, which is still opt-in.
  Measure the `on+pair` arm before proposing to fold it into this switch.
* **W7B-4, `Start.match`'s own artifact.** When something calls `Start.match` as a call (not spliced), its own
  compile splices `Greedy` at the top level already (wave 9). This switch changes that compile only through its
  nested rows (`Greedy`'s `next.match` -> `LastNode`).
* **W7B-5, flip rule.** Flip to default ON only if `on` moves the Matcher rows by the 15 % the flip rule needs
  and the broad battery row is flat.

### W7-B round 2 (2026-10-24; pending build): the losses on the split rows, and the cost rule

**First build (host c9, instructions per op, (I(3N)-I(N))/2N, loaded host).** `def` is the default;
`B` is `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE=1`; `B2` is `B` plus `NESTED_BIMORPHIC_SPLICE=1`,
`INLINE_BUDGET=2600` and `INLINE_MAX_NEST=8`. Engagement: 12 `nest-chain ... SPLICED` lines on
`regex-groups`, 3 of 15 on `matchonly`.

| row | def | B | B2 |
|---|---|---|---|
| `regex-find-ci` | 51 271 | 33 714 (0.66x) | 33 756 |
| `regex-matches` | 19 989 | 18 627 | 18 318 |
| `regex-find-email` | 61 695 | 61 013 | 61 158 |
| `regex-split` | 114 859 | 135 532 (1.18x) | 127 060 |
| `regex-groups` | 32 228 | 33 893 (1.05x) | 31 217 |
| `split-ws` | 59 262 | 63 444 (1.07x) | 65 386 |
| `split-comma` | 10 221 | 10 380 | 9 730 |

**Why the split rows lose (by reading; the census fields below check it).** `regex-split` and `split-ws`
are the two rows that compile a pattern per op: `String.split` takes no fast path for `\s+` or `,\s*`, so
`Pattern.compile` runs on every op. Round 1 changed three things there that do not remove any call per op:

* **Every chain site counted as hot, rooted or not.** In round 1 a site was hot when it had 100 samples or was on
  the entry prefix. That admits chains that run once per root entry: `Node.study`'s `next.study(info)` under
  `Pattern.compile`. Each such link removes one call per op. It also widens the compile root's frame, which is
  cleared on every entry, and takes splice budget from the small `Pattern` helpers (`peek`, `has`, `skip`,
  `unread`). Those helpers are called per character of the pattern (wave 8b). `B2`'s larger budget won back
  half of `regex-split`'s loss, which is what budget pressure predicts.
* **The 8 extra nested hot-cap slots went to every nested row.** That included non-chain bodies of 36 to 325
  bytes, which the four slots had held to the cold cap, all through the compile path.
* **Same-selector calls that are not chains.** Round 1 also took same-selector calls that are no chain, such as
  `AbstractSet.hashCode` -> `e.hashCode()` (the orchestrator's census showed a `nest-chain` refusal there). The
  refused ones cost nothing, but the rule had no business there.

`find-ci` wins because its chain is rooted twice over. `Start.match`'s position loop calls `Branch.match`, and
`Branch.match` loops over the alternatives, so every link removed is a call per position.

**Fix (same switch, still opt-in).**

1. **Chain family.** A chain call's constant-pool class is never `java/lang/Object` (`is_chain_target`). The VM
   resolver also asks that the enclosing body's class is a subclass of that class (`Pattern$GroupHead` of
   `Pattern$Node`, `jit_known_class` + `is_subclass_of`).
2. **Loop-rooted chains only** (`strip_chain_tree`, the planner, after the admission strips). A chain row is kept
   only when a loop roots it:
   * its site is in a loop of its enclosing body;
   * or the enclosing body is itself a kept chain row;
   * or the enclosing body is the top-level body and the top-level site is in a loop of the compiling method.

   This is the "hot relative to the root's entry count" rule in structural form: the chain then runs per loop
   iteration, and each link removes calls per root entry. A chain entered once per root entry is dropped, and its
   calls stay.
3. **Tree byte cap.** The links kept in one top-level tree add at most `FreqInlineSize` (325) bytecode bytes,
   counted per link as its body plus its non-link bodies (`predicate.is`, `charAt`), in tree order. That is a
   quarter of the default splice budget, and it bounds the frame the chain adds. `regex-groups` keeps `GroupHead`
   -> `Greedy` -> `GroupTail` (about 320 bytes) and drops the second group's links.
4. **Extra hot-cap slots for kept links only** (`nested_hot_cap_pcs`, by method key). Every other nested row
   keeps the slots it had with the switch off.

Dropping a row keeps its call and never withdraws an admitted parent (a link may keep its next-link call). The
soundness argument above is unchanged.

**Census fields to grep (`CRATONVM_DBG_JITC=1`).**

* `nest-chain ... depth=D in <body> samples=N in_loop=true|false -> class C SPLICED (...)` / `refused: <why>`:
  the VM resolver, per chain site whose body the plain rule refused.
* `ir-chain-tree <cn>.<mn><desc> at pc=P in <root>: kept=K unrooted=U capped=C link_bytes=B added_locals=L
  max_depth=D top_rooted=true|false`: the planner, per top-level tree that holds a chain row. Two checks:
  * on `split-ws` and `regex-split`, the `Pattern.compile` / `Pattern.<init>` trees should show `unrooted>0`
    and `kept=0`;
  * the `Matcher.search` / `find` trees should show `kept>0`.
* The frame size per compiled method is already printed: `[ir-frame-clear] method=<key> frame=<bytes>
  words=.. cleared=..`. Compare `method=java/util/regex/Matcher.search` (and `.find`, and
  `java/util/regex/Pattern.split`) between the off and on arms to test W7B-1. If the split rows still lose
  while their trees show only rooted `Start` -> `Greedy` -> `LastNode` links, the cost is in the larger root
  frame. That frame is W7-C's area (prologue and frame record), not this rule's.

**A/B.** As above. Arms: off, `=1`, and `=1` + `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1`. Watch for a loss of
`find-ci`'s 0.66x: `Branch.match`'s chain is loop-rooted and about 215 bytes, so it should stay.

### W7-B round 3 (2026-10-24; pending build): rare links, and links that drop their own artifact's splice

**Build c11 (`jit-r25-arch` d8e1d1c9ae), def -> `=1`, instructions per op.** The compile-path trees under
`Pattern.compile`, `Start.<init>` and `Node.study` now read `kept=0 unrooted=1`.

| row | def | `=1` | ratio |
|---|---|---|---|
| `regex-find-ci` | 53 608 | 33 667 | 0.63x |
| `regex-matches` | 21 084 | 19 013 | 0.90x |
| `regex-find-email` | 63 458 | 60 200 | 0.95x |
| `regex-split` | 117 149 | 133 959 | 1.14x, loss |
| `regex-groups` | 32 474 | 35 862 | 1.10x, loss |
| `split-ws` | 62 515 | 61 293 | 0.98x |
| `split-comma` | 10 139 | 9 920 | 0.98x |

The `find-ci` trees: `Matcher.search pc=121 kept=4 capped=1`, `Branch.match pc=50`, `Start.match pc=44`. The
`regex-groups` trees: `Start.match pc=44` and `GroupHead.match pc=28`.

**What the remaining losses have in common (by reading; not yet confirmed).** The trees that remain are all
loop-rooted. Two kinds of kept link can still cost more than the call they remove:

1. **A link that keeps a next-link call its own artifact had spliced.** The link rule lets a spliced link keep
   its next-link call. But when `GroupTail.match` or `BmpCharPropertyGreedy.match` runs as its own optimizing
   artifact, that artifact guard-splices its own `next.match`: `GroupTail` pc 70 as a top-level bimorphic
   site, `Greedy`'s back-off loop as an in-loop site. Spliced one level down, that call becomes a real call
   again on every execution, through an inline cache the splice does not seed
   (`CRATONVM_JIT_IR_SPLICE_CALLEE_PROFILE_SEED` is off by default). This is wave 12's
   `keep-loop-splice-caller` problem one level down. In `regex-groups` it is the `GroupTail` pc 70 pair,
   which `=1` alone leaves without a row.
2. **A link on a rare site.** In `,\s*` (`regex-split` runs it twice per op), `\s*` follows the `,` in about
   one position of twenty. In `(\w+)=(\w+)`, the tail after `=` runs once per pair. Such a link adds its body
   to the frame of a body that is entered at every position, and the fresh-frame clear grows with it, while
   the link itself almost never runs. HotSpot does not inline such a site past `MaxInlineSize`: C2's
   `InlineFrequencyRatio` is 0.25 of the caller's entries.

`split-ws` (`\s+`: `Start` -> `Greedy` -> `LastNode`, every link frequent, `LastNode` a leaf row) has neither,
and it is flat. `find-ci` keeps `Start` -> `Branch` -> `SliceI`, all frequent. Its rare tail
(`SliceI` -> `BranchConn` after a matched alternative) is a kept call either way.

**Fix 2 (same switch, still opt-in).**

* **Frequency rule** (`chain_frequency_verdict`, VM resolver). A chain link is admitted only when its site runs
  on at least 250 per mille of the enclosing body's entries. That is the receiver samples at the site over the
  body's invocation count, both from the same interpreted window (C2's `InlineFrequencyRatio`). An invocation
  count of 0 is no evidence, and the structural rule then decides alone.
* **Own-artifact rule** (VM resolver, chain-shaped bodies at depth >= 1). A link body whose next-link call has
  no nested row, while its own optimizing artifact guard-splices that call (`guarded_splice_class_at_bci`) and
  the call is frequent, is refused:
  `inline-resolve REFUSED ... chain-link-keeps-spliced-call: ... samples=N invocations=M; kept as a call`.
  The parent then keeps its call into that artifact. The parent's own resolution asks the same question of
  that call, so the refusal moves up the chain until it reaches a link whose next call is rare or spliced. A
  rare kept call keeps the link (`find-ci`'s `SliceI`).
* **Family.** The chain call's constant-pool class must also be a class, not an interface. That removes
  decorator delegation such as `c.iterator()` on a `Collection`.
* **The planner judges only chain-only rows.** The planner's cost rule (`strip_chain_tree`) now judges only rows
  the chain rule alone admits. A same-selector row the plain nested rules admit (a leaf under the hot cap, a
  body within the cold cap) is left exactly as with the switch off. That removes the
  `AbstractSet.iterator ... kept=0` noise, and it means the switch can no longer drop a row the default build
  splices.

**Decisive census fields (`CRATONVM_DBG_JITC=1`, `=1`).**

* `nest-chain ... samples=N invocations=M freq_permille=F in_loop=... -> class C refused: rare site (F per mille
  of the body's entries, under 250)`: rule 1.
  * On `regex-split`, expect it at `BmpCharProperty.match` callee_pc=36 (the `\s*` after `,`).
  * On `regex-groups`, expect it at the tail of each group.
* `inline-resolve REFUSED Pattern$<Node>.match... depth=D: chain-link-keeps-spliced-call: the next-link call at
  callee_pc=P is spliced (class C) in this body's own compile and frequent (samples=N invocations=M)`: rule 2.
  On `regex-groups`, expect `GroupTail` pc 70 when the bimorphic nested switch is off.
* `ir-chain-tree ... kept= unrooted= capped= link_bytes= added_locals= max_depth=`: what remains per tree.
  `find-ci` should still show `kept>0` under `Matcher.search` pc 121.

If `regex-split` still loses with no `rare site` or `chain-link-keeps-spliced-call` refusal on its trees,
check two things next:

* the acceptance gate (`grep acceptance` for `Matcher.search` / `Start.match` / `Pattern.split`): an
  optimizing body the gate discards runs its single-pass body;
* `[ir-frame-clear] method=... frame=` for the same three methods, between the arms.

**A/B.** Arms: off, `=1`, `=1` + `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1` (with pairs, `GroupTail` pc 70
gets a row, so rule 2 stops refusing it).

### Wave 8 W8-5 (2026-10-25; pending build): charge each link for the frame it adds to the root

**c12 (instructions per op; HotSpot / default / `=1`).**

| row | HotSpot | default | `=1` | `=1` vs default |
|---|---|---|---|---|
| `regex-find-ci` | 8 996 | 59 065 | 38 220 | win |
| `split-ws` | 11 401 | 56 862 | 64 912 | 1.14x |
| `cached` | 10 674 | 49 697 | 60 401 | 1.22x |
| `matchonly` | 7 074 | 31 019 | 36 507 | 1.18x |
| `regex-split` | 29 381 | 120 246 | 128 450 | 1.07x |

The flip rule needs a 15 % win somewhere and no row more than 10 % worse. The orchestrator's hypothesis is
that the losing rows pay for root-frame growth (a larger setup, clear and publication) for links that are
cold on their path.

**Rule (same switch, planner side, `strip_chain_tree`).** Every chain row the chain rule alone admits is now
priced against the frame it adds to the compile root:

* A link's **entries** are counted per thousand entries of the top-level body. Each one is the product, down
  the tree, of every site's frequency in its enclosing body: receiver samples at the site over the body's
  interpreted invocation count. The VM now carries that count on every resolved body as
  `InlineSite::callee_invocations`, filled only under the switch; 0 means no evidence, read as once per
  entry.
* The **saving** is entries x (one compiled call, `CHAIN_CALL_INSTRS` = 40 instructions, plus the clear of
  the link's own artifact frame).
* The **growth** is 1000 x the clear of the link's frame in the root, paid on every entry. The frame is
  estimated as `est_frame_bytes` = 8 x (bytecode bytes + locals) over the link and its non-link bodies. That
  is calibrated on `Greedy`'s 912-byte frame. The clear is one instruction per 32 bytes.
* A link whose saving does not cover its growth is dropped (`costly`), and the call stays at its pc. A
  760-byte link pays from about 0.37 entries per top-level entry; a 384-byte one from 0.23.

The `[ir-chain-tree]` line gains these fields:
`costly= min_entry_permille= est_frame_before= est_frame_after=`. The two `est_frame_*` fields are the
top-level tree's estimated frame without and with the kept links. The line now prints only for a tree whose
links the rule judged. The root's real frame is still `[ir-frame-clear] method=<root> frame=<bytes>`.
Compare it with `est_frame_after` and between the arms.

**Expected per-row effect: an honest estimate, not a promise.**

* **Unchanged.** `find-ci`'s links run several times per entry: `Start` -> `Branch` once per position, and
  `Branch` -> `SliceI` once per alternative. They pay many times over, so the win should hold.
* **Rows whose links are cold** on their path, entered under 0.2-0.4 times per top-level entry (a group's
  tail, `\s*` after `,`, `LastNode` behind a failed scan): those links are now dropped. That should move
  `regex-split` and `regex-groups` back toward the default.
* **`split-ws` / `cached` / `matchonly`.** The pattern is `\s+` (`Start` -> `Greedy` -> `LastNode`). By this
  model `Greedy` runs about five times per `Start` entry (once per position), so it pays and is kept, and
  `LastNode` is kept at about one in five. The rule therefore leaves these rows as c12 measured them,
  **unless** the trace shows smaller entry counts than this.
  * If `min_entry_permille` on their `Matcher.search` / `find` trees is well over 1000 and the rows still
    lose 14-22 %, the loss is not root-frame growth.
  * Next suspects, in order:
    1. the acceptance gate discarding the larger root's optimizing body (`grep acceptance` for
       `Matcher.search` / `find` / `Start.match`: a discarded body runs its single-pass code);
    2. the root's real frame against `est_frame_after` (a much larger real frame means the estimate is the
       wrong cost);
    3. in-splice field reads and guards that `Greedy`'s own artifact proves once (`PROVEN_RECEIVER_FIELDS`
       is per body).

**Decisive census.** On `split-ws`, take the `ir-chain-tree` line for `Matcher.search` (or `find`) and its
`min_entry_permille`, `costly` and `est_frame_after`, together with that method's `[ir-frame-clear] frame=`
in both arms.

### Wave 9 W9-3 (2026-10-25; pending host run): one line per chain compile that names its fate

**c18 (`=1` against the default, two repeats; the default itself moves up to 26 % between runs).**
`find-ci` holds at 0.55-0.61x. `split-ws` is 1.08-1.15x, `regex-split` 1.07-1.10x, `cached` 1.03-1.07x and
`matchonly` 0.94 / 1.22x. As predicted, the W8-5 frame rule did not move these rows.

**What reading the code says about each suspect.**

1. **The acceptance gate** (`lib.rs`, after `ir_evidence::take()`). An optimizing body is published only
   when `ir_evidence::accept` admits it. `CompileRecord::added_ns_per_execution` charges 171 ns for every
   call that the lowerer had to route through the blind `jit_invoke_dispatch` helper inside a spliced body
   (`blind_dispatches_in_splice`, counted in `ir_lower` where the IC cascade is not taken), and credits only
   4 ns per spliced frame. One such call in the root (`Matcher.search` / `find`) is enough to refuse the
   whole body, and then the root runs its **single-pass** body, with none of the round's splices.
   * Kept virtual calls in a splice normally get an inline cache. A call has none when the planner gave it
     no IC slots, or when its arity is outside `ir_ic_admits_arity`.
   * Reading the code did not show whether any chain-kept call misses its slots. The line below settles it.
2. **The real frame.** `CompiledMethod::osr_frame_size` is the lowered body's `SUB RSP` frame. W8-5 only
   estimated it (8 bytes per bytecode byte and local).
3. **Field proofs.** `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS` (default ON since round 25) proves a
   receiver's layout once per body. In a spliced `Greedy`, the root's own proofs apply, and the
   `ir proven-receiver-fields` line already prints per method.

**New diagnostic (`CRATONVM_DBG_JITC=1`, `jit/src/lib.rs`, `ChainCompileCensus`).** Every compile in which
the chain rule kept a link prints, with its outcome:

```
[cratonvm-jitc] chain-compile <root>: trees=T links=K costly=C min_entry_permille=M est_frame_added=E
  real_frame=F verdict=accepted|REFUSED by the acceptance gate (single-pass body runs)|NOT-LOWERED (...)
  spliced_frames=S blind_in_splice=B added_ns=N
```

**Host run (one binary built from this commit; `R24RegexSplitRows` compiled into `$P`).**

```
for row in split-ws matchonly cached regex-split regex-find-ci; do
  for arm in "" 1; do
    CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE=$arm CRATONVM_DBG_JITC=1 \
      cratonvm --jdk-only -cp $P R24RegexSplitRows count $row 20000 20000 2>&1 \
    | grep -E 'chain-compile|ir-chain-tree|\[ir\] acceptance .*java/util/regex/(Matcher\.(search|find)|Pattern\$Start\.match|Pattern\.split)|\[ir-frame-clear\] method=java/util/regex/(Matcher\.(search|find)|Pattern\$Start\.match|Pattern\$BmpCharPropertyGreedy\.match)|proven-receiver-fields.*method=java/util/regex/(Matcher\.(search|find)|Pattern\$(Start|BmpCharPropertyGreedy)\.match)' \
    > w93-$row-${arm:-off}.txt
  done
done
```

**How to read it.**

* **Suspect 1 is the cause** if any `chain-compile` line of a losing row says `verdict=REFUSED` or
  `NOT-LOWERED` while the `find-ci` ones say `accepted`. The fix is then on the planner side: a chain link
  must not leave a call without inline-cache slots (`blind_in_splice` names how many). Alternatively, under
  the switch, a refused chain compile can be re-planned without its links.
* **Suspect 2 is the cause** if the verdicts are `accepted` and the `on` arm's `real_frame` is several times
  the `off` arm's `[ir-frame-clear] frame=` for the same root, well above `est_frame_added`. The links then
  cost a frame W8-5 did not price, and W8-6's slot sharing is the remedy.
* **Suspect 3 is the cause** if the frames are close and the root's `proven-receiver-fields` line shows
  `loads full=` growing by about the spliced `Greedy`'s own `proven=` count. The splice then re-pays the
  per-read layout checks that `Greedy`'s own body had proved once.
* **None of the three** if every root says `accepted`, the frames are close and the `full=` counts match.
  The cost is then inside the spliced code itself: compare `CRATONVM_DBG_JIT_DISASM` of
  `Matcher.search` between the arms.

### Wave 10 W10-3 (2026-10-25; pending host run): the spliced chain pushed `ASCII.getType` past the nest cap

**c22 (the W9-3 census).** Every chain compile reads `verdict=accepted blind_in_splice=0`. The real frame is
about half the estimate (`Matcher.search` 1 040-1 184 bytes), and `proven-receiver-fields` reads
`loads full=0` in both arms. So it is none of suspects 1-3, and the cost is in the spliced code.

**The disassembly** (`count split-ws 20000 20000`, `Matcher.search` full/ir: off 11 787 bytes, on 22 375).
Through `Start`'s position loop, the on-arm body runs the spliced `Greedy` scan:

1. the `next` field load;
2. the `Greedy` class test;
3. the `matcher.to` and `predicate` field loads, each behind the layout-flag check;
4. the inline `String.charAt`;
5. the lambda class test (`0x80000000`);
6. `ch < 128`;
7. then a **real call** at `+0x2fb8` to the compiled `ASCII.getType`. Before it come 15 `mov qword [rbp-X],0`
   frame-block home clears, the frame-record word, the `[rbp-38h]` check and the stack-limit check; after it,
   two thread-local stores and the overflow test.

The resolver trace explains the call: `nest-static java/util/regex/ASCII.getType(I)I at callee_pc=1 depth=6
-> refused`. The chain is `search` -> `Start` (0) -> `Greedy` (1) -> `CharPredicate.is` forwarder (2) ->
`lambda$ASCII_SPACE$0` (3) -> `ASCII.isSpace` (4) -> `ASCII.isType` (5), so `getType` falls at depth 6, the
cap. There only the leaf tier applies, and its limit is 16 bytes; `getType` is 17. In `Greedy`'s own artifact
(the off arm's per-position callee), `getType` sits at depth 4 and is spliced
(`nest-static ...getType... depth=4 -> SPLICED`).

**Per-op accounting (`split-ws`; estimates from the listing, not measured).**

* **Cost per character the scan reads:** about 70 instructions.
  * the call sequence: 15 clears, about 8 bookkeeping and stack-check instructions, the call, 2 thread
    stores and the overflow test;
  * `getType`'s own body: prologue, frame clear, the `ctype` static read and epilogue, about 35.
* **Characters per op:** the 96-character text, plus the second character of each whitespace run, is about
  115 per op.
* **Total:** about 8 000 instructions per op. The c12 delta was 64 912 − 56 862 = 8 050, and `cached`,
  `matchonly` and `regex-split` scan the same `\s` predicate.
* **What the splice saves:** about 96 `Start` -> `Greedy` calls per op, at about 40-60 instructions each plus
  `Greedy`'s frame clear. That is the win the round was after, and the per-character call cancels it.

**Fix: `CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH`.** It is default ON under `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`
(inert without it), with `=0` to kill it. Code is in `jit/src/lib.rs` (`r25_w7b_chain_splice`) and the
leaf-tier filter in `jit_bridge.rs`.

* With `CRATONVM_JIT_IR_INLINE_MAX_NEST` unset, the nest depth becomes the builder's frame cap: 7 with the
  leaf tier, 8 frames, the most `IrBuilder::begin_splice` opens and the most the existing frame clamp allows.
  The default stays 6. An explicit knob still wins. HotSpot's `MaxInlineLevel` is 15.
* The leaf tier past the cap takes a call-free static body of up to 24 bytes, with the same opcode rule as
  `ir_leaf_splice_body`.
* With `search` as the root, `getType` (depth 6) now resolves in the full tier. One level further out, with
  `find` as the root, `getType` (depth 7) is a 17-byte leaf at the leaf tier. Either way it is spliced.
* The byte budget, every admission rule and the builder's frame bound are unchanged.

**Expected.** The per-character call should go away on `split-ws` / `cached` / `matchonly` / `regex-split`.
That is about −8 000 instructions per op against c12's `=1`, which leaves these rows at or below the
default: the `Start` -> `Greedy` call saving remains. `find-ci` should keep its win.

**Check.** Run the same rows and arms as W9-3.

* `grep 'nest-static java/util/regex/ASCII.getType'` must read `SPLICED` at every depth with `=1`.
* The `Matcher.search` listing must show no call between the `0x80000000` lambda test and the `and eax,800h`.
* For the A/B, add the arm `=1 CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH=0`, which reproduces c22.

### Wave 10 W10-4 (2026-10-25; pending host run): the `\w` predicate inside a union lambda is a call per character

**Profile.** Host c22, default arm, `count regex-find-email 300000 600000`, `perf record -e instructions:u`,
`CRATONVM_JIT_PERF_MAP=1`:

| body | share |
|---|---|
| `Pattern$Start.match` [c2] | 34.6 % |
| `CharPredicates.lambda$ASCII_WORD$0(I)Z` [c2], its own compiled body | 20.5 % |
| `BmpCharPropertyGreedy.match` | 14.9 % |
| `BmpCharProperty.match` | 11.9 % |
| `Pattern$BitClass.is` | 4.1 % |
| `Pattern.lambda$Single$0` [c1] | 0.8 % |
| its not-entrant stub | 0.3 % |
| lambda-adapter stubs | about 1 % |

**Why the `\w` lambda is a call (by reading).**

* `[\w.+-]+` and `[\w-]+` compile to `BmpCharPropertyGreedy` over a union predicate. The predicate is
  `CharPredicate.union`'s lambda, `ch -> is(ch) || p.is(ch)`: `this` is the `\w` lambda and `p` is a
  `BitClass`.
* The greedy node's own compile splices its `predicate.is` site as that lambda: the forwarder plus
  `lambda$union$N`, through the lambda route. The union body itself does not appear in the profile.
* The union body's two calls are `invokeinterface CharPredicate.is`, and one level down an interface site has
  no profiled nested route by default (`CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE` is opt-in, and
  `nest-lambda-guarded` exists only at depth 0). Both stay calls: `lambda$ASCII_WORD$0` and `BitClass.is`,
  once per character.
* Reached, the `\w` chain (`lambda$ASCII_WORD$0` -> `ASCII.isWord` -> `isType` -> `getType`) ends at depth 6
  from the greedy node's own compile. That is the W10-3 cap problem again.

**Fix: `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE`** (opt-in when written; **default ON since the 2026-10-25 wave-10
flip, `=0` kills it**; `jit/src/lib.rs` `mod r25_w10_4_lambda_iface`, plus one
term in `jit_bridge.rs`).

* An interface site inside a spliced body whose method is a javac lambda implementation (`lambda$...`) takes
  the existing `nest-profiled-iface` row. That row is the callee profile's dominant class (80 %, or the
  compiled cache's) behind an exact class test, with the site's interface call and its inline cache on the
  miss edge. A lambda-proxy receiver resolves through the nested lambda route.
* The switch also opens `CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH` (W10-3: frame-cap depth, 24-byte leaf tier), so
  `getType` is spliced too.
* Bimorphic pairs follow only with `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE=1`.

**Expected.** About 25 % of the row's instructions are now the two inner calls (20.5 % + 4.1 %, plus their
call overhead in the callers). With them spliced, `regex-find-email` should drop by roughly a fifth. Any row
whose char class is a union (`[\w-]`, `[\w.+-]`) gains the same.

**Engagement:**

```
CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE=1 CRATONVM_DBG_JITC=1 cratonvm --jdk-only -cp $P R24RegexSplitRows count regex-find-email 20000 20000 2>&1 \
  | grep -E "nest-lambda-iface .*in java/util/regex/Pattern\\\$CharPredicate\.lambda\\\$union|nest-profiled-iface java/util/regex/Pattern\\\$CharPredicate\.is\(I\)Z .*guard on class|nest-static java/util/regex/ASCII\.getType"
```

Expected output:

* `nest-lambda-iface ... in java/util/regex/Pattern$CharPredicate.lambda$union$N...`;
* `nest-profiled-iface ...CharPredicate.is(I)Z ... -> guard on class 2147483xxx` for the `\w` proxy, and the
  `BitClass` id for `p`;
* `getType` reading `SPLICED`.

`perf` on the row should lose the `lambda$ASCII_WORD$0 [c2]` and `BitClass.is` lines.

**Not fixed here: `lambda$Single$0` [c1] and its not-entrant stub.** `jit/src/lambda_adapter.rs` builds one
thunk per `(proxy class, impl entry)` (`AdapterKey::impl_entry`) and installs it in the inline cache. The
thunk tail-jumps to the entry it was built for. When the C2 body supersedes the C1 one, an inline cache that
still holds the thunk keeps entering the old C1 body, and through its not-entrant stub once that is
invalidated. The fix is to re-point the cache, or to rebuild the thunk on supersede. That is
`supersede-baked-callers`' job, which today does not cover IC-installed adapter thunks; it is filed here as the
next item (about 1-2 % of `find-email`).

### Wave 10 W10-5 (2026-10-25; pending host run): lambda-adapter thunks follow a superseded implementation

**The defect.** W10-4 found it in the profile: `Pattern.lambda$Single$0 [c1]` takes 2.9 % of `regex-split`, and
0.8 % plus 0.27 % (`not-entrant-stub lambda$Single$0`) of `regex-find-email`.

* `jit/src/lambda_adapter.rs` builds one thunk per `(proxy class, implementation entry, shape)` and installs
  it in the MIC / PIC / megamorphic table as if it were the receiver's compiled method. The thunk tail-jumps
  to the fixed entry it was built for.
* On a C1 -> C2 supersede, `JitCache` retargets the caches that publish the displaced body
  (`retarget_superseded_inline_caches`). A cache holding a thunk publishes the thunk, not the body, so it was
  never reached.
* So every SAM call through it kept entering the C1 body, and the not-entrant stub once that body was
  redirected. The VM's lambda helper does re-read the implementation after a publication
  (`lambda_jit_site_code`), but it only runs on a cache miss, and a cache holding the thunk never misses.

**Fix: `CRATONVM_JIT_LAMBDA_ADAPTER_SUPERSEDE`.** Default ON, `=0` kill. Code is in `jit/src/lib.rs`
`r25_w10_5_adapter_supersede` and `jit/src/lambda_adapter.rs` `w10_5_supersede`, called from the supersede
path in `JitCache` beside the megamorphic-table retirement, under `mutation`, after the `superseded` flag.

* Every thunk whose recorded `(entry, artifact id)` names the displaced body is taken out of the thunk map.
  An address the allocator reused never matches.
* Each thunk is then handled like a superseded body:
  1. marked superseded, then retired, so every installer refuses it;
  2. every cache slot that publishes it is cleared (`retarget_superseded_inline_caches` on the thunk);
  3. it is removed from the shared megamorphic table;
  4. it is handed to the retirement queue.
* The next call through each cache misses into the lambda helper, which reads the current body and builds a
  thunk for it.
* `adapter_entry` refuses a superseded body, before the build and again under the map lock before the insert.
  That is the same pairing it already uses for a retired body, so a thunk raced in after the sweep is never
  findable.

**Soundness.** A thunk roots its implementation (`_direct_callee_roots`), so the displaced body stays mapped
until the thunk leaves the retirement queue, which happens once no thread can still be executing it. No cache
can jump into reclaimed code: the cleared slots' owners go through the same deferred release. The baked-caller
rule is unchanged. No statics.

**Host check (`regex-split`, `regex-find-email`, default build, so the switch is on).**

* With `CRATONVM_DBG_JITC=1`, `grep 'lambda-adapter-supersede .*lambda\$Single'` shows
  `thunks=N cache_slots_cleared=M` with N ≥ 1.
* `perf record -e instructions:u` with `CRATONVM_JIT_PERF_MAP=1`, then
  `perf report --sort symbol | grep -E 'lambda\$Single\$0'`: the `[c1]` line and the
  `not-entrant-stub java/util/regex/Pattern.lambda$Single$0` line should be gone, leaving only `[c2]`, or the
  splice.
* The `CRATONVM_JIT_LAMBDA_ADAPTER_SUPERSEDE=0` arm reproduces the old profile.

### Wave 10 W10-3b (2026-10-25; pending host run): the bytecode-spliced `charAt`, and the matchonly bimodality

**Host c24 (W10-3 in, NCS = `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`).** Instructions per op, default / NCS:
`split-ws` 55 738 / 59 619, `matchonly` 32 047 / 35 727 (bimodal under NCS).

**Profiles** (`perf record -e instructions:u`, `CRATONVM_JIT_PERF_MAP=1`).

* On `split-ws` and `matchonly`, in both arms, the hot body is `Pattern$Start.match`'s own C2 artifact, at
  35-38 % and 66-68 %.
* On `matchonly` the probe's OSR body (`spin`) calls it through `find` / `search`. `Matcher.search` itself
  never shows.
* Scaled to instructions per op, NCS moves `Start.match` by +3 300 and `spin` by +2 000, and `LastNode` by
  −1 900 (its call is gone).

**What differs inside `Start.match`'s C2 body (the c24 listings, `dis3-ws-*.txt`).**

* In the default arm, `Greedy`'s `seq.charAt(i)` (nested at depth 1) stays a call, and the inline String
  prefix answers it (`IrStringSearchKind::SeqCharAt`): the String class test, the coder byte, one bounds
  compare and the load, about 22 instructions (`d44`-`dde` in the off listing).
* Under NCS, the chain rule opened the profiled interface route for every interface site of a chain body
  (`nest-profiled-iface java/lang/CharSequence.charAt(I)C ... -> guard on class 6`). So the site became a
  bytecode splice of `String.charAt` -> `isLatin1` -> `StringLatin1.charAt` -> `checkIndex`. Per character
  that is:
  * the `COMPACT_STRINGS` static;
  * two field reads behind the layout flag;
  * a static read;
  * two index checks;
  * stores and reloads.

  About 45 instructions (`e46`-`fcd` in the on listing).
* At about 115 characters per op, that is about 2 600 instructions, most of the loss.

**The bimodality** (`mo-1..6`: slow runs 1, 4, 5 at 108-110 G; fast runs 2, 3, 6 at 101 G).

* In fast runs, `Matcher.search`'s C2 body carries the `Start` -> `Greedy` chain: 22 864 bytes, with an
  `ir-chain-tree ... at pc=121 in Matcher.search ... kept=1` line.
* In slow runs 4 and 5 it is a 7 821-byte body with no `ir-chain-tree` line at all. So the chain rows were
  gone before the chain rule ran, and `search` calls `Start.match`'s own artifact.
* The planner strips every nested row over the cold cap under a top-level site it does not call hot
  (`ir_splice_strip_cold_nested_hot_rows`: in a loop, or 256 recorded calls). `search`'s `root.match` site is
  neither when `search` reaches the optimizing tier early. Its C1 body takes over the counting.
* Run 1 (slow, with a chain body) also built a different first-tier `Start.match` (14 804 bytes against
  9 631). That one is not explained by the files.

**Fix (two kill switches, both default ON under the chain switches, both inert without them).**

* `CRATONVM_JIT_IR_CHAIN_STRING_PREFIX_KEPT` (also under `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE`):
  * an interface site of a chain or lambda body that the inline String prefix answers keeps its call
    (`chain-prefix-kept ... -> kept as a call` under `CRATONVM_DBG_JITC`);
  * the chain link rule counts that call as answered, not as a kept call (`first_kept_non_link_call`), so
    the `Greedy` link is still admitted.
* `CRATONVM_JIT_IR_CHAIN_LOOP_ROOTED_HOT`: a splice tree with a chain link inside a loop of its own body
  (`Start`'s position loop) keeps its nested hot-cap rows under a top-level site the plan does not call hot.
  Such a link runs once per iteration of that loop, however rarely the top-level site itself ran.

**Expected.**

* `split-ws`, `cached` and `matchonly` lose the per-character spliced `charAt`, about −2 600 instructions per
  op against c24's NCS, which takes them back to about the default.
* `matchonly`'s slow mode, the one that kept `search` without the chain, should disappear. Both modes then
  read as the fast mode, about 101 G on the `mo` runs.
* `find-ci` is untouched: it has no `charAt` site in its chain bodies.

**Host check.**

* `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE=1 CRATONVM_DBG_JITC=1 ... count split-ws N 3N 2>&1 | grep -E 'chain-prefix-kept java/lang/CharSequence.charAt|nest-profiled-iface java/lang/CharSequence.charAt|ir-nested-hot-cap-cold-site .*Matcher.search|ir-chain-tree .*in java/util/regex/Matcher.search'`.
  Expected: `chain-prefix-kept` lines, no `nest-profiled-iface ...charAt` line, and an `ir-chain-tree` line
  for `search` on every run.
* Rerun the 6 `matchonly` runs. All of them should land in the fast mode.
* A/B arms: NCS=1, NCS=1 + `CRATONVM_JIT_IR_CHAIN_STRING_PREFIX_KEPT=0`, and NCS=1 +
  `CRATONVM_JIT_IR_CHAIN_LOOP_ROOTED_HOT=0`.

### Wave 11 W11-4 (2026-10-25; pending host run): the scan loop's invariant work, through the existing loop proofs

**Status.** NCS (`CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`) and `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE` are default ON
since `9f3ad6b409`. The rows are still 3.6-5.3x HotSpot by instructions. The hot body of the `\s+` rows is
`Pattern$Start.match`'s own C2 artifact. Its spliced `BmpCharPropertyGreedy` scan runs about 75
instructions per character (c24 `dis3-ws-1.txt`); HotSpot runs 8-12.

**Loop-invariant work in that loop, per character:**

* the `predicate` field read, with its layout-flag check;
* the lambda class test, materialised to 0/1, stored, reloaded and tested;
* the `charAt` prefix's `String` class test, coder byte and `value` read.

**What is already in the tree.** Five opt-in passes answer exactly these once at the loop entry. They run
after the splice, on the spliced headers: W6-C, plus W7-D's `_SPLICED` arm with the chain frame.

| switch | removes from the loop |
|---|---|
| `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE` | nothing alone: plants the loop-entry `String` guard that licenses the others |
| `_SEQ_CHARAT_PREDICATE_SPLICED` | the same for a loop spliced from a callee (the greedy scan inside `Start.match`) |
| `_SEQ_CHARAT_HOIST` | the prefix's class test, coder and `value` / length reads |
| `_SEQ_CHARAT_FIELD_CLASS_PREDICATE` | the `predicate` reads and the lambda class test with its 0/1 materialisation, and the miss-arm call |
| `_SEQ_CHARAT_ENTRY_BOUND` | the per-character `i < count` / `0 <= i` index guards |

**Why the earlier measurements showed nothing.** They were measured in waves 6 and 7, when the greedy body
was not spliced into its caller. Their key is the `CharSequence.charAt` prefix CALL. Under the round-1 chain
rule that call was spliced as bytecode, and only W10-3b
(`CRATONVM_JIT_IR_CHAIN_STRING_PREFIX_KEPT`, default ON) keeps it a call in a chain body. So none of the five
could reach the hot loop until now.

**Change: `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS`** (opt-in; `jit/src/ir_optimize.rs`
`r25_w11_4_chain_loop_proofs`, and one `||` in each of the five readers).

* It turns the five on together, for the A/B.
* An explicit `=0` of one of them still kills that one, and the others stay on.
* No new pass and no new soundness argument: each pass's own (module notes of `seq_charat_predicate.rs`,
  `charat_trap.rs` and the W7-D section of `r25-constant-and-redundant-branches-FIXED-20261024.md`) applies
  unchanged.

**Dumps that would confirm it** (one host binary, `--jdk-only`, `R24RegexSplitRows`, NCS default ON):

1. Engagement on the hot body, both arms:
   ```
   CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS=$arm CRATONVM_DBG_JITC=1 CRATONVM_DBG_LICM=1 \
     cratonvm --jdk-only -cp $P R24RegexSplitRows count split-ws 250000 750000 2>&1 \
   | grep -E 'ir-seq-charat-(predicate|w7d) java/util/regex/Pattern\$(Start|BmpCharPropertyGreedy)|\[ir-licm\] (seq charAt|charAt hoist|charAt entry bound)'
   ```
   With `=1`, expect:
   * `planted spliced` > 0 (or `planted loops` > 0 for `Start`'s own loop);
   * `hoisted calls` > 0;
   * on the `ir-seq-charat-w7d` line, `guards` > 0 with `tests folded` / `reads replaced` > 0;
   * `bounds` > 0.

   A zero beside a refusal field names the premise that failed. The likely ones are `withheld` (the
   all-or-nothing outer loop, W7D-P2) and `writer` (a kept call in the loop, W7D-P3).
2. The listing:
   `CRATONVM_DBG_JIT_DISASM='Pattern$Start.match,Pattern$BmpCharPropertyGreedy.match'` on the same run,
   both arms. In the scan loop, these should be gone from between the back edge and the header:
   * the `predicate` load with its `cmp qword [r11+8],0`;
   * the `cmp dword [rax],80000000h` with its `mov eax,1` / `xor eax,eax` materialisation;
   * the `cmp dword [rax],6` / coder test.
3. A/B. Instructions per op for every row, in these arms:
   * `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS` unset;
   * `=1`;
   * `=1` plus `CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE=0`;
   * `=1` plus `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST=0`.

   Then the facts row and the 66-probe battery in both modes. The guards deopt on a non-`String` subject,
   and on a predicate or next node of another class.

**If a refusal blocks the hot loop.** The W7-D page's proposals are the next step:

* W7D-P2: a per-call trap licence instead of the all-or-nothing loop.
* W7D-P3: admit pure writers, and a resolved `CheckCast` helper.

### Wave 12 W12-4 (2026-10-25; pending host run): offer the loop proofs the inner scan loop

**c29 (`CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS=1`).**

* The bundle costs 1-6 %, and `HOIST=0` brings the rows back to the default.
* The `split-ws` census shows:
  * the `String` predicate plants at `Start.match`'s own position loop (combined bci 31);
  * the hoist moves the scan loop's `charAt` reads to that outer entry;
  * the field-class predicate refuses that loop: "an opaque writer";
  * the entry bound refuses it: "no dominating `i < x`".

  The `header 9` in those lines is the region's node id, the same loop.

**Reading the shape.** The outer loop holds the spliced `BmpCharPropertyGreedy`.

* Its back-off `next.match` is the only plain call. The chain rule refused that link as rare (the
  `LastNode` row at 194 ‰), so the call sits in the outer loop but outside the scan loop. This is the
  opaque writer, by elimination: every other call in the loop is a trapping `charAt`, a miss arm, or
  spliced. The census now names it (below).
* The `charAt` index is the greedy's own `i`, bounded by `i < to`. Here `to` is read once per position,
  invariant only in the inner scan loop. So no outer-loop bound can answer it, and the `i <= guard` of
  `Start`'s own loop bounds a different variable. That makes item (3) moot.
* The inner scan loop, which carries the per-character work, was never offered to either pass. Both take
  only the headers the predicate planted.
* Why the hoist alone costs: from the census, the hoist leaves two guards and a load per character in place
  of the prefix's one compare. Its `value` / coder / count reads now live across the whole outer body,
  including its calls. This needs the listing to confirm (below).

**Change: `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS`** (opt-in, also under `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS`;
`jit/src/ir_optimize.rs` `r25_w12_4`, three hooks in `seq_charat_predicate.rs`).

* P2 (field class) and P3 (entry bound) are offered every loop nested in a `String`-guarded loop. A loop
  counts as nested when its header or its entry control lies in the guarded body.
  * The inner loop's `charAt` calls trap under the outer licence; that is why the hoist could move them.
  * Its writers are those traps, the predicate's miss-arm call and the spliced lambda body.
  * Its bound is its own `i < to` header test, with `to` invariant in it, and `count` hoisted to the
    outer entry.
* P2 also takes as a miss-arm call one that the test's miss projection reaches through straight-line
  control (guards, no merge). It is as dead as the projection once the test folds.
* P3's bound walk passes through guards on its way up. A guard only narrows the path, so the edge above
  it still dominates.
* `CRATONVM_DBG_LICM` now names the writer that refused a loop: node, op, bci, call target and control
  input.

**Soundness.** Unchanged, per loop. The inner pre-header is a block of the outer body. Each new guard deopts
to the inner header with the inner loop-entry frame, in chain form for a spliced header. The three-way
argument of the module notes holds per loop. W7D-P2 (a per-call trap licence) is not needed for this loop:
the outer licence already covers it.

**Expected effect.**

* P2 folds the lambda class test and its 0/1 materialisation, and the per-character `predicate` read.
* P3 replaces the hoist's two per-character guards with one guard at the inner entry, per position.

Together that should turn the hoist from a loss into a win on the `\s+` rows. This is an estimate.

**Dumps to confirm** (one binary, NCS default ON, `count split-ws 250000 750000`):

1. Census:
   ```
   CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS=1 CRATONVM_DBG_JITC=1 CRATONVM_DBG_LICM=1 ... 2>&1 \
   | grep -E 'ir-seq-charat-(predicate|w7d) java/util/regex/Pattern\$Start|\[ir-licm\] (seq charAt field class|charAt entry bound|charAt hoist)'
   ```
   Expect:
   * a planted line for a second header (the inner one);
   * `the opaque writer is n.. Call ... -> java/util/regex/Pattern$Node.match...` for the outer header;
   * `charAt entry bound: ... answered at the entry` for the inner header.
2. Listing: `CRATONVM_DBG_JIT_DISASM='Pattern$Start.match'` in three arms: `=1`; `=1` with
   `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS=0`; `=1` with `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST=0`. These decide
   what the hoist costs: per-iteration reloads of the hoisted `value` / coder / count homes, and the
   guards' deopt stubs. Also read the `[ir-frame-clear]` frame size of `Start.match` in each arm.
3. A/B as for W11-4, plus the arm `=1` with `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS=0`.

### Wave 13 W13-4 (2026-10-25; pending host run): the hoist is the loss, so it leaves the bundle

**c30 A/B** (instructions per op): default / `CHAIN_LOOP_PROOFS=1` / `=1` with `CHAIN_INNER_LOOP_PROOFS=0`.

| row | default | `=1` | `=1`, inner off |
|---|---|---|---|
| `split` | 109 450 | 115 608 | 118 244 |
| `split-ws` | 55 926 | 58 154 | 58 214 |
| `cached` | 49 871 | 52 640 | 52 032 |
| `matchonly` | 31 303 | 33 673 | 33 215 |

The bundle loses 3-8 %. c29's `=1 HOIST=0` arm was at the default.

**What the census says** (`w13/w124-*.txt`, `split-ws`).

* **Field-class predicate (P2), inner scan loop.** It now plants there: `Start.match` region 24, bci 99,
  `planted 1 guard(s)`, `tests folded=1 reads replaced=1`. So the W12-4 inner-loop offer works.
* **P2, outer position loop.** Still refused, and the writers are now named:
  * the predicate's miss-arm call `CharPredicate.is` (n85, bci 116, control `Proj(1)`). Its test is not a
    plan of the outer loop, whose `this.next.predicate` base varies per position;
  * in round 1, the greedy's kept back-off `Node.match` (n145, bci 160).

  Both refusals are correct.
* **The entry bound (P3).** It answers nothing (`entry-bound loops=2 bounds=0 refused: fact=1`). So the
  hoist's two per-character guards stay. Meanwhile the hoisted `String` reads sit at the outer loop's entry,
  live across the whole position loop and its calls.
* **Matching the rows.** The A/B matches that: the hoist is the loss, and P2 on the inner loop is a small
  win (`split` 118 244 -> 115 608).
* **The listings are missing.** None of the three dumps holds a `Start.match` listing. The disassembly filter
  lost its `$` in the shell and matched `Pattern.matcher` instead. So the reload-versus-guard question
  cannot be answered from these files.

**Decision.** (b) for the hoist, (a) for the rest.

* `CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS` keeps the `String` predicate, its spliced arm, the field-class
  predicate and the inner-loop offer.
* It no longer includes `_SEQ_CHARAT_HOIST`, nor `_SEQ_CHARAT_ENTRY_BOUND`, which only rewrites the hoist's
  guards (`jit/src/ir_optimize.rs` `r25_w13_4`).
* Either one is still `=1`-able alone.
* The entry bound's census now prints where its walk stopped: the index node, and the control chain from the
  guard up to the header, with each `If`'s condition.

**What to capture next** (single-quote the filter):

```
CRATONVM_JIT_IR_CHAIN_LOOP_PROOFS=1 CRATONVM_DBG_JITC=1 CRATONVM_DBG_LICM=1 \
CRATONVM_DBG_JIT_DISASM='Pattern$Start.match' \
  cratonvm --jdk-only -cp $P R24RegexSplitRows count split-ws 250000 750000 2> w134.txt
grep -F 'charAt entry bound' w134.txt
grep -F 'ir-seq-charat' w134.txt | grep -F 'Pattern$Start'
```

* Run it in two arms: the bundle as now, and with `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST=1
  CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND=1` (the W11-4 composition).
* The `unanswered:` line names why the bound is not found.
* The listing of the hoisted arm shows whether `value` / coder / count are reloaded from their homes each
  character.
* A/B: default, `=1`, and `=1 CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE=0`. Expect `=1` at or slightly
  below the default, from P2 on the inner loop.

**The per-character cost, then.** P2 alone removes only the `predicate` read and the lambda class test. The
rest of the about 75 instructions per character is:

* the trapping `charAt` prefix (its class test, compact test and coder test);
* the homes written per value;
* the lambda's `isSpace` -> `isType` -> `getType` chain, with its static `ctype` read checks;
* the safepoint poll.

Most of that is register allocation and lowering (W7-A's register allocator, `CRATONVM_JIT_IR_REG_ALLOC`),
not loop proofs. The next lane should A/B the scan loop under the register allocator before adding more
proofs.

## Round 24 (2026-10-24, dev `ec551c685e`, Linux host): the gap is code quality inside every body, not call count

Status: OPEN, re-scoped. No code change landed this round: the two built were neutral (below). The
gate (within 3x HotSpot) is not reachable by targeted fixes; what would reach it is the redesign listed
at the end of this section. The sibling page has the split / `Pattern.compile` rows.

**Probe and method.** The n23p probes were not kept, so `tools/probes/R24RegexSplitRows.java` now
holds every row of both pages (`regex-*`, `split-ws`, `split-comma`, `split-comma-10`, `compile`,
`cached`, `matchonly`, `list`) plus `facts` (a split / matches / replaceAll / replaceFirst / find /
group matrix, empty parts, the syntax error, `ArrayStoreException`). Facts equal HotSpot 25 in both
modes. The host (16 cores) sat at load 40+, where wall and even thread-CPU ns/op of one binary moved
1.5x run to run, so A/Bs used user-mode instructions per op: `java ... R24RegexSplitRows count <row>
N N` and `... count <row> N 3N` under `perf stat -e instructions:u`, (I(3N) - I(N)) / 2N. That repeats
to 5-8 % on both VMs and cancels startup and most compilation.

**Instructions per op, HotSpot 25 / CratonVM `--jdk-only` (ZGC default):**

| row | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `regex-matches` | 3 695 | 23 500 | 6.4x |
| `regex-find-email` | 9 675 | 80 600 | 8.3x |
| `regex-find-ci` | 7 366 | 67 900 | 9.2x |
| `regex-groups` | 6 035 | 37 900 | 6.3x |
| `regex-split` | 31 596 | 136 600 | 4.3x |
| `regex-replaceall` | 28 034 | 65 300 | 2.3x |
| `regex-string-matches` | 12 868 | 36 900 | 2.9x |
| `matchonly` | 7 005 | 36 700 - 40 900 | 5.2-5.8x |

The time ratio is larger than the instruction ratio (CratonVM's IPC is lower): CPU-time `regex-groups`
3 090 / 260 ns.

**Where the instructions go.** `perf record -e instructions:u` of `regex-groups`, every sampled IP
mapped into the `CRATONVM_DBG_JIT_DISASM=1` dumps: frame stores 19 %, branches 19 %, compares 16 %, the
prologue's fresh-frame clear 11 %, shadow-stack push / copy-back 9 %, frame loads 8 %+. The hot bodies
(`BmpCharPropertyGreedy`, `GroupHead`, `Start`, `Matcher.group`, `newString`, `subSequence`) show the
same shape: every intermediate is stored to its home and reloaded (`mov [rbp-170h],rax; mov
rax,[rbp-170h]; test eax,eax`), booleans are materialised and compared again, every reference field
read is the layout-epoch compare, the six-compare containment test and the per-object compact test,
and every call pushes and copies back all reference homes (seven in `GroupHead.match`, three of them
copies of the same parameters) around a 0x330-byte zeroed frame.

**Inlining more does not help on its own.** The nested `next.match` sites inside spliced bodies all
took a nest-exact row for the constant-pool class `Pattern$Node`, which never arrives and pre-empted
the profiled route (the evidence drop exists only under the opt-in
`CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_TRAP`). A build that dropped the contradicted row by default
engaged at every chain site (`nest-exact ... class 577 dropped`), but the profiled bodies it exposes
(`GroupHead` 47, `Greedy` 89, `GroupTail` 111 bytes) are not leaves, so the nested leaf rule still
refused them; with `CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP_LEAF=0` as well, `matchonly` rose from 36.7k
to 54.8k instructions per op and `split-ws` from 71.9k to 86.1k. A spliced body costs as much as the
call it replaces. Both the drop and a second change (maps stop naming a reference parameter's zeroed
prologue word) measured within noise on every row and were not landed.

**Every existing opt-in measured on this row set (instructions, n1 = dev + the drop above):**
`HOME_RELOAD_FORWARD`, `TRUSTED_REF_GETFIELD`, `GP_WIDE`, `DEOPT_IMAGE_SIZED` + `SAVE_BANDS_SIZED`,
`DEOPT_IMAGE_EXITLESS`, `PROLOGUE_THREAD_IN_RAX`, `PARAM_COPY`, `LS_LOOP_WEIGHT`,
`POLYMORPHIC_GUARDED_SPLICE`, `GUARDED_HOT_FREQ_COUNT`, `SP_CODE_POINT_SCALAR`, the three allocation map
sinks, `PRECISE_FRAME_LIVENESS=0`, `ZERO_FRESH_FRAMES=0`: every arm within 0.8-1.2x of default on every
row, none a consistent win, so nothing was flipped.

**What would close it (the redesign; one item at a time, behind switches, instruction-counted):**
1. A larger allocatable register file (today the five callee-saved registers) and no write-through
   home store for a value no reachable deopt or safepoint names, with deopt frame states that name
   registers. Frame stores and loads are about 30 % of the executed instructions.
2. Per-call shadow publication of only the reference homes live across the call (or a frame block that
   is compatible with the precise-liveness retention claims), instead of every defined home.
3. A GC-side per-body claim that primitive colours are never roots (the colouring already proves it,
   `verify_slot_colouring`), so the prologue clears only the words the band scan can root (see
   `docs/internal/fixed-bugs/r16-frames-fresh-frame-zeroing-residual-FIXED-20261002.md`, "What is left").
4. Field reads of a receiver whose layout and compactness the body proved once, without the per-read
   epoch / containment / compact sequence.
Only after those would deeper `Node.match` inlining pay.

## Wave 7 (lane L1, 2026-10-21; pending build): the `CharPredicate.is` site, as a major-receiver guarded splice

**n23-w6 baseline.** `R23ArraysEqRegex --jdk-only`, P-core pinned, ns/op, two reps:

| row | n23-w6 | HotSpot |
|---|---|---|
| `regex-matches` | 1795 / 1822 | 246-297 |
| `regex-find-email` | 3869 / 2760 | 265-273 |
| `regex-find-ci` | 2283 / 1700 | 180-201 |
| `regex-split` | 21029 / 9856 | 1126-1236 |
| `regex-replaceall` | 8128 / 5069 | 874-1075 |
| `regex-groups` | 4463 / 4049 | 269-322 |
| `regex-string-matches` | 3098 / 2552 | 638-703 |

**Why the `\w` predicate is not inlined.** `CRATONVM_DBG_JITC=1`, `-Dr23.rows=regex-groups`:
- At `Pattern$BmpCharPropertyGreedy.match` pc 26 the profile is `obs=2221 types=5 top=76% second=9%`.
- Every optimizing-tier reader declines it:
  - the one-class splice needs 80 %;
  - the bimorphic splice needs a runner-up of 10 %+ and a pair of 90 %+ (here 85 %);
  - the polymorphic splice (opt-in) needs every chosen class at 10 %+.
- So the site stays a full compiled call per character (`ir-direct-call MISSED`).
- The other `is` site, `BmpCharProperty.match` pc 19, is monomorphic and was already spliced as the lambda's
  transparent forwarder plus its implementation (`lambda-proxy-census ... SPLICED`). Lambda receivers therefore
  splice fine once a reader names the class.

**Fix: `CRATONVM_JIT_IR_MAJOR_RECEIVER_GUARDED_SPLICE`** (perf, default ON, `=0` kill; `jit/src/ir.rs`
`profile_major_receiver_class`, chained last into the planner's one-class guard read in `jit/src/lib.rs`).
- When the one-class, bimorphic and polymorphic readers all decline a site, its top class at 50 %+ (evidence floor
  as for the others, class id non-zero) is spliced behind one exact class test.
- The miss edge is the site's unchanged call.
- At this site, 76 % of the per-character calls become the inlined six-instruction lambda body. The other 24 % pay
  one class-id compare more than today.
- Sound for the same reasons as the one-class guarded splice (same resolver, same dependency recording, same miss
  edge).
- Test: `r23_l1_the_major_receiver_policy_takes_only_the_gap` (`ir.rs`). It covers the regex profile, each other
  reader's site, the 50 % bar, the evidence floor, the unknown class and the kill switch.

**Expected after the build:**
- `regex-groups`, `regex-find-*` and `regex-matches` lose one compiled call per subject character on the `\w` /
  `\d` greedy loops.
- Together with L5's wave-7 prologue change (only the GC-visible slots are zeroed), the remaining calls get
  cheaper as well.
- An estimate of the gain needs the build; the page was not re-measured.

**A/B to add:** `R23ArraysEqRegex`, all rows, `--jdk-only`, with `CRATONVM_JIT_IR_MAJOR_RECEIVER_GUARDED_SPLICE=0`
against unset. Facts must be unchanged in both modes (battery line `w1L5|R23ArraysEqRegex|||`). With
`CRATONVM_DBG_JITC=1` the site prints `ir-profile-guard Pattern$CharPredicate.is(I)Z at pc=26 in
...BmpCharPropertyGreedy.match...`.

**Side observation, not flipped.** The opt-in `CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE=1` on n23-w6 (two reps,
noisy) gave:
- `regex-matches` 1492 / 1271;
- `find-email` 2656 / 2386;
- `find-ci` 1537 / 1442;
- `split` 11009 / 11207;
- `replaceall` 5027 / 5300;
- `groups` 3976 / 4042.

That is a small gain on the Matcher rows. It needs its own pinned x3 A/B before any flip.

**What remains after the fix (the page stays OPEN).**
- **`Pattern$Node.match` chains.** `GroupTail.match`, `GroupHead.match` and the like are megamorphic virtual calls
  (`callee-unresolved` / `cold-site` in the census), each a full call per node per position. HotSpot inlines them
  into a profiled tree; the optimizing tier has no per-receiver chain for a site with more than four classes.
  - The next step is a type-switch over the profiled receivers, guarded and with a call miss edge, for small
    bodies. It is wider than this wave.
- **The per-call frame of small optimizing bodies:** L5's wave-7 prologue work.

Status: FIX IN CODE for the `Matcher` rows (wave 2c, pending orchestrator build); the `String.matches` / `replaceAll`
rows stay open (see the wave 2c section). Filed 2026-10-21 (JIT round 23 wave 2b, lane L5).

## Wave 6 (lane L5): the Matcher rows have no runtime-native share left

RIP sample of `R23ArraysEqRegex -Dr23.rows=regex-groups` on **n23-w5** (`--jdk-only`, P-core pinned): **316k leaf
samples are compiled code**, the next leaf is `memmove` at 729 and every other leaf is under 50 -- no native, no
helper, no funnel (the wave-2c `CRATONVM_REGEX_MATCHER_BYTECODE` default keeps the Matcher natives out). Mapped to
compiled bodies (`CRATONVM_DBG_JITC=1` entry ranges, scratchpad `maprip.py`), of 132k attributed JIT samples:

| body | samples |
|---|---|
| `CharPredicates.lambda$ASCII_WORD$0(I)Z` (IR) | 44 069 |
| `Pattern$GroupTail.match` (IR) | 29 696 |
| `Pattern$BmpCharPropertyGreedy.match` | 14 189 |
| `Pattern$BmpCharProperty.match` | 8 635 |
| `Pattern$GroupHead.match` | 8 523 |
| `Matcher.search`, `Pattern$Start.match`, `StringLatin1.newString`, `LastNode.match`, `groups()` | 2-5k each |

Why (`CRATONVM_DBG_JIT_DISASM`, `hotins.py`): the `\w` predicate is called once per character through
`Pattern$CharPredicate.is` from `BmpCharPropertyGreedy.match`, and that site is not inlined
(`inline-plan ... Refuse(ReceiverNotDominant { types: 5 })`, `ir-direct-call MISSED ... CharPredicate.is`), so each
character pays a full compiled call into a 6-instruction lambda whose optimizing-tier body opens a 0x200-byte frame
and zeroes it with four `vmovdqu ymm` stores in the prologue -- the samples spread over that prologue and the
`ctype` load. `GroupTail.match` is the same shape (a frame per node of the `Node.match` chain). HotSpot inlines the
bimorphic / profiled predicate and the node chain.

Both causes are the optimizing tier's code generation (the prologue's frame zeroing, `ir_lower.rs`
`emit_zero_frame_*`; the receiver-not-dominant refusal for a 5-type interface site) -- lane L1's files this round,
out of this lane's reach (`jit/src/ir*.rs`). Nothing in the runtime natives remains on these rows.

Status after wave 6: OPEN, owner lane L1 (IR prologue cost of small bodies + polymorphic interface inlining at
`CharPredicate.is` / `Node.match`). The `String` rows' per-call share was closed in waves 4-5 (thin helpers,
scratch, known-String read, spliced bind, `Pattern.compile` flip).

Battery (unchanged facts, both modes): `w1L5|R23ArraysEqRegex|||`; A/B lines for the wave-5 switches stay:
`w1L5|R23ArraysEqRegex||CRATONVM_STRING_REGEX_KNOWN_STRING_READ=0|`,
`w1L5|R23ArraysEqRegex||CRATONVM_JIT_IR_SPLICED_STRING_REGEX_DIRECT=0|`.

## Wave 5 (lane L5): the String residual on n23-w4 (pending build)

P-core pinned (`start /affinity FFFF`), three interleaved reps, `R23ArraysEqRegex` ns/op, HotSpot / n23-w4
`--jdk-only` / `--compatible`: `regex-string-matches` 638-703 / 3027-3385 / 2905-3836, `regex-replaceall` 874-1075 /
4304-4979 / 4352-5602, `regex-split` 1126-1236 / 11880-13900 / 11998-14111 (the Matcher rows: `regex-matches`
246-297 / 1297-1487, `regex-find-email` 265-273 / 2663-2981, `regex-find-ci` 180-201 / 1582-2037, `regex-groups`
269-322 / 3726-4013 -- lane L6's).

* **`String.matches` x8: the thin helper binds, the String reads are what is left.** RIP sample of the probe's own
  row (`-Dr23.rows=regex-string-matches`): `jit_string_matches_direct` -> `string_regex_thin` engaged, no funnel;
  the leaves are the two String reads through the context's per-field accessors (`vm_exec` `get_field` 11k,
  `resolve_field_descriptor_byte_cached` ~13k, the field-layout `VersionCache` ~11k, the heap `get_field` /
  `num_slots` / `check_field_index` ~28k of ~180k). Fix: **`CRATONVM_STRING_REGEX_KNOWN_STRING_READ`** (perf,
  default ON, `=0` restores `decode_string_chars`): a new `NativeHeapAccess::append_known_string_units` (default:
  the units reader; the VM: `vm_object::append_java_string_units`, the in-place payload decode without the
  class-identity lock a speculative caller needs -- the receiver and pattern are typed `String` by the method's
  signature) feeds the scratch path; a `false` falls back to `decode_string_chars` (both lossless). Test
  `known_string_read_matches_decode`.
* **`RxLoop.matches()` spliced into an optimizing body left `String.matches` on the inline cache + native funnel**
  (`jit_invoke_virtual_mic`, `try_lean_native_door`, `safe_native_call_impl` leaves; 5.9 us per eight calls):
  a method with its OWN `String.matches` site never reaches the optimizing tier (`string_site_bails_ir_planning`),
  so the wave-4 IR bind could only fire for spliced sites, and the splice path never asked it. Fix:
  **`CRATONVM_JIT_IR_SPLICED_STRING_REGEX_DIRECT`** (perf, default ON, `=0` off): the post-build `Op::Call` pass
  of `try_compile_inner` (the one that binds spliced `System.arraycopy` calls) binds a spliced
  `matches` / `replaceAll` / `replaceFirst` call to `string_regex_helper`'s entry with the VM context.
  `CRATONVM_DBG_JITC=1` prints `string-regex-direct bind (ir spliced)`.
* **`split("\\s+")`: `Pattern.compile`'s stand-in, then compiled JDK regex code.** RIP sample of `RxLoop split-ws`:
  `?jit` 70k of ~120k samples (the JDK `Pattern` / `String.split` bodies compiled), the `Pattern.compile`
  `Intrinsic` stand-in ~10 % (`invoke_on_class_shared_inner`, the interpreter, `new_object_initialized`,
  `find_method_recursive`). A/B of the wave-4 opt-in `CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE=1` on n23-w4 (pinned,
  x3 interleaved): `--jdk-only` 9360-9927 -> 6943-7661 ns/op, `--compatible` 9496-9913 -> 6780-7254; every
  `R23ArraysEqRegex` fact equal to HotSpot in both modes with it on. **Flipped default ON** (marker `R23-W5-FLIP`
  in `regex_matcher.rs` and `flag_groups.rs`, `=0` restores the rows; the test asserts the new default). The
  checked-in `jdk-only-kind-map-25-linux.tsv` baselines still list the two `Pattern.compile` `intrinsic` rows and
  will drop them when regenerated. The compiled-code share is filed separately:
  `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`.

Status after wave 5: the `String` rows' per-call costs are cut as far as the native door goes; what remains of
`regex-split` is compiled JDK code (the new page). The page stays OPEN for lane L6's Matcher rows.

## Wave 4: the String rows on n23-w3 (memo built)

`RxLoop` (scratchpad of lane L5) on **n23-w3.exe**, `--jdk-only`, RIP-sampled and flag-read-counted
(`CRATONVM_DBG_FLAGREADS=1`); the host was loaded by the w3 battery, so the ns/op are indicative only:
`matches` x8 14674-20791 ns/op (HotSpot 1288), `replaceAll` x2 12406-23589 (1067), `split("\\s+")`
20023-41742 (999).

* **Uncached flag reads.** `CRATONVM_STRING_REGEX_EXACT` and (wave 3's own) `CRATONVM_STRING_REGEX_MEMO` were
  read through `runtime_flag_default_on` on every call: 3.2 M reads each in a 6 s `matches` spin, 0.93 M in a
  `replaceAll` spin. Each read hashes the name against the declared-flag set and copies an `OsString`
  (`runtime_var_os` + `HashMap::contains_key<str>` + allocator leaves); `CRATONVM_REGEX_PATTERN_COMPILE_CACHE`
  the same on every `Pattern.compile` (109k reads in the `split("\\s+")` spin). Fix: all three memoised in a
  `MemoSlot` (1 off, 2 on; an installed override resets and re-derives it), no behaviour change, no switch.
* **Per-call allocation** (`RtlAllocateHeap` was the largest leaf of both rows: 11.4k of 164k samples in
  `matches`, 14.9k in `replaceAll`, plus `RtlFreeHeap` / `GetProcessHeap`): the memo path still read the receiver
  and the pattern into fresh `Vec<u16>`s and built a `from_utf16_lossy` `String` per call, and `replaceAll`
  allocated a `regex::Captures` per match and re-encoded the subject to UTF-16. Fix:
  **`CRATONVM_STRING_REGEX_SCRATCH`** (perf, default ON, `=0` restores the allocating memo path;
  `lang_string::string_regex_exact_scratch`): per-thread scratch buffers for the two reads and the ASCII
  subject (taken out of the slot for the call, so a nested call finds empty ones; a buffer grown past 64 Ki
  elements is dropped), and `exact_regex_replace_units`, the same walk as `exact_regex_replace_with` over the
  ASCII units with one reused `CaptureLocations`. Tests (`native-builtins` `r23_w4_string_regex_scratch_tests`):
  the replace twin against the per-match one over 16 patterns x 8 subjects x 5 replacements x first-only; every op
  (matches, replaceAll, replaceFirst, split 0 / -1 / 2) through `string_regex_exact` with the switch on and `=0`,
  each asked twice so the second call reuses the buffers.
* What remains per call after both: the native funnel crossing (`safe_native_call_impl`, `try_lean_native_door`,
  argument validation through `is_object_address`, ~15 % of the `matches` samples) and the String field reads
  (`get_field` / `resolve_field_descriptor_byte_cached` / field-layout MRU, ~10 %). A thin direct helper for the
  four `String` regex `Intrinsic`s (the `vectorizedHashCode` helper's shape) would remove the first; it is the
  next step if the build's A/B still leaves the rows >5x HotSpot.
* `split("\\s+")` is not a `String` native row on n23-w3: the leaf is compiled JDK bytecode (`?jit` 40k of the
  samples; `Pattern.<init>` / `Pattern.split`), plus `Pattern.compile`'s `Intrinsic` stand-in constructing the
  JDK `Pattern` through `new_object_initialized` (`invoke_on_class_shared_inner` + the interpreter, ~8 %). That
  stand-in only calls the JDK constructor when the (opt-in) cache is off; letting it yield to the bytecode is a
  registration change (`register_regex_natives` and `lib.rs` both register it, the latter as `Intrinsic`) left
  for a reviewed wave. The compiled-bytecode share is a JIT code-quality gap, not a per-call cost.

Build A/B: `R23ArraysEqRegex` `regex-string-matches`, `regex-replaceall`, `regex-split` and `RxLoop matches` /
`replaceall` in `--jdk-only` and `--compatible`, default vs `CRATONVM_STRING_REGEX_SCRATCH=0` (the memo fix is
not switchable; compare with n23-w3 for it); every `fact` line equal to HotSpot.

## Wave 4b: the two leftovers (pending build)

* **Thin helpers for the `String` regex `Intrinsic`s** -- **`CRATONVM_JIT_STRING_REGEX_DIRECT`** (perf, default ON,
  `=0` restores the plain dispatch; read per site at compile time). Both compile doors (`jit/src/lib.rs`
  `string_regex_helper`, beside the `Class.getName` binds) bind `invokevirtual` / `invokespecial`
  `String.matches(String)`, `replaceAll(String, String)` and `replaceFirst(String, String)` to new direct
  helpers (`DirectHelperTable::string_matches`, three words, and `string_replace[2]`, four words; ABI 25):
  `vm/src/jit/helpers.rs` `jit_string_matches_direct` / `jit_string_replace_direct::<FIRST>`. They run the
  registered native's FIRST step, its exact path (`lang_string::string_regex_thin`, only its successful answers),
  on the caller's thread with no funnel crossing; a null receiver is the JEP 358 receiver NPE (as the
  `getName` helper), and every other outcome -- the exact path off or declining, a non-ASCII receiver, a pattern
  outside the subset, a null regex / replacement, any exception -- dispatches the registered native, which runs
  the same exact path first and so answers or raises exactly as before (the exact path's only side effect is
  the result `String`). Under `--jdk-only` the bind asks the registry's admission (the rows are reviewed
  `Intrinsic`s); in `--compatible` the registered row is the same function. The fourth `CRATONVM_NATIVE_STRING_REGEX`
  row, `replace(CharSequence, CharSequence)`, is literal (not regex) and calls `toString()` on its arguments
  (Java code), so it has no funnel-free answer and is not bound. Tests: `jit` `r23_w4_string_regex_bind_tests`,
  `native-builtins` `thin_door_is_the_exact_paths_success_only`.
* **`Pattern.compile` yields to the JDK body** -- **`CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE`** (perf, **opt-in**
  `=1`, for the coordinator's A/B). In a real-JDK registry (`drops_real_layout_synthetic`) with the opt-in
  compile cache off, neither `Pattern.compile` row is registered -- not the `Intrinsic` one in
  `register_essential_natives_with_shims` nor the ambient one in `register_regex_natives`
  (`regex_matcher::pattern_compile_yields_to_bytecode`, asked at both sites) -- so there is nothing for
  `resolve_dispatch` to prefer over the JDK's `return new Pattern(regex, flags)`. In that configuration the native
  only constructed that same Pattern through `new_object_initialized`, so the Java-visible answer (the Pattern,
  a fresh one per call, every `PatternSyntaxException`) is the JDK's either way. A legacy synthetic-layout
  registry and the cache keep the rows. No allow-list, no new stub; the default registration set is unchanged,
  so `registry_census` and the router census need no new baseline (with `=1` the real-JDK registry loses two
  `Intrinsic` rows, not stubs). Test: `native-builtins/tests/regex_parse_leaf_rows.rs`
  `pattern_compile_rows_yield_to_bytecode_only_when_asked` (default / opted in / cache on, both modes). Expected
  win: `split("\\s+")`'s ~8 % in `invoke_on_class_shared_inner` + the interpreter, and a JDK factory the JIT can
  inline; risk: none for answers, so OFF only until the A/B says it pays.

Probe: `R23ArraysEqRegex` gained `fact string-regex-warm` (hash and length over 6 subjects x 8 regexes x 6
replacements x matches / replaceAll / replaceFirst / split, nulls, non-ASCII, `[`, `$9` and `\` replacements
included, asked after a 200k warm loop) plus `-head` and `fact pattern-compile-warm` (find, identity
`compile("q") == compile("q")` false, a syntax error, `CASE_INSENSITIVE`). On n23-w3 they equal HotSpot in both
modes. (`Pattern.compile(null)` was dropped from the fact: HotSpot answered a null-message NPE there, its
`OmitStackTraceInFastThrow` artefact after the warm loop's many null-regex throws, not a stable answer.)

Battery lines (both modes, ZGC default): `R23ArraysEqRegex -Dr23.reps=7` default vs
`CRATONVM_JIT_STRING_REGEX_DIRECT=0`, vs `CRATONVM_STRING_REGEX_SCRATCH=0`, and default vs
`CRATONVM_REGEX_PATTERN_COMPILE_BYTECODE=1` (rows `regex-string-matches`, `regex-replaceall`, `regex-split`); every
`fact` line equal to HotSpot's; `CRATONVM_DBG_JITC=1` shows `string-regex-direct bind (ir|sp)`.

## Wave 3: where the String rows' time goes

RIP-sampled on n23-w1 (`RipSampler`, `--jdk-only`, one row spinning; per-op HotSpot / CratonVM: `matches` x8 1288
/ 27258 ns, `replaceAll` x2 1067 / 18846, `split(",")` 258 / 7519, `split("\\s+")` 999 / 74485):

* `String.matches` / `replaceAll` (the `Intrinsic` natives): no single hot spot; the largest leaves are the
  allocator (`RtlAllocateHeap` + `RtlFreeHeap` ~19% of samples), then the per-call re-derivation of the same
  pattern -- `java_regex_exact_subset` (the parse), `compile_anchored_cached` (a global `Mutex` + `HashMap<String>`
  lookup of a freshly `format!`ted anchored string) and `regex_automata` `create_cache` / `clone`: the cached
  `regex::Regex` is CLONED per call, and a clone carries a fresh cache pool, so every search built a new matcher
  cache. Fix: `CRATONVM_STRING_REGEX_MEMO` (default ON, `=0` restores): a per-thread memo of the last 16 patterns'
  parse and their two compiled regexes, used by reference (`lang_string::exact_regex_memo`; test
  `r23_l5_exact_regex_memo_answers_what_the_per_call_path_answers`). Expected to remove the parse, the format, the
  lock, the clone and the cache build per call; the String read / UTF-16 -> `String` conversions remain.
* `String.split(",")` takes the JDK single-char fast path (no regex at all): its cost is the reference
  `System.arraycopy` at the end (`subList(..).toArray(String[])`, `Object[] -> String[]`): 3.0 us for ten elements
  against HotSpot's 33 ns. Filed as `r23-w3-reference-arraycopy-costs-microseconds-FIXED-20261021.md` (it is every
  `toArray` / collection copy, not a regex cost).

## Wave 2c: the natives yield to the JDK bytecode

Measured on n23-w1.exe with the existing switches (ns/op, `--jdk-only` / `--compatible`; every fact line equal to
HotSpot's in all eight runs):

| row | default | `NATIVE_MATCHER_FIND=0` | `NATIVE_STRING_REGEX=0` | both `=0` | HotSpot |
|---|---|---|---|---|---|
| matches x8 | 1835 / 1992 | 2307 / 2011 | 2936 / 1773 | 2206 / 2624 | 434 |
| find e-mail | 7537 / 13449 | 4265 / 3348 | 11778 / 8352 | 4611 / 4533 | 605 |
| find CI | 42044 / 42748 | 2187 / 1846 | 39975 / 28315 | 4507 / 2917 | 496 |
| split x3 | 139933 / 132640 | 34241 / 35722 | 167666 / 135448 | 41819 / 44872 | 2806 |
| replaceAll x2 | 26912 / 24526 | 16499 / 22574 | 86674 / 75892 | 32063 / 42836 | 1664 |
| find + group | 48370 / 41269 | 7967 / 11300 | 45333 / 44257 | 10596 / 10447 | 519 |
| String.matches x8 | 37187 / 26227 | 32519 / 26531 | 163575 / 177193 | 192596 / 150390 | 1909 |

The real-layout `Matcher` natives (`Intrinsic`, kept over the JDK bytecode by `keep_real_matcher_find_fastpath`)
LOSE to the compiled JDK bytecode by 2-20x; the four `String` regex `Intrinsic`s (`CRATONVM_NATIVE_STRING_REGEX`)
still WIN (the JDK `String.matches` / `replaceAll` bodies are 4-6x slower). Fix: `CRATONVM_REGEX_MATCHER_BYTECODE`
(default ON, `=0` restores the natives) folds into both readers of the Matcher gate (`types::flags`
`natives.native_matcher_find` for registration, vm `env_cache::native_matcher_find` for the JIT's exact-receiver
callback table), and also gates the `hitEnd` / `requireEnd` `Intrinsic`s that only serve the fast path. No
allow-list: the rows are simply not registered, so `resolve_dispatch` finds the JDK bytecode. Tests:
`types` `native_flags_defaults_match_the_original_call_sites`, `native-builtins/tests/regex_parse_leaf_rows.rs`
(`matcher_natives_yield_to_bytecode_by_default`; the leaf test now asks under `=0`). Residual: the `String`
convenience rows remain 10-20x HotSpot either way; the next step there is the per-op profile below.
Area: `native-builtins/src/regex_matcher.rs` (the real-layout `Matcher.find` / `group` natives),
`native-builtins/src/lang_string.rs` (`String.matches` / `replaceAll` / `split` exact path), the JIT's treatment
of natives that shadow JDK methods (`native-shadow` inline / devirt refusals).
Severity: MEDIUM (performance; every answer equals HotSpot's).

## Measurement

Probe `C:\craton\n23p\w1L5\R23ArraysEqRegex.java` (`-Dr23.rows=regex-...`), ns per op, HotSpot 25 / `--jdk-only` /
`--compatible`:

| row | HotSpot | jdk-only | compatible |
|---|---|---|---|
| `Pattern.matcher(s).matches()` x8 | 434 | 2290 | 2234 |
| `find()` loop, e-mail pattern | 605 | 11887 | 12488 |
| `find()` loop, `CASE_INSENSITIVE` | 496 | 20192 | 20001 |
| `Pattern.split` + `String.split` x2 | 2806 | 88416 | 63080 |
| `replaceAll` x2 | 1664 | 24727 | 18271 |
| `find()` + `group(1)`, `group(2)` | 519 | 41249 | 30617 |
| `String.matches` x8 | 1909 | 22134 | 23026 |

`CRATONVM_REGEX_BRIDGE_EXACT=0` and `CRATONVM_STRING_REGEX_EXACT=0` (in `--jdk-only`) each left the rows within
run-to-run noise, so the regex proposals that widen the exact fast path (W10L4-1/-2, W11L2-3) would not close it.

## What the trace shows

`CRATONVM_DBG_JITC=1 CRATONVM_DBG_IR_COMPILES=1` on `regex-groups`: `Matcher.find()Z` and `Matcher.group(I)` are
refused for splicing and for final devirtualisation ("a registered native shadows it for some receiver"), so each
call is a native funnel crossing from compiled code; `Pattern.matcher` is refused for its exception table and
`Matcher.<init>` for its allocations, so both stay calls.

## What would have to happen

A profile of one row (the Windows RIP sampler, `cratonvm-windows-rip-sampler`) to split the per-op time into the
funnel crossings, the `Matcher` / `int[] groups` / `String` allocations, the native's text read (`matcher_read_input_cached`)
and the engine; then either a thin direct helper for `Matcher.find` / `group` (the `vectorizedHashCode` helper's
shape, skipping the funnel) or letting the JDK bytecode compile and splice where the native buys nothing.

## Wave 7 A/B (orchestrator, n23-w7 6a11d90dcf, P-cores, 3 interleaved reps)

RxLoop, default vs `CRATONVM_JIT_IR_MAJOR_RECEIVER_GUARDED_SPLICE=0` (ns/op): matches 1936-2138 / 1943-2041, split-ws 6249-6800 / 6064-6355, split-comma 824-928 / 833-862. No measurable effect, so the switch is now **opt-in** (`=1`). Whether the CharPredicate.is site is spliced at all needs `CRATONVM_DBG_JITC=1` (look for `ir-profile-guard ... CharPredicate.is`). The megamorphic `Pattern$Node.match` chain remains.

## Wave 8 (lane L5, 2026-10-22; pending build): the `Pattern$Node.match` chain was held to the cold body cap

**Why the chain stays calls.** The n23-w7 `CRATONVM_DBG_JITC=1` trace of `R23ArraysEqRegex regex-groups`
(`C:\craton\n23p\w7L1\jg.txt`) shows that the chain's sites are not megamorphic. Their bodies were simply too
large for a site the planner calls cold:

| site | profile | calls at compile | outcome |
|---|---|---|---|
| `GroupTail.match` pc 70 | bimorphic, `top=599:50% second=605:49%` | 152 | `first-body-refused` |
| `BmpCharProperty.match` pc 36 | one class, 92 % | 185 | profiled class kept as a call |
| `GroupHead.match` pc 28 | one class | 214 | kept as a call |
| `Start.match` pc 44 | 4 types, 58 % / 31 % | 1186 | no reader (pair 89 %) |

- Every `Node.match` body of the chain is over the cold guarded cap of 35 bytes (JDK 25 `javap`). `LastNode` is
  45 bytes, `GroupHead` 47, `BmpCharProperty` 55, `BmpCharPropertyGreedy` 89 and `GroupTail` 110.
- A straight-line site became hot (the hot cap, 325 bytes, shared by a multi-class site's bodies) only from 256
  recorded calls. The chain's methods reach the optimizing tier with 150-215 calls per site, so each body was
  refused by size (`ir_profile_guarded_site_capped`) and each node stayed a full compiled call per position.
- HotSpot treats a site as frequent from `InlineFrequencyCount` = 100 calls. From there it inlines the bimorphic
  `GroupTail` site and the major-receiver sites at `FreqInlineSize`.
- The IR tier's virtual dispatch is already an inline MIC/PIC cascade with a hashed megamorphic tail
  (`ir_lower.rs` `emit_inline_cache_call`). The cost is the frame per node, not the dispatch, so a type switch of
  direct calls would not remove it.

**Fix: `CRATONVM_JIT_IR_GUARDED_HOT_FREQ_COUNT`** (perf, default ON, `=0` restores the 256-call floor;
`jit/src/lib.rs` `ir_guarded_site_is_hot` / `ir_guarded_splice_hot_min_calls`).
- A top-level guarded site outside every loop counts as hot from 100 recorded calls, HotSpot's
  `InlineFrequencyCount`.
- The rule covers the one-class, bimorphic, polymorphic and major-receiver readers, and the planner's
  nested-hot-row strip that asks the same question.
- The nested-row hot rule (`ir_nested_guarded_body_cap`) is unchanged.
- The per-compile bounds also stay: four hot bodies, and at most half of the remaining budget each.
- Sound for the hot cap's reason: only the allowed body size changes, never which body a class gets. The tests
  are exact and the miss edge is the site's call.
- Test: `r23_w8_l5_the_frequency_floor_is_hotspots_inline_frequency_count` (`lib.rs`,
  `nio21w6_guarded_hot_cap_tests`). It covers the 100 floor, the chain's counts, receiver-sample counting, the
  bimorphic body share and the `=0` kill. `hot_means_in_a_loop_or_counted` now runs under `=0`.

**What the orchestrator must measure.** Build `n23-w8.exe` and run each A/B pinned to the P-cores, as 3
interleaved reps of default against `CRATONVM_JIT_IR_GUARDED_HOT_FREQ_COUNT=0`:
- `cd C:\craton\n23p\w1L5\cls && n23-w8.exe --jdk-only -cp . R23ArraysEqRegex` (all rows; the Matcher rows
  `regex-groups`, `regex-find-*` and `regex-matches` are the target), and the same with `--compatible`. Every
  `fact` line must equal HotSpot's in both modes (battery line `w1L5|R23ArraysEqRegex|||`).
- `cd C:\craton\n23p\w5L5\cls && n23-w8.exe --jdk-only -cp . RxLoop <matches|split-ws|split-comma|replaceall> 3`.
- Engagement: with `CRATONVM_DBG_JITC=1 -Dr23.rows=regex-groups`, the census line should read
  `ir-bimorphic-census ...Node.match... at pc=70 in ...GroupTail.match...: ... -> planned 599+605`, and
  `ir-profile-guard ... at pc=36 in ...BmpCharProperty.match` should appear.
- Because the switch reaches every guarded site with 100-255 calls, also run a broad battery row (Spring Boot or
  the `R16`/`R19` splice probes) for compile-time and code-size regressions before keeping the default.

**What remains (OPEN).**
- `Start.match` pc 44 (58 % / 31 % / two minor classes) and `Matcher.search` pc 121 fall between every reader.
  HotSpot keeps those as virtual calls too, so they are not the gap.
- Nested chain levels (a `Node.match` body inside a spliced `Node.match` body) take the nested hot cap only for a
  leaf body (`CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP_LEAF`), and no chain body is a leaf. One level of splicing
  per compiled node is therefore what this switch can give.
- The per-call frame of small optimizing bodies (wave 7's prologue work) remains for the calls that are left.
- `String.split` / `Pattern.compile` per call and per-element allocation belong to lane L6 (sibling page
  `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`).

## Wave 8 A/B (orchestrator, n23-w8b, P-cores, 3 interleaved reps)

RxLoop, ns/op, default (`CRATONVM_JIT_IR_GUARDED_HOT_FREQ_COUNT` on) vs `=0`:

| row | on | off |
|---|---|---|
| matches | 4904 / 3050 / 3188 | 6481 / 2926 / 2975 |
| split-ws | 13161 / 9858 / 10398 | 19408 / 9748 / 9625 |
| split-comma | 1981 / 1306 / 1310 | 2542 / 1363 / 1418 |
| replaceall | 8082 / 4781 / 4873 | 8573 / 4586 / 4379 |

Rep 1 was a warm-up outlier. Reps 2-3 show no win, and most rows are slightly worse. The switch is therefore now
**opt-in** (`=1`; reader `runtime_flag_on`, flag row `off_word: None`) and is kept, not deleted. With it off, the
`Node.match` chain stays at the cold body cap as before. Nobody has yet checked, with `CRATONVM_DBG_JITC=1`,
whether the bodies that `=1` splices pay for their guard and code size. The page stays OPEN on the items listed
under "What remains".

## Wave 8b (lane L5, 2026-10-22; pending build): what one compiled call costs, and code placement

L5 now owns the per-call cost of the regex and split rows; L6 has finished its part. Measured on
`C:\craton\n23p\n23-w8c.exe` (P-cores, `--jdk-only`).

**One compiled-to-compiled virtual call.** Probe `C:\craton\n23p\w8L5\NodeCall.java`, row `poly`: three
`final` subclasses round-robin through an IR MIC/PIC site into one-line bodies. CratonVM takes 4.3-4.6 ns/call,
HotSpot 2.07.
- The callee's IR body is listed in `w8L5\dis_poly.txt` (`NodeCall$C.m`). It runs these steps:
  1. `push rbp`, `sub rsp, 0xF0`;
  2. the fresh-frame clear: seven `vmovdqu ymm` stores and `vzeroupper`;
  3. the argument stores;
  4. the frame record `[gs:14F0]` / `[gs:14F8]`;
  5. the thread fetch and the shadow-stack watermark save, and their restore in the epilogue;
  6. the entry safepoint poll.
- The fresh-frame clear costs nothing measurable here: `CRATONVM_JIT_IR_ZERO_FRESH_FRAMES=0` gives 4.38 / 4.41 ns
  against 4.38 / 4.45 ns default. `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1` and `CRATONVM_JIT_IR_SAVE_BANDS_SIZED=1`
  are also flat.
- On `RxLoop split-ws` the whole clear is at most about 5 %: `ZERO_FRESH_FRAMES=0` gave 9188 / 9217 / 9555 ns/op
  against 9771 / 9993 / 9397 ns/op default.
- The clear is also required. The collector's band scan roots every unwritten word at the entry poll
  (`docs/internal/fixed-bugs/r16-frames-fresh-frame-zeroing-residual-FIXED-20261002.md`). No clear was removed
  and no frameless entry was built. Skipping the clear soundly needs a GC-side lean-frame claim, and that file
  belongs to the GC round.

**Where the per-call time actually goes: every body starts on the same cache set.**
- A RIP sample of `RxLoop split-ws` (all JIT rips mapped by `CRATONVM_DBG_JITC` entry ranges; scratchpad
  `rip8/`) found 78.5k of 107.7k samples in compiled code, spread over about 30 hot bodies.
- 6.4 % of those compiled-code samples sit on **byte 0** of a body, and 14 % in the first 0xa0 bytes. Small
  bodies (`Pattern.peek`, `has`, `skip`, `unread`, `nextEscaped`, `ArrayList.<init>`) spend 50-90 % of their
  samples there.
- Every `ExecutableBuffer` is its own `VirtualAlloc`, so every body begins at a 64 KiB boundary (4 KiB on Unix,
  same effect). Byte X of every body therefore falls in the same L1i, uop-cache and BTB set. Round-robin calls
  over dozens of bodies keep evicting each other's entry and prologue lines.
- C probe `C:\craton\n23p\w8L5\c\alias.c`: JIT-like bodies, each in its own 64 KiB allocation, called
  round-robin on one P-core (ns/call, base-aligned vs coloured):

| bodies | base-aligned | coloured |
|---|---|---|
| 16 | 11.3 | 8.6 |
| 32 | 20.3 | 8.1 |
| 64 | 24.3 | 8.2 |

The same pattern holds for longer bodies (16.4 vs 32 ns/call at 64 bodies).

**Fix: `CRATONVM_JIT_CODE_ENTRY_COLOUR`** (perf, default ON, `=0` kill; `jit/src/exec_memory.rs`).
- `ExecutableBuffer::new` maps 4 KiB more than before. It starts the code `code_entry_colour(base)` bytes in:
  a whole cache line chosen by a splitmix64 hash of the base's page number, so 0 to 4032 bytes.
- `ptr` is still the code start and the entry, and every recorded offset is unchanged. The struct gains `lead`
  and `os_len`.
- The free (`free_executable(mapping_base, os_len)`), the tail trim (counted from the base) and `finalize` /
  `make_writable` use the mapping base.
- `platform::make_executable` / `make_writable` / `make_code_patchable` now round their range out to whole
  pages (`page_span`, the identity for an aligned pointer). The not-entrant patch and the test leaves protect
  through `as_ptr()`, and Unix `mprotect` refuses an unaligned address. Windows already behaved this way.
- Arena blocks (`new_in`, inert) are never coloured.
- Cost: after the trim, about half a page of extra commit per body on average (a page when the lead pushes the
  code over a page boundary).
- Tests:
  - `exec_memory`: `r23_w8_l5_colours_are_lines_under_the_span_and_spread` and
    `r23_w8_l5_a_coloured_buffer_trims_finalizes_and_runs`. The two old trim tests now pin `=0`.
  - `platform`: `r23_w8_l5_page_span_covers_the_range_from_its_page`. `a_protect_failure_carries_errno` now asks
    the OS arm directly and asserts that the public call succeeds.

**Estimate.** The C probe's 30-body figure is about 12 ns saved per call, but that is an upper bound. A
reasonable estimate for `split-ws`, `matches` and the Matcher rows is 5-15 %. The entry and prologue samples
are 10 % of all samples, and the same aliasing applies to the rest of each body. NodeCall `poly` (five hot
bodies) should not move.

**Measure (n23-w8d).** P-cores, 3 interleaved reps, default against `CRATONVM_JIT_CODE_ENTRY_COLOUR=0`:
- `RxLoop matches|split-ws|split-comma|replaceall 4` (`C:\craton\n23p\w5L5\cls`);
- `R23ArraysEqRegex`, all rows, in both modes (`C:\craton\n23p\w1L5\cls`). Facts must be unchanged;
- `NodeCall poly 3` (`C:\craton\n23p\w8L5\cls`) as a control;
- one broad battery row for commit size and code-cache behaviour.

**What is left.**
- The calls themselves: about 2.3 ns/call over HotSpot on `poly` (frame record, watermark save/restore, entry
  poll).
- The Node chain inlining (the opt-in hot-frequency switch above).
- Steady-state recompiles of `Pattern` compile internals, still logging at 22 s into a 40 s `split-ws` run.
  That is lane L1's `Pattern.compile`.

**Wave 8b follow-up (n23-w8f, pending build): the code-cache cap charge of a coloured body.**

`vm/tests/code_cache_sweep_phases.rs` `the_sweeper_recovers_a_code_cache_full_of_dead_phases` failed with the
colour on and passed with `=0`. The test's 1 MiB cap stayed full, and no phase-3 body could compile.

- The charge and the credit already matched each other, since both used `capacity`. The fault was the charge
  itself: a coloured trim keeps `roundup(lead + need) - lead` bytes. That is up to most of a page more than the
  uncoloured `roundup(need)` whenever the lead pushes the code over a page boundary, so bodies were charged
  about 50 % more on average.
- **Decision:** the cap counts code, as HotSpot's code cache does, and not the placement padding.
  - A new field, `ExecutableBuffer::charged`, holds the buffer's cap charge. Creation charges it, the trim
    reduces it to `trimmed_charge(len, page)` (the uncoloured trim's capacity, independent of the lead), and
    `Drop` credits exactly that field.
  - With the colour off, `charged == capacity` at every step, which is the old accounting.
  - The colour's real extra commit (at most one page per body after the trim) is deliberately not charged to
    the cap.
- Test: `r23_w8b_l5_a_coloured_body_charges_the_cap_what_an_uncoloured_one_does` (`exec_memory`, seven code
  lengths, coloured vs `=0`).

**Wave 8b A/B (orchestrator, n23-w8g, P-cores, 3 reps; rep 3 ran on a loaded host).** RxLoop, ns/op,
colour on vs `CRATONVM_JIT_CODE_ENTRY_COLOUR=0`:

| row | colour on | `=0` |
|---|---|---|
| matches | 3111 / 2877 / 3883 | 2746 / 2211 / 2805 |
| split-ws | 9266 / 7011 / 12056 | 9814 / 6382 / 12064 |
| split-comma | 1219 / 1017 / 1740 | 1395 / 974 / 1419 |
| replaceall | 4242 / 3779 / 4797 | 4451 / 3680 / 4406 |

There is no win, and `matches` is worse with the colour on. The switch is now **opt-in** (`=1`): the reader is
`runtime_flag_on` and the flag row has `off_word: None`. It is kept, not deleted. The cap-charge fix stays; with
the colour off it changes nothing (`charged == capacity`).

The coloured-arm tests now pin `=1`. These are `r23_w8_l5_a_coloured_buffer_trims_finalizes_and_runs` and
`r23_w8b_l5_a_coloured_body_charges_the_cap_what_an_uncoloured_one_does`.

NodeCall's line reads `poly 4,38 ns/op`, with a comma decimal from the host locale. A grep for `ns/op` matches it.

**The case for revisiting.** The C probe `C:\craton\n23p\w8L5\c\alias.c` measures the effect in isolation on
this CPU. Bodies are base-aligned vs coloured, one P-core, ns/call:

| bodies | base-aligned | coloured |
|---|---|---|
| 16 | 11.3 | 8.6 |
| 32 | 20.3 | 8.1 |
| 64 | 24.3 | 8.2 |

The split-ws RIP profile still puts 6.4 % of compiled-code samples on byte 0 of a body. The RxLoop rows did not
move, so one of two things is true:
- the entry-byte samples are skid from the indirect call or the return rather than L1i misses;
- the hot set on these rows is small enough, or the bodies are spread enough by their size, that set conflict
  is not the limit.

Before any re-flip, sample split-ws with `=1` and check whether the byte-0 share drops. Also A/B a workload with
many more hot bodies (Spring Boot, the R16 poly rows).

## Wave 9 (lane L5, 2026-10-22; pending build): the per-position Start -> Greedy call on `split-ws`

No CratonVM binary was on disk this wave. The findings below come from the n23-w8c `CRATONVM_DBG_JITC` log of
`RxLoop split-ws` (scratchpad `rip8/vm4.err`) and the w8c RIP sample.

**Which RxLoop rows run the Node chain at all.**
- `matches` and `replaceall` go through the `String` regex thin helpers (wave 4b), not the JDK `Pattern$Node`
  chain. The chain cannot move those two rows.
- `split-comma` takes the JDK single-char fast path.
- Only `split-ws` runs the chain: `Pattern.split` -> `Matcher.find` -> `search` -> `Start.match` -> one
  `next.match` per text position into `BmpCharPropertyGreedy` (`\s`).
- The wave-8 opt-in `CRATONVM_JIT_IR_GUARDED_HOT_FREQ_COUNT` targets `GroupTail.match`. Only `R23ArraysEqRegex
  regex-groups` reaches that method, so its wave-8 RxLoop A/B could not show anything. **Re-A/B it on
  `R23ArraysEqRegex -Dr23.rows=regex-groups,regex-find-email` before closing it.**

**The defect.**
- `Start.match` pc 44 is an in-loop site with 16 712 calls and one profiled class: 608, which is `Greedy`.
- The site was **refused**:
  `inline-resolve REFUSED Pattern$Node.match ... keep-lambda-site-caller: the SAM call at callee_pc=26 splices
  only in this body's own compile [tier=ir route=receiver=608]`. Only the 27-byte base `Node.match` was appended
  as an exact-receiver row, and it never applies.
- So every text position paid a full compiled call into `Greedy.match`, and from there a second call. There is
  no inlining across the chain's busiest edge.
- Lane L2's wave-5 rule `CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER` keeps a body as a call when its SAM call would
  stay a call inside the splice. That is right when the forwarder cannot be spliced one level down.
- But the nested lambda route (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_NESTED`, default ON) can splice it. It was
  only ever reached through the general nested interface rows, `CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE` and
  `_SPLICE_NESTED_PROFILE_TRAP`, which are both opt-in.

**Fix: `CRATONVM_JIT_IR_NESTED_LAMBDA_SAM_GUARDED`** (perf, default ON, `=0` kill;
`vm/src/runtime/interpreter/jit_bridge.rs`, inside the keep-the-caller block of `resolve_inline_site_from`).
- When a depth-0 body's SAM call names one lambda proxy (`one_lambda_proxy_receiver`: 80 % of an evidenced row,
  proxy id range, now shared with the keep rule), the resolver first resolves that proxy at `nest_depth + 1`.
  That resolution goes to `resolve_ir_lambda_sam_site`, which gives the forwarder plus its implementation.
- The forwarder is pushed as a nested guarded row: guard class = the proxy, miss edge = the call. The planner
  registers it as exact + `add_nested_guarded_receiver_site` under the default-ON
  `CRATONVM_JIT_IR_NESTED_GUARDED_SPLICE`, and the builder's `0xb9` arm takes it.
- Only when that is refused does the keep rule refuse the body, exactly as before.
- Sound for the reason every nested guarded row is: the test is exact, the body is the proxy's own forwarder, and
  any other receiver takes the unchanged call.
- `CRATONVM_DBG_JITC=1` prints `nest-lambda-guarded <site> at callee_pc=26 depth=1 -> proxy N (forwarder spliced
  behind a class test)`.
- Tests (`jit_bridge.rs`):
  - `r23_w9_l5_nested_lambda_sam_guarded_switch_and_order`: the switch default and kill, and a source pin that
    the guarded row is tried before the refusal;
  - `r23_w9_l5_one_lambda_proxy_receiver_reads_the_row`: the 80 % bar, the regex 76 % row, an ordinary class,
    the evidence floor.

**Expected.** On `split-ws`, `Start.match` now splices `Greedy.match` with its `\s` predicate inlined behind two
class tests. The per-position compiled call into Greedy and Greedy's predicate call both go. That is about 90
positions per op, at the 4-5 ns per compiled call that wave 8b measured, plus the frames. The rest of
`split-ws` is L6's and L1's (allocation, `Pattern.compile`, `split` bodies), so the row cannot close from this
alone.

**Build and A/B (n23-w9).** Run P-cores-pinned, 3 interleaved reps, default against
`CRATONVM_JIT_IR_NESTED_LAMBDA_SAM_GUARDED=0`:
- `RxLoop split-ws 4` (`C:\craton\n23p\w5L5\cls`), with `split-comma` as a control;
- `R23ArraysEqRegex` (all rows, both modes; facts unchanged, battery line `w1L5|R23ArraysEqRegex|||`).
  `regex-split` and `regex-find-*` take the same `Start` -> `CharPropertyGreedy` edge.
- Engagement: `CRATONVM_DBG_JITC=1 ... RxLoop split-ws 4 2>&1 | grep -E "nest-lambda-guarded|keep-lambda-site-caller"`.
  With the switch on, the `keep-lambda-site-caller` refusal for `route=receiver=608` should be gone.
- Also run `R23ArraysEqRegex -Dr23.rows=regex-groups,regex-find-email` with
  `CRATONVM_JIT_IR_GUARDED_HOT_FREQ_COUNT=1` against unset, the wave-8 opt-in on the rows it targets.

## Wave 12 (lane L5, 2026-10-23; pending build): `matchonly` calls Greedy once per text position

**Measured** on a scratch copy of `n23-w11a.exe`, P-cores. Probe: `W8L6SplitParts matchonly`, a `Matcher.find`
loop over `\s+` with no part allocation. CratonVM 2407-2791 ns/op, HotSpot 355-378, so 7x.

**Where the time goes.** RIP sample with `CRATONVM_DBG_JITC` entry ranges (scratchpad `w12L5/m0.*`): 99.6 % of all
samples are compiled code.

| body | samples |
|---|---|
| `BmpCharPropertyGreedy.match`, its own C2 body | 101k |
| `Matcher.search` | 67k |
| `matchOnly` | 16k |
| `Matcher.<init>` / `reset` | 10k |

Inside Greedy, the hottest bytes are the entry and the 912-byte fresh-frame clear (the zeroing loop and stores
at 0x22-0x9a, about 16 % of Greedy). The rest is its fast path, where each field read checks the global layout
flag and the per-object compact bit, and the `matcher` parameter's reference reads also take the six-compare
containment test.

**Why Greedy is a call per position.** The wave-9 row works in `Start.match`'s own compile: its artifact
splices `Greedy` at pc 44 with the `\s` lambda (`nest-lambda-guarded ... SPLICED`). But `Matcher.search`
profile-guard-splices `Start.match` at its pc 121. Inside that splice, Start's pc 44 gets only a nest-exact
row for the constant-pool class `Pattern$Node` (`nest-exact ... class 598`). That row never applies, and it
pre-empts the nest-profiled route; the receiver-evidence drop of that row is only under the opt-in
`CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_TRAP`. Even reached, Greedy (89 bytes, not a leaf) would fail the nested
leaf rule. So the inlined loop pays a full compiled call into Greedy's own body per position, about 90 per
`find` pass, each with the 912-byte frame clear.

**Fix: `CRATONVM_JIT_IR_KEEP_LOOP_SPLICE_CALLER`** (perf, default ON, `=0` kill;
`vm/src/runtime/interpreter/jit_bridge.rs` `keep_loop_splice_caller_enabled` /
`keep_loop_splice_caller_lost_class`, applied after the nested resolution of a depth-0 IR body). This is the
loop-site twin of L2's `CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER`.
- When a depth-0 body has an in-loop virtual or interface call (`bytecode_pc_in_loop`) that its own optimizing
  artifact guard-splices (`CompiledMethod::guarded_splice_class_at_bci`), and no nested row is guarded on that
  class, the body is refused:
  `inline-resolve REFUSED Pattern$Node.match ... keep-loop-splice-caller: the in-loop call at callee_pc=44 is spliced (class 608) only in this body's own compile`.
- `Matcher.search` then makes one call per `find` into `Start.match`'s artifact, whose loop runs with Greedy
  and the lambda spliced, instead of one call per position.
- Test: `r23_w12_l5_keep_loop_splice_caller_rule` (switch default and kill; the pure rule on the pc-44 shape,
  a reproduced class, a bimorphic second, not in a loop, no guard, static and special calls).

**Not taken, measured on w11a (2 reps each, noisy and bistable).** `TRUSTED_REF_GETFIELD=1` gave 2371 / 3922.
`ZERO_FRESH_FRAMES=0` gave 2678 / 2303. `NESTED_GUARDED_HOT_CAP_LEAF=0` gave 2704 / 2739.
`SPLICE_NESTED_EXACT_TRAP=1` gave 2532 / 4133. `DEOPT_IMAGE_SIZED + SAVE_BANDS_SIZED` gave 2765 / 2972. Default
was 2791 / 2550. None is a clear lever on its own.

**Measure (n23-w12).** Pinned, 3 interleaved reps, default against `CRATONVM_JIT_IR_KEEP_LOOP_SPLICE_CALLER=0`:
- `W8L6SplitParts matchonly 4` and `cached 4` (`C:\craton\n23p\w8L6\cls`);
- `RxLoop split-ws 4` (`C:\craton\n23p\w5L5\cls`);
- `R23ArraysEqRegex`, all rows, both modes (`C:\craton\n23p\w1L5\cls`; facts unchanged).

Engagement: `CRATONVM_DBG_JITC=1 ... matchonly 4 2>&1 | grep keep-loop-splice-caller`. Also run a broad
battery row (Spring Boot or R16 poly): the rule refuses depth-0 splices whose loop call the callee's artifact
splices, anywhere.

### n23-w12a result (orchestrator, 2026-10-23)

`CRATONVM_JIT_IR_KEEP_LOOP_SPLICE_CALLER` **lost** and is now **opt-in** (`=1`). Pinned, 3 interleaved reps, `W8L6SplitParts`, ON (the default at the time) against `=0`:

| Row | ON (ns) | `=0` (ns) |
|---|---|---|
| `cached` | 4481 / 4314 / 4066 | 3337 / 3216 / 3235 |
| `full` | 5286 / 5237 / 5516 | 4402 / 4512 / 4408 |
| `matchonly`, `comma`, `list` | unchanged | unchanged |

Keeping the caller as a call costs more than the per-iteration compiled call it was meant to save. The gap stays open.
