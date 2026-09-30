# JIT round 14 wave 3, lane calls: proposals (call cost, inline pricing, literal folds)

Status: OPEN (proposal book; ranked; nothing here is a defect)
Area: `jit/src/ir_lower.rs` (call cold tails), `jit/src/lib.rs` (inline pricing, IC grace), `jit/src/ea_ir_bridge.rs` (literal folds), `x64/op_invoke.rs`
Found by: round 14 wave 3 lane calls

Context: this wave landed CC3-1's direct half (`CRATONVM_JIT_IR_CROSS_CALL_COLD_TAILS`), OD-1
(`CRATONVM_JIT_OSR_SPLICE_ENTRY_LOOP_HOT`) and I7-4's fold (`CRATONVM_JIT_IR_STRING_LITERAL_HASH_FOLD`,
input patch `r14w3-calls-literal-hash-plumbing-patch-FIXED-20260929.md`). Ranked by expected value per
unit of risk. (Named `calls3` because `jit-r14-calls-proposals.md` is wave 1's book.)

## C14W3-1. Price in-loop static sites hot at method entry (rank 1)

- **Benefit:** by default no static or `invokespecial` site is ever priced hot
  (`r14w3-calls-static-sites-always-priced-cold-FIXED-20260929.md`: loop profiles need
  `CRATONVM_TIER_PGO`, and static sites have no receiver profile), so every in-loop static helper
  between 36 and 325 bytes stays a compiled call in the method-entry tiers. Every benchmark kernel
  that factors its loop body into a static helper pays a call per iteration.
- **Cost:** small: `bytecode_pc_in_loop` as hot evidence when the profile carries no loop data, in
  the two method-entry planners (the OD-1 argument: the compile is the frequency evidence).
- **Risk:** medium (code size, frame size); needs a bench before the default.
- **First step:** the page's probe run: count `CalleeTooLarge` refusals of in-loop static sites
  under `CRATONVM_DBG_JITC=1` on CratonBench, default against `CRATONVM_TIER_PGO=1`.

## Round 14 wave 4 (lane calls2): C14W3-1 landed

`lib.rs` `method_entry_hot_loop_ranges` (every loop of the method when the profile carries no loop
evidence), read by `build_single_pass_tables`; switch `CRATONVM_JIT_INLINE_LOOP_SITES_HOT`, default
ON pending the bench. See `r14w3-calls-static-sites-always-priced-cold-FIXED-20260929.md`.

## C14W3-2. The inline-cache hit arms' cold tails, with the hit falling through (rank 2)

- **Benefit:** CC3-1's other half. Deferring only the cold bytes (`emit_ic_hit_exit`'s service)
  keeps the taken `JNO .hit_join`; the branch goes too only if the hit's exit becomes `CMP; JO
  .cold` falling through INTO `.hit_join`, which needs `.hit_join` emitted right after the MIC arm
  (the rungs keep a `JMP`). One taken branch per monomorphic IR virtual call.
- **Cost:** medium: the IC arm's patch lists (`hit_patches`, `done_patches`, `tail_patches`) and
  the republish kill switch's edge choice; the service copies back (`after_reload = false`), so the
  CC2-1 admission applies (no roots, no pending publication) until CC3-3.
- **Risk:** medium.
- **First step:** read the landed census (`ir-call-cold-tails ... ic_hit_exits=<n>
  ic_admissible=<m>`) on `hashmap` and `R12Mega4OneSite`; worth it only where `m` is a large share.

## C14W3-3. Widen the M8-1 inline-cache stamp to every Rust helper entry (rank 3)

- **Benefit:** `r12w7-mega6` is left with a thread spinning in pure machine code. Any Rust helper
  compiled code calls is, like the miss handlers, outside every inline-cache probe (no probe spans
  a call), so a spinner that allocates past its TLAB (`jit_new_object` slow path), resolves a
  static, or takes a safepoint poll's slow path could vouch too, without the handshake.
- **Cost:** small per helper: one `cratonvm_jit::jit_ic_quiescent_point()` at entry (a TLS read, a
  load, a store). Candidates by call frequency in loops: the allocation slow paths, the
  `ldc`/`getstatic` helpers, the safepoint poll slow path (`gc_barrier`, GC session's file).
- **Risk:** low (the argument is M8-1's; the stamp never feeds code release).
- **First step:** `R14MicSpinnerMisses` with a spinner that allocates, `mic_grace_lag_refused`
  before and after stamping the allocation slow path.

## C14W3-4. Fold the other `"literal".xxx()` answers (rank 4)

- **Benefit:** with the site's text known at compile time (the I7-4 plumbing), `length()`,
  `isEmpty()`, `charAt(k)` for a constant `k` in range, and `equals("other literal")` are
  constants too; `switch` over strings compiles to `hashCode()` + `equals(literal)` chains whose
  `equals` receiver is the literal.
- **Cost:** small: carry the literal's UTF-16 length (and the text, for `charAt`/`equals`) beside
  its hash in the same resolver answer.
- **Risk:** low; out-of-range `charAt` must keep its call (it throws).
- **First step:** after the plumbing patch, a census of `ConstString` receivers per `String`
  method across CratonBench under `CRATONVM_DBG_IR_COMPILES=1`.

## Round 14 wave 4 (lane calls2): C14W3-4 landed

`ea_ir_bridge.rs` `ir_fold_string_literal_queries` (`length`, `isEmpty`, in-range constant
`charAt`, `equals` against the same node / `null` / another literal, and the string access
expansion's `value.length >>> coder`); switch `CRATONVM_JIT_IR_STRING_LITERAL_QUERY_FOLD`, default
ON. Its input (each `ldc` site's UTF-16 text) is the exact patch
`r14w4-calls2-literal-text-plumbing-patch-FIXED-20260929.md`.

## C14W3-5. The single-pass tier's direct-call sentinel path out of line (rank 5)

- **Benefit:** the same taken-branch-over-cold-code shape exists in `x64/op_invoke.rs`'s direct
  call and IC hit paths (`emit_post_invoke_exception_check` and the callee-deopt service), which
  every C1 body and every OSR body from the single-pass door runs.
- **Cost:** medium (the single-pass emitter has no deferred-tail list; the OSR door's bodies read
  `call_exc` facts at emission).
- **Risk:** medium.
- **First step:** count sentinel checks per single-pass body in the hot set (`CRATONVM_DBG_JIT_GEN`).

## C14W3-6. Let the OSR door's loop evidence reach the method-entry compile (rank 6)

- **Benefit:** a method first compiled by OSR is later recompiled at entry, where (C14W3-1) its
  loops look cold again. Recording the OSR trigger's loop in `MethodProfile::loops` (a back-edge
  count at the threshold, keyed by `LoopExtents::back_edge_key`) would make the later compile
  agree with the OSR body about which sites are hot, even with `CRATONVM_TIER_PGO` off.
- **Cost:** small (the OSR trigger knows the back edge and the count).
- **Risk:** low-medium: other readers of `loops` (loop-trip speculation) would start seeing data in
  default runs; gate them or use a separate field.
- **First step:** list every reader of `MethodProfile::loops` and what each does with an entry.
