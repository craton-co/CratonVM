# Matcher rows: the chain entry, `Matcher.group` and the predicate pairs stay calls

**Status:** OPEN (the rows are still over the gate). On 2026-10-29 the coordinator flipped the bundle to
default ON (`=0` kills each switch). See "Bundle flip" below.

**Area:** the splice admission for nested calls. `jit/src/lib.rs`:
* `mod rx27_chain_entry`;
* `mod rx27_nested_in_loop_hot`;
* `r25_w7b_chain_splice::chain_bimorphic_predicates_enabled` / `nested_pair_admitted_row`;
* `strip_chain_rows`, `ir_nested_guarded_body_cap_in`, `ir_psplice_nested_exact_row_admitted`,
  `ir_splice_prune_cold_nested_in`.

The VM resolver is `vm/src/runtime/interpreter/jit_bridge.rs` `resolve_inline_site_from` (the profiled-row
filter and the pair arm; not bimodal's keep-loop block).

**Severity:** medium. The profiler's page (`rx27-w4-per-row-instruction-breakdown-20261028.md`) prices these at
1.3k (`regex-matches`), 1.5k (`regex-groups`) and 2.2k (`regex-find-email`) instructions per op.

## Evidence (by reading against the profile; the census below confirms or corrects it)

In both rows the driver's compile splices the row lambda, so the lambda's calls are NESTED rows. Nested rows
answer to stricter rules than top-level ones.

1. **`regex-matches`.** `matches()` -> `match()` splice: a forwarder, then a nest-exact body. `match()`'s
   `parentPattern.matchRoot.match(..)` is a profiled nested row of `BmpCharPropertyGreedy.match` (89 bytes, with
   calls).
   * The plain nested rule admits a body over 35 bytes only as a leaf.
   * The W7-B chain rule admits it only at a call of the body's own selector. `Matcher.match(II)Z` ->
     `Node.match(..)Z` is not one.

   So the greedy node's own artifact is called once per subject. That costs its 928-byte frame clear
   (1 155 instr/op), a stack guard and argument moves: the 7.2k symbol. Two more things come into it:
   * `Pattern.matcher` is a call unless `CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM` is on (catch-all handler).
     Under it, `Matcher.<init>` (whose `reset()` fills `groups`) is a nest-exact row with a loop in its tree,
     which `ir_psplice_nested_exact_row_admitted` refuses over 35 bytes.
   * The greedy node's `predicate.is` site sees two lambda classes (`[a-z]` and `\d`, one bytecode site
     shared by both `BmpCharPropertyGreedy` instances). So one lambda stays an inline-cache call:
     `CharPredicates.lambda$ASCII_DIGIT$0` (361 instr/op, `reg move` 119), and the link rule then refuses
     the greedy body (`keeps a call at body pc ...`).
2. **`regex-groups`.** `m.group(1)` / `m.group(2)` sit in the lambda's `while (m.find())` loop. `Matcher` is
   final, so the row is statically bound (guard 0). Over 35 bytes, the planner's cold prune
   (`ir_splice_prune_cold_nested`) keeps such a row only on its body's entry prefix or the profiled dominant
   path. A loop body is after the loop's branch.
   * If the census shows `ir-splice-cold-nested` for the lambda's tree, that is the refusal.
   * If it shows `nest-static-final ... group` refused, the nested hot cap (256 samples or the entry prefix)
     refused it.

   Both now read "in a loop" as hot under one switch.
3. **`regex-find-email`.** The `reg move` class (4.3k) on `Start.match` / `BmpCharPropertyGreedy.match` comes
   in two kinds:
   * **IC calls at the shared `predicate.is` site.** The pattern's greedy nodes run the union lambda (two
     instances, one proxy class) and the `[a-z]` range lambda, so the second class is a real call with its
     argument moves. That is fixed below.
   * **Values held in frame homes inside the spliced bodies.** Spliced arguments are already SSA values
     (`IrBuilder::begin_splice` takes them off the abstract stack), so the moves left there are home and
     spill traffic. That is the regalloc lane's lever, not a splice change.

## Revised the same day: "dse0" (entry opt-in again, deep spine ON; pending build)

Lane bimodal's A/B on bimodal-10 (14 rows, 5 reps) found `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE` was the one
bundle member that regressed `cached` (+19 %) and `split-ws` (+14 %). The default set is now:

* the entry back to opt-in;
* the other four bundle switches ON;
* `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` default ON.

