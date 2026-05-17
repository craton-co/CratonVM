# Round 8 JIT Review

`jit/` crate. Round-7 wave 1+2 audit + remaining gaps + new angles.

## 1. [CRIT-VERIFIED] SATB pre-write barrier passes OLD value

**Where:** `x64.rs:8737-8771` (aastore). `load RAX=array, RCX=index` -> `bounds_check` -> `emit_ref_aload_regs` (RAX := OLD ref) -> `mov ARG_REGS[0],heap` -> `mov ARG_REGS[1], RAX` -> CALL -> reload + inline store. Inline aload precedes inline store; ARG_REGS[1]=OLD. Comment at 8758 is stale. **Fix (LOW):** retitle.

## 2. [CRIT-VERIFIED] Direct-call >ARG_REGS bail aborts cleanly

**Where:** `x64.rs:10518, 10564, 10724` (sibling-tail, direct, self-recursive). Each `return false` fires BEFORE any code emission (only simulated-stack pops happened). `compile_bytecode→false` propagates to `compile()→None` at `x64.rs:12201-12203`; the entire `Compiler` (incl. `self.buf`) is dropped. **No half-emitted CompiledMethod escapes.**

## 3. [VERIFIED-CORRECT] Audit cluster (no fix needed)

- **PIC slot-0 mod=00 assert** `x64.rs:11071-11075` — `ARG_REGS[1]` is the receiver (Win64 RDX(2) / SysV RSI(6)); low3 neither 4 (SIB) nor 5 (RIP). Sound.
- **`emit_imul_const` SHL fast-path** `x64.rs:4354-4360` — `val>0 && (val as u32).is_power_of_two()` -> `k∈[4,30]` (1<<31=i32::MIN excluded). `SHL EAX,imm8` valid 0..=31. `EAX<<k == (EAX*2^k) mod 2^32`; MOVSXD matches Java `imul` for ALL inputs (comment at 4357 wrongly restricts to non-negative).
- **CMOV helpers REX/opcode** `x64.rs:3927-3975` — `REX.W | R(dst≥8) | B(src≥8)` + `0F cc` + ModRM(11,dst,src) matches Intel SDM `0F 4x /r`. Sound but no caller (#9).
- **`JIT_BAIL_LIST` key** `lib.rs:2141-2163` — `compute_jit_key_hash` (1668) same hash as JitCache get/insert/remove (1780/1799/1827).
- **`null_check_elim::analyze`** `null_check_elim.rs:76-89` — returns empty `NullCheckInfo`; `is_nonnull` short-circuits on `pc≥masks.len()`(=0). Safe.
- **ProfileStore `name_index` collision check** `profile.rs:411-421` — match arm requires class_id+name+desc eq; mismatch falls through to `methods.write()`; counter at 432-435.
- **`ir_lower` disp range** `ir_lower.rs:122, 153, 166, 179` — all use `(i8::MIN as i32..=i8::MAX as i32)`. Correct.
- **`ir_optimize::gvn` FxHashMap determinism** — node iteration is `0..len` (Vec order); FxHashMap only queried by hash, never iterated. Deterministic.

## 4. [LOW] `emit_movq_*` off-by-one in disp8 range check

**Where:** `x64.rs:3659, 3686`. Callers pass positive `offset` (frame depth); fn negates for disp. Guard `(-128..=127).contains(&offset)` mis-rejects `offset==128` (disp `-128` fits i8). Wastes 3 bytes per spill at that depth. **Fix:** `if (1..=128).contains(&offset)`.

## 5. [HIGH] CMOV peephole unwired

`x64.rs:3927-3975` all `dead_code`. Every `if_icmpXX; iconst; goto; iconst` lowers to a branch. **Fix:** in `if_icmp*` match `[if_icmpXX +6, iconst, goto, iconst]` -> `CMP; MOV EAX,then; MOV ECX,else; CMOVcc EAX,ECX`.

## 6. [HIGH] Stack-arg setup for >ARG_REGS direct calls

TODO at `x64.rs:10509/10557/10717`. Win64: 32-byte shadow + `8*(args-4)` padded to 16; SysV: 16-aligned `8*(args-6)`, no shadow. Spill `arg_slots[reg_limit..]` to `[RSP+i*8]` after `SUB RSP,frame`; restore at return. Removes all three `return false` bails.

## 7. [HIGH] LICM / const-fold / DSE missing on live codegen

`ir_optimize.rs:32-100` folds on the dormant IR; `x64.rs:2142-2276` LICM only for aaload/FP. Live bytecode path emits `iadd` for `1+1`, no DSE for repeated frame stores, no LICM for non-array loads. **Fix:** const-fold peephole; per-block `last_store[slot]=pc` map; skip store if killed before read.

## 8. [MED] Fixed tiered thresholds; no PGO regalloc; no branch alignment

`tiered.rs:70-81` (200/5000/10000 hardcoded), `regalloc.rs:510-565` (static `use_count/degree`, ignores `ProfileStore`), no `.p2align` in `x64.rs`. **Fix:** env-driven `CompilationPolicy`; weight regalloc by `MethodProfile::trip_total`; NOP-pad to 16B before back-edge targets at patch time.

## 9. [MED] No test fixture for OSR reg-resident elision

`lib.rs:893-918`; `jit/tests/differential.rs` has 0 hits for `osr_trampoline`. Wave-2 elision unprotected against regression. **Fix:** test with 4 locals (2 reg-resident, 2 spilled), disassemble body, assert 2 frame-store MOVs not 4.

## 10. [MED] Callee-saved oop coverage at safepoints deferred

`x64.rs:3429-3454`. Same gap as round-4 #15 / round-5 #9 / round-7 #11. Relies on Rust helpers' prologue saves landing in conservative sweep — fragile under LTO inlining. **Fix:** per-local oop bit; pre-safepoint emit `MOV [RBP-(idx+1)*8], reg` for reg-resident oops; add `reg_oops` field to `OopMapEntry`.
