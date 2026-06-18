# Round 5 JIT Performance & Correctness Review

Scope: `jit/` crate + `vm/src/jit/helpers.rs`. Audits round-4 wave-1 + wave-2 changes (PIC arg-hoist, CALL rel32, MOVQ direct, self-move peephole, null_check_elim wiring, pc-indexed FxHashMaps, PIC mod=00 slot-0, GVN FxHasher, ir_lower disp8, deopt sentinels) and remaining HIGH/MED items.

## 1. [CRIT] Direct-call codegen silently truncates args > ARG_REGS.len()

**Where:** `jit/src/x64.rs:10354-10358`, `10379-10389`, `10524-10533`, `10584-10594`

Every `if i + 1 < ARG_REGS.len()` / `if i < ARG_REGS.len()` guard inside the direct-call (`invokestatic` / `invokespecial`-via-direct / `invokevirtual`-direct / self-call) just *drops* every arg past the register file: no bail, no stack-arg setup, no abort. On Windows (4 ABI regs), any `callee_needs_ctx=true` method with **≥ 4 Java args** has its 4th+ args silently replaced by whatever was last in R9/RSP-shadow. The wave-2 helper-path `bail_to_interpreter` does NOT cover this — direct-call sites don't go through `call_jit_compiled_method_entry`. **Fix:** before the for loop, `if arg_slots.len() + (callee_needs_ctx as usize) > ARG_REGS.len() { fall through to the dispatch-helper path }`. Or emit Windows shadow-space / SysV stack args (see finding #8).

## 2. [CRIT] `null_check_elim::analyze` is single-pass — wired output is unsound at branch targets

**Where:** `jit/src/null_check_elim.rs:84-241`, wired at `jit/src/x64.rs:11491-11503,11541-11553` (ifnull / ifnonnull elision)

`analyze` walks PCs linearly with a single `current: u64` mask; it never propagates predecessor masks across forward branches and never iterates to fixpoint. Concretely: a forward `if_icmpne L1` jumps over a `getfield` that sets `local 0` non-null; at `L1` the analyzer still reports `local 0` non-null because the linear sweep carried the post-getfield mask forward. Round 4 wave 2 wired `is_local_nonnull` to skip `TEST RCX,RCX; JZ throw_npe` for `ifnull` / `ifnonnull` — the JIT now omits NPE checks on possibly-null values, breaking JVMS §6.5 (and silently corrupting Java programs that intentionally test for null after a branch). **Fix:** convert to a worklist-based meet-over-paths analysis (predecessor union = AND for proven-nonnull lattice), or — short-term — only consult `is_nonnull(pc, l)` when the immediately-prior bytecode is a non-branch instruction (i.e. no other CFG edge enters `pc`).

## 3. [CRIT] PIC inline class_id load encoding assumes `ARG_REGS[1] & 7 ∉ {4, 5}`

**Where:** `jit/src/x64.rs:10827-10834`

`MOV EAX, [ARG_REGS[1]]` is emitted as `0x8B recv` (or `0x41 0x8B recv&7` for R8+) using **mod=00**. That encoding silently reinterprets `r/m=4` as SIB-required and `r/m=5` as RIP-relative. Today `ARG_REGS[1]` is RDX (Win, low3=2) or RSI (SysV, low3=6), so it's safe; but the encoding has no debug_assert and any future ARG_REGS shuffle that puts RSP/RBP/R12/R13 there will produce silent wrong-PC dispatch. **Fix:** add `debug_assert!(recv_reg & 7 != 4 && recv_reg & 7 != 5)`, or fall back to the disp8-with-zero (mod=01) form when low3 ∈ {4, 5}.

## 4. [HIGH] PIC slot-0 mod=00 saved one byte but kept the slot 1/2 reload symmetry inconsistent

**Where:** `jit/src/x64.rs:10866-10882`

