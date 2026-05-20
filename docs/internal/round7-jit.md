# Round 7 JIT Review

`jit/` crate. Round-6 wave-1 audit + remaining gaps + new angles.

## 1. [CRIT] `compile_bytecode` bail not propagated → recompile every 2000 calls

**Where:** `x64.rs:10380,10422,10583,10665` (`return false`) → `lib.rs:2521` → `interpreter.rs:10641,11642,11682,11947`

Trigger fires `try_compile` on `invoc % 2000 == 0`. On bail, `try_compile→None`; neither `compilation_complete` nor `on_c2_bailout` (`tiered.rs:489,537`, test-only) records failure. Every 2k calls JIT redoes bytecode walk, IR, info-vec allocs, escape_analysis, null_check_elim, codegen up to overflow — discarded. **Fix:** call `on_c2_bailout(&key)` in `try_compile→None` arms + `compile_blocked: AtomicBool` in `MethodState` checked by `should_compile`.

## 2. [CRIT] ProfileStore `name_index` returns wrong profile on hash collision

**Where:** `profile.rs:344-366`. `get_or_insert_borrowed` keys `name_index: FxHashMap<u64, Arc<…>>` by SipHash and returns slot on hit **without re-verifying** `(class_id, name, desc)`. Colliding methods share one `MethodProfile` — receiver/branch/trip counts mix; PIC inlines wrong target. P≈2.7e-10 at 100k methods. **Fix:** store `(MethodKey, Arc)` in `name_index`, verify triple, fall through to `methods.read().get(&key)` on mismatch.

## 3. [CRIT] `null_check_elim::analyze` still runs every compile despite disabled consumer

**Where:** `x64.rs:11856` (call), `x64.rs:3319-3335` (predicate=false). Producer still runs, allocating `Vec<u64>` of `code_len` and executing all transfer ops — output never read. ~30µs / ~32KB per 4KB method. **Fix:** replace call site with `NullCheckInfo::default()` until rewrite (#12).

## 4. [HIGH] PIC slot-0 debug_assert on `ARG_REGS[1]` — verified correct

**Where:** `x64.rs:10914-10929`. Targets `ARG_REGS[1]` (receiver: RDX/RSI), matching `emit_load_local(ARG_REGS[j+1],…)` at 10902 (j=0=receiver). Sound. **No fix.**

## 5. [HIGH] OSR trampoline unconditional frame-slot store (round-4 #11 / round-5 #8 unfixed)

**Where:** `lib.rs:884-927`. Lines 887-895 emit `MOV [rbp-(i+1)*8],RAX` for every local regardless of register/XMM assignment; 897-925 also copy to reg. **Fix:** wrap 893-895 in `if dst_reg_opt.is_none() && xmm_opt.is_none()`.

## 6. [HIGH] Code cache: no pooling, no LRU, no coalescing

**Where:** `lib.rs:185-198`, `platform.rs:69-100`, `lib.rs:1675-1830`. Each method = own `VirtualAlloc`; Win 64KB granularity wastes 59KB per 5KB body — 50k methods → 3GB. `JitCache`/arenas unbounded; per-method `VirtualFree` = TLB shootdown. **Fix:** slab — reserve 256MB, 64B-aligned blocks, free-list+coalescing; cap `JitCache` ~5k LRU.

## 7. [HIGH] No CMOV anywhere — every small-diff select is a branch

**Where:** grep `0x0F, 0x4` in `x64.rs` → 0 hits. **Fix:** detect `if_icmpXX` + `iconst/sipush; goto; iconst/sipush` → emit `CMP; CMOVcc EAX,ECX`.

## 8. [HIGH] `emit_imul_const` misses 16/32/64/128+ powers of 2

**Where:** `x64.rs:4217-4262`. Only 0,1,-1,2,3,4,5,8,9; 16+ → `IMUL imm32` vs `SHL k`. **Fix:** before match, `if val>0 && val.is_power_of_two() { emit SHL EAX, val.trailing_zeros() as u8; return; }`.

## 9. [HIGH] No LICM for `getfield`/`getstatic` on loop-invariant base

**Where:** `x64.rs:2142-2208` (aaload only), `:2210-2276` (FP only). Iterator-style `n=n.next`, `i<this.size` reload every iter. **Fix:** extend `find_loop_hoists` to `aload L; getfield F` where `L∉modified_locals` and `F` non-volatile; hoist to frame slot/callee-saved.

## 10. [MED] 24 redundant `MOVSXD RAX,EAX` after every 32-bit op

**Where:** `x64.rs:4037..4287` (24 sites). Widens even when next bytecode re-truncates. **Fix:** `last_eax_needs_sext` flag; MOVSXD lazily on first `pop_to_rax` in i64 context.

## 11. [MED] Precise oop map omits callee-saved regs (round-4 #15 / round-5 #9 unfixed)

**Where:** `x64.rs:3423-3465`. Only `StackSlot::Frame`; R12-R15 oops invisible to precise walker. **Fix:** add `reg_oops: Vec<(u8,i16)>` to `OopMapEntry`; push for live-oop callee-saveds; walker reads from *callee's* saved-area.

## 12. [MED] Meet-over-paths null-check design (round-6 TODO)

**Where:** `null_check_elim.rs`. BBs at branch targets; `pred` graph. Per-BB IN/OUT `u64`; meet=AND across preds; transfer forward in BB. Worklist; entry IN=sig-params; O(blocks×⌈locals/64⌉) (height ≤64). Per-PC mask via replay from IN.

## 13. [MED] Stack-arg setup for >4-arg direct calls (round-6 TODO)

**Where:** bails `x64.rs:10380,10422,10583,10665`. After regs filled, `k in reg_limit..len`: Win `MOV [RSP+32+(k-reg_limit)*8],…` (32B shadow mandatory); SysV `MOV [RSP+(k-reg_limit)*8],…`. Track `max_outgoing_stack_args`; prologue adds `((max_out*8+shadow+8)+15)&!15` to `frame_size`. Sibling-tail: force non-tail if caller's max_out < callee's.

---

CRIT=3, HIGH=5, MED=4, info=1. #1/#2/#3 = round-6 regressions — wave-1. #6/#9 = highest-leverage perf. #4 confirms round-6 PIC fix.
