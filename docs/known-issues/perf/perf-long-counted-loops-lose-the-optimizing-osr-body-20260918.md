# `long`-counted loops run in single-pass OSR code: a constant division refuses the optimizing OSR body, and `lcmp; if` is never fused

**Status:** open (performance) -- partially resolved: wave 6 (irlower6) fused the `lcmp` exit test in the loop shape it missed and restored the single-pass OSR preference; wave 8 (irl8) landed the `FCmp` half of proposed fix 2 (`fcmp/dcmp; if<lt|le|gt|ge>` -> one `UCOMIS; Jcc`, kill switch `CRATONVM_JIT_IR_FUSED_FCMP=0`) and declined fix 3 (relaxed admission) with reasons. Still open: the admitted optimizing body is slower than the single-pass one on the `long` shapes. Wave 9 (irl9) narrowed it with numbers: the remaining gap is instruction count per iteration (accumulator shuffles and no unrolling), not the admission, the compare fusion or the poll layout. See "Status after wave 9". Wave 10 (irl10) landed item 1 (three-operand forms) and the recognition half of item 2, unbuilt; see "Status after wave 10". Wave 11 (irl11) measured wave 10 on the built binary (the optimizing body gained 10-15 %, single-pass still wins; the ArithProbe "regression" is noise), removed the two carry shuffles (`RcxTwin`, 15 -> 13 instructions per `mul` iteration expected) and made constant-divisor loops unrollable, unbuilt; see "Status after wave 11".
**Owner-area:** `../../../vm/src/runtime/interpreter/jit_bridge.rs` (`try_osr`: admission requires
`ir_osr_entry_addr(pc).is_some() && ir_osr_sentinel_free`), `../../../jit/src/ir_lower.rs`
(`lower_inner` skips OSR entry emission for non-sentinel-free bodies; `emit_div_zero_guard`,
`emit_div_overflow_guard`), `../../../jit/src/ir.rs` / `../../../jit/src/ir_optimize.rs` (`Op::LCmp` feeding
`Cmp(cc, lcmp, 0)` is never canonicalized to a long `Cmp(cc, a, b)`).
**Found by:** JIT review round 9, wave 2, lane `perfdiag`, 2026-09-18.
**Related:** `ir-constant-divisor-still-pays-the-zero-and-overflow-tests-20260918.md` (the
guard itself; this page adds its OSR consequence).

## Evidence

`CratonBench arithmetic` (4 401 ms vs HotSpot 2 174 ms) is one call of
`benchArithmetic(2_000_000_000L)`, so only its OSR body matters. `CRATONVM_DBG=jitc`:

```text
[cratonvm-jitc] OSR-compile CratonBench.benchArithmetic(J)J entry_pc=5 entry=... len=802
[cratonvm-jitc] osr optimizing CratonBench.benchArithmetic pc=5: stub=false entries=[] sentinel_free=false
[cratonvm-jitc] OSR-reuse ... len=802
```

— the optimizing OSR artifact is built, has **no** OSR entry and is refused, and the
802-byte single-pass (`osr/sp`) body runs. `diag/src/ArithOsr.java` isolates the trigger
(1e9 iterations each):

| loop body | optimizing OSR admitted? | CratonVM | HotSpot |
|---|---|---:|---:|
| `s += i*3 + (i>>1)` | yes (`stub=true entries=[5] sentinel_free=true`) | 1 251 ms | 357 ms |
| `s += i*3 - i/2` | **no** (`sentinel_free=false`) | 1 878 ms | 457 ms |
| `s += i*3 + i%7` | **no** | 2 032 ms | 715 ms |
| the CratonBench expression | **no** | 2 226 ms | 943 ms |
| same, `int` induction | **no** | 1 788 ms | 955 ms |

So any constant `/` or `%` makes the whole loop fall back to single-pass OSR code (the
division's zero/overflow deopt stubs make the body non-sentinel-free, and
`lower_inner` then does not even emit the OSR entry).

Even when admitted, the optimizing body loses 3.5x on the simplest loop. Its loop header
(`CRATONVM_DBG_JIT_DISASM=ArithOsr.mulOnly`, `osr-optimizing/ir`):

```asm
cmp rax,rcx ; setg al ; setl dl ; movzx eax,al ; movzx edx,dl ; sub eax,edx ; movsxd rax,eax
mov [rbp-78h],rax ; mov rax,[rbp-78h]          ; lcmp result round-trips its home
mov ecx,0 ; cmp eax,ecx ; setge al ; movzx eax,al
mov [rbp-0A0h],rax                              ; boolean spilled...
...                                             ; loop body
mov rax,[rbp-0A0h] ; test eax,eax ; jne exit    ; ...and reloaded to branch
```

15 instructions and two memory round trips where HotSpot has `cmp; jge`. The single-pass
body does the same `setg/setl/sub/movsx` materialization plus a store of the `lcmp`
result per iteration, and a `mov rax,r8 / mov r8,rax` shuffle around every operation.

## Root cause

1. **Admission:** `try_osr` accepts an optimizing artifact only if it is
   sentinel-free (no deopt stub, no call-exception stub). Constant-divisor `Div`/`Rem`
   still emit a zero-test deopt point (see the related page), so the loop is refused.