The slot-0 CMP shrinks from 4→3 bytes (correct ModRM `0x02`). But the inter-slot rel8 jne patches still target `slot_starts[i+1]` computed *after* slot 0's now-shorter body — `next_slot_patches` resolves correctly because it uses `self.buf.pos()` at slot-start time. Good. However the `debug_assert!((-128..=127).contains(&rel))` at line 10960 is now closer to the rel8 limit (1 byte saved per upstream slot × every preceding slot). For `n=4` on Win64 the cumulative pre-slot-1 body is borderline (~110 bytes); add a fuzz/large-method test or convert inter-slot JNE to rel32 to remove the silent-overflow risk.

## 5. [HIGH] `emit_call_absolute` computes rel32 against `buf.as_ptr() + buf.pos()` — patched calls may go stale

**Where:** `jit/src/x64.rs:5424-5440`

Rel32 is computed at emission time using the *current* mmap base. `JitBuf::reserve` does not relocate (verified — `ptr` is set once by `ExecutableBuffer::new`). However, **OSR trampolines** are allocated in a *separate* `ExecutableBuffer` (`jit/src/lib.rs:937+`) at a different base; if the trampoline ever emits `emit_call_absolute` against a helper, the displacement is computed against the trampoline buf base. This is currently fine (trampolines emit MOV imm64 + JMP), but the helper is also reachable to subsequent JIT recompilation buffers whose base differs. Any code that copies bytes from one buffer to another (none today; verified) would silently break. **Fix:** document the buffer-affinity invariant on `emit_call_absolute`, and assert `addr` is in the executable-page range against this buffer's `as_ptr()..as_ptr()+capacity`.

## 6. [HIGH] Outline for >4-arg JIT call stack-arg setup (wave-2 explicit TODO)

**Where:** finding #1's fix path + `vm/src/jit/helpers.rs:163` (`call_jit_compiled_method_entry` register_limit)

Plan: at the direct-call site, after loading the first `ARG_REGS.len() - (ctx ? 1 : 0)` args into registers, write each remaining arg to `[RSP + shadow + (k-N)*8]` (Windows: shadow=32, args 5+ at RSP+32, RSP+40, …; SysV: shadow=0, args 7+ at RSP+0, RSP+8, …). Increase `frame_size` by `max((extra_args*8 + 8) & !15, 0)` per-method (track `max_outgoing_stack_args`). Mirror in `call_jit_compiled_method_entry`'s transmuted call tables (add `n=5..=8` arms). This removes the wave-2 `bail_to_interpreter` slow path for all but exotic ≥9-arg signatures.

## 7. [HIGH] Inline FS:[off] / GS:[off] thread-pointer load is feasible via startup probe

**Where:** `jit/src/x64.rs:5653-5680` (the wave-2 TODO comment), `vm/src/jit/helpers.rs:29` (`JIT_THREAD` thread_local)

Approach the wave-2 comment dismissed is tractable: at JIT startup, the helper `jit_get_current_thread` runs once, reads `let probe: u64; asm!("mov {}, fs:[0]", out(reg) probe);` (Linux/FreeBSD) or `gs:[0x30]` (Win TEB self), and returns `(&raw const JIT_THREAD as u64) - probe` as a signed displacement. Stash the i32 in a new `JitRuntimeHelpers::jit_thread_tls_disp32` field. JIT replaces the `CALL get_current_thread; TEST RAX,RAX; JE slow` with `64 4C 8B 14 25 <disp32>` (`MOV R10, FS:[disp]`) on Linux (9 bytes, zero branches) — falling back to the helper call when `jit_thread_tls_disp32 == 0` (probe failed / Windows pre-TLS-alloc). Saves ~5 ns per `new`.

## 8. [HIGH] OSR trampoline still emits frame-slot store for every local (round-4 #11, unfixed)

**Where:** `jit/src/lib.rs:884-927`

Lines 894-895 emit `MOV [rbp - (i+1)*8], RAX` unconditionally; lines 897-909 then *also* move into `dst_reg` when one exists. For a method with 8 register-resident locals, that's 8×7 = 56 wasted bytes per trampoline plus an L1d store the JIT body never reads. **Fix:** wrap lines 894-895 in `if dst_reg_opt.is_none() && xmm_opt.is_none()`.

