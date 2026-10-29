# find-email: the "arg shuffle" moves are residency copies, and the second predicate class is a miss-edge call

**Status:** OPEN. One opt-in switch is built and waiting for a host run.

**Area:** `jit/src/ir_lower.rs`, in `gp_load_value`, `publish_def_at_store`, `store_rax` and
`verify_publish_reload_elides`.

**Found:** rx27 wave 9, lane regalloc (code), with the prof lane's census and disassembly on b16 (int16
`78333dc842`, chain bundle default ON). Host files: `/root/rx27/prof/lanes/b16/email-*.txt`, plus `email.asm` and
`email.map`.

**Parent:** `rx27-w4-per-row-instruction-breakdown-20261028.md`. It priced "arg shuffles and IC calls into the
predicate lambdas (`reg move` 4.3k): 2.2k".

## What the profile's `reg move` class is

The b16 profile, about 35.9k instr/op:

| symbol | instr/op | share |
|---|---:|---:|
| `Pattern$Start.match` | 23 574 | 65.7 % |
| `Pattern$BmpCharPropertyGreedy.match` | 9 661 | 26.9 % |
| `spin` | 1 788 | 5.0 % |
| `Pattern.lambda$Range$0` | 504 | 1.4 % |
| `Pattern$LastNode.match` | 305 | 0.8 % |
| `lambda-adapter` stub | 43 | 0.1 % |

* **No ABI marshalling is hot.** A spliced body takes its arguments as SSA values (`IrBuilder::begin_splice`
  pops them off the abstract stack), so only a remaining real call can emit argument moves.
  * Start.match has 211 `mov reg,reg` instructions, taking 9.8 % of its samples. The greedy node has 169, taking
    4.1 %.
  * None of the hot ones moves a value into rdi/rsi/rdx before a `CALL`.
  * The only `CALL`s in those bodies are `jit_getfield` slow arms. They are not executed: the call, its `cmp` and
    its `jo` have 0 samples.
* **The hot moves are the accumulator lowering's residency copies.** They come in three shapes:
  1. **Publish, then read straight back.** For example, Start.match +2140: `movsxd rax,[rax+rcx*4+10h]` /
     `mov rbx,rax` / `mov rax,rbx` / `and eax,10700h` (the `ASCII.ctype` word). The same shape is at +feb and
     on the greedy node's byte loads (+20b7, +55c).
  2. **A bounds-check index copied into ECX.** For example, +29da and +2126: `mov ecx,r15d` / `cmp ecx,[rax+8]` /
     `jae`. `gp_load_index_rcx` needs ECX zero-extended for the `[rax+rcx*k+16]` address that follows.
  3. **A boolean materialised in a register.** For example, +2a2f: `mov r13d,0|1` / `mov rax,r13` /
     `mov [rbp-90h],rax` / `cmp dword [rbp-90h],0` / `je`.

## The shared `predicate.is` site

* **Is the second lambda class still a real call? Yes.** `lambda$Range$0` and its adapter stub are separate
  symbols (504 + 43 instr/op).
* **No pair forms at pc 26.** The pc-26 row names a single class, 2147483652 (the union lambda,
  `lambda$union$0`). The Range lambda (`[a-z]{2,}`, the third greedy node) is too rare at that shared site for
  the pair shares (`nested_bimorphic_receiver_pair`). It runs only after a `.` matched. So it is the
  miss edge of the single-class guard: an IC call through the adapter thunk into its own compiled body.
* The census shows `nest-pair-declined ... callee_pc=26 depth=6 ... the row names no pair` and many
  `inline-resolve REFUSED Pattern$CharPredicate.is ... ir-splice-target-not-provably-monomorphic` lines.
* At depth 6 (`CRATONVM_JIT_IR_INLINE_MAX_NEST`) the depth-3..5 splices stop: `refused by the resolver (no
  body)`, and `Node.match` at pc 70 is over its cap.