Against the bundle default: `cached` 45.1k -> 35.8k (-21 %), `split-ws` 49.3k -> 42.9k (-13 %),
`regex-split` -5 %, every other row within 1.5 %. Facts, GC probes and `J27DeoptRows` are identical in both
modes.

## Bundle flip (2026-10-29, coordinator decision; pending build)

These five switches are now default ON together, each with a `=0` kill:

* `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`
* `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`
* `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES`
* `CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM`
* `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS`

Evidence: on regex-matches, -8.6 / -8 / -11.8 % on b7 / b8 / b9, with disjoint ranges each time;
regex-groups -11.5 % on b7; find-email and find-ci -0.3 / -0.4 %, disjoint; facts and the GC battery
identical. They flip together because the entry alone regressed regex-groups by 11 %. The code anchor is
`rx27_chain_entry::bundle_flip_evidence`.

Every unit test that assumed "unset means off" now pins `=0`. The commit lists them. Still to run: the full
suites, the 89-probe battery in both modes, and a broad compile row (the in-loop hot cap touches every
compile).

## Measured on cvm-rx27b8 / b9 (lane prof, icount2 main/op, median of 5, ranges in brackets; with the wave-7 fix)

| row | b9 def | b9 allc | allc/def | b8 def | b8 ent | b8 allc |
|---|---:|---:|---:|---:|---:|---:|
| regex-matches | 16 642 (16 641-17 392) | 14 682 (14 612-14 708) | 0.88, disjoint | 16 753 | 16 596 | 15 390 (14 612-16 516) |
| regex-groups | 21 776 (21 775-24 512) | 20 813 (20 811-24 751) | 0.96, overlapping | 22 720 | **25 209** (24 951-25 647) | 21 651 |
| regex-find-email | 35 842 | 35 737 | 1.00, disjoint | 35 842 | - | 35 737 |
| regex-find-ci | 24 559 | 24 455 | 1.00, disjoint | 24 792 | - | 24 483 |
| matchonly | 30 274 (27 716-30 835) | 27 018 (25 671-28 411) | bimodal | 30 274 | - | 28 411 |

* **`allc` (entry + in-loop + predicate pairs + the cold handler arm).** regex-matches is 11.8 % below
  `def` on b9 with disjoint ranges, and 8 % on b8. That is under the 15 % flip bar, so the switches stay
  opt-in.
* **`ent` alone regresses regex-groups by 11 %** (b8, disjoint). The chain entry must not be used
  without the predicate pairs. The cause is not yet read; the census to take is
  `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE=1 ... count regex-groups` with `chain-entry|nest-chain|ir-chain-tree`.
* The `allcg` arm on b9 (`allc` + `GUARDED_HOT_BY_RECEIVERS`) gave the same regex-groups median as
  `allc` with a narrower range. That switch was removed (`75b8639190`) because it was inert for its case.
  The narrower range reads as the bimodality's sampling, not as an effect.
* Facts: R24 and the b9 battery (34/34) equal HotSpot in both modes under `allc`.
* Engagement on b10 (int12): all 6 `allc` processes plan `Matcher.match` at 1 020 bytes with the pc-26 pair.
  The only `nest-pair-declined` lines are `a body was refused`, at depth 5-6 (the nest budget).

## Measured on cvm-rx27b7 (lane prof, icount2 main/op, median of 5; before the wave-7 fix)

| row | b6 | def | ent | loop | pred | allc | allc/def |
|---|---:|---:|---:|---:|---:|---:|---:|
| regex-matches | 16 538 | 16 540 | 16 700 | 16 503 | 16 863 | 15 124 | 0.91 |
| regex-groups | 22 724 | 23 413 | 21 939 | 22 490 | 22 826 | 20 709 | 0.89 |
| regex-find-email | 38 119 | 39 231 | 38 122 | 38 113 | 37 880 | 38 062 | ~0.97 (noise) |
| regex-find-ci | 27 850 | 26 907 | 26 907 | 26 907 | 26 907 | 26 863 | 1.00 |
| matchonly | 28 860 | 27 075 | 28 959 | 27 075 | 29 520 | 27 075 | 1.00 |

* Only `allc` moves anything, and only on regex-matches and regex-groups. On regex-matches `allc` is the
  only arm with the cold handler arm, so `Pattern.matcher` splices in the lambda. With it, `Matcher.match`'s
  own compile grows from 89 to 1 020 spliced bytes (`ir-splice-nested-exact 18 bodies` at pc 121). That is
  the likely source, since the chain-entry rule itself never fired.
