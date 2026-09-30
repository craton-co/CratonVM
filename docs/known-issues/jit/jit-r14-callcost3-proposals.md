# JIT round 14 wave 7, lane callcost3: proposals (call cost after the fold through self-calls)

Status: OPEN (proposal book; ranked; nothing here is a defect)
Area: `jit/src/ir_lower.rs` (entry fold, caller-side answer, prologue)
Found by: round 14 wave 7 lane callcost3

Context: this wave made the entry fold evaluate direct self-calls
(`CRATONVM_JIT_IR_ENTRY_FOLD_SELF_CALLS`) and filled the parameter register in the prologue
(`CRATONVM_JIT_IR_PARAM_REGISTER_FILL`); see `r12w8-callcost-fib-call-anatomy-20260927.md`,
"Round 14 wave 7". Ranked by expected value per unit of risk.

## CC7-1. A wider fold as a table load (MEDIUM for `fib`, LOW-MEDIUM)

* **Benefit.** With self-calls evaluated, the fold is capped only by
  `ENTRY_FOLD_MAX_VALUES` (4): `fib` could fold `n` in `2..=12` just as soundly. Each extra
  folded argument removes the full activations of that argument (and, through CE-1, the calls
  with it); two more values remove another `phi^2` of the remaining activations.
* **Cost.** Past four values the `CMOVE` chain grows linearly; replace it with a constant
  table in the code buffer: `CMP p, hi; JA-style range check (unsigned p - lo <= hi - lo);
  MOV EAX, [rip + table + 4*(p - lo)]` (one `LEA`/`MOVSXD`), both in the entry stub and in
  `emit_self_call_answer`. Rip-relative data already exists for jump tables (`Op::Switch`).
* **Risk.** Low-medium: the table must be emitted after the body and the displacement patched;
  the range check must be unsigned on `p - lo` (a negative `p` must not index).
* **First step.** Raise the cap to 8 under a switch with the CMOV chain, and time `fib` at 4, 6,
  8 values (the chain's length against the removed calls).

## CC7-2. Fold through self-calls whose other arguments change (LOW-MEDIUM, MEDIUM)

* **Benefit.** `acc(n, a)`-shaped bodies (accumulator in a second parameter) fold nothing:
  the evaluation knows only the compared parameter, so `acc(n - 1, a + n)` gives up.
* **Cost.** Evaluate symbolically: the fold value becomes an affine function of the other
  parameter (`a + k(n)`), answered as `LEA RAX, [a_reg + k]`. Only for `Add`/`Sub` chains on
  the non-compared parameter.
* **Risk.** Medium: a second value domain in the evaluator; overflow is fine (wrapping) but
  every op on the symbolic value must be affine or the evaluation gives up.
* **First step.** Count, in the probe batteries, recursive methods whose fast return answers a
  parameter other than the compared one (`CRATONVM_DBG_IR_ENTRY_FOLD=1`, reason
  `the returned n... is not a known int`).

## CC7-3. CC6-1 in a contained form: the dominated site reads the dominating answer (LOW, MEDIUM)

* **Benefit.** One macro-fused `CMP/JG` per answered sibling pair (`fib`'s spliced leaves).
* **Cost.** The compare at the dominated site can only be skipped on a path that knows the
  dominating site answered, and that path ends at the dominating `.keep` join. A contained
  form: when the two sites are ADJACENT in one block (nothing lowered between the first
  site's `store_rax` and the second site's answer check except pure nodes whose values die
  at the second site), emit the second site's answer on the first site's answer path and jump
  to the second site's `.keep`, replaying the first `.keep`'s store there; admit only when the
  lowering state at the second `.keep` is provably the same on both paths (no residency
  transition, no home store elided in between).
* **Risk.** Medium: it is a second join with state reasoning of CE-1's kind; small saving.
* **First step.** The disassembly of `CratonBench.fib` after this wave: how many answered pairs
  are adjacent in that sense.

## CC7-4. Evaluate the fold once per method, not once per compile (LOW, LOW)

* **Benefit.** Compile time only: the fold (and with self-calls, up to 64 body walks) is
  repeated at every recompile of the same method.
* **Cost.** Cache `(values, stop)` on the method's compile record keyed by the bytecode hash.
* **Risk.** Low; only worth it if a census shows recursive methods recompiled often.
* **First step.** `CRATONVM_DBG_IR_ENTRY_FOLD=1` over the Spring sample: lines per method.