* **Price.** At most the 547 instr/op of the two symbols, plus the call overhead in the greedy body. A pair at
  that site would need the shares lowered for one row, which is the chain lane's admission and not a lowering
  change. It is the smaller lever.

## Built: `CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE` (opt-in, `dbc4a18eeb`)

The switch removes shape 1.

* `publish_def_at_store` records the value, its register and the code offset where the publish ends. Its own
  home store right behind the publish keeps that record.
* A read of that value into RAX at exactly that offset (`gp_load_value`) emits nothing: RAX is the value on the
  fall-through path. This is the same `buf.pos()` proof the tail carry uses.
* A block start clears the record.
* After the body, `verify_publish_reload_elides` scans every rel32 that may name a skipped read, with no
  exemptions. A hit refuses the attempt for that value, and the retry lowers again without it (`elide_excluded_ids`).
* MIR mode turns it off.
* Only reads into RAX change. Home words, frame states, GC maps and the deopt image are unchanged.

**Census:** `[cratonvm-jitc] ir-publish-reload-elide <method>: skipped=N refused=R excluded=E`. A positive
control is `skipped > 0` on `Pattern$Start.match` under `=1`.

**Tests:** `publish_reload_elide_tests` (`rx27w9_a_read_behind_its_publish_is_not_emitted`, `rx27w9_the_scanner`).

**Expected effect, by reading:**
* The removed instructions are a share of the 2.6k `reg move` in Start.match and the 0.4k in the greedy body.
  The disassembly shows at least three hot sites of the shape, each with 70-140 samples.
* A few hundred instr/op on find-email. Similar on every row whose spliced bodies publish and read back,
  which is all of the regex rows.
* It is not the profile page's 2.2k. Most of that class is shapes 2 and 3.

## Not built (next levers in this class)

* **Shape 2.** Comparing and addressing with the resident index directly needs its high half proven zero. A
  32-bit producer zero-extends; a value published from a 64-bit RAX does not. One `MOV ECX, r32` per access
  remains unless the planner proves that per value.
* **Shape 3.** A boolean feeding only an `If` should branch on the producer's flags (the fused-compare path). Its
  frame home and slot compare come from it being named in a frame state or having two uses. That is the
  `ir-home-keep` census's question.
* **The ctype test.** The `and eax,10700h` / store / `cmp dword [rbp-138h],0` at +2140 is the shape
  `CRATONVM_JIT_IR_AND_ZERO_TEST_BRANCH` folds into one `TEST`. On b9, azt + azg + ebb (`rall2`) gave find-email
  -2.0 %, disjoint.

## Host arms

```
DIR=/root/rx27/p REPS=5 tools/probes/icount2.sh rx27-pre "regex-find-email regex-find-ci matchonly regex-matches regex-groups split-ws compile" \
  def:$T: \
  pre:$T:CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE=1 \
  prez:$T:CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE=1,CRATONVM_JIT_IR_AND_ZERO_TEST_BRANCH=1,CRATONVM_JIT_IR_AND_ZERO_TEST_GUARD=1 \
  hs:hs:
CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE=1 CRATONVM_DBG_JITC=1 $B -cp $P R24RegexSplitRows count regex-find-email 200000 200000 2>&1 \
  | grep -E 'ir-publish-reload-elide .*(Start|BmpCharPropertyGreedy)|ir-elide-refused'
```

Also run `R24RegexSplitRows facts` in both modes with `pre`, and the jit lib suite with the switch exported.

## b17 engagement (int17 `f5b421d7a7`, lane prof)

Run: `count matchonly 200000 200000` with `=1`.

* **Skipped reads:**
  * Start.match: 1.
  * BmpCharPropertyGreedy.match: 3.
  * Matcher.search: 2.
  * ConcurrentHashMap.addCount: 4.
  * Every other method, the regex predicates included: 0.
* **Refused:** 0 in every method. No branch landed on a skipped read.
* **Facts:** 16/16 identical to HotSpot in both modes under def and pre, the GC battery included.

The counts are small. The hot pairs this page cites are in find-email's bodies, not matchonly's. Still pending:

