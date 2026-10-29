# The regex position loop re-reads its node fields per character (rx27 lane scanloop)

**Status: OPEN.** Area: JIT optimizing tier, loop proofs (`jit/src/ir_optimize/seq_charat_predicate.rs` W7-D P2,
`r25_w12_4`). Severity: performance only (no wrong answer); the top residual of the `\s+` / `\w+` rows on
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`.

## rx27 wave 4 (lane scanloop, 2026-10-28; pending build): P-1 as identity class tests

`CRATONVM_JIT_IR_SEQ_CHARAT_IDENTITY_TESTS` (opt-in). This is P-1 without the writer proof that P-2 would supply.

P2 refuses the position loop because the back-off `next.match` call can write anything. This arm leaves the reads
where they are and replaces only the class TESTS:

* At the loop entry it reads the class-tested chain once: `P1 = this.next`, `P2 = P1.predicate`, and so on.
* It guards each entry value's exact class there (deopt to the loop entry, charged as a class predicate).
* Each in-loop test `ExactClassIs(x, Ck)` of a read of that chain becomes `x == Pk`. This covers the guarded
  splice's `If` and the guard P2 planted at the inner scan loop's entry.

Why it is sound: `x == Pk` implies `x`'s class is `Ck`, so the new test is true only where the old one was. If the
node graph changes under the loop, the `If` takes its miss arm (the virtual call), and a `Guard` deopts at its own
frame. The reads stay, so every write in the loop, calls included, is still seen. A key whose field the loop itself
stores to is not taken.

What it saves per position: the class tests become one compare each, which the default compare fusion turns into
`CMP`/`Jcc`. That is item (b), and the class half of item (a). The `next` / `predicate` loads themselves stay
until the back-off link is spliced (P-2, chain-splice admission, another lane). With P-2 in, P2's induction would
also take the reads out of the loop.

* Code: `seq_charat_predicate::w7d::plant_identity_tests` (`identity_keys`, `identity_tests_in_loop`), run after
  P2 on the loops the String predicate planted.
* Unit tests: `rx27w4_a_class_test_behind_a_writer_becomes_an_identity_compare` (P2 refuses the same loop),
  `rx27w4_a_chain_of_reads_is_keyed_through_the_entry_values`.
* Census: `[cratonvm-jitc] ir-seq-charat-identity-tests <method> header nR bci B: keys= entry guards= ifs= guards=`
  and `refused: <why>` when nothing was planted.

## rx27 wave 3 (lane scanloop, 2026-10-28; pending build): the String reads and bounds of the position loop

`CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST` (opt-in). It turns on three things together:

* The `charAt` hoist (`charat_trap::hoist_seq_charat_string_reads`). It already puts the `String` class guard and
  the `value` / `coder` / length reads at the entry of the String-guarded loop, which is `Start`'s OUTER position
  loop.
* A new `coder == LATIN1` guard at that entry. With it, the call becomes one byte load and `& 0xFF`. This replaces
  the branchless LATIN1/UTF16 decode, about ten instructions per character. Kill switch:
  `CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST_LATIN1=0`.
* The entry bound (W6C-P3), offered the nested scan loop (W12-4). The greedy scan's own `i < to` test, with `to`
  read once per position, then answers both per-character index guards. It does so with `to <= count` and
  `0 <= init` at the INNER entry, once per position.

Per character, what is left of `charAt` is the byte load and the mask.

That is the part of the W13-4 loss this arm can name from the IR: the decode and the two guards. The other
suspect, values live across the outer body, cannot be checked without the listing. A UTF16 subject deopts at the
outer entry. After the de-spec limit, that header plants nothing: the class kind, as for the String guard.

Unit tests:

* `seq_charat_predicate::w7d::tests::rx27w3_the_position_loop_pays_one_byte_load_per_character` (a nested
  position / scan loop: after plant, hoist and bound, one byte load at the call, no per-character index guard,
  `coder == 0` at the outer entry, `to <= count` and `0 <= i` at the inner entry; off: the call stays);
* `rx27w3_the_latin1_kill_switch_keeps_the_branchless_decode`;
* `charat_trap::tests::rx27w3_the_outer_hoist_switches`;
* `charat_trap::tests::rx27w3_the_outer_hoist_reads_one_latin1_byte`.

Engagement (`split-ws`, `regex-find-ci`, `regex-find-email`):

```
CRATONVM_JIT_IR_SEQ_CHARAT_OUTER_HOIST=1 CRATONVM_DBG_JITC=1 CRATONVM_DBG_LICM=1 \
  $B --jdk-only -cp /root/rx27/p R24RegexSplitRows count split-ws 250000 750000 2>&1 \
  | grep -E "ir-seq-charat-(outer-hoist|w7d|predicate) java/util/regex|ir-scan-loop java/util/regex|charAt entry bound" \
  | sort | uniq -c
