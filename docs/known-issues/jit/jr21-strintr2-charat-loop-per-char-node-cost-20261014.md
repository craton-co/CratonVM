> **j27 wave 9 (lane W9VER, 2026-10-29): VERIFIED (Windows) for facts and unit tests; the per-char cost itself is perf (out of scope), so the page stays filed for that.** Windows, release LTO-off build of the tree at `47479dc6e0` plus the other wave-9 lanes' WIP (`C:\craton\j27-ver-bin\cvm-ver-2.exe`, test exes from the same tree), HotSpot 25.0.3 as the reference. `cratonvm-jit --lib jr21_charat` 8 passed (builder / guard-fact / decode tests). `J27Latin1CharRows` (30 facts) and `J27StrpinRows` (7 facts) equal HotSpot in both modes at the default and with `CRATONVM_JIT_IR_STRING_CHAR_DECODE=0` (`--jdk-only`). `R21CharatRows` / `R21StrintrRows` are not in this tree, not run.

# A compiled `String.charAt` loop costs ~5-7 ns per char: the expansion's node count, not a call

> **j27 wave 8 (lane W8LOOP, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION.** No code change. I re-read
> `ir_check_elim::guard_proven` and `strictly_less`.
> * Only `Cmp(Lt|Gt)` facts prove a guard.
> * An `If` edge's fact is read through `CmpOp::negate`. That is sound because a float compare reaches `Op::Cmp`
>   only as an `FCmp` result tested against 0, never as two FP operands.
> * The unit-stride rule needs every back edge to pass `x < y`, so `x + 1` cannot wrap.
> * The `StrCharDecode` arm zero-extends the index (`MOV ECX`) and stays behind its guards.
>
> The register-residency part (item 3) is the promoted-loop page's open part. The new opt-in
> `CRATONVM_JIT_IR_UNROLL_WIDE_PURE` does not reach this loop: its guards and its decode keep it on the
> per-copy-frames route. Verify: the table below.

> **j27 wave 1 (lane LOOPCG, 2026-10-28): FIXED IN CODE (STRINTR21-5 items 1 and 2 present, default ON),
> PENDING HOST VERIFICATION.** On this tree `Op::StrCharDecode` (built by `try_string_access_intrinsic` under
> `CRATONVM_JIT_IR_STRING_CHAR_DECODE`, lowered as one `TEST coder` / byte-or-char load) and the dominating-fact
> guard elision (`CRATONVM_JIT_IR_GUARD_FACT_ELIDE`, `ir_check_elim`) are both in, both `runtime_flag_default_on`
> with registry rows. Re-read the lowering arm: the `MOVZX` of the LATIN1 byte and the UTF16 `MOVZX WORD`
> (`emit_gpr_array_elem_load(Char)`) are right, the index is zero-extended, the op stays pinned behind its
> guards. j27 adds the page's item 3 lever for the loop around it: `CRATONVM_JIT_IR_LINEAR_REASSOC` and
> `CRATONVM_JIT_IR_POLL_WORD_SELECT` (see `jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md`);
> register residency is still the open part. Verify: the A/B/C/D table in "HOW TO VERIFY" below, plus an arm E
> = `CRATONVM_JIT_IR_POLL_WORD_SELECT=0`; facts equal to HotSpot in every arm.

Status: PENDING HOST MEASUREMENT (JIT round 21, lane charat: STRINTR21-5 implemented behind two new default-ON
switches; only the A/B below decides them -- see "Round 21 fix (lane charat)" at the end)
Area: JIT optimizing tier, `IrBuilder::try_string_access_intrinsic` (`jit/src/ir.rs`) and its lowering
(`jit/src/ir_lower.rs`: slot-homed values, `Op::Guard`, `Op::ArrayLoad` null / bounds checks); single-pass
STRING_ACCESS region (`jit/src/x64/op_invoke.rs`) for the pinned population
Found by: JIT round 21 lane strintr2 (wave 19, rows charat-l1 / charat-u16 of
`jr21-strintr-string-pin-residual-20261015.md`); base 5906cd86a4

## Evidence

`R21StrintrRows.charAtL1` / `charAtU16`: `for (k = 0; k < s.length(); k++) acc += s.charAt(k);` over 64 words.
The row prints ns per OUTER iteration (one whole word), not per char:

| row | chars per op (mean) | CratonVM ns/op | ns/char | HotSpot ns/op | ns/char |
|---|---:|---:|---:|---:|---:|
| charat-l1 | 31.5 (`12 + nextInt(40)`) | 154 | ~4.9 | 8 | ~0.25 |
| charat-u16 | ~48 (`len + 1 + len / 2`) | 332 | ~6.9 | 10 | ~0.2 |

A call per char would read ~76 ns per char (the dispatched arm of
`docs/internal/performance/string-charat-loop-cost-and-the-unsteerable-intrinsic-FIXED-20260902.md`), so the
inline expansion is engaged; ~15-20 cycles per char is what it costs.

What runs per char in the IR body (reading `try_string_access_intrinsic` and the lowering on this base), after
`licm-read-hoist` has moved `coder`, `value` and `value.length` to the pre-header:

* the loop condition's `s.length()` expansion: `Guard(recv != null)`, `Guard(value != null)`, `char_count`;
* the body's `s.charAt(k)` expansion: the same two null guards again, `Guard(k >= 0)`, `Guard(k < char_count)`;
* `off = k << coder` (a variable shift through CL), `hi_off = off + coder`;
* two `Op::ArrayLoad(MemKind::Byte)`, each lowered with `emit_array_null_bounds_guards_for` (its own null test
  and bounds compare, unless `ir_check_elim` proves them -- it has no fact relating `k << coder` to
  `value.length >>> coder`);
* the branchless combine: `lo & 0xFF`, `hi & 0xFF`, `<< 8`, `0 - coder`, `&`, `|`;
* every node's result stored to and reloaded from its frame slot (the lowerer homes every value).

The single-pass region (the pinned population) is no better per char: it re-reads `value` and `coder` through the
per-object `GC_FLAG_COMPACT` dispatch on every call and guards the layout epoch.

The U16 row's higher per-char cost is not explained by this reading (the decode is branchless and identical for
both coders); the pin page's B arm also moved it 331 -> 520 without touching its method. Both want a perf-map
profile of the timed call before anything is built on them.

