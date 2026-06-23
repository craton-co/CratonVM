# Round 9 JIT Review

`jit/` crate. Round-8 regression audit + remaining gaps.

## 1. [CRIT] Math.min/max CMOV opcodes inverted

**Where:** `x64.rs:10612-10619` (int), `10632-10639` (long).
After `CMP EAX, ECX` (op `0x39 0xC8`), flags = `a-b`; `L` = SF≠OF = `a<b`. Code uses `CMOVL EAX,ECX` for **min** — loads `b` when `a<b`, i.e. **max**. Symmetric for max (`CMOVG`). `Math.min(3,5)` → cmp(L) → cmovl → 5. Wrong. Author comment "fires when ECX < EAX" inverts the CMP. Zero test coverage in `jit/tests`.
**Fix:** swap — MIN→`0x4F` (CMOVG), MAX→`0x4C` (CMOVL). Add diff test.

## 2. [HIGH] NPE flag bypasses method exception table

**Where:** `interpreter.rs:12680-12684`, fed by `x64.rs:7603-7646` stub.
Stub sets `JIT_PENDING_NPE`, returns `i64::MIN`. Drain converts to `Err(NPE)` and propagates to caller — does **not** call `route_jit_exception_through_method` like `JIT_PENDING_EXCEPTION` does at 12657. `try { arr[0]=…; } catch (NPE) {…}` on null `arr` skips the local handler. Round-8 #1 traded `process::abort` for silent mis-routing.
**Fix:** build an NPE `ObjectRef` and route through `route_jit_exception_through_method(... usize::MAX, npe)`.

## 3. [HIGH] ProfileStore debug_assert fires on benign race

**Where:** `profile.rs:441-458`.
Probe-A misses fingerprint (`None`); another thread inserts our exact triple; probe-B sees matching entry; assert fires. The "absent → present" transition is the comment's own legal case but the assert forbids it. Crashes debug builds under concurrent first-touch.
**Fix:** drop the `debug_assert!`; only `if !equal(k) { counter += 1; }`.

## 4. [MED] `emit_movq_*_rbp_*` off-by-one survives round-8 fix

**Where:** `x64.rs:3679` (store), `3706` (load).
Round-8 #4 fixed `modrm_rbp_disp` to `(-127..=128)` but the two MOVQ helpers still guard `(-128..=127)`. Both encode `(-offset) as i8`. Wastes 3 bytes per XMM spill at depth 128.
**Fix:** widen both to `(-127..=128)`.

## 5. [MED] Null-store stub has no oop map for its CALL

**Where:** `x64.rs:7629`.
Stub omits `emit_oop_map_for_safepoint` before `CALL helpers.bastore`. Leaf-on-null today, but any future safepoint-poll edit makes simulated-stack oops invisible. Fragility class of round-8 #10.
**Fix:** add `self.emit_oop_map_for_safepoint();` before the call.

## 6. [LOW] CMOV intrinsic skips `flush_scratch_registers`

**Where:** `x64.rs:10594-10640` vs FMA at `10548-10593`.
FMA flushes, CMOV doesn't. Touches only RAX/RCX/flags so safe today, but inconsistent with siblings — fragile under regalloc churn.
**Fix:** add `flush_scratch_registers()` at head of both min/max arms.

## 7. [HIGH] Tiered + branch align + PGO regalloc still TODO

`tiered.rs:70-81` hardcodes 200/5000/10000; `regalloc.rs:510-565` ignores `ProfileStore`; no `.p2align` on back-edge targets. Round-8 #8 carried.
**Fix:** env-driven `CompilationPolicy`; weight regalloc by `MethodProfile::trip_total`; NOP-pad loop headers to 16B at patch time.

## 8. [HIGH] LICM / const-fold / DSE missing on live codegen

`ir_optimize.rs:32-100` is dormant IR; `x64.rs:1310+` LICM is aaload+FP only. No `getfield`/`getstatic` hoist, no `iadd 1,1` fold, no DSE for repeat-stores-to-same-local. Round-8 #7 carried.
**Fix:** after aaload pass, hoist invariant `getfield`/`getstatic` (receiver loop-invariant + field not stored in loop); per-block `last_store[slot]` for DSE.

## 9. [MED] Stack-arg setup for >ARG_REGS direct calls

`x64.rs:10509/10557/10717` bail `return false`. Round-8 #6 carried.
**Fix:** spill `args[reg_limit..]` to `[RSP+i*8]`; Win64 = 32B shadow + `8*(n-4)` pad-16; SysV = `8*(n-6)` pad-16.

## 10. [LOW] Asymmetric null-check: stores guarded, loads not

`helpers.rs:746-748` claims inline `TEST/JZ` is a guard, but round-8 added it only for the 8 store opcodes. iaload/aaload/.../saload still rely on page-fault through `MOV R10D, [RAX+12]` — same "false promise" the round-8 doc condemns.
**Fix:** mirror `emit_null_check_array_store` for loads (rename `_array_access`), feed the same stub.

## 11. [LOW] No intrinsic-correctness test fixture

`jit/tests/differential.rs` covers 0 intrinsics. With #1: every branchless intrinsic can ship inverted, undetected.
**Fix:** add `jit_intrinsic_correctness.rs` — JIT vs interpreter for min/max/abs/fma/sqrt; include `i32::MIN`/`i64::MIN`/`±NaN`.