```

Expected:

* `ir-seq-charat-outer-hoist ... coder == LATIN1 guarded, one byte load` for `Pattern$Start.match`;
* `entry-bound ... bounds=1 non-negative=1` on its `ir-seq-charat-w7d` line;
* `ir-scan-loop ... charAt calls=0` lines, because the call is gone.

If the bound is not answered, the `[ir-licm] charAt entry bound: header R: nG unanswered: ...` line names where
the walk stopped.

Still open on this page: P-1 and P-2 (the `predicate` / `next` reads and the lambda class test of the outer loop).
This arm does not touch them.

## What costs per character

`Pattern$Start.match` runs `for (; i <= guard; i++) if (next.match(matcher, i, seq)) ...`, with the
`BmpCharPropertyGreedy` link spliced: `to = matcher.to; while (i < to && predicate.is(seq.charAt(i))) ...;
if (i >= to) ...; while (n >= cmin) { if (next.match(..)) return true; ... }`.

On the `\s+` rows the INNER scan loop runs about one iteration per text position: a non-space character ends it at
once. So per character the code is the OUTER position loop's body, and every per-position read is a per-character
read:

* `this.next` and its class test (the guarded `Greedy` splice);
* `next.predicate` and the lambda class test (item (a) and (b) of the j27 box);
* `matcher.to`, `cmin`;
* the `charAt` prefix (item (c); rx27 wave 1 drops its receiver tests,
  `CRATONVM_JIT_IR_SEQ_CHARAT_PROVEN_STRING`).

This also says why the two existing levers do little here:

* W12-4 plants the field-class predicate (P2) at the INNER scan loop (W13-4 census: `region 24, bci 99, tests
  folded=1 reads replaced=1`). Its pre-header read and guard run once per entry of the inner loop, which is once
  per position: the per-character count does not drop.
* An inner-loop `charAt` hoist would pay its class guard, `coder`, `value`, null guard and length once per position
  for the same reason. rx27 did not build it. The outer-loop hoist (W11-4) lost 3-8 % (W13-4); its listing was
  never captured, so why it lost is still unexplained.

## Why the outer loop is refused

P2 on `Start`'s position loop (W13-4 census) names two writers:

1. the predicate's miss-arm call `CharPredicate.is` (n85, bci 116). Its test reads `next.predicate`, whose base
   is the in-loop read of `this.next`. P2 plans one level per round (`classed` lets a read through `p0` qualify
   only in the NEXT round), and round 0 is refused outright by this writer, so round 1 never comes.
2. `Greedy`'s back-off `next.match` (n145, bci 160). The chain rule refused that link (`LastNode`) as rare
   (194 per mille), so it is a plain call in the outer loop. After it returns `false` the loop goes on, so it is
   a real writer on a path to the back edge.

Both refusals are correct as the pass stands.

## What would close it

* **P-1 (this lane's area): plan the chain in one round.** A key whose base is a read of another key of the same
  round (`this.next.predicate` under `this.next`) is planned with it. Its pre-header read is `p1 = p0.f`, after
  `p0`'s class guard. The miss calls of its tests count as miss arms. The induction is unchanged: while every
  earlier iteration ran the hit arms, nothing wrote `this.next` or `p0.predicate`, so the reads are `p0` and
  `p1`. Alone this engages nothing on `split-ws`, because writer 2 remains.
* **P-2 (lanes B/F, chain-splice admission): splice the back-off link.** `LastNode.match` stores only `Matcher`
  fields, so spliced it is a field writer that provably misses `Start.next` and `Greedy.predicate`, which P2
  already admits. The other option, without the splice, is a guard on the back edge after the call (`this.next ==
  p0`, `p0.predicate == p1`), deopting with the header frame at its back-edge values. That frame helper does not
  exist yet (`loop_entry_frame` refuses in-loop values).

With both, the outer pre-header answers `next`, `predicate` and both class tests once per `Start.match` call
instead of once per character. `matcher.to` and `cmin` would follow through the field forward, which is blocked
by the same calls.

## Evidence still missing

The c24 listing (`dis3-ws-1.txt`) is the only listing of the scan loop. The W13-4 capture lost its `$` in the
shell and matched `Pattern.matcher`. Capture first, with the filter single-quoted:
`CRATONVM_DBG_JIT_DISASM='Pattern$Start.match'`, together with `CRATONVM_DBG_JITC=1 CRATONVM_DBG_LICM=1`
(`[ir-licm] seq charAt field class predicate: ... the opaque writer is ...` names the writers).