2. **`lcmp` not fused:** the builder emits `Op::LCmp` (→ {-1,0,1}) and then
   `If(Cmp(cc, lcmp, 0))`. `ir_optimize` only constant-folds `LCmp`; it never rewrites
   `Cmp(cc, LCmp(a,b), 0)` to `Cmp(cc, a, b)` on `Long`, so the fused-branch lowering
   (`CRATONVM_JIT_IR_FUSED_BRANCH`, default on) never sees a compare whose only use is
   the `If` — it sees two chained compares, and both results get home words.

## Proposed fix

1. Land the related page's fix (no zero/overflow guard and no deopt point for a non-zero
   constant divisor; `graph_cannot_deopt` aware of it). That makes these loops
   sentinel-free and admits the optimizing OSR body with no change to `try_osr`.
2. Add the peephole in `ir_optimize` (simplify pass): `Cmp(cc, LCmp(a,b), Const 0)` →
   `Cmp(cc, a, b)` with long operands (valid for all six `cc`, since `lcmp` is exactly
   the sign of `a - b` without overflow); same for `FCmp` with the NaN-bias preserved
   (`fcmpl`/`fcmpg` choose the unordered answer, so only the ordered-compare forms fold
   directly).
3. Longer term, relax the OSR admission: allow deopt stubs whose frame states the OSR
   entry can describe, instead of requiring zero sentinels.

## Expected gain

`arithmetic` 4.4 s → roughly the admitted-body speed plus the fused compare: estimated
2.5–3 s (the header shrinks from ~15 to 2 instructions per iteration on a ~25-instruction
loop), closing most of the 2x gap. Every `for (long i...)` loop and every `lcmp`/`fcmp`
branch in IR-compiled code benefits from (2).

## How to verify

```bash
cd C:/craton/jitr9-probes
for v in mul div rem full int; do CRATONVM_DBG=jitc "$VM" --java-home "$JH" -cp diag/cls ArithOsr $v 1000000000 2>&1 | grep -E "ms \[|osr optimizing"; done
# after (1): every line reports sentinel_free=true entries=[<pc>]
CRATONVM_DBG_JIT_DISASM=ArithOsr.mulOnly "$VM" --java-home "$JH" -cp diag/cls ArithOsr mul 1000000000 2>&1 | grep -c setg
# after (2): 0 in the osr-optimizing/ir body
```

## Status after wave 3: the `try_osr` side (lane `vmbridge3`)

**Admission was widened in wave 2, and that made this page's loops slower.**
Wave 2 made a constant divisor guard-free, so these loops are now
sentinel-free and the door admits the optimizing body
(`stub=true entries=[5] sentinel_free=true` on every `ArithOsr` variant). That
body is slower than the single-pass one it replaces. Measured on the wave-2
binary, `CRATONVM_JIT_OSR_OPTIMIZING=1` vs `=0`, 1e9 iterations, two runs each:

| loop | optimizing OSR body | single-pass OSR body |
|---|---:|---:|
| `mul` (`lcmp`, no division) | 1 309 / 1 323 ms | 1 026 / 1 025 ms |
| `div` | 3 601 / 3 377 ms | 1 607 / 1 357 ms |
| `rem` | 3 740 / 3 512 ms | 2 102 / 2 204 ms |
| `full` | 6 679 / 6 391 ms | 2 280 / 2 512 ms |
| `int` | 3 289 / 3 116 ms | 2 261 / 2 377 ms |
| `CratonBench arithmetic` | 8 650 ms | 3 411 ms |
| `CratonBench matrix` (control, no div/lcmp) | 2 099 ms | 2 344 ms |
| `CratonBench sieve` (control) | 4 352 ms | 4 295 ms |