## What a fix looks like (STRINTR21-5 in `jit-r21-strintr-proposals.md`)

1. **One decode node instead of ten.** An IR op (or a `ScalarIntrinsic` row) `StrCharDecode(value, index, coder)`
   whose lowering is the single-pass body's: `TEST coder; JNZ u16; MOVZX EAX, BYTE [value + index + 16]; JMP;
   u16: MOVZX EAX, WORD [value + index*2 + 16]` -- one load per char, no second bounds check (the `index <
   char_count` guard proves both forms). It must carry the bounds guard as its token
   (`with_guard_token`), or scheduling may hoist the load above the guard; every pass that matches on the op
   (verify, GVN, EA, alias classes, the aarch64 backend's exhaustive matches) needs its arm. Cost: medium.
2. **Guards the loop already proved.** The receiver and `value` null guards of the condition's `length()` and of
   the body's `charAt` are four per char on a loop-invariant receiver. A per-receiver memo in the builder (the
   expansion at the loop header already guarded `recv` and `value`; the body's expansion of the same SSA receiver
   in a block it dominates can reuse the guard), or loop predication of invariant guards to the pre-header, takes
   them out. Cost: medium (dominance is the whole question).
3. The general part -- register-resident loop values and vectorising the reduction -- is the codegen backlog the
   20260902 page handed on ("the `char[]` control in the same run reads ~1.0 against HotSpot's ~0.3").

## Probe

`C:\craton\jr21-ssh\probes\R21StrintrRows.java -Dr21.rows=charat-l1,charat-u16`, `--jdk-only` and `--compatible`;
for attribution `CRATONVM_DBG_IR_STRING=1` (`emitted_charAt`), `CRATONVM_DBG=licm` (the four read hoists) and
`CRATONVM_DBG_JIT_DISASM` + a perf IP histogram of `charAtL1`'s body. A fix shows as ns/op falling in proportion
to chars per op; `charat-oob` and the `fact` lines must not move.

## Round 21 fix (lane charat, STRINTR21-5)

Not built or measured in the lane (no host access); the orchestrator builds, runs the unit tests and the A/B.

### 1. One decode node: `Op::StrCharDecode` (`CRATONVM_JIT_IR_STRING_CHAR_DECODE`, default ON, `=0` kill)

* `jit/src/ir.rs`: new op `Op::StrCharDecode`, inputs `[ctrl, mem, value, index, coder]` (+ the bounds guard as its
  trailing guard token when `CRATONVM_JIT_IR_GUARD_TOKEN` is on), result `Int`. `try_string_access_intrinsic`
  builds it after its four guards instead of `off = k << coder`, two `ArrayLoad(Byte)`, `off + coder` and the
  six-node combine (reader `ir_string_char_decode_enabled`; `=0` builds the old nodes byte for byte). It does not
  advance the memory token (like `ArrayLength`).
* Its tables, each stated rather than left to a catch-all: `expected_input_type`, `memory_shape` (`(5,
  ArrayRead)`), `guard_shape` (token at 5), `access_location` (an `ArrayElem` of a `byte[]` with an `Absent` index:
  it aliases byte-array elements only, never a field, and is never must-alias with a byte access, so no
  store-to-load forward can hand it a byte), `memory_shape_of` (`Reads`), `may_raise` (`false`: it checks nothing,
  its guards leave), the static-load memo (`Keep`, as the two `ArrayLoad`s), the putfield narrowing (`char` is
  already narrow); `ir_verify` (control pin at 0 -- so `ir_schedule::pinned_anchor` keeps it in its guards' block,
  and as an impure node it keeps its place after them in `priority_sort_block`'s side-effect chain -- arity
  `(5, 6)`, data slots 2..); `regalloc::ir_op_defines_value`; `ir_optimize::is_dce_root` (an unread one goes) and
  `blocks_direct_static_hoist` (a read, as `ArrayLoad`); `range_analysis` (`[0, 65535]`); `ir_schedule::op_latency`.
* `jit/src/ir_lower.rs`, arm `Op::StrCharDecode`: `MOV RDX, coder; MOV RAX, value; MOV ECX, index; TEST EDX, EDX;
  JNZ u16; MOVSX RAX, BYTE [RAX+RCX+H]; MOVZX EAX, AL; JMP done; u16: MOVZX EAX, WORD [RAX+RCX*2+H]; done:` and one
  `store_rax`. Both loads go through `emit_gpr_array_elem_load` (no new `HEADER_SIZE` site, so
  `layout_const_inventory` does not move). Listed in `op_defines_result_slot`, `op_cannot_deopt` and
  `op_home_is_one_store_rax` (a dropped home / RAX carry is legal: one `store_rax`, `Int`, no deopt).

Why it is right: the guards `index >= 0` and `index < value.length >>> coder` put byte `index` (LATIN1) and bytes
`2*index`, `2*index + 1` (UTF16) inside `value` -- the same argument the old byte loads relied on for their second
byte -- and the little-endian `MOVZX WORD` is exactly `lo | hi << 8`. `coder` is only 0 or 1; any non-zero coder
takes the UTF16 path, which the guard (shift by `coder`) also bounds.

### 2. Guards a dominating fact proves (`CRATONVM_JIT_IR_GUARD_FACT_ELIDE`, default ON, `=0` kill)

* `jit/src/ir_check_elim.rs`: `analyze` now also answers `CheckElision::guard_elided(guard)`, from the same
  dominator-order walk and block facts as the null / bounds elision (`guard_proven`):
  0. `Guard(x != null)` with `x` known non-null (a dominating dereference or null guard, an allocation, the
     receiver) -- the loop body's `s != null` and `value != null` after the header's;
  1. `Guard(a < b)` (or `b > a`) under a dominating `a < b`: an `If` edge (the javac `for` test `k >= count` on its
     fall-through edge) or an earlier guard of the same pair -- the body's `k < count`;
  2. `Guard(x >= 0)` (or `0 <= x`) with `x` proven non-negative (`proven_non_negative_without_unit_stride`'s arms,
     or a unit-stride induction `phi(c >= 0, x + 1)` whose every back edge passes an `If` edge proving `x < y`).
  A guard that is an exception-edge anchor, or sits in an unreachable block, keeps its check.
* `jit/src/ir_lower.rs`, the `Op::Guard` arm: a guard `guard_elided` names (and that has no implicit-throw plan)
  emits nothing -- no compare, no pad, no deopt point -- and notes `GuardElided` evidence.

Why it is right: a guard deopts exactly when its condition is zero; past a dominating proof of the same SSA
condition (SSA operands are not redefined in between; the edge block of an `If` is entered only through that edge)
it cannot be zero, so the guard can never fire, and dropping it drops only a dead deopt exit. The dominance and
fence rules are the ones the array null / bounds elision already rests on.

Net per char of `for (k = 0; k < s.length(); k++) acc += s.charAt(k)` (by reading, unmeasured): the body keeps no
guard at all, and one decode node replaces ten; the header keeps its `s != null` guard (see CHARAT21-1 in
`jit-r21-charat-proposals.md`) and the loop test.

The SIOOBE path is unchanged in both modes: a guard that can fail is never elided (an index not covered by the
loop test -- `charAt(k + 1)`, a loop from a variable start, a bound past `length()` -- keeps its guards), and a
failing guard still deopts to the `invokevirtual`, where the interpreter raises the JDK's own exception and
message. `CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW` / `..._ACCESSOR_CALL_PREFIX` (wave 19) act before the
expansion is built and are untouched.

### Unit tests

* `ir.rs` `jr21_charat_builder_tests`: the charAt site builds one `StrCharDecode` and no byte load (`=0`: none and
  two); the op's tables.
* `ir_lower.rs` `jr21_charat_tests`: the decode, EXECUTED, reads the LATIN1 byte zero-extended and the UTF16 unit
  (`0xE9`, `0xFF`, `0x20AC`, `0xFFFF`, `0xD800`); a second null guard proven by the first emits no deopt point
  (`=0`: both); both switches default ON with `=0` as their kill word.
* `ir_check_elim.rs` `jr21_charat_guard_fact_tests`: in the expansion's loop shape the body's three guards are
  proven, the header guard and an exit-edge `k < count` guard are not; stride 2 keeps `k >= 0`; `=0` proves nothing.

### HOW TO VERIFY

Probe `C:\craton\jr21-ssh\probes\R21CharatRows.java` (rows `charat-inv-l1/-u16` loop-invariant String,
`charat-outer-l1/-u16` String per outer iteration, `charat-field-l1/-u16` String read from a field; `fact` checksums
of all six kernels, a field switched inside the inner loop, and the SIOOBE messages of `charAt(k + 1)`, a negative /
`Integer.MIN_VALUE` start and a bound past `length()`, LATIN1 and UTF16), `--jdk-only` and `--compatible`, one row
per process, interleaved arms:

| arm | env |
|---|---|
| A (default) | -- |
| B | `CRATONVM_JIT_IR_STRING_CHAR_DECODE=0` |
| C | `CRATONVM_JIT_IR_GUARD_FACT_ELIDE=0` |
| D (round-20 control) | both `=0` |

Expected: every `fact` line identical to HotSpot 25 in all four arms and both modes; A below D on all six rows
(the page's 154 / 332 ns per word on the outer rows are the D numbers), B and C each between. Engagement:
`CRATONVM_DBG_IR_STRING=1` prints `emitted_charAt` > 0 and `[ir-string] guard facts R21CharatRows.<kernel>...:
proven null=.. lt=.. nonneg=..` with `lt` and `nonneg` > 0 for the six kernels (a zero there with the switch on is
the "covered by nothing" signal: check that GVN merged the header's and the body's `count`). Also re-run
`R21StrintrRows -Dr21.rows=charat-l1,charat-u16,charat-oob,scan-escape` (its facts must not move) and the jit
unit suites (`ir_check_elim`, `ir_lower`, `ir` tests; `the_two_value_defining_enumerations_agree`,
`every_ir_op_is_lowered_or_declared_unlowerable`, `every_eligible_op_is_claimed_or_explicitly_rejected`,
`layout_constant_inventory`). If C (guard facts off) wins on any unrelated benchmark, the guard elision is the
switch to turn off, not the decode.

Not addressed here: the U16 row's higher per-char cost than L1 (the page's open question; the decode is the same
for both coders now, so an A-arm gap between `-l1` and `-u16` per char is the next thing to profile).
