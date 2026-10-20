> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/jit/churn-loop-runs-in-the-single-pass-osr-body-20261019.md`.

# The churn loop runs its whole life in the single-pass OSR body, and its constructor call may be a dispatch round trip (addressed to the JIT owners)

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (JIT tier).** The churn loop's 7-10x (local rows, `G1ChurnPauseProbe`) is the tier (J1: the IR tier refuses `main`), the call (J2: `Node.<init>` on `jit_invoke_dispatch`), J3 and the frame homes (J4); ZGC's share (item Z) has its own proposal (`zgc-zt4ca2-proposal-sink-the-blind-spill-at-single-pass-zgc-new-20261019.md`). Move to `docs/known-issues/jit/churn-loop-runs-in-the-single-pass-osr-body-20261019.md`.

> **STATUS (2026-10-19, zt t4/ca2): OPEN, addressed to the JIT tier's owners; traced by reading, nothing run (no builds in the zt round).** `G1ChurnPauseProbe`'s compiled loop is ~10x HotSpot per iteration (320-430 ns against 34-41 ns on the orchestrator's Windows box; `zgc-zt4ch-churn-throughput-gap-is-not-in-the-collections-SUPERSEDED-20261019.md`). Reading the loop's code path, the ZGC-specific part of an iteration is two inline allocations (no call on either fast path), two fused reference-store gates and one extra blind spill (below, item Z). The rest -- the tier, the call, the frame homes -- is the JIT's and is collector-independent. Deciding rows CA2-1..CA2-3 in `docs/internal/zgc-round-zt-20261018/t4-ca2-report.md`. This page also carries the zs2ca2 "frame homes" item (item J4).

Area: `jit/src/ir.rs` (indy refusal), `vm/src/runtime/interpreter/jit_bridge.rs` (OSR tier choice), `jit/src/lib.rs` (single-pass direct-callee bind), `jit/src/x64/inlining.rs` (splice walker), `jit/src/ir_lower.rs` (frame homes).

## The loop (`tools/probes/G1ChurnPauseProbe.java` :55-75)

One activation of `main`; per inner iteration `new Node(null, 224, i)` (the constructor allocates `new byte[224]` and stores two references), two field loads, a `long` add. Every 16,384 iterations a 4,096-step walk. A string concatenation (`invokedynamic`) follows the loop (:78).

## J1. The optimizing tier refuses the whole method, so the loop never leaves the single-pass OSR body

- The IR builder's `invokedynamic` arm (`jit/src/ir.rs` :24884-24941) tries the guarded call, the lambda word, capture-new and the bridge (`indy_bridge_site_at`, :24911, opt-in), then plants an uncommon trap. When the trap is refused it bails the whole build: `if !self.plant_uncommon_trap(code, code_len, pc, TrapCause::Indy) { return ir_build_bail(line!(), pc); }` (:24922-24924).
- The trap is refused by the replay guard (`site TRAP REFUSED`, :17547-17559) because `trap_replay_is_safe` (:17417-17438) requires the bytecode before the site to be side-effect free, and the loops before the concat `putfield` and `new`.
- The OSR optimizing build compiles the FULL method (`build_osr_optimizing_artifact` -> `compile_optimizing_artifact(shared, &inputs, true)`, `jit_bridge.rs` :12255), so a site after the loop's exit still bails it, and the decline is memoised method-wide (`osr_optimizing_method_wide_key`, :12385-12390).
- `try_osr` (:13377) then falls back to the single-pass `compile_osr_artifact` (:13800). No profit-driven exit from a single-pass OSR body was found: its back-edge polls leave only on the JVMTI / redefinition verdict bit (`jit/src/x64/safepoint.rs` :1577-1585; `jvmti_events.rs` :1478-1519). One activation therefore runs single-pass for the rest of the run.
- The `ldiv` after the loop (:77) does not force single-pass here (`every_gated_loop_calls` lifts the preference, `jit_bridge.rs` :13027-13030, :13113-13120); the indy refusal does.

Existing levers: `CRATONVM_JIT_IR_INDY_BRIDGE_WITH_INLINE=1` (`ir.rs` :35076-35098) bridges the concat instead of trapping, so the IR builder does not bail; `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` plants the trap anyway (unsafe by its own doc). What would fix the shape in general: an OSR build that does not walk the bytecode the loop cannot reach (a loop-region build), or a trap that is sound after side effects because it deoptimises at the site instead of replaying the method.

## J2. `Node.<init>` is a real call in the single-pass body, and may be a `jit_invoke_dispatch` round trip for the life of the body

- The single-pass splice walker has no arm for `newarray` (`jit/src/x64/inlining.rs`: opcode arms :3641-5569, `_ =>` returns `false` at :5605-5608), so a constructor that allocates an array is never spliced (`try_emit_inline` rolls back, `x64/op_invoke.rs` :4911-4932). Escape analysis drops only trivial `<init>()V` (`escape_analysis.rs` :858, :1571), so neither object is scalar-replaced.
- Whether the call is a direct `CALL` is decided ONCE, when the OSR body is compiled (`build_single_pass_tables`, `jit/src/lib.rs` :67137-67251): if `Node.<init>` had no compiled entry at that instant, the site is counted in `DIRECT_CALLEE_BIND_MISSES` (:67143-67150) and stays on `jit_invoke_dispatch` for the life of the body. The late-bind cell that repairs that is gated on `invoke_kind == 3 && opcode == 0xb8` (`invokestatic` only, :69371-69376).
- This probe makes the miss likely: `main`'s FIRST loop (the 98,304-node live-set build, :47-49) is the one that OSR-compiles the body, early, possibly before `Node.<init>` has been compiled (the background worker passes a lookup-only resolver, :56520-56545). The same comment measured a dispatch round trip at 200-800 ns per allocation on the host, which alone covers the 320-430 ns per iteration.
- Even when bound, the direct call costs roughly 35-60 caller instructions around the `CALL` (argument copies, a stack argument on Win64, a full blind spill + shadow push because the receiver is an oop argument, the post-call republish, two `CMP RAX,1` checks; `x64/op_invoke.rs` :8695-9108) plus 25-40 of callee prologue / epilogue (`x64/frames.rs` :863-1200). HotSpot inlines the constructor.

What would fix it: a late-bind cell for `invokespecial` (the `invokestatic` one, extended), and a splice arm for constant-length `newarray` (the IR tier already has the inline array bump).

## J3. No allocation is eliminated

HotSpot C2 inlines `Node.<init>`; `dead` does not escape, so it can scalar-replace the `Node` (the `byte[224]` is above `EliminateAllocationArraySizeLimit`, 64, so it is allocated but its `length` folds to 224). The single-pass tier allocates both every iteration. Only an optimizing OSR body (J1) could do better.

## J4. The IR tier's frame homes (carried from `docs/internal/gc/zgc-zs2ca2-compiled-allocation-loop-is-instruction-bound-SUPERSEDED-20261019.md`)

On the zs2ca2 count (`Zs2Ca2AllocWindows.burstFixed0`, Oct-03 binary), about 40 of ~85 instructions per iteration were not allocation: the index and induction homed to frame slots, the `aastore` checks, the barrier gates and the poll; HotSpot pays about 5 for them. The register-allocation part (values homed to frame slots across a loop with a safepoint) is the IR tier's. Row I1 (Windows-runnable): `CRATONVM_DBG_JIT_DISASM=Zs2Ca2AllocWindows.burstFixed0 cratonvm -Xmx2g -cp $P Zs2Ca2AllocWindows 3 2>fixed0.disasm`, count the fast-path instructions from the loop head to the back edge and list the `[rbp-..]` loads and stores of the index / induction.

## Z. What is ZGC's in the iteration (for the record; nothing here is a call on a fast path)

- `new Node` (single-pass, `TlabPostInit::ZgcAnnounce`): bump + header + the LONG inline start-bit store + commit, ~45-50 instructions, no call (`x64/objects.rs` `emit_inline_tlab_new_with` :4329-4927; announce :1063-1193).
- **One ZGC-only cost here:** because ZGC must be told about the object, `skip_helper` is false (`x64/op_object.rs` :104), and the allocation spill sink requires `skip_helper` (:135-139). So on ZGC every single-pass `new` pays the full blind GPR spill (up to 11 stores) and a shadow push that Generational / G1 sink onto the slow path, although the inline announce arm makes no call either. Proposal: `docs/internal/proposals/gc/zgc-zt4ca2-proposal-sink-the-blind-spill-at-single-pass-zgc-new-20261019.md`.
- `new byte[payloadBytes]` inside `Node.<init>` (IR tier when compiled alone): the run-time-length inline bump with the inline announce, ~46 instructions plus the safepoint-id store, no call (`runtime_lowering.rs` `emit_inline_tlab_newarray_ir_impl`, `ir_lower.rs` :33254-33343). Constant 224 only if the constructor were inlined (J2).
- `this.next = next`, `this.payload = arr` (IR tier): the fused gate, 11-13 instructions each on the not-marking path, no card or remembered-set work (`ir_lower.rs` :34726-34744, :18330-18467). `this` is not "fresh" for the opt-in fresh-receiver elision (a `Param`, not an `Op::New`; :57471).
- `dead.payload`, `dead.tag` (single-pass): no ZGC load barrier in emitted code (`x64/licm.rs` :709-813); 8-10 instructions each.
- Back-edge poll: `TEST byte [rip+d], 0xFF; JZ` (`x64/safepoint.rs` :1509-1535), collector-independent.

## What would close it

Rows CA2-1 (bind census and dispatch entries), CA2-2 (`IB` arm: the optimizing OSR body) and CA2-3 (`main` vs `method` shape) name the owner of the 10x. If CA2-1 shows the `<init>` site on the dispatch helper, J2's late bind is the fix; if `IB` brings `main` to `method`'s CPU, J1 is.