* the find-email census;
* the def/pre pair count in the find-email disassembly;
* the icount2 set (def / pre / prez / oip / hs).

If pairs remain under pre, their producer publishes by a route the switch does not hook. Hooked today:
`publish_def_at_store` and the home store right behind it. Not hooked: `gp_store_value`, the `lea` forms, and the
W9-2 direct ALU.

### find-email on b17 (lane prof, `/root/rx27/prof/lanes/b17/email-pre.txt`)

* **Skipped reads:**
  * Start.match: 2.
  * BmpCharPropertyGreedy.match: 3.
  * BmpCharProperty.match: 1.
  * `lambda$union$0`: 1.
* **Refused:** 0 everywhere.
* **Adjacent publish/read-back pairs:** Start.match goes from 1 to 0, BmpCharPropertyGreedy from 2 to 0. The def pairs
  are the ones this page names: the `ctype` read at +2133 (`mov r13,rax` / `mov rax,r13`), and the greedy node's
  byte loads at +55c and +2fcf.
* **Static instruction counts grow anyway:** Start.match 3727 -> 3734, the greedy node 3491 -> 3496. The switch
  emits no new instruction, so the growth is a different compile shape. The two runs' bodies differ elsewhere.
* The removed pairs are 3 instructions on the per-character path, so the expected gain is small. The icount2 set
  decides.

### icount2 on b17 (lane prof; main/op, median of 5, ranges in brackets)

| row | def | pre | prez (pre + azt + azg) | oip |
|---|---:|---:|---:|---:|
| regex-find-email | 36 192 (36 192-38 289) | 36 012 | **35 382 (35 125-35 833)** | 36 341 |
| regex-find-ci | 24 455 (24 455-26 010) | 24 451 | 24 439 | **23 808 (23 779-23 822)** |
| matchonly | 27 018 (25 671-28 411) | **25 552 (24 898-25 552)** | 27 417 | 25 430 (24 875-26 779) |
| regex-matches | 14 728 | 14 782 | 14 611 | 15 361 |
| regex-groups | 21 251 | 20 908 | 21 028 | 21 053 |
| split-ws | 49 676 (40 388-52 972) | 50 843 | 49 077 | 50 362 |
| compile | 4 377 | 4 450 | 4 385 | 4 343 |

**pre alone** is -0.5 % on find-email, with overlapping ranges. Its only disjoint reading is matchonly, -5.4 %, but
that row is bimodal: def's range spans 25.7-28.4k, and pre's top sits just under def's bottom. Removing three
instructions per character does not explain 1.5k instr/op. So this is not yet evidence that pre pays. The rule
says it goes unless an interleaved repeat on matchonly and find-email shows the same thing.

**prez** is -2.2 % on find-email, disjoint. That is the azt + azg gain again: `rall2` gave -2.0 % on b9, also
disjoint. It is the and-zero folds that pay on find-email, not pre.

**oip** is -2.6 % on find-ci, disjoint; b15 had the same median with overlapping ranges. Together with compile
-3.0 % on b15 (psc + oip), it is the one regalloc switch with two disjoint readings.

Facts under pre: 16/16 equal HotSpot in both modes.

## Flip (2026-10-29, coordinator decision on b17)

`PUBLISH_RELOAD_ELIDE`, `AND_ZERO_TEST_BRANCH`, `AND_ZERO_TEST_GUARD` and `OPERAND_IN_PLACE` are default ON together,
and `=0` kills each one.

* **Evidence:** the 14-row, 5-rep run. `rb` (all four on top of the chain default `dse0`) against `dse0`: find-ci
  -2.8 % and find-email -4.8 %, both disjoint, and no row worse.
* **Why pre stays in:** without it (`rbx`), find-ci stayed bimodal (23 832 / 25 383); with it, `rb` reads
  23 828-23 852.
* **Facts:** identical, and the GC battery (G1, Serial) is identical in both modes.

MIR mode still turns all four off. EBB was removed in the same series.
