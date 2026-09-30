# JIT round 14 wave 6, lane callcost2: proposals (call cost after CE-1)

Status: OPEN (proposal book; ranked; nothing here is a defect)
Area: `jit/src/ir_lower.rs` (direct self-calls, the entry fast return / fold, the prologue)
Found by: round 14 wave 6 lane callcost2

Context: this wave landed CE-1 (`CRATONVM_JIT_IR_SELF_CALL_ANSWER`): a direct self-call whose
argument the callee's entry answers without a frame is answered at the call site
(`r12w8-callcost-fib-call-anatomy-20260927.md`, "Round 14 wave 6"). Ranked by expected value per
unit of risk.

## CC6-1. One compare for a sibling pair of answered self-calls (MEDIUM for `fib`, LOW)

* **Benefit.** `fib(n-1) + fib(n-2)` asks `n-1 <= 3` and then `n-2 <= 3` at the two sites. When
  both arguments are `p - k` of the same value `p` (the builder's `Sub(p, Const)`), the first
  site's compare decides the second's (`p - 1 <= 3` implies `p - 2 <= 3`), and the second site
  is dominated by the first on every path. The dominated site can skip its `CMP/JG` and, on the
  first site's answer path, its whole answer is a table lookup on `p` (both answers from one
  `CMOV` chain), so the pair costs one compare and one join instead of two.
* **Cost.** Reuse `plan_self_call_sample_elision`'s dominance facts; record per site the answered
  range as an interval of `p` (`Sub(p, k)` with a constant `k`); at the dominated site emit the
  answer unconditionally when the dominating site's interval implies it, otherwise as today.
* **Risk.** Low-medium: the implication must be checked on the 32-bit wrapping values (`p - 2`
  wraps for `p = MIN + 1`; restrict to intervals that cannot wrap given the dominating compare).
* **First step.** The disassembly of `CratonBench.fib` with CE-1 on, counting the answered
  sites whose argument is `Sub(p, k)` of a value another answered site also subtracts from.

## CC6-2. The parameter straight into its register (F8-4 / LC-5, restated after CE-1) (LOW-MEDIUM, MEDIUM)

* **Benefit.** One store-forwarded reload (`MOV r, [rbp-8]` right after the prologue stored it)
  per activation that reaches the prologue. After CE-1 those are the activations with `n >= 4`
  (for `fib`), so the saving per `fib(N)` is smaller than when LC-5 was written.
* **Cost.** In `emit_prologue`, after the parameter stores, `MOV assigned_gpr(p), abi_reg(p)`
  for each `int`/`long` `Op::Param` with an assigned GPR; in the `Op::Param` arm, skip
  `publish_gp_from_slot`'s load and just `mark_gp_reg_live` -- only when no node lowered before
  it in block 0 writes that register. The robust form of that condition is a planner pin: give
  every `Op::Param` interval a start at position 0 (`ls_certain_refusals` / the interval builder
  in the linear scan), so no earlier value can be handed its register.
* **Risk.** Medium: a wrong register at entry is a wrong answer everywhere downstream; the
  outlined entry poll's slow path and the thread fetch's helper both run between the prologue
  and the `Op::Param` node and must preserve the callee-saved file (they do: C ABI), which is
  why the fill belongs in the prologue.
* **First step.** A census (`CRATONVM_DBG_IR_LINEAR_SCAN=1`) of how often a parameter's
  assigned register is handed to a value defined before it in block 0.

## Round 14 wave 7 (lane callcost3): CC6-2 landed

`CRATONVM_JIT_IR_PARAM_REGISTER_FILL` (default ON; census `CRATONVM_DBG_IR_PARAM_FILL=1`).
`emit_prologue` fills `MOV dst, abi` and marks the parameter live; the `Op::Param` arm skips
its reload while `gp_reg_owner[dst]` still names it. Instead of a planner pin, the existing
per-register owner interlock (`mark_gp_reg_live`) clears the fill when anything is published
into `dst` first, and the admission (block 0, only `Param`/`Const` before it) keeps that
bookkeeping complete. Details: `r12w8-callcost-fib-call-anatomy-20260927.md`, "Round 14 wave 7".

## CC6-3. A compile-time answer for a constant self-call argument (LOW, LOW)

* **Benefit.** `f(0)` / `f(k)` written with a literal argument (a seed call inside the body, a
  memo warm-up) is declined by CE-1 today (`the compared argument is a constant`). The answer
  is known at compile time: the site could emit `MOV RAX, answer` and no call path at all.
* **Cost.** In `emit_self_recursive_call`, before the sample: when the argument is a constant in
  the answered range, emit the value and skip the sample, marshal and call. The bookkeeping the
  call path does for the lowering state (`deferred_self_call_sp_id`, `self_call_sites_lowered`,
  the dominated-sample plan: a site whose sample dominates others must keep it) has to be
  replayed exactly, so the first cut should admit only a site that dominates no other sample.
* **Risk.** Low.
* **First step.** Count constant-argument direct self-calls in the probe batteries.

## CC6-4. CE-1 for bodies that publish roots (LOW-MEDIUM, MEDIUM)

* **Benefit.** A recursive method that also holds a reference (a tree walk with an `int` depth
  guard, `f(node, depth)`) keeps every call. Its answer path would run the site's shadow push
  (emitted by `emit_safepoint_map` before the site) and the reload after `.keep` for nothing.
* **Cost.** Admit such bodies; since the answer path reaches no safepoint the reload restores
  the words the push saved, which is correct but costs the push/reload pair; the better form
  moves the answer check before the map (so the push is skipped too), which needs the map's
  emission split from its bookkeeping.
* **Risk.** Medium: the push/reload balance is checked by `lower_inner` (it discards an
  unbalanced body), and the frame-block mode changes both halves.
* **First step.** A census of declined sites by reason (`CRATONVM_DBG_IR_SELF_CALL_ANSWER=1`) on
  the Spring sample and the probe batteries: how many say `the body publishes roots`.

## CC6-5. Answer a direct CROSS call from the callee's published entry plan (MEDIUM, HIGH)

* **Benefit.** `isEven(n) -> isOdd(n - 1)` style mutual recursion and small leaf helpers with an
  argument guard (`if (n <= 0) return 0;`) pay a full call for the guard's answer.
* **Cost.** Publish the entry fast return / fold plan in the callee's `CompiledMethod`; a caller
  binding a direct call (`direct_calls`) reads it at compile time and emits the CE-1 answer.
* **Risk.** High: the plan belongs to one version of the callee; a redefinition or a
  recompilation with a different plan must invalidate the caller (a dependency edge the
  direct-call binding does not record today). Only worth it with that dependency machinery.
* **First step.** Count direct cross-call sites whose callee has an entry fast return.