## 9. [MED] Register-resident oops invisible to precise oop map at safepoints

**Where:** `jit/src/x64.rs:3431-3445` (`emit_oop_map_for_safepoint`)

Only `StackSlot::Frame` oops are recorded. Java locals assigned to a callee-saved GPR (R12–R15) or stack-slots of kind `StackSlot::CalleeSaved(r)` are missing. Conservative scan covers this **only** because Rust helpers preserve callee-saved regs (saving them in *their* frame), but if the runtime ever moves to strict-precise mode (or LTO inlines the helper into a leaf that omits the save), oops are lost. **Fix:** for each `r` in `alloc_used_regs` whose live-range covers this PC and whose abstract type is oop, push `callee_saved_base + (idx_of(r))*8` to `frame_slot_offsets` — register IS spilled to that slot in the prologue, but the slot holds the *caller's* value, not the current one. Real fix needs a register-kind table in the oop map (separate `reg_oops: Vec<u8>` field) and runtime stack-walker support that knows callee-saveds live in the *callee* frame, not ours.

## 10. [MED] PUSH/POP callee-saved still bypassed (round-4 #6, unfixed; OSR-coupling claim revisited)

**Where:** `jit/src/x64.rs:5060-5075` (prologue), `5141-5155` (epilogue), `lib.rs:884-927` (OSR trampoline)

Bail reason in wave 1 was "OSR coupling" — but the trampoline writes to `[rbp - (i+1)*8]` (local area), not the callee-saved area. The callee-saved-base offset is computed from `frame_size` and is independent of how callee-saveds got onto the stack. **Decoupling path:** keep the explicit `callee_saved_base` field, but emit `PUSH reg` per callee-saved before `SUB RSP, locals_size`; the prologue still tracks each reg's saved-slot offset (= `-(8*(idx+1))` from RBP after `PUSH RBP; MOV RBP,RSP; PUSH r12; PUSH r13 …`). OSR trampoline doesn't need changes because it never touches the callee-saved slots. Saves ~30 bytes per method, frees RAX in epilogue (removes the R11 routing at line 5151-5154).

## 11. [MED] GVN map keys on raw hash — first-seen-wins on collision, no chaining

**Where:** `jit/src/ir_optimize.rs:347-369`

`FxHashMap<u64, NodeId>` stores a single node per hash. On any hash collision (rare for FxHasher on small (op, ty, inputs) keys, but non-zero), the second node fails the equality check and is **silently kept un-deduplicated** — and the third+ identical nodes never even see each other because they keep colliding against the first. **Fix:** key on a proper composite (`FxHashMap<(Op, IrType, SmallVec<[NodeId; 4]>), NodeId>`), or use the hash only as a bucket key with `Vec<NodeId>` values and full linear search on collision. Today's behavior is a missed-optimization, not a soundness bug.

## 12. [LOW] Per-bytecode `env::var_os` lookups still on hot path (round-4 #9, partial — `x64.rs:10632`)

**Where:** `jit/src/x64.rs:10632`, `11477` *(still present)*, `11508`, `11526`

Wave 2 left the `std::env::var_os("CRATONVM_DBG_JIT_GEN").is_some()` calls in the per-invoke emission body. Compile-time only, but a 1k-method megamorphic dispatch still acquires the env mutex 1k+ times. **Fix:** cache `let dbg = std::env::var_os(…).is_some();` once at the top of `compile_bytecode`.

---

## Summary

| Severity | Count |
|----------|-------|
| CRIT     | 3     |
| HIGH     | 5     |
| MED      | 3     |
| LOW      | 1     |

The two correctness regressions from round-4 wave-2 (findings #1, #2) should land first — #1 corrupts every multi-arg direct-call site on Windows, #2 elides JVMS-required NPE checks. #3 is latent today but a footgun for any ARG_REGS evolution.