**Change.** `../../../vm/src/runtime/interpreter/jit_bridge.rs`, `try_osr` (~4155): a
new arm ahead of the optimizing compile. When
`osr_prefer_single_pass_enabled()` (`CRATONVM_JIT_OSR_PREFER_SINGLE_PASS`,
**default ON**, `=0` restores wave 2's admission) and
`osr_optimizing_not_known_better(code)` (~4043) holds, the door keeps the
single-pass body and records the refusal in the existing refusal memo. The
predicate is true when the method contains `idiv`/`ldiv`/`irem`/`lrem`/`lcmp`,
found by an instruction-boundary walk with `cratonvm_jit::bytecode_insn_len`.
Because of the memo, the scan runs once per `(method, pc)` and no
optimizing compile is paid for a body that would not be entered.
`CRATONVM_DBG=jitc` prints `osr optimizing ... not attempted -- single-pass
preferred`. Admission was NOT widened further (the brief's rule for this
wave). Tests: `jit_bridge::r9w3_osr_single_pass_preference_tests`.

This is a policy for "the optimizing body is not known to be better", not a
correctness gate. Re-measure with `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0` once
the optimizing tier has closed this page's two causes:

* Strength-reduced constant division. Lane `irlower3` is landing
  `CRATONVM_JIT_IR_CONST_DIV` this wave.
* `lcmp` fused into the branch (proposed fix 2).

Then narrow the predicate or flip the default. The flag's `INVENTORY` row is a
cross-lane request in `../../internal/jit-review-r9/NOTES-w3-vmbridge3.md`.

The page stays **open** for the optimizing tier's own code quality: proposed
fix 2 (the `lcmp` peephole) and the 3.5x on the simplest admitted loop.

## Status after wave 3: the lowering side (lane `irlower3`)

Both of the optimizing body's causes named above are addressed in `../../../jit/src/ir_lower.rs`
(not built yet at the time of writing -- numbers below are to be taken after the build):

1. **Constant division** -- strength-reduced in the lowering (shifts for `+-2^k`,
   Hacker's Delight multiply-high otherwise, `NEG`/`XOR` for `+-1`), default on,
   `CRATONVM_JIT_IR_CONST_DIV=0` to restore `IDIV`. Details and tests in
   `../../internal/fixed-bugs/ir-constant-divisor-still-pays-the-zero-and-overflow-tests-FIXED-20260918.md`
   (`## Resolution of item 4`).
2. **`lcmp` fused into its branch** -- done in the LOWERING rather than as the proposed
   `ir_optimize` peephole (a lowering-only change needs no new IR invariant, and if an
   `ir_optimize` rewrite lands later this simply stops firing). `prepare_fusion_tables`
   marks an `Op::LCmp` `fused_lcmp` when its only use is an already-fused
   `Cmp(cc, lcmp, 0)` (or `Cmp(cc, 0, lcmp)`), it sits in the same block before the
   compare, nothing scheduled after it takes either operand's frame colour or register
   (the same clobber test the compare fusion uses), and no deopt that can actually happen
   names it (`deopt_named`, refined by `compute_deopt_named_reachable` -- the builder's
   frame state at the `if<cc>` bci always lists the `lcmp` result). The `LCmp` arm then
   emits nothing, and `lower_terminator`'s fused `If` compares the two longs directly
   (`fused_lcmp_operands`; their `Long` type selects the 64-bit `CMP`), i.e. one
   `CMP r64, r64 | [mem] | imm; Jcc`. The prediction is checked against the emission:
   `frame_value_for` latches `UnallocatedValue` if a frame state at a TRANSFER bci names a
   fused `lcmp` (quiet elsewhere, like a dropped home), so a misprediction costs the
   method, never the answer. Kill switch: the existing `CRATONVM_JIT_IR_FUSED_BRANCH=0`.
   Test: `r9w3_lcmp_branches_fuse_into_one_compare` (all six `if<cc>`, EXECUTED over
   pairs differing only in the high word / low word and at MIN/MAX, asserts no `SETG`).

Still open: the `FCmp` half of proposed fix 2 (the NaN bias makes only some forms fold),
and proposed fix 3 (relaxed admission). With (1) and (2) landed, lane `vmbridge3`'s
`osr_optimizing_not_known_better` predicate should be re-measured as its section says:

```bash
for v in mul div rem full int; do CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 "$VM" --java-home "$JH" -cp diag/cls ArithOsr $v 1000000000; done
CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 CRATONVM_DBG_JIT_DISASM=ArithOsr.mulOnly "$VM" --java-home "$JH" -cp diag/cls ArithOsr mul 1000000000 2>&1 | grep -c setg   # expect 0 in osr-optimizing/ir
```

## Status after wave 6 (lane `irlower6`, owns both sides)

### What the wave-5 binary showed

Wave 3's `r9w3_lcmp_branches_fuse_into_one_compare` fuses `lcmp; if<cc>` in a
straight-line body, but in the OSR loop it never fired. `ArithOsr.mulOnly`'s
`osr-optimizing/ir` body on w3b, w4, w5 and w5b still carried `setg; setl; sub; movsxd`,
a store and reload of the `lcmp` result, `cmp; setge; movzx`, a store of the boolean,
and a reload plus `test; jne` at the bottom. That is two `setg` per body on every binary.
The cause is `prepare_fusion_tables`: the compare's clobber test refused the compare
because the `lcmp` result's frame colour is reused by the loop body's first temporary
(the `lcmp`'s range ends at the compare). The `lcmp` fusion was only attempted after the
compare passed that test, so the refusal blocked both fusions. The other `ArithOsr`
shapes (`div`, `rem`, `full`) and `CratonBench.benchArithmetic` already fused: 0 `setg`.

Meanwhile wave 5 made the SINGLE-PASS OSR body register-resident and 2x unrolled
(13 register-only instructions per iteration for `mul`). The wave-3 comparison that
turned `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS` off therefore reversed. On w5b, `=0`
(optimizing) vs `=1` (single-pass), 1e9 iterations, two runs each:

| loop | optimizing | single-pass | HotSpot |
|---|---:|---:|---:|
| `mul` | 1 709 / 1 720 | 790 / 762 | 525 |
| `div` | 1 241 / 1 166 | 868 / 844 | 496 |
| `rem` | 1 428 / 1 440 | 1 160 / 1 128 | 788 |
| `full` | 1 908 / 1 784 | 1 526 / 1 560 | 1 154 |
| `int` | 1 942 / 1 982 | 2 032 / 2 038 | 1 236 |

`CratonBench arithmetic`: 4 420 / 4 029 / 3 970 ms optimizing, 3 400 / 3 056 / 3 197
single-pass. `matrix` and `sieve` (no gated opcode) were a wash.
`CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK=1` (the obvious lever for `full`'s two spilled
single-use temporaries) did not help: `div` 1 044/1 030 vs 1 079/1 059, `full`
1 794/1 712 vs 1 871/1 836.

### What changed

1. **Lowering (`../../../jit/src/ir_lower.rs`, `prepare_fusion_tables`).** A clobbered compare
   input no longer stops the search. When the compare is `Cmp(cc, LCmp(x, y), 0)` and
   the `lcmp` passes its own tests (single use, not reachably deopt-named, `x`/`y` not
   clobbered after the `lcmp`), both are fused together. The branch then reads only `x`
   and `y` (`fused_lcmp_operands`), so the colours of the `lcmp` result and of the
   constant are irrelevant. Test: `r9w6_a_long_counted_loop_fuses_its_lcmp_exit_test`.
   It EXECUTES `mulOnly`'s loop (optimized and not) over counts including
   `i64::MIN`/0/1/100 003, and asserts no `SETG`.
2. **`try_osr` (`../../../vm/src/runtime/interpreter/jit_bridge.rs`).**
   `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS` is back to default ON (`=0` opts out), per the
   table above. `osr_optimizing_not_known_better` now names only `ldiv`, `lrem` and
   `lcmp`. `idiv`/`irem` were dropped because the `int` row shows the optimizing body is
   at least as fast there. Test: `r9w3_osr_single_pass_preference_tests` (updated: `idiv`
   and `irem` now keep the optimizing body, and `ldiv` is covered).

### Still open

* The optimizing body is slower than the new single-pass body on every `long` shape.
  For `full` the visible costs are two single-use temporaries (`i*3`, `i*3 - i/2`)
  spilled to their homes, because they are not adjacent to their consumer, and
  RAX/RCX shuffling around each op. For `mul`, re-measure after this build with
  `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0`. With the fused exit test its body is about
  18 register instructions per iteration against single-pass's 13 unrolled. If it now
  beats single-pass, drop `lcmp` from the predicate. The predicate should keep only
  the shapes where single-pass still wins.
* The `FCmp` half of proposed fix 2.
* Proposed fix 3 (relaxed admission).

## Status after wave 7 (lane `mutrec7`): re-measured, no code change

This is the re-measure that wave 6 asked for. It uses the w6 binary (which has wave 6's
`lcmp` fusion fix), 1e9 iterations, one run each. `=0` is the optimizing OSR body and `=1`
is the default single-pass body.

| loop | `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0` | `=1` (default) |
|---|---:|---:|
| `mul` | 661 ms | 610 ms |
| `div` | 973 ms | 691 ms |
| `rem` | 1133 ms | 901 ms |
| `full` | 1486 ms | 1255 ms |
| `int` | 1652 ms | 1540 ms |

Single-pass still wins every shape. The gap on `mul` has narrowed to 8% (it was 2.2x on
w5b), so the `lcmp` fusion is working. The predicate
(`osr_optimizing_not_known_better`: `ldiv`/`lrem`/`lcmp`) stays as it is.

Two items are still open:

* **The `FCmp` half of proposed fix 2.** It is not attempted. No probe in this page's set
  has an `fcmp`/`dcmp` loop exit, so there would be no measurement to justify it.
* **Proposed fix 3 (relaxed admission).** It is not attempted either. It buys nothing
  while the admitted optimizing body is slower than the single-pass one.

## Status after wave 8 (lane `irl8`)

### The `FCmp` half of proposed fix 2: landed (lowering, not an `ir_optimize` rewrite)

Done where wave 3 did the `lcmp` half, and for the same reason (no new IR invariant):
`../../../jit/src/ir_lower.rs` `prepare_fusion_tables` now accepts an `Op::FCmp` under a fused
`Cmp(cc, fcmp, 0)` / `Cmp(cc, 0, fcmp)` on exactly the terms the `lcmp` block asks
(single use, same block and before the compare, not reachably deopt-named, neither
input's frame colour / GPR / XMM reused by anything scheduled after it), marks it in
the new `Lowerer::fused_fcmp`, and the `FCmp` arm then emits nothing. `lower_terminator`'s
fused `If` asks `Lowerer::fused_fcmp_branch` and emits `UCOMISS/UCOMISD XMM0, XMM1; Jcc`.

Only the ORDERING conditions fold. `fcmp_fused_branch(cc, nan_greater)` is the whole
table: with `UCOMIS p, q` setting CF on `p < q` or unordered and ZF on `p == q` or
unordered, `JA`/`JAE` are "NaN false" and `JB`/`JBE` are "NaN true". `fcmpl`/`dcmpl`
(NaN -> -1) make `< 0`/`<= 0` NaN-true and `> 0`/`>= 0` NaN-false, i.e. `UCOMIS a, b`
with `JB/JBE/JA/JAE`; `fcmpg`/`dcmpg` (NaN -> +1) take the mirrored condition on
`UCOMIS b, a`. Every answer is ONE flags predicate, so the `cc ^ 1` the branch layout
takes is its exact complement, NaN included. `==`/`!=` need ZF and PF together, which
one Jcc byte cannot express; those keep the materialised {-1,0,1} (and keep today's
code byte-for-byte). A clobbered compare is fused only together with its `fcmp`, as
wave 6 does for `lcmp`, because the branch then reads the two FP inputs, not the
compare's. `frame_value_for` latches `UnallocatedValue` if a transferring frame state
names a fused `fcmp`, exactly like a fused `lcmp` (a misprediction costs the method,
never the answer).

Kill switch: `CRATONVM_JIT_IR_FUSED_FCMP=0` (default ON; `CRATONVM_JIT_IR_FUSED_BRANCH=0`
still turns every fusion off). The INVENTORY row is a cross-lane request in
`../../internal/jit-review-r9/NOTES-w8-irl8.md`.

Tests (`../../../jit/src/ir_lower.rs`):
* `r9w8_fcmp_branches_fuse_into_one_ucomis` -- EXECUTED for `fcmpl/fcmpg/dcmpl/dcmpg`
  x all six `if<cc>`, optimized and not, over every pair of {0.0, -0.0, 1.0, -1.0, 1.5,
  NaN, +inf, -inf}; asserts no `SETA AL`/`SETB AL` for the four ordering conditions.
* `r9w8_the_fused_fcmp_flag_table_matches_the_three_way_result` -- the flags table
  against the JVMS three-way result for every ordered/unordered outcome, and its
  `^ 1` complement.

Probe: `C:\craton\jitr9-probes\irl8\src\FcmpLoop.java` (`d < n` exit test = `dcmpg;
ifge`, body `x > t` = `dcmpl; ifle`, plus a `float` twin). Before, on the w7b binary
(which predates this change), `calls` mode (400k calls x 2000 iterations, the C2
full-compile body): 4 800 / 4 719 ms, HotSpot 773 ms; `osr` mode: 1 968 / 1 946 ms,
HotSpot 1 044 ms. The `full/ir` body of `dloop` materialises BOTH compares
(`ucomisd; setb; seta; movzx; movzx; sub; movsxd; store; reload; cmp; setge; movzx;
store` ... `reload; test; jne`). After this change the exit test should become
`ucomisd; jbe/ja`. The body's `if (x > t)` will NOT fuse on that shape: the scheduler
placed its compare in the loop header (schedule-early; `x`'s update is hoisted above
the exit test) while its `If` is in the body block, and fusion is same-block only.
`CRATONVM_JIT_IR_SINK_EQUAL_DEPTH=1` (the lever that would sink it) made the probe
slower on w7b (8 108 / 8 189 ms against 5 331 / 5 542 ms), so it was not pursued.
Re-measure on the integrated build:

```bash
cd C:/craton/jitr9-probes/irl8
for m in calls osr; do "$VM" --java-home "$JH" -cp cls FcmpLoop $m; CRATONVM_JIT_IR_FUSED_FCMP=0 "$VM" --java-home "$JH" -cp cls FcmpLoop $m; done
CRATONVM_DBG_JIT_DISASM=FcmpLoop.dloop "$VM" --java-home "$JH" -cp cls FcmpLoop calls 2>&1 | grep -A400 "full/ir" | grep -c "setb\|seta"   # 4 before, 2 after
```

### Proposed fix 3 (relaxed admission): declined

It is not in this lane's files (the admission is `try_osr` in
`../../../vm/src/runtime/interpreter/jit_bridge.rs`), and it still buys nothing: every
admitted-vs-single-pass measurement since wave 5 (the tables above, last wave 7) has
the single-pass OSR body winning on the `long` shapes this page is about, and the
`osr_optimizing_not_known_better` preference exists precisely to NOT take the
optimizing body there. Widening admission to bodies with deopt stubs would admit
more of a body that loses. Revisit only after the optimizing `long` body wins with
`CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0`.

## Status after wave 9 (lane `irl9`): narrowed with numbers, no code change

### Re-measured (w8b binary, one session, interleaved, 1e9 iterations, ms)

`sp` = default (`CRATONVM_JIT_OSR_PREFER_SINGLE_PASS` on, single-pass OSR body), `opt` =
`CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0` (optimizing OSR body), `opt+outline` = also
`CRATONVM_JIT_IR_POLL_OUTLINE=1`. Two runs each, run 1 / run 2 (a third, earlier pass gave the
same ordering: `mul` 528/516 vs 701/721).

| loop | sp | opt | opt+outline |
|---|---:|---:|---:|
| `mul` | 451 / 444 | 622 / 622 | 636 / 672 |
| `div` | 608 / 620 | 828 / 845 | 805 / 828 |
| `rem` | 956 / 824 | 1 081 / 1 068 | 1 034 / 1 119 |
| `full` | 1 247 / 1 250 | 1 349 / 1 765 | 1 656 / 1 873 |
| `int` | 1 504 / 1 672 | 1 472 / 1 566 | 1 500 / 1 651 |

Single-pass still wins every `long` shape (28-38 % on `mul`), and the `int` row is a wash, so
the `osr_optimizing_not_known_better` predicate (`ldiv`/`lrem`/`lcmp`) stays as it is.
`CRATONVM_JIT_IR_ISEL_EMIT=1` is far worse (`mul` 1 183, `full` 1 809: it has no register
residency), and outlining the poll buys nothing.

### Where the 30 % is (instruction accounting, `CRATONVM_DBG_JIT_DISASM=ArithOsr.mulOnly`)

Per iteration, executed, flag clear:

| | optimizing (`osr-optimizing/ir`) | single-pass (`osr/sp`, 2x unrolled) |
|---|---:|---:|
| arithmetic + exit test (`lea/sar/imul/add/add`, `cmp; jge`) | 7 | 7 |
| accumulator shuffles (`mov rax,r15` x2, `mov rcx,rax` x2, `mov rax,r14`, `mov r12,rax`) | 6 | 5 (`mov rax,r12; mov r8,rax; mov r9,r12; mov r8,r12; mov r12,r8`) |
| back-edge phi moves (`mov r14,r12; mov r15,r13`) | 2 | 0 (updated in place) |
| poll (`test; je`) + back jump | 3 | 1.5 |
| **total** | **18** | **13.5** |

18 / 13.5 = 1.33, against a measured 622 / 450 = 1.38: the body is throughput-bound on
instruction count, and every item is visible. The exit compare is fused (one `cmp r15,rbx;
jge`), the constant division is strength-reduced, and admission is not the issue.

### What would close it (all in `ir_lower.rs`, none attempted unbuilt)

1. **Three-operand forms instead of the accumulator route** (-3 to -4 per iteration):
   `imul rax, r15, 3` (`IMUL r, r/m, imm`) instead of `mov rax,r15; imul rax,3`; an `LEA
   dst,[a+b]` form of `Op::Add` when both operands are in registers (the `emit_add_lea`
   family covers only `x + const`); a shift computed directly into the carry register.
   Each touches an arm that `every_folded_arm_reaches_rcx_only_in_its_register_form` and the
   carry contract audit, so it wants a build and the audit updated with it.
2. **2x unrolling of a counted OSR loop body** (-1.5): halves the poll and the back jump,
   which is the single-pass tier's whole remaining advantage beyond (1).
3. **Phi coalescing** (-2): `linear-scan-no-phi-coalescing-costs-a-move-per-carried-value`,
   closed WONTFIX this wave -- two rename-eliminated moves out of 18, behind a five-site
   change whose failure mode is a wrong loop-carried value.

Until (1) and (2) land, the single-pass preference is the right default and this page stays
open as a performance note.

## Status after wave 10 (lane `irl10`): items 1 and 2 implemented, not yet measured

Nothing below was built in this wave, so no number here comes from the new code.
Baseline for the A/B, interleaved in one session on the w8b and w9b binaries
(1e9 iterations, ms, `sp` = default, `opt` = `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0`):

| loop | w8b sp | w8b opt | w9b sp | w9b opt |
|---|---:|---:|---:|---:|
| `mul` | 465 / 510 | 703 / 708 | 519 / 524 | 719 / 665 |
| `div` | 646 / 727 | 866 / 929 | 713 / 649 | 1 044 / 869 |
| `full` | 1 225 / 1 355 | 1 561 / 1 416 | 1 337 / 1 257 | 1 573 / 1 377 |

`CRATONVM_JIT_IR_PARTIAL_UNROLL=1` (with and without `CRATONVM_JIT_IR_PER_COPY_FRAMES=1`)
changed nothing on w9b (`mul` 643 / 640 / 636 / 740 ms), and `CRATONVM_DBG_UNROLL=1` said why:
`region 4: bail -- the exit test names none of the 1 constant-stride phi(s)`. The loop test is
`Cmp(ge, LCmp(i, n), 0)`, and `analyze_counted_loop` only matched `Cmp(cc, phi, bound)`, so no
`long` counted loop has ever been recognised, for full or for partial unrolling.

### What changed

1. **Three-operand `IMUL`** (`../../../jit/src/ir_lower.rs`, `Lowerer::emit_imul_reg_imm3`, called at
   the top of `Op::Mul`'s integer arm; encoder `imul_rax_reg_imm_bytes`). `i * 3` with `i`
   resident is `imul rax, r15, 3` instead of `mov rax, r15; imul rax, 3`. It declines, emitting
   nothing, unless the second operand folds on the same `alu_imm32` predicate as
   `emit_imul_imm` and the first operand is resident and not carried. The accumulator
   sequence and the RCX/carry audits are unchanged.
   `every_folded_arm_reaches_rcx_only_in_its_register_form` lists the helper in
   `OUTSIDE_CALLS`, with its justification.
2. **Register-register `LEA` for `Op::Add`** (`Lowerer::emit_add_lea_reg_reg`, reached from
   `emit_add_lea` when the second operand is not a folding constant; encoder
   `lea_reg_base_index_bytes`). `s + t` becomes `lea r12, [r14 + rcx]` (the result's
   register) or `lea rax, [...]` (followed by the arm's `store_rax`), instead of
   `mov rax, r14; add rax, rcx; mov r12, rax`. It applies when `s` is resident and `t` is
   either resident or held for this node by an RCX carry. Same gate
   (`CRATONVM_JIT_IR_ADD_LEA`, default on, `=0` restores the accumulator). It has the same
   contract as the `x + k` direct form, and a new source test checks it.
3. **`lcmp` loops are counted loops** (`../../../jit/src/ir_optimize.rs`, `analyze_counted_loop` via
   the new `lcmp_operands_of_zero_test`). `Cmp(cc, LCmp(a, b), 0)` is read as `a cc b`, and
   `Cmp(cc, 0, LCmp(a, b))` as `b cc a`. That is exact for all six signed `CmpOp`s, because
   `lcmp` is the sign of `a - b` computed without overflow. The graph is not rewritten; only
   the recognition and the trip simulation read the long operands. Effects:
   * a constant-bound `long` loop is now a full-unroll candidate on the default path;
   * a runtime-bound one reaches the partial unroller under `CRATONVM_JIT_IR_PARTIAL_UNROLL=1`.

   That flag is still default-off and still needs per-copy frames, so wave 9's item 2
   (2x unrolling) stays behind it.

Expected per-iteration count for `mulOnly`'s optimizing body is 15 (was 18):
`lea r13; mov rax,r15; sar; mov rcx,rax; imul rax,r15,3; add rax,rcx; mov rcx,rax;
lea r12,[r14+rcx]; cmp; jge; test; je; mov; mov; jmp`. The single-pass body is at 13.5, so
the single-pass preference predicate stays as it is until this is measured.

Tests (all unbuilt):

* `ir_lower::tests::r9w10_irl10_three_operand_encoders`: encoder bytes, including the SIB
  traps.
* `ir_lower::tests::r9w10_irl10_counted_loop_three_operand_forms_execute`: `int` and `long`
  `mulOnly`, EXECUTED against a reference. It also asserts that the `int` body contains the
  three-operand `IMUL`.
* `ir_lower::tests::r9w10_irl10_the_reg_reg_lea_form_publishes_what_it_does_not_store`
* `ir_optimize::r9w10_irl10_tests::a_long_lcmp_loop_is_a_counted_loop`
* `ir_optimize::r9w10_irl10_tests::the_lcmp_read_through_is_exact_and_oriented`

### To measure after the build

```bash
for v in mul div full; do CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 "$VM" --java-home "$JH" -cp diag/cls ArithOsr $v 1000000000; done   # vs w9b, interleaved
CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 CRATONVM_JIT_IR_PARTIAL_UNROLL=1 CRATONVM_JIT_IR_PER_COPY_FRAMES=1 CRATONVM_DBG_UNROLL=1 "$VM" ... ArithOsr mul 1000000000   # expect "partially unrolled counted loop"
CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 CRATONVM_DBG_JIT_DISASM=ArithOsr.mulOnly "$VM" ... | grep -E "imul rax,r15|lea r12"
```

Still open:

* The carry shuffle: `mov rcx,rax` after the shift and after the inner add. An `add rcx,rax`
  form would drop one.
* 2x unrolling on by default. Partial unroll plus per-copy frames need a soak first.
* Phi coalescing (WONTFIX in wave 9).

The single-pass preference stays the default until the optimizing body measures faster.

## Status after wave 11 (lane `irl11`)

### Wave 10 measured on the built binary (w10 vs w9b, interleaved, 3 rounds, 1e9 iterations, ms)

`sp` = default (single-pass OSR body), `opt` = `CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0`.
`CRATONVM_JIT_RETIRE_CELL=1` on both (the new default).

| loop | w9b sp | w9b opt | w10 sp | w10 opt |
|---|---:|---:|---:|---:|
| `mul` | 507 / 517 / 514 | 654 / 728 / 668 | 492 / 492 / 475 | 576 / 588 / 576 |
| `div` | 695 / 676 / 659 | 871 / 961 / 874 | 678 / 657 / 855 | 745 / 758 / 889 |
| `full` | 1 227 / 1 286 / 1 237 | 1 691 / 1 471 / 1 430 | 1 298 / 1 245 / 1 334 | 1 478 / 1 392 / 1 563 |
| `int` | 1 765 / 1 619 / 1 525 | 1 671 / 1 716 / 1 541 | 1 532 / 1 458 / 1 686 | 1 525 / 1 497 / 1 772 |
| `ArithProbe` (2e9) | 2 469 / 2 554 / 2 536 | | 2 566 / 2 511 / 2 540 | |
| `CratonBench arithmetic` | 2 710 / 2 463 | | 2 490 / 2 496 | |

* Wave 10's three-operand forms work: the optimizing `mul` body went from ~680 to ~580 ms
  (`imul rax,r15,3` and `lea r12,[r14+rcx]` are in the disassembly). Single-pass still wins
  `mul` by ~17 %, so the single-pass preference stays.
* **The integrator's ArithProbe 2 330 -> 2 439 ms is noise.** Interleaved, w9b and w10 overlap
  (2 469-2 554 vs 2 511-2 566), and ArithProbe runs the SINGLE-PASS OSR body (its loop has
  `ldiv`/`lrem`, so `osr_optimizing_not_known_better` keeps single-pass), which the three-operand
  change does not touch. `CratonBench arithmetic` is likewise level.
* The optimizing `mul` loop on w10 (`CRATONVM_DBG_JIT_DISASM=ArithOsr.mulOnly`) is 15
  instructions: `lea r13,[r15+1]; mov rax,r15; sar rax,1; mov rcx,rax; imul rax,r15,3; add
  rax,rcx; mov rcx,rax; lea r12,[r14+rcx]; cmp r15,rbx; jge; test [poll]; je; mov r14,r12; mov
  r15,r13; jmp`.
* **Partial unrolling already beats single-pass on `mul`** on w10:
  `CRATONVM_JIT_IR_PARTIAL_UNROLL=1 CRATONVM_JIT_IR_PER_COPY_FRAMES=1` gives 459 / 461 ms
  against 557 / 550 (opt) and ~490 (sp) -- `partially unrolled counted loop (region 4, factor 4,
  7 cloned nodes/copy, per-copy frames: true)`. `div`/`rem`/`full`/`int` did not unroll at all:
  `bail -- unclonable body node Div` / `Rem`.

### What changed (unbuilt)

1. **`RcxTwin`: the two carry shuffles are gone** (`../../../jit/src/ir_lower.rs`: `struct RcxTwin`,
   `Lowerer::{offer_alu_rcx_twin, offer_shift_rcx_twin, record_rcx_twin, apply_rcx_twin}`, the
   `store_rax` carry branch). A value planned as an RCX carry whose home is dropped used to be
   finished with `MOV RCX, RAX`. The arm now records the equal-length run that computes the same
   value straight into RCX -- `ADD/AND/OR/XOR RCX, RAX` or `IMUL RCX, RAX` for the commutative
   register forms, `MOV RCX, r; SAR/SHL/SHR RCX, imm` for a folded shift of a resident value --
   and `store_rax` patches it in (byte-for-byte checked at offer, position-checked at apply)
   instead of emitting the `MOV`. The `mul` loop becomes `lea r13,[r15+1]; mov rcx,r15; sar
   rcx,1; imul rax,r15,3; add rcx,rax; lea r12,[r14+rcx]; ...`: 13 instructions, the single-pass
   body's count. Kill switch `CRATONVM_JIT_IR_CARRY_RCX_TWIN=0` (default ON; `rcx_twins=` in the
   `[ir-ls]` census line). This is the "carry shuffle" item of the wave-10 status and the
   "accumulator shuffles" row of wave 9's accounting.
2. **A constant-divisor division is clonable by the unroller** (`../../../jit/src/ir_optimize.rs`,
   `unroll_body_node_clonable`; `ir_lower::int_division_cannot_trap` made `pub(crate)`). A
   `Div`/`Rem` whose divisor is a non-zero constant at its width cannot trap (the prediction
   `graph_cannot_deopt` already makes and the lowering's guard-free constant-divisor path
   honours), so it is cloned like the multiply it lowers to. This is what stood between the
   `div`/`rem`/`full`/`int` loops and the partial unroller that already wins on `mul`; with the
   default flags it also lets a constant-bound loop containing `x / 10` be fully unrolled where
   frames allow.

Tests (unbuilt):
* `ir_lower::tests::r9w11_irl11_rcx_carry_twins_execute` -- the `long` `mulOnly` loop and an
  `int` loop putting `*`, `^`, `<<`, `|`, `&`, `>>>`, `+` on carry chains, EXECUTED against a
  reference, optimized and not; asserts no adjacent `add rax,rcx; mov rcx,rax` survives.
* `ir_optimize::r9w11_irl11_tests::a_division_is_clonable_exactly_when_it_cannot_trap`.
* `every_folded_arm_reaches_rcx_only_in_its_register_form` updated: `offer_shift_rcx_twin` is on
  its `OUTSIDE_CALLS` list, with the reason (it emits nothing; only `store_rax` does, where it
  would have written RCX anyway).

### Not done, and why

* **Loop-carried moves** (`mov r14,r12; mov r15,r13` on the back edge). They need the
  allocator to give a phi's back-edge value the phi's own register, which is the phi-coalescing
  change wave 9 closed WONTFIX (five sites, failure mode a wrong loop-carried value). Partial
  unrolling amortises them instead: with factor 4 they are paid once per four iterations.
* **Back-edge unroll by default.** `CRATONVM_JIT_IR_PARTIAL_UNROLL` and
  `CRATONVM_JIT_IR_PER_COPY_FRAMES` stay default OFF: the evidence above is one probe family, and
  the wave-10 note that per-copy frames need a soak still stands. The integrator can measure it
  directly after this build:

```bash
for v in mul div rem full int; do for f in 0 1; do CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 CRATONVM_JIT_IR_PARTIAL_UNROLL=$f CRATONVM_JIT_IR_PER_COPY_FRAMES=$f "$VM" --java-home "$JH" -cp diag/cls ArithOsr $v 1000000000; done; "$VM" --java-home "$JH" -cp diag/cls ArithOsr $v 1000000000; done
CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 CRATONVM_JIT_IR_PARTIAL_UNROLL=1 CRATONVM_JIT_IR_PER_COPY_FRAMES=1 CRATONVM_DBG_UNROLL=1 "$VM" ... ArithOsr full 1000000 2>&1 | grep UNROLL   # expect "partially unrolled", no "unclonable body node Rem"
CRATONVM_JIT_OSR_PREFER_SINGLE_PASS=0 CRATONVM_DBG_JIT_DISASM=ArithOsr.mulOnly "$VM" ... ArithOsr mul 1000000 2>&1 | grep -c "mov rcx,rax"   # 0 in osr-optimizing/ir
```

  If the optimizing body then wins every row with both flags on, flip the two flags and narrow
  `osr_optimizing_not_known_better` (in `jit_bridge.rs`) accordingly; that is a cross-lane step.

## Integrator measurement after wave 11 (w11 binary, 2026-09-19)

`ArithProbe`, interleaved, 3 reps: w10 2 434 / 2 447 / 2 437 ms; w11 2 427 / 2 438 / 2 473 ms.
That is neutral, as expected: the probe runs the single-pass OSR body.
`CRATONVM_JIT_IR_CARRY_RCX_TWIN` is default ON with no regression in the probes or the
regression suite. The page's own `mul` A/B recipe is still to be run.