* `pred`'s regex-matches and matchonly readings are bimodal (12-22 % spread).
* Neither `allc` gain reaches the 15 % bar, so all four switches stay opt-in.
* Facts: R24 14/14 equal HotSpot in both modes under `def`, `allc` and `loop`.
* Unit tests: `rx27_chain_entry_*`, `rx27_in_loop_*` and the predicate-pair test pass on b7.
* The re-run on cvm-rx27b8 (with the wave-7 fix `b5035b9605`) is queued.

## Engagement on cvm-rx27b8 (with the wave-7 fix; lane prof, `/root/rx27/prof/lanes/b8/chain-q.txt`)

The chain entry engages under `allc`:

* **regex-matches.**
  * `chain-entry Pattern$Node.match at callee_pc=121 depth=1/2/4 in Matcher.match(II)Z`.
  * `nest-exact ... callee_pc=121 -> class 576 dropped (the receiver evidence names another class)`.
  * `nest-profiled ... -> guard on class 586 (body 89 bytes, cap 325; samples 1170)` at depths 1 and 2.
    At depth 4 the resolver has no body: the nest budget.
* **regex-groups.** The same in `Matcher.search(I)Z`: class 591 (`Start`), 422 samples, depths 1 and 2.

Facts: 14/14 equal HotSpot in both modes under `def` and `allc`. The b8 icount2 set is pending.

## Wave 7: why `chain-entry` never printed on cvm-rx27b7, and the fix (same switch)

The host census on b7 (`/root/rx27/prof/lanes/b7/chain-q.txt`, lane prof) shows the following:

* `Matcher.match(II)Z` (regex-matches) and `Matcher.search(I)Z` (regex-groups) are compile roots of their
  own. Each guard-splices the greedy node / `Start` at its pc 121 as a TOP-level hot site
  (`ir-guarded-hot-cap ... at pc=121 in Matcher.match: class id 586`).
