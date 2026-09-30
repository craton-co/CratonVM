# JIT round 14 wave 5, lane callcost: proposals (call cost)

Status: OPEN (proposal book; ranked; nothing here is a defect)
Area: `jit/src/ir_lower.rs` (the direct self-call, the entry fast return and the entry fold)
Found by: round 14 wave 5 lane callcost

Context: this wave landed the entry fold (`CRATONVM_JIT_IR_ENTRY_FOLD`): a self-recursive body
answers the first few call-free, pure arguments past its entry fast return from a `CMOVE`
table before the prologue. See `r12w8-callcost-fib-call-anatomy-20260927.md`, "Round 14 wave 5".
Ranked by expected value per unit of risk.

## CE-1. Answer a direct self-call in the CALLER when its argument is in the fast-return or fold range (HIGH for `fib`, MEDIUM)

* **Benefit.** After the entry fast return and the fold, a leaf activation of `fib` still costs
  the caller's `mov rcx,[rbp-ctx]`, the argument move, `CALL`, the callee's `CMP/Jcc` pair(s),
  the stub, `RET`, the post-call RBP republish and the `CMP RAX,1; JO` sentinel test: about 12
  instructions and a call/return pair, for ~80% of all `fib` calls (`n <= 3`). Emitting the same
  compare at the call site (`CMP arg, hi; JLE .inline_answer`) and producing the answer into the
  call's result register there skips all of it; the answer path has no call, so no republish, no
  sentinel and no stack sample.
* **Cost.** In `emit_self_recursive_call`: after the argument is staged in its ABI register,
  emit the entry fast return's compare and the fold's selects against it, write the result where
  the call's result goes (`store_rax` / its register), and jump over the `CALL` and its sentinel.
  The plan (`plan_entry_fast_return`, `plan_entry_fold`) is per compile, so both are known before
  the first self-call is lowered.
* **Risk.** Medium: the answer path must leave the lowering state exactly as the call path does
  at the join (the call's residency bookkeeping, `invalidate_ref_residency`, the deferred sp id),
  so the first cut should admit only reference-free bodies (`lean_ref_free_prologue`) and join
  with the result in RAX right before the existing `.keep` label, where both paths already agree.
* **First step.** A census of self-call arguments by value on `CratonBench.fib` (they follow the
  Fibonacci mix; the point is to confirm the ~80% by counting) and the disassembly of one site.

### Round 14 wave 6 (lane callcost2): CE-1 landed

`jit/src/ir_lower.rs` `emit_self_call_answer` (switch `CRATONVM_JIT_IR_SELF_CALL_ANSWER`, default
ON; census `CRATONVM_DBG_IR_SELF_CALL_ANSWER=1`). The answer is emitted after the site's stack
sample (not before: a dominating sample must still run) and joins right before the `.keep`
store, as proposed; admission is a reference-free body, no carried argument, the compared
argument in a register or home word. Without a fold any fast-return condition is answered, and
the fast return's value may be any parameter's argument. Details and tests:
`r12w8-callcost-fib-call-anatomy-20260927.md`, "Round 14 wave 6".

## CE-2. Fold `long` parameters and results (LOW-MEDIUM, LOW)

* **Benefit.** The fold reads only `int` parameters and returns only `int` results; a `long`
  recursion (`long fib(long n)`, `long fact(int n)`) keeps its call-free activations.
* **Cost.** `entry_fold_int_op` for 64-bit ops, a `long` compare in the plan (the fast return
  would need `emit_cmp_reg_imm(.., wide = true)` too), and `MOV RAX, imm64` / `MOV R11, imm64`
  selects in the stub.
* **Risk.** Low: the evaluator is the same walk; the result representation is the full 64 bits.
* **First step.** Count `long`-returning self-recursive methods in the probe batteries and the
  Spring sample (`CRATONVM_DBG_IR_ENTRY_FOLD=1` prints nothing for them today).

## CE-3. Fold past a pure static-final read (LOW, LOW-MEDIUM)

* **Benefit.** A `static final int` threshold (`if (n < CUTOFF) return small(n)`) is usually a
  constant after the builder, but a non-constant-folded `static final` read is a `LoadStatic`,
  which the fold's purity test refuses.
* **Cost.** Admit a `LoadStatic` of a `static final` field of an initialized class whose value
  the builder could have folded, reading the value at compile time.
* **Risk.** Low-medium: `static final` fields can be written by reflection/`Unsafe` before
  initialization completes; only admit a class whose initialization has finished.
* **First step.** Check whether the IR builder already folds such reads (then this is moot).

## CE-4. A fold-aware splice depth (LOW-MEDIUM, MEDIUM)

* **Benefit.** The fold's range is set by how many spliced levels bottom out without a call
  (`IR_RECURSIVE_INLINE_MAX_COPIES = 2` gives `fib` `{2, 3}`). One more level widens the range
  but grows the body; with the fold the trade moves (the call-free activations are nearly free).
* **Cost.** Measure `fib` at 2 and 3 copies with the fold on (the constant is in `jit/src/ir.rs`).
* **Risk.** Medium: code size and compile time for every self-recursive body.
* **First step.** The measurement.
