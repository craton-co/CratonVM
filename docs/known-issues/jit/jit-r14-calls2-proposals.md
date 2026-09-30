# JIT round 14 wave 4, lane calls2: proposals (inline pricing, literal folds, IC layout)

Status: OPEN (proposal book; ranked; nothing here is a defect)
Area: `jit/src/lib.rs` (inline planners), `jit/src/ir.rs` (string access expansion), `jit/src/ea_ir_bridge.rs` (literal folds), `jit/src/ir_lower.rs` (IC sites)
Found by: round 14 wave 4 lane calls2

Context: this wave landed C14W3-1 (`CRATONVM_JIT_INLINE_LOOP_SITES_HOT`) and C14W3-4's fold
(`CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD`, input patch
`r14w4-calls2-literal-text-plumbing-patch-FIXED-20260929.md`), and wrote C14W3-2 up as a design
(`r14w4-calls2-ic-hit-cold-tail-needs-an-out-of-line-region-20260929.md`). Ranked by expected value
per unit of risk.

## C2W4-1. A structural-evidence method budget between the cold and hot ones (rank 1)

- **Benefit:** C14W3-1 gives every method with ANY loop `MAX_INLINE_BUDGET_HOT` (2000) in a
  default run, because the loops are now hot evidence. A method with a rarely-run retry loop and
  many cold sites outside it gets the hot budget too (the cold sites keep the 35-byte cap, so the
  exposure is bounded, but the in-loop sites of a big method can now spend up to 2000 bytes where
  profiled evidence might have said 750). A middle budget (say 1200) for "loops known only from
  the bytecode" keeps the per-site tier and bounds code growth.
- **Cost:** small (`method_entry_hot_loop_ranges` answers whether the evidence was structural;
  `build_single_pass_tables` picks the budget).
- **Risk:** low.
- **First step:** the bench of C14W3-1 both ways; if single-pass code bytes (`inline_tally`
  `inlined_bytecodes` in the metrics report) grow more than the time improves, try the middle budget.

## C2W4-2. The optimizing tier's splice planner spends its budget in pc order (rank 2)

- **Benefit:** `ir_tier_attempt`'s splice loop walks `scan.invoke_ops` in bytecode order and stops
  at `IR_INLINE_MAX_SITES` (24) or when `IR_INLINE_MAX_TOTAL_BYTES` (1300) runs out. A method whose
  first sites are cold setup calls (before its loop) can spend the budget there and leave its
  in-loop sites as calls. Planning in-loop sites first (`bytecode_pc_in_loop`, then pc) is the
  HotSpot order (hotter sites first).
- **Cost:** small in code (iterate a sorted copy of `invoke_ops`), but the loop is shared with the
  synchronized-splice block and the chain bookkeeping: check nothing in it assumes ascending pc
  (the combined-buffer offsets do not; `ir_spliced_ranges` consumers must be checked).
- **Risk:** medium (touches the busiest planner loop).
- **First step:** a census under `CRATONVM_DBG_JITC=1` of compiles that hit the budget or the
  site cap with an unplanned in-loop site left (one line per such site).

## C2W4-3. Skip the string access expansion on a literal receiver (rank 3)

- **Benefit:** `IrBuilder::try_string_access_intrinsic` expands `"lit".charAt(k)` into loads,
  guards and a branchless decode; C14W3-4 folds only the count (`value.length >>> coder`), so the
  decode's loads stay. Handing the builder the same literal texts and answering the site with a
  constant before expanding removes the loads and guards too.
- **Cost:** small (a text table on `IrBuilder`, set by `ir_tier_attempt` beside `set_ldc_string_info`;
  the refusal and constant push inside the function).
- **Risk:** low (the fold's argument, earlier in the pipeline).
- **First step:** after the plumbing patch, count expansions on `ConstString` receivers
  (`note_string_intrinsic` has the counters).

## C2W4-4. One literal resolver instead of two (rank 4)

- **Benefit:** the hash map (I7-4) and the text map (C14W3-4) are filled from the same constant-pool
  read; `String.hashCode` is a pure function of the text, so the JIT can compute the hash from the
  units and the hash resolver can go (one class-manager read per `ldc` site per compile, not two).
- **Cost:** small (drop `cp_ldc_string_hash_resolver`, compute `s[0]*31^(n-1)+...` wrapping in
  `ir_tier_attempt`).
- **Risk:** low; keep a unit test pinning `"polygenelubricants"` to `Integer.MIN_VALUE`.
- **First step:** apply the plumbing patch, then fold the two maps.

## C2W4-5. Static-site call counts as hot evidence where the profile has them (rank 5)

- **Benefit:** `MethodProfile::call_sites` holds direct counts for static sites (recorded on the
  slow resolution path only), and `call_site_is_hot` never reads them. With C14W3-1 an in-loop
  site is already hot; a hot static site OUTSIDE any loop (a method called in a caller's loop that
  makes one static call per invocation) is still cold. If `call_sites` were recorded per execution
  (it is not today), `call_site_count(pc)` would price it.
- **Cost:** medium (the interpreter records the count only on first resolution; a per-execution
  counter costs every interpreted static call).
- **Risk:** low-medium (interpreter hot path).
- **First step:** list the readers and writers of `call_sites` and what recording per execution
  would cost in `R13Mega9CallAnatomy`-style interpreted loops.