* In the row lambda's compile, `Matcher.matches` -> `Matcher.match` is spliced (`ir-splice-bytes ...
  Matcher.matches ... 191 bytes`). Its pc 121 then resolves only through the constant-pool exact row
  (`nest-exact ... callee_pc=121 -> class 576`, `Pattern$Node`, whose body no receiver runs). The profiled
  route, where the entry rule sits, was never reached.

Two reasons stack:

1. **The exact row was not dropped.** `ir_nested_receiver_evidence_contradicts` reads the enclosing
   body's interpreted row and its artifact's inline cache. `Matcher.match` compiled before the lambda, so
   its interpreted profile has no row at pc 121. Its artifact SPLICES that site, so it has no cache there
   either. Neither read contradicts `Pattern$Node`.
2. **Reached, the entry would have been `ColdSite`.** pc 121 is not in a loop of `Matcher.match`, not on
   its entry prefix, and has no samples.

Fix, under `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`:

* The artifact's guard-spliced class (`guarded_splice_class_at_bci`) also contradicts the exact row, and
  the contradicted exact row is dropped at any site under the switch.
* The profiled route already takes that class (`CRATONVM_JIT_IR_NESTED_GUARD_CLASS_EVIDENCE`, default ON).
  For an entry body over the cold cap, with no interpreted row, the row is synthesized from that evidence
  (`rx27_chain_entry::artifact_hot_row`): `max(100, the enclosing body's invocations)` samples of the
  class. The artifact splicing a body over 35 bytes there means its compile judged the site hot.
* The VM carries that row in `callee_receiver_profile`, so the planner's link rule reads the same row.

Test: `rx27_chain_entry_an_artifact_guarded_site_is_hot`.

Expected host lines with `allc`: `chain-entry java/util/regex/Pattern$Node.match... at callee_pc=121 ...
in java/util/regex/Matcher.match(II)Z`, then `nest-chain ... -> class 586 SPLICED`. The `nest-exact
...callee_pc=121 -> class 576` line goes away in the lambda compile.

## Change (opt-in switches)

* **`CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`** (item 1). A profiled nested row whose BODY is chain-shaped is
  admitted by `chain_link_verdict`:
  * a hot site;
  * the multi-body hot cap;
  * every call of the body spliced, or else its next link.

  It applies in the VM resolver and in the planner (`chain_row_admitted_in`). `strip_chain_tree` prices it as
  a link, rooted when its site or any site above it is in a loop (new `ancestor_loop` argument).
  Census: `chain-entry ...` followed by that site's `nest-chain ... SPLICED | refused: <why>`.
* **`CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`** (items 1 and 2). A nested call in a loop of its enclosing body is
  hot:
  * the nested hot cap applies (profiled rows keep their leaf rule);
  * a nest-exact tree may hold a loop (`Matcher.<init>`);
  * the cold prune keeps that row and every row below it (`Matcher.group`).
* **`CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES`** (items 1 and 3). An interface site of a chain body may hold
  the SPLICE19-4 two-class row, as wave 3's `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS` lets a chain site hold
  one. The resolver's pair arm, the planner strip and append (`nested_pair_admitted_row`) and the builder
  gate (`nested_pair_switch_on`) all ask it.

None of them emits anything new. Each adds bodies through rows, guards and miss edges that already exist.

**Risks.**
* `NESTED_IN_LOOP_HOT` is broad: every compile's in-loop nested sites. Compile size and time need a broad
  row.
* A pair of lambda forwarders is a frameless nested pair. If the builder declines that shape, the W5-2
  rebuild drops the site, which costs the site and never soundness. `ir-nested-bimorphic-splice` in the
  census says whether it opened.

## Host

```
B=/root/r25-bin/cvm-$T; P=/root/rx27/p
ALL="CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE=1 CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT=1 CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES=1 CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1"
env $ALL CRATONVM_DBG_JITC=1 $B -cp $P R24RegexSplitRows count regex-matches 20000 20000 2>&1 \
  | grep -E 'chain-entry|nest-chain .*in java/util/regex/Matcher|guard on classes|ir-nested-bimorphic-splice|ir-splice-cold-nested|splice-cold-handler-arm|Matcher\.<init>'
env $ALL CRATONVM_DBG_JITC=1 $B -cp $P R24RegexSplitRows count regex-groups 20000 20000 2>&1 \
  | grep -E 'Matcher\.group|ir-splice-cold-nested|nest-static.*group'
```

Expected:
* a `chain-entry java/util/regex/Pattern$Node.match... in java/util/regex/Matcher.match(II)Z` line, then
  `nest-chain ... SPLICED`;
* `guard on classes A+B` at the greedy node's `predicate.is` pc;
* no `ir-splice-cold-nested` line for the lambda's `group` rows.

`perf` should lose the `BmpCharPropertyGreedy.match`, `Pattern.matcher`, `Matcher.group` and
`lambda$ASCII_DIGIT$0` symbols on these rows.

```
DIR=/root/rx27/p REPS=5 tools/probes/icount2.sh rx27f5 "regex-matches regex-groups regex-find-email regex-find-ci matchonly" \
  def:$T:CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE=0 \
  ent:$T:CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE=1,CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES=1 \
  loop:$T:CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT=1 \
  pred:$T:CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES=1 \
  all:$T:CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE=1,CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT=1,CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES=1,CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1 \
  allc:$T:CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE=1,CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT=1,CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES=1,CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM=1,CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS=1 \
  hs:hs:
```

Expected, by reading:
* `ent` and `all` on `regex-matches` (up to the 1.3k plus the greedy node's frame clear);
* `loop` on `regex-groups` (part of `Matcher.group`'s 3.6k: the call, frame and argument moves; the
  substring allocation stays);
* `pred` on `regex-find-email` (the second-lambda calls);
* `matchonly` and `find-ci` flat, or small gains.

Batteries:
* `R24RegexSplitRows facts` in both modes with `allc`;
* the jit lib suite (new tests below) and the vm lib suite (`jit_bridge.rs` changed);
* a broad compile row (Spring Boot or `R16AllocPremium hashmap-cached`) with `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT=1`
  for compile time and code size.

## Tests (`jit` lib)

* `rx27_chain_entry::tests::rx27_chain_entry_a_chain_body_entered_from_a_non_chain_site_is_a_link`
* `rx27_chain_entry::tests::rx27_chain_entry_a_loop_above_roots_the_entry`
* `rx27_nested_in_loop_hot::tests::rx27_in_loop_a_nested_site_in_a_loop_takes_the_hot_cap`
* `rx27_nested_in_loop_hot::tests::rx27_in_loop_the_cold_prune_keeps_a_row_in_a_loop`
* `r25_w7b_chain_splice::tests::rx27_chain_a_chain_bodys_predicate_site_takes_a_pair`

## What would close it

Each lever's census engages, `allc` moves the three rows by the profiler's estimates, and facts equal HotSpot in
both modes. Any lever that engages without moving its row gets that number written here before it is
withdrawn.
