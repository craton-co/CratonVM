// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Invocations: `invokevirtual`, `invokespecial`, `invokestatic`, `invokeinterface` and `invokedynamic` in the single-pass backend's bytecode walk.
//!
//! Split out of `Compiler::compile_bytecode` (`bytecode_walk.rs`), which keeps
//! the walk loop and one dispatch `match` that routes each opcode to its
//! family (`jit-god-functions-and-request-side-channels-FIXED-20260912.md`).
//! The arms are the walk's own, moved unchanged except for how they leave the
//! walk: `continue` became `return WalkStep::Next(pc)` and `return x` became
//! `return WalkStep::Return(x)`.

use super::bytecode_walk::*;
use super::*;

impl Compiler {
    /// Lower one bytecode of this family at `pc`. The result is the pc the walk
    /// continues at, or the value `compile_bytecode` returns.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn walk_invoke(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        pc: usize,
        dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            0xb8 => self.walk_invokestatic(code, code_len, op, pc, dead, branch_targets),
            0xb6 | 0xb7 | 0xb9 => {
                self.walk_invoke_instance(code, code_len, op, pc, dead, branch_targets)
            }
            0xba => self.walk_invokedynamic(code, code_len, op, pc, dead, branch_targets),
            _ => {
                // `compile_bytecode` routed an opcode here that this family
                // does not lower: a dispatch table bug, refused rather than
                // emitted.
                self.fail("singlepass-codegen/walk-family-misdispatch");
                WalkStep::Return(false)
            }
        }
    }

    /// Lower `invokestatic`: a self-call, a direct call, an inlined body, an intrinsic or the dispatch helper at `pc`.
    #[allow(clippy::too_many_arguments)]
    fn walk_invokestatic(
        &mut self,
        code: &[u8],
        code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // invokestatic — self-call, direct call, inline, or dispatch helper
            0xb8 => {
                self.flush_scratch_registers();

                // TDigest's private quantile/cdf kernels use exactly:
                // Integer.valueOf(i) -> Function.apply(Object) ->
                // checkcast Double -> Double.doubleValue().  Scalarize that
                // erased adapter only in those private kernels, retaining the
                // generic invoke lowering for every other call site.
                let tdigest_numeric_kernel = self
                    .method_label
                    .starts_with("org/elasticsearch/tdigest/Dist.quantile")
                    || self
                        .method_label
                        .starts_with("org/elasticsearch/tdigest/Dist.cdf");
                if tdigest_numeric_kernel
                    && pc + 14 <= code.len()
                    && code[pc + 3] == 0xb9
                    && code[pc + 8] == 0xc0
                    && code[pc + 11] == 0xb6
                {
                    if self.stack.len() >= 2 {
                        // Keep the lambda receiver on the simulated stack
                        // through the safepoint so the oop map roots it.
                        let Some(&index_slot) = self.stack.last() else {
                            // The specialized pattern was recognized but
                            // its simulated stack no longer matches. Bail
                            // out of JIT compilation; the interpreter can
                            // execute the ordinary invoke path safely.
                            self.fail("singlepass-codegen/lambda-int-to-double-stack-shape");
                            return WalkStep::Return(false);
                        };
                        let lambda_slot = self.stack[self.stack.len() - 2];
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], lambda_slot);
                        self.load_slot_to_reg(ARG_REGS[2], index_slot);
                        self.emit_pre_safepoint_spill();
                        self.emit_call_absolute(self.helpers.lambda_int_to_double);
                        self.emit_oop_map_for_safepoint();
                        let _ = self.pop_stack();
                        let _ = self.pop_stack();
                        self.emit_post_invoke_exception_check(b'D');
                        self.push_from_rax_as_xmm0();
                        pc += 14;
                        return WalkStep::Next(pc);
                    }
                }

                // Check for invoke_info (fallback to jit_invoke_dispatch)
                let info_ptr = self
                    .invoke_info_idx
                    .get(&pc)
                    .map(|&i| self.invoke_info[i].1);

                // A GPU kernel call keeps its dispatch helper.
                //
                // The offload hook lives in `jit_invoke_dispatch`, and
                // this arm has TWO doors that never reach it: the
                // inliner (checked first, "most profitable") and this
                // backend's own direct-call map. Splicing the kernel's
                // body into the caller, or binding a raw CALL to its
                // compiled entry, both compile the caller and silently
                // end offload at that site -- which is what
                // `offload_jit_gate` used to refuse the whole method to
                // prevent.
                //
                // Read off the site's own `JitInvokeInfo`. No info means
                // this walk cannot name the callee, so it does not
                // filter: failing open leaves the site exactly as it was
                // before this existed.
                //
                // Unarmed (no `--gpu`) `is_kernel` is one relaxed bool,
                // so a CPU-only run pays a load per compiled
                // invokestatic SITE at compile time and nothing at all
                // at run time.
                let site_is_gpu_kernel = info_ptr.is_some_and(|ip| {
                    // SAFETY: `invoke_info` owns every pointer it hands
                    // out for the life of this compile.
                    let info = unsafe { &*(ip as *const crate::JitInvokeInfo) };
                    // `keeps_dispatch_helper`, not `is_kernel`: inlining is
                    // one-way, and a registry miss can mean "could not have
                    // known yet". See its AUDIT 2026-09-07 note.
                    info.invoke_kind == 3
                        && crate::offload_hook::keeps_dispatch_helper(
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        )
                });

                // Check for inline site first (most profitable)
                if !site_is_gpu_kernel && self.inline_sites.contains_key(&pc) {
                    if self.try_emit_inline(pc) {
                        pc += 3;
                        return WalkStep::Next(pc);
                    }
                }

                // Check for direct call target
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let direct = if site_is_gpu_kernel {
                    None
                } else {
                    self.direct_calls_idx.get(&pc).map(|&i| {
                        let dc = &self.direct_calls[i].1;
                        note_emit_direct(&self.method_key, pc, dc.entry);
                        (dc.entry, dc.needs_context, dc.num_params, dc.return_type)
                    })
                };

                let direct = direct.filter(|(entry, _, _, _)| {
                    if *entry != crate::JitIntrinsic::ArraycopyPrimitive.as_entry() {
                        return true;
                    }
                    !self
                        .despec
                        .as_ref()
                        .is_some_and(|registry| registry.contains(&self.method_key, pc as u32))
                });
                // E27-1 N2b: `indexOf(I)` is intrinsified ONLY where the
                // needle is a compile-time constant in `0..=0xFFFF`, which
                // is the range on which the inline single-code-unit scan
                // and `code_point_needle` are the same function. Filtered
                // HERE, before the intrinsic ladder, so a declined site
                // takes the ordinary dispatch it takes today — the same
                // shape as the `ArraycopyPrimitive` despec filter above.
                // Deliberately NOT a runtime screen: see
                // `prev_insn_int_const` for why that would be a cliff.
                let direct = direct.filter(|(entry, _, _, _)| {
                    if *entry != crate::JitIntrinsic::StringIndexOfChar.as_entry() {
                        return true;
                    }
                    matches!(prev_insn_int_const(code, code_len, pc), Some(0..=0xFFFF))
                });

                if let Some((callee_entry, callee_needs_ctx, callee_params, ret_type)) = direct {
                    if callee_entry == crate::MATH_SQRT_INTRINSIC {
                        // Math.sqrt(double) intrinsic: inline SQRTSD — no call overhead
                        let arg_slot = self.pop_stack();
                        // Flush any OTHER Xmm(0) slots that would be clobbered by SQRTSD
                        // (the arg itself is fine — it's consumed)
                        self.flush_xmm0_slots();
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    // MOVSD XMM0, XMMn
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                    }
                                }
                                // else: already in XMM0
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                self.emit_movq_xmm_from_rax(0);
                            }
                        }
                        self.emit_sqrtsd_xmm0();
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_FLOOR_INTRINSIC
                        || callee_entry == crate::MATH_CEIL_INTRINSIC
                        || callee_entry == crate::MATH_RINT_INTRINSIC
                    {
                        // Math.floor/ceil/rint intrinsic: ROUNDSD XMM0, XMM0, imm8
                        let arg_slot = self.pop_stack();
                        self.flush_xmm0_slots();
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                    }
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                self.emit_movq_xmm_from_rax(0);
                            }
                        }
                        // ROUNDSD XMM0, XMM0, imm8
                        // Encoding: 66 0F 3A 0B C0 imm8
                        let imm8 = if callee_entry == crate::MATH_FLOOR_INTRINSIC {
                            0x09u8 // round toward -inf, inexact suppress
                        } else if callee_entry == crate::MATH_CEIL_INTRINSIC {
                            0x0Au8 // round toward +inf, inexact suppress
                        } else {
                            0x08u8 // round to nearest even, inexact suppress
                        };
                        self.buf.emit(&[0x66, 0x0F, 0x3A, 0x0B, 0xC0, imm8]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_ABS_DOUBLE_INTRINSIC {
                        // Math.abs(double): clear sign bit (bit 63)
                        let arg_slot = self.pop_stack();
                        self.flush_xmm0_slots();
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                    }
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                self.emit_movq_xmm_from_rax(0);
                            }
                        }
                        // Load sign mask 0x7FFFFFFFFFFFFFFF into RCX, then MOVQ XMM1, RCX, ANDPD XMM0, XMM1
                        // MOV RCX, imm64
                        self.buf.emit_byte(0x48); // REX.W
                        self.buf.emit_byte(0xB9); // MOV RCX, imm64
                        self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
                        // MOVQ XMM1, RCX
                        self.emit_movq_xmm_from_gpr(1, RCX);
                        // ANDPD XMM0, XMM1: 66 0F 54 C1
                        self.buf.emit(&[0x66, 0x0F, 0x54, 0xC1]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_ABS_FLOAT_INTRINSIC {
                        // Math.abs(float): clear sign bit (bit 31)
                        let arg_slot = self.pop_stack();
                        self.flush_xmm0_slots();
                        match arg_slot {
                            StackSlot::Xmm(xmm) => {
                                if xmm != 0 {
                                    let modrm = 0xC0 | (xmm & 7);
                                    if xmm >= 8 {
                                        self.buf.emit(&[0xF3, 0x41, 0x0F, 0x10, modrm]);
                                    } else {
                                        self.buf.emit(&[0xF3, 0x0F, 0x10, modrm]);
                                    }
                                }
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, arg_slot);
                                // MOVD XMM0, EAX: 66 0F 6E C0
                                self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                            }
                        }
                        // Load sign mask 0x7FFFFFFF into ECX, MOVD XMM1, ECX, ANDPS XMM0, XMM1
                        // MOV ECX, imm32
                        self.buf.emit_byte(0xB9);
                        self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
                        // MOVD XMM1, ECX: 66 0F 6E C9
                        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]);
                        // ANDPS XMM0, XMM1: 0F 54 C1
                        self.buf.emit(&[0x0F, 0x54, 0xC1]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::MATH_ABS_INT_INTRINSIC {
                        // Math.abs(int): branchless absolute value
                        let arg_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg_slot);
                        // MOV ECX, EAX: 89 C1
                        self.buf.emit(&[0x89, 0xC1]);
                        // SAR EAX, 31 (sign-extend to all bits): C1 F8 1F
                        self.buf.emit(&[0xC1, 0xF8, 0x1F]);
                        // XOR ECX, EAX: 31 C1
                        self.buf.emit(&[0x31, 0xC1]);
                        // SUB ECX, EAX: 29 C1
                        self.buf.emit(&[0x29, 0xC1]);
                        // MOV EAX, ECX: 89 C8
                        self.buf.emit(&[0x89, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::MATH_ABS_LONG_INTRINSIC {
                        // Math.abs(long): branchless 64-bit absolute value
                        let arg_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg_slot);
                        // MOV RCX, RAX: 48 89 C1
                        self.buf.emit(&[0x48, 0x89, 0xC1]);
                        // SAR RAX, 63: 48 C1 F8 3F
                        self.buf.emit(&[0x48, 0xC1, 0xF8, 0x3F]);
                        // XOR RCX, RAX: 48 31 C1
                        self.buf.emit(&[0x48, 0x31, 0xC1]);
                        // SUB RCX, RAX: 48 29 C1
                        self.buf.emit(&[0x48, 0x29, 0xC1]);
                        // MOV RAX, RCX: 48 89 C8
                        self.buf.emit(&[0x48, 0x89, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::MATH_FMA_DOUBLE_INTRINSIC {
                        // T1.1.28 — Math.fma(double, double, double).
                        //
                        // Per JLS, `Math.fma(a, b, c)` computes `a*b + c`
                        // as if with unlimited intermediate precision and
                        // then rounded once. We lower to a direct call
                        // into `jit_math_fma_double`, which delegates to
                        // Rust's `f64::mul_add` — that maps to
                        // `VFMADD231SD` on FMA3-capable hosts and to a
                        // correctly-rounded software fused operation on
                        // everything else. Either path satisfies the JLS
                        // single-rounding requirement.
                        //
                        // extern "C" fn(f64, f64, f64) -> f64 — arguments
                        // pass in XMM0, XMM1, XMM2 on both SysV and Win64.
                        self.flush_scratch_registers();
                        let c_slot = self.pop_stack();
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        // Load a into RAX then → XMM0.
                        self.load_slot_to_reg(RAX, a_slot);
                        self.emit_movq_xmm_from_rax(0);
                        self.load_slot_to_reg(RAX, b_slot);
                        self.emit_movq_xmm_from_rax(1);
                        self.load_slot_to_reg(RAX, c_slot);
                        self.emit_movq_xmm_from_rax(2);
                        self.emit_call_absolute(self.helpers.math_fma_double);
                        // Result in XMM0 → move to RAX and push as FP.
                        self.emit_movq_rax_from_xmm(0);
                        self.push_from_rax_as_xmm0();
                    } else if callee_entry == crate::MATH_FMA_FLOAT_INTRINSIC {
                        // T1.1.28 — Math.fma(float, float, float) — same
                        // plan via `jit_math_fma_float` / `f32::mul_add`.
                        self.flush_scratch_registers();
                        let c_slot = self.pop_stack();
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.emit_movq_xmm_from_rax(0);
                        self.load_slot_to_reg(RAX, b_slot);
                        self.emit_movq_xmm_from_rax(1);
                        self.load_slot_to_reg(RAX, c_slot);
                        self.emit_movq_xmm_from_rax(2);
                        self.emit_call_absolute(self.helpers.math_fma_float);
                        self.emit_movq_rax_from_xmm(0);
                        self.push_from_rax_as_xmm0();
                    } else if callee_entry == crate::MATH_MIN_INT_INTRINSIC
                        || callee_entry == crate::MATH_MAX_INT_INTRINSIC
                    {
                        // Round-8 Bug 8 — branchless Math.min(int,int) /
                        // Math.max(int,int) via CMOV. Pop b then a (a is
                        // the deeper operand, the leftmost arg in the JLS
                        // signature). After `CMP EAX, ECX` (a vs b):
                        //   * CMOVL EAX, ECX fires when `a < b` and
                        //     overwrites EAX (=a) with ECX (=b) — i.e.
                        //     keeps the LARGER value in EAX. This is MAX.
                        //   * CMOVG EAX, ECX fires when `a > b` and
                        //     overwrites EAX (=a) with ECX (=b) — i.e.
                        //     keeps the SMALLER value in EAX. This is MIN.
                        // Round-9 CRIT fix: the previous version had these
                        // two swapped, so `Math.min(3, 5)` returned 5 and
                        // `Math.max(3, 5)` returned 3.
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.load_slot_to_reg(RCX, b_slot);
                        // CMP EAX, ECX — sets flags for signed compare.
                        self.emit_cmp_r32_r32(RAX, RCX);
                        let cc = if callee_entry == crate::MATH_MIN_INT_INTRINSIC {
                            0x4Fu8 // CMOVG — if a > b, replace a with b (keep smaller)
                        } else {
                            0x4Cu8 // CMOVL — if a < b, replace a with b (keep larger)
                        };
                        // CMOVcc EAX, ECX (32-bit, no REX.W): 0F 4c C1
                        self.buf.emit(&[0x0F, cc, 0xC1]);
                        // The 32-bit CMOV zero-extends the selected value
                        // into the upper 32 bits of RAX. The JIT's value
                        // ABI keeps `int`s sign-extended to 64 bits (see
                        // i2b/i2s/i2l, which all MOVSXD to 64-bit), so a
                        // negative result such as `Math.min(-7, 4)` must
                        // be re-extended or it surfaces as a large
                        // positive (`-7` → `0xFFFFFFF9`). MOVSXD RAX, EAX.
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::MATH_MIN_LONG_INTRINSIC
                        || callee_entry == crate::MATH_MAX_LONG_INTRINSIC
                    {
                        // Round-8 Bug 8 — 64-bit Math.min(long,long) /
                        // Math.max(long,long) via REX.W CMP + CMOV. Same
                        // semantics as the int variants but 64-bit.
                        // Round-9 CRIT fix: opcodes were swapped (see int
                        // variant above for the full rationale).
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.load_slot_to_reg(RCX, b_slot);
                        // CMP RAX, RCX (REX.W): 48 39 C8
                        self.buf.emit(&[0x48, 0x39, 0xC8]);
                        let cc = if callee_entry == crate::MATH_MIN_LONG_INTRINSIC {
                            0x4Fu8 // CMOVG — if a > b, replace a with b (keep smaller)
                        } else {
                            0x4Cu8 // CMOVL — if a < b, replace a with b (keep larger)
                        };
                        // CMOVcc RAX, RCX (REX.W): 48 0F 4c C1
                        self.buf.emit(&[0x48, 0x0F, cc, 0xC1]);
                        self.push_from_rax();
                    } else if callee_entry == crate::MATH_MIN_FLOAT_INTRINSIC
                        || callee_entry == crate::MATH_MAX_FLOAT_INTRINSIC
                        || callee_entry == crate::MATH_MIN_DOUBLE_INTRINSIC
                        || callee_entry == crate::MATH_MAX_DOUBLE_INTRINSIC
                    {
                        // `Math.min`/`Math.max` for float and double.
                        //
                        // SSE's MINSS/MAXSS are NOT Math.min/Math.max.
                        // Per the SDM, `MINSS dst, src` returns `src`
                        // whenever both operands are zero or either is
                        // NaN. Java's javadoc requires the opposite in
                        // both cases:
                        //
                        //   * "If either value is NaN, then the result is
                        //     NaN" -- and the JDK body returns the NaN
                        //     ARGUMENT, whose payload bits are observable
                        //     through `Float.floatToRawIntBits`.
                        //   * "this method considers negative zero to be
                        //     strictly smaller than positive zero", so
                        //     min(+0.0f, -0.0f) is -0.0f whichever way
                        //     round the arguments come, and max is +0.0f.
                        //
                        // The sequence below gets both right. For `min`:
                        //
                        //     t1 = MINSS(a, b)      ; a<b ? a : b
                        //     t2 = MINSS(b, a)      ; b<a ? b : a
                        //     r  = t1 OR t2
                        //
                        // For ordered, unequal inputs t1 == t2 == the
                        // smaller value, so the OR is the identity. For
                        // +-0.0 the two MINSSs return the two DIFFERENT
                        // zeros, and OR-ing their bit patterns sets the
                        // sign bit iff either was -0.0 -- exactly "the
                        // result is negative zero whenever one of them
                        // is". `max` is the mirror image: MAXSS and AND,
                        // so the sign survives only when BOTH were -0.0.
                        //
                        // NaN is then patched with two never-taken
                        // branches rather than folded into the bitwise
                        // trick, because OR-ing a NaN with the other
                        // operand's bits yields *a* NaN but not *the* NaN
                        // Java returns. `Math.min(a, NaN)` returns the
                        // second argument (`a <= b ? a : b` in the JDK
                        // body is false when unordered) and
                        // `Math.min(NaN, b)` returns the first.
                        let is_double = callee_entry == crate::MATH_MIN_DOUBLE_INTRINSIC
                            || callee_entry == crate::MATH_MAX_DOUBLE_INTRINSIC;
                        let is_min = callee_entry == crate::MATH_MIN_FLOAT_INTRINSIC
                            || callee_entry == crate::MATH_MIN_DOUBLE_INTRINSIC;
                        self.flush_xmm0_slots();
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.emit_movq_xmm_from_rax(0); // XMM0 = a
                        self.load_slot_to_reg(RAX, b_slot);
                        self.emit_movq_xmm_from_rax(1); // XMM1 = b

                        // MOVAPS/MOVAPD XMM2 <- XMM0 (save `a`) and
                        // XMM3 <- XMM1 (save `b`) for the NaN fixups.
                        let movap: &[u8] = if is_double {
                            &[0x66, 0x0F, 0x28]
                        } else {
                            &[0x0F, 0x28]
                        };
                        self.buf.emit(movap);
                        self.buf.emit_byte(0xD0); // XMM2 <- XMM0
                        self.buf.emit(movap);
                        self.buf.emit_byte(0xD9); // XMM3 <- XMM1

                        // MIN/MAX SS/SD: XMM0 op= XMM1, then XMM1 op= XMM2.
                        let prefix = if is_double { 0xF2u8 } else { 0xF3u8 };
                        let op = if is_min { 0x5Du8 } else { 0x5Fu8 };
                        self.buf.emit(&[prefix, 0x0F, op, 0xC1]); // XMM0, XMM1
                        self.buf.emit(&[prefix, 0x0F, op, 0xCA]); // XMM1, XMM2

                        // ORPS/ORPD for min, ANDPS/ANDPD for max.
                        let bitop = if is_min { 0x56u8 } else { 0x54u8 };
                        if is_double {
                            self.buf.emit(&[0x66, 0x0F, bitop, 0xC1]);
                        } else {
                            self.buf.emit(&[0x0F, bitop, 0xC1]);
                        }

                        // NaN fixups. UCOMISS/UCOMISD sets PF when its
                        // operands are unordered, so comparing a register
                        // with itself tests "is this NaN".
                        let ucomis: &[u8] = if is_double {
                            &[0x66, 0x0F, 0x2E]
                        } else {
                            &[0x0F, 0x2E]
                        };
                        // UCOMIS XMM2, XMM2 -- is `a` NaN?
                        self.buf.emit(ucomis);
                        self.buf.emit_byte(0xD2);
                        // JNP .check_b (a is not NaN)
                        self.buf.emit(&[0x7B, 0x00]);
                        let jnp_check_b = self.buf.pos() - 1;
                        // MOVAP XMM0 <- XMM2: return `a` with its exact bits.
                        self.buf.emit(movap);
                        self.buf.emit_byte(0xC2);
                        // JMP .done
                        self.buf.emit(&[0xEB, 0x00]);
                        let jmp_done = self.buf.pos() - 1;

                        let check_b = self.buf.pos();
                        // UCOMIS XMM3, XMM3 -- is `b` NaN?
                        self.buf.emit(ucomis);
                        self.buf.emit_byte(0xDB);
                        // JNP .done (neither is NaN: keep the bitwise result)
                        self.buf.emit(&[0x7B, 0x00]);
                        let jnp_done = self.buf.pos() - 1;
                        // MOVAP XMM0 <- XMM3: return `b` with its exact bits.
                        self.buf.emit(movap);
                        self.buf.emit_byte(0xC3);

                        let done = self.buf.pos();
                        for (patch, target) in
                            [(jnp_check_b, check_b), (jmp_done, done), (jnp_done, done)]
                        {
                            // Cast: usize offsets to i64 for the rel8 patch math.
                            let rel = target as i64 - (patch as i64 + 1);
                            debug_assert!(
                                (-128..=127).contains(&rel),
                                "Math.min/max fp intrinsic rel8 out of range: {rel}"
                            );
                            Self::patch_rel8_or_bail(&mut self.buf, patch, rel);
                        }

                        self.stack_push(StackSlot::Xmm(0), is_double);
                    } else if callee_entry == crate::MATH_MULTIPLY_HIGH_INTRINSIC
                        || callee_entry == crate::MATH_UNSIGNED_MULTIPLY_HIGH_INTRINSIC
                    {
                        // Math.multiplyHigh(JJ)J / unsignedMultiplyHigh(JJ)J —
                        // high 64 bits of the 128-bit product. The hottest leaf
                        // in the SunEC P-256 Montgomery field multiply, called
                        // once per limb pair. One-operand `IMUL r64` (signed) /
                        // `MUL r64` (unsigned) compute RDX:RAX = RAX * r64; the
                        // high half lands in RDX. Multiplication is commutative,
                        // so operand order is irrelevant to the result.
                        //
                        // flush_scratch_registers() above already spilled every
                        // value-stack slot out of RAX/RCX/RDX (locals live only
                        // in callee-saved R12-R15/RBX/RSI/RDI, deferred-spill
                        // slots only in R8/R9), so clobbering RAX/RCX/RDX here is
                        // safe — same contract the Math.min/max long path relies
                        // on, extended to RDX.
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.load_slot_to_reg(RCX, b_slot);
                        // IMUL RCX (48 F7 E9) signed / MUL RCX (48 F7 E1) unsigned.
                        let modrm = if callee_entry == crate::MATH_MULTIPLY_HIGH_INTRINSIC {
                            0xE9u8 // /5 IMUL
                        } else {
                            0xE1u8 // /4 MUL
                        };
                        self.buf.emit(&[0x48, 0xF7, modrm]);
                        // MOV RAX, RDX (48 89 D0) — high half is the result.
                        self.buf.emit(&[0x48, 0x89, 0xD0]);
                        self.push_from_rax();
                    }
                    // --- invokestatic intrinsic family regions ---
                    // A follow-up agent for family <TAG> appends its
                    // codegen as `else if callee_entry ==
                    // crate::JitIntrinsic::Foo.as_entry() { ... }`
                    // strictly between that family's BEGIN/END markers.
                    // An empty region contributes nothing, so the
                    // `if let Some(...) = direct` chain stays valid.
                    //
                    // ===== INTRINSIC REGION BEGIN: INT_BITS =====
                    // java.lang.Integer bit-manipulation intrinsics
                    // (Phase 1a). Each pops `num_params` ints off the
                    // operand stack, computes into EAX and pushes the
                    // result. All emitted code is bit-identical to the
                    // JDK semantics (verified by intrinsic_int_bits.rs).
                    // --- FP_BITS: Double bit reinterpretation ---
                    //
                    // One `MOVQ` each. These replaced 361 million checked
                    // native-bridge crossings in one run of
                    // `PSquarePercentileTest`; see the FP_BITS region in
                    // `jit/src/lib.rs` for the census.
                    //
                    // RAW semantics come free: `MOVQ` moves all 64 bits,
                    // NaN payload included, which is exactly what
                    // `doubleToRawLongBits` is specified to return. The
                    // canonicalising `doubleToLongBits` is not matched by
                    // the resolver and so cannot reach here.
                    else if callee_entry == crate::JitIntrinsic::DoubleToRawLongBits.as_entry() {
                        // Double.doubleToRawLongBits(d): the argument's
                        // 64 bits, unchanged, as a long.
                        let arg = self.pop_stack();
                        match arg {
                            StackSlot::Xmm(xmm) => self.emit_movq_rax_from_xmm(xmm),
                            // A frame slot or GPR already holds the raw
                            // 64-bit pattern -- the JIT stores a double
                            // as its bits -- so this is a plain load and
                            // no XMM round trip is needed.
                            _ => self.load_slot_to_reg(RAX, arg),
                        }
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongBitsToDouble.as_entry() {
                        // Double.longBitsToDouble(bits): the mirror.
                        let arg = self.pop_stack();
                        self.flush_xmm0_slots();
                        self.load_slot_to_reg(RAX, arg);
                        self.emit_movq_xmm_from_rax(0);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else if callee_entry == crate::JitIntrinsic::IntBitCount.as_entry() {
                        // Integer.bitCount(i): POPCNT EAX, EAX. The
                        // matcher only registers this when has_popcnt()
                        // is true, so the instruction is always valid.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // POPCNT EAX, EAX: F3 0F B8 C0
                        self.buf.emit(&[0xF3, 0x0F, 0xB8, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::IntNumberOfLeadingZeros.as_entry()
                    {
                        // Integer.numberOfLeadingZeros(i): result is 32
                        // for input 0, else 31 - floor(log2(i)).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_lzcnt() {
                            // LZCNT EAX, EAX: F3 0F BD C0 — defined to
                            // return 32 for a zero input, matching JDK.
                            self.buf.emit(&[0xF3, 0x0F, 0xBD, 0xC0]);
                        } else {
                            // Fallback: BSR ECX, EAX gives the MSB index
                            // and sets ZF iff the input is zero. We pick
                            // ECX = -1 on a zero input so the subsequent
                            // `31 - ECX` formula yields 32.
                            //   MOV EDX, -1
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&(-1i32).to_le_bytes());
                            // BSR ECX, EAX: 0F BD C8 (ZF=1 if EAX==0)
                            self.buf.emit(&[0x0F, 0xBD, 0xC8]);
                            // CMOVZ ECX, EDX: 0F 44 CA (consumes BSR's ZF)
                            self.buf.emit(&[0x0F, 0x44, 0xCA]);
                            // MOV EAX, 31: B8 1F 00 00 00
                            self.buf.emit(&[0xB8]);
                            self.buf.emit(&31i32.to_le_bytes());
                            // SUB EAX, ECX: 29 C8  → EAX = 31 - index
                            self.buf.emit(&[0x29, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::IntNumberOfTrailingZeros.as_entry()
                    {
                        // Integer.numberOfTrailingZeros(i): result is 32
                        // for input 0, else the LSB index.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_bmi1() {
                            // TZCNT EAX, EAX: F3 0F BC C0 — defined to
                            // return 32 for a zero input, matching JDK.
                            self.buf.emit(&[0xF3, 0x0F, 0xBC, 0xC0]);
                        } else {
                            // Fallback: BSF ECX, EAX gives the LSB index
                            // and sets ZF iff the input is zero; pick 32
                            // for the zero case.
                            //   MOV EDX, 32
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&32i32.to_le_bytes());
                            // BSF ECX, EAX: 0F BC C8 (ZF=1 if EAX==0)
                            self.buf.emit(&[0x0F, 0xBC, 0xC8]);
                            // CMOVZ ECX, EDX: 0F 44 CA
                            self.buf.emit(&[0x0F, 0x44, 0xCA]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntReverseBytes.as_entry() {
                        // Integer.reverseBytes(i): BSWAP EAX (0F C8).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        self.buf.emit(&[0x0F, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntHighestOneBit.as_entry() {
                        // Integer.highestOneBit(i): the value with only
                        // the highest set bit of `i`, or 0 when i == 0.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // MOV EDX, EAX: 89 C2 — preserve the original.
                        self.buf.emit(&[0x89, 0xC2]);
                        // BSR ECX, EAX: 0F BD C8 — ECX = MSB index
                        // (undefined for input 0, handled below).
                        self.buf.emit(&[0x0F, 0xBD, 0xC8]);
                        // MOV EAX, 1: B8 01 00 00 00
                        self.buf.emit(&[0xB8]);
                        self.buf.emit(&1i32.to_le_bytes());
                        // SHL EAX, CL: D3 E0 — EAX = 1 << index.
                        self.buf.emit(&[0xD3, 0xE0]);
                        // TEST EDX, EDX: 85 D2 — set ZF iff input was 0.
                        self.buf.emit(&[0x85, 0xD2]);
                        // CMOVZ EAX, EDX: 0F 44 C2 — input 0 → result 0.
                        self.buf.emit(&[0x0F, 0x44, 0xC2]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntLowestOneBit.as_entry() {
                        // Integer.lowestOneBit(i): i & -i. Naturally
                        // yields 0 for input 0, matching the JDK.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // MOV ECX, EAX: 89 C1
                        self.buf.emit(&[0x89, 0xC1]);
                        // NEG EAX: F7 D8  → EAX = -i
                        self.buf.emit(&[0xF7, 0xD8]);
                        // AND EAX, ECX: 21 C8  → EAX = -i & i
                        self.buf.emit(&[0x21, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntReverse.as_entry() {
                        // Integer.reverse(i): reverse the bit order via
                        // the standard SWAR sequence. The JDK does five
                        // stages; the final two (swap byte pairs, then
                        // swap halves) are exactly BSWAP, so we emit
                        // three SWAR stages followed by BSWAP.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // One SWAR stage for (mask, shift):
                        //   MOV ECX, EAX
                        //   SHR EAX, shift
                        //   AND EAX, mask
                        //   AND ECX, mask
                        //   SHL ECX, shift
                        //   OR  EAX, ECX
                        for &(mask, shift) in
                            &[(0x5555_5555u32, 1u8), (0x3333_3333, 2), (0x0F0F_0F0F, 4)]
                        {
                            // MOV ECX, EAX: 89 C1
                            self.buf.emit(&[0x89, 0xC1]);
                            // SHR EAX, imm8: C1 E8 ib
                            self.buf.emit(&[0xC1, 0xE8, shift]);
                            // AND EAX, imm32: 25 id
                            self.buf.emit(&[0x25]);
                            self.buf.emit(&mask.to_le_bytes());
                            // AND ECX, imm32: 81 E1 id
                            self.buf.emit(&[0x81, 0xE1]);
                            self.buf.emit(&mask.to_le_bytes());
                            // SHL ECX, imm8: C1 E1 ib
                            self.buf.emit(&[0xC1, 0xE1, shift]);
                            // OR EAX, ECX: 09 C8
                            self.buf.emit(&[0x09, 0xC8]);
                        }
                        // BSWAP EAX: 0F C8 — swaps the four bytes, which
                        // completes the 8- and 16-bit reversal stages.
                        self.buf.emit(&[0x0F, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntCompare.as_entry() {
                        // Integer.compare(x, y): branchless (x>y)-(x<y).
                        // The stack holds x (deeper) then y.
                        let y = self.pop_stack();
                        let x = self.pop_stack();
                        self.load_slot_to_reg(RAX, x);
                        self.load_slot_to_reg(RCX, y);
                        // CMP EAX, ECX: 39 C8 — signed compare x vs y.
                        self.buf.emit(&[0x39, 0xC8]);
                        // SETG AL:  0F 9F C0 — AL = 1 if x > y.
                        self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                        // SETL DL:  0F 9C C2 — DL = 1 if x < y.
                        self.buf.emit(&[0x0F, 0x9C, 0xC2]);
                        // MOVZX EAX, AL: 0F B6 C0
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                        // MOVZX EDX, DL: 0F B6 D2
                        self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                        // SUB EAX, EDX: 29 D0 → -1, 0 or 1.
                        self.buf.emit(&[0x29, 0xD0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::IntRotateLeft.as_entry()
                        || callee_entry == crate::JitIntrinsic::IntRotateRight.as_entry()
                    {
                        // Integer.rotateLeft(i, distance) / rotateRight:
                        // ROL/ROR EAX, CL. x86 masks CL & 0x1f for a 32-bit
                        // rotate, byte-identical to the JDK (rotation mod 32),
                        // so no explicit distance masking is needed. Stack:
                        // i (deeper), distance (top).
                        let dist = self.pop_stack();
                        let val = self.pop_stack();
                        self.load_slot_to_reg(RAX, val);
                        self.load_slot_to_reg(RCX, dist);
                        // ROL EAX, CL: D3 /0 = D3 C0 ; ROR EAX, CL: D3 /1 = D3 C8
                        let modrm = if callee_entry == crate::JitIntrinsic::IntRotateLeft.as_entry()
                        {
                            0xC0u8
                        } else {
                            0xC8u8
                        };
                        self.buf.emit(&[0xD3, modrm]);
                        // MOVSXD RAX, EAX (48 63 C0): a 32-bit rotate may set
                        // the high bit (negative int); re-extend to the
                        // canonical sign-extended 64-bit int form the value
                        // ABI expects (mirrors the Math.min int path).
                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                        self.push_from_rax();
                    }
                    // ===== INTRINSIC REGION END: INT_BITS =====

                    // ===== INTRINSIC REGION BEGIN: LONG_BITS =====
                    // java.lang.Long bit ops (Phase 1b). Each long operand
                    // occupies one 64-bit JIT stack slot; load_slot_to_reg
                    // loads the full 64 bits. All instructions below are
                    // REX.W-prefixed (0x48) so they operate on the whole
                    // 64-bit value. bitCount/numberOfLeadingZeros/
                    // numberOfTrailingZeros return an int (0..=64) which
                    // is left in EAX with the upper 32 bits cleared.
                    else if callee_entry == crate::JitIntrinsic::LongBitCount.as_entry() {
                        // Long.bitCount(j): 64-bit POPCNT. Matcher gates
                        // this on has_popcnt(), so the instruction is
                        // always valid here. Result 0..=64 fits in EAX.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // POPCNT RAX, RAX: F3 48 0F B8 C0
                        self.buf.emit(&[0xF3, 0x48, 0x0F, 0xB8, 0xC0]);
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::LongNumberOfLeadingZeros.as_entry()
                    {
                        // Long.numberOfLeadingZeros(j).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_lzcnt() {
                            // LZCNT RAX, RAX: F3 48 0F BD C0 — defined to
                            // yield 64 for a zero input, matching the JDK.
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBD, 0xC0]);
                        } else {
                            // BSR fallback. BSR RCX, RAX sets ZF iff the
                            // source is zero and otherwise leaves the
                            // highest set-bit index (0..=63) in RCX.
                            //   nlz = 63 - index   (for a non-zero input)
                            //   nlz = 64           (for a zero input)
                            // 63 - index == index ^ 63 for index in 0..=63,
                            // computed with XOR so RAX is left untouched
                            // for the TEST that re-derives the zero case.
                            // BSR RCX, RAX: 48 0F BD C8
                            self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]);
                            // XOR ECX, 63: 83 F1 3F — ECX = 63 - index
                            // (garbage if input was 0; fixed up below).
                            self.buf.emit(&[0x83, 0xF1, 0x3F]);
                            // MOV EDX, 64: BA 40 00 00 00
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&64i32.to_le_bytes());
                            // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                            self.buf.emit(&[0x48, 0x85, 0xC0]);
                            // CMOVZ RCX, RDX: 48 0F 44 CA — input 0 → 64.
                            self.buf.emit(&[0x48, 0x0F, 0x44, 0xCA]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry
                        == crate::JitIntrinsic::LongNumberOfTrailingZeros.as_entry()
                    {
                        // Long.numberOfTrailingZeros(j).
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        if crate::x64::has_bmi1() {
                            // TZCNT RAX, RAX: F3 48 0F BC C0 — defined to
                            // yield 64 for a zero input, matching the JDK.
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBC, 0xC0]);
                        } else {
                            // BSF fallback. BSF RCX, RAX sets ZF iff the
                            // source is zero and otherwise leaves the
                            // lowest set-bit index (0..=63) in RCX, which
                            // is exactly ntz for a non-zero input. For a
                            // zero input the result must be 64.
                            // BSF RCX, RAX: 48 0F BC C8
                            self.buf.emit(&[0x48, 0x0F, 0xBC, 0xC8]);
                            // MOV EDX, 64: BA 40 00 00 00
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&64i32.to_le_bytes());
                            // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                            self.buf.emit(&[0x48, 0x85, 0xC0]);
                            // CMOVZ RCX, RDX: 48 0F 44 CA — input 0 → 64.
                            self.buf.emit(&[0x48, 0x0F, 0x44, 0xCA]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                        }
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongReverseBytes.as_entry() {
                        // Long.reverseBytes(j): 64-bit BSWAP.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // BSWAP RAX: 48 0F C8
                        self.buf.emit(&[0x48, 0x0F, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongHighestOneBit.as_entry() {
                        // Long.highestOneBit(j): 1L << bitIndex of the MSB,
                        // or 0 for a zero input. BSR leaves the index in
                        // RCX; SHL forms the mask; a CMOVZ keyed on the
                        // original input restores 0 for the zero case.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // BSR RCX, RAX: 48 0F BD C8 — RCX = MSB index.
                        self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]);
                        // MOV EDX, 1: BA 01 00 00 00 (RDX = 1, upper bits 0).
                        self.buf.emit(&[0xBA]);
                        self.buf.emit(&1i32.to_le_bytes());
                        // SHL RDX, CL: 48 D3 E2 — RDX = 1 << index
                        // (garbage if input was 0; fixed up below).
                        self.buf.emit(&[0x48, 0xD3, 0xE2]);
                        // XOR ECX, ECX: 31 C9 — RCX = 0 (zero-input result).
                        self.buf.emit(&[0x31, 0xC9]);
                        // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                        self.buf.emit(&[0x48, 0x85, 0xC0]);
                        // CMOVZ RDX, RCX: 48 0F 44 D1 — input 0 → 0.
                        self.buf.emit(&[0x48, 0x0F, 0x44, 0xD1]);
                        // MOV RAX, RDX: 48 89 D0
                        self.buf.emit(&[0x48, 0x89, 0xD0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongLowestOneBit.as_entry() {
                        // Long.lowestOneBit(j): j & -j. Naturally yields 0
                        // for a zero input, matching the JDK.
                        let arg = self.pop_stack();
                        self.load_slot_to_reg(RAX, arg);
                        // MOV RCX, RAX: 48 89 C1
                        self.buf.emit(&[0x48, 0x89, 0xC1]);
                        // NEG RCX: 48 F7 D9 — RCX = -j
                        self.buf.emit(&[0x48, 0xF7, 0xD9]);
                        // AND RAX, RCX: 48 21 C8 — RAX = j & -j
                        self.buf.emit(&[0x48, 0x21, 0xC8]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongCompare.as_entry() {
                        // Long.compare(x, y): branchless signed
                        // (x > y) - (x < y). The stack holds x (deeper)
                        // then y. SETcc reads the flags from CMP without
                        // disturbing them; the int result lands in EAX.
                        let y = self.pop_stack();
                        let x = self.pop_stack();
                        self.load_slot_to_reg(RAX, x);
                        self.load_slot_to_reg(RCX, y);
                        // CMP RAX, RCX: 48 39 C8 — signed 64-bit compare.
                        self.buf.emit(&[0x48, 0x39, 0xC8]);
                        // SETG AL:  0F 9F C0 — AL = 1 if x > y.
                        self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                        // SETL DL:  0F 9C C2 — DL = 1 if x < y.
                        self.buf.emit(&[0x0F, 0x9C, 0xC2]);
                        // MOVZX EAX, AL: 0F B6 C0
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                        // MOVZX EDX, DL: 0F B6 D2
                        self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                        // SUB EAX, EDX: 29 D0 → -1, 0 or 1.
                        self.buf.emit(&[0x29, 0xD0]);
                        self.push_from_rax();
                    } else if callee_entry == crate::JitIntrinsic::LongRotateLeft.as_entry()
                        || callee_entry == crate::JitIntrinsic::LongRotateRight.as_entry()
                    {
                        // Long.rotateLeft(i, distance) / rotateRight:
                        // ROL/ROR RAX, CL. x86 masks CL & 0x3f for a 64-bit
                        // rotate, byte-identical to the JDK (rotation mod 64).
                        // Descriptor is (JI)J: the long value (deeper) then
                        // the int distance (top), each one JIT stack slot.
                        let dist = self.pop_stack();
                        let val = self.pop_stack();
                        self.load_slot_to_reg(RAX, val);
                        self.load_slot_to_reg(RCX, dist);
                        // ROL RAX, CL: 48 D3 C0 ; ROR RAX, CL: 48 D3 C8.
                        let modrm =
                            if callee_entry == crate::JitIntrinsic::LongRotateLeft.as_entry() {
                                0xC0u8
                            } else {
                                0xC8u8
                            };
                        self.buf.emit(&[0x48, 0xD3, modrm]);
                        self.push_from_rax();
                    }
                    // ===== INTRINSIC REGION END: LONG_BITS =====

                    // ===== INTRINSIC REGION BEGIN: ARRAYCOPY =====
                    //
                    // De-spec guard: a call site whose src/dst are ALWAYS
                    // a reference-element array (e.g. `char[][]`) fails
                    // Guard 4 below on every single invocation, so this
                    // speculative fast path deopts every call. Each deopt
                    // evicts the artifact and triggers a background
                    // recompile that re-emits the SAME unconditional
                    // guard, so the method deopts forever — creating a
                    // continuous compile/evict race window. If the
                    // resume's epoch-staleness check ever loses that race
                    // (the live epoch was bumped by an in-flight
                    // recompile), the deopt falls back to the documented
                    // whole-method re-run, which DOUBLE-EXECUTES every
                    // side effect the method already performed before
                    // reaching this call (e.g. a `stack[ptr--]` decrement
                    // already committed to the heap) — the mechanism
                    // behind the JDT `Parser` stack-corruption bug
                    // (jasper-jdt-parser-arrayindexoutofbounds.md).
                    // `real_frame_deopt_resume_and_despeculate` already
                    // records this bci in the de-spec registry after
                    // `PER_BCI_DESPEC_LIMIT` deopts, exactly like the
                    // loop-header speculative-BCE guards (see
                    // `DespecRegistry::contains` above); this intrinsic just
                    // needs to honor it on recompile — bail to the generic
                    // (non-speculative) call dispatch below instead of
                    // re-emitting a guard proven to always fail.
                    else if callee_entry == crate::JitIntrinsic::ArraycopyPrimitive.as_entry()
                        && !self
                            .despec
                            .as_ref()
                            .is_some_and(|registry| registry.contains(&self.method_key, pc as u32))
                    {
                        // Phase 2 — System.arraycopy(src, srcPos, dst,
                        // dstPos, len). The descriptor is type-erased;
                        // the element kind is only known at runtime.
                        //
                        // Strategy (roadmap §3.3/§3.4): inline a
                        // primitive fast path. The emitted code performs
                        // a runtime dispatch — null checks, an
                        // is-array + same-primitive-element-kind check,
                        // and the five fused bounds checks of
                        // `native_system_arraycopy`. Whenever ANY guard
                        // is unsatisfied — null receiver, non-array,
                        // reference array, mismatched/incompatible
                        // element kinds, or an out-of-bounds position —
                        // control branches to the uncommon-trap deopt
                        // stub (DEOPT_REASON_BOUNDS_CHECK = 2). That
                        // re-runs the whole method in the interpreter,
                        // which dispatches `System.arraycopy` through
                        // the native registry. The native impl throws
                        // NullPointerException / ArrayStoreException /
                        // ArrayIndexOutOfBoundsException and applies the
                        // GC store barrier exactly — so correctness for
                        // every bailed case is delegated verbatim and
                        // reference arrays are never inlined.
                        //
                        // The proven-safe primitive case copies via
                        // `REP MOVSB` with memmove semantics: when src
                        // and dst are the SAME array and dstPos > srcPos
                        // the regions may overlap forward, so the copy
                        // runs backward (STD) in that case and forward
                        // (CLD) otherwise. Distinct arrays are distinct
                        // allocations and never overlap.
                        //
                        // Operand stack (deepest first): src, srcPos,
                        // dst, dstPos, len.
                        self.flush_scratch_registers();
                        // Step 6: snapshot the pre-pop JVM operand stack
                        // (src, srcPos, dst, dstPos, len) so a bail from
                        // any null/bounds guard resumes precisely at this
                        // invokestatic bci rather than re-running from entry.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::BoundsCheck,
                            );
                        }
                        let len_slot = self.pop_stack();
                        let dst_pos_slot = self.pop_stack();
                        let dst_slot = self.pop_stack();
                        let src_pos_slot = self.pop_stack();
                        let src_slot = self.pop_stack();

                        // Pin all five operands into owned frame scratch
                        // slots. After the five pops `next_spill_offset`
                        // sits below the slots the operands occupied, so
                        // these offsets are guaranteed in-frame (the
                        // call site had >=5 operand-stack entries, hence
                        // max_stack >= 5). They are scratch-only:
                        // arraycopy pushes nothing, so the next bytecode
                        // re-allocates spill slots from the same base.
                        //
                        // The base is pushed BELOW every operand's own
                        // frame home, so the five stores below cannot land
                        // on a slot one of them still lives in.
                        //
                        // The overlap is real and this is where it comes
                        // from: `pop_stack` reclaims a Frame slot that sits
                        // at the top of the spill region, so the five pops
                        // just above rewound `next_spill_offset` back OVER
                        // the very homes `flush_scratch_registers` spilled
                        // the oop operands into. Taking the base from
                        // `next_spill_offset` therefore aliases them by
                        // construction.
                        //
                        // The emitter used to work around that for its OWN
                        // reads only, by loading all five into distinct
                        // GPRs before storing any (kept below — it costs
                        // nothing and defends the ordering directly). But
                        // the aliasing has a SECOND consumer that the
                        // workaround does not reach: the deopt snapshot
                        // taken above names each operand's ORIGINAL frame
                        // home, and reads it when the bail actually fires —
                        // long after `s_src_pos`'s store has overwritten
                        // `dst`'s home with srcPos. A reference-array
                        // `arraycopy` (which always bails here, by design)
                        // then resumed in the interpreter with `Object(1)`
                        // — the literal srcPos — where `dst` belonged: not
                        // a plausible heap pointer, degraded to null by
                        // `CompactValue::to_value`, and `System.arraycopy`
                        // threw NullPointerException. `RMethodSiteCache`'s
                        // `mixedRefAndPrimitive` is the witness; the bogus
                        // payload tracks srcPos exactly (`srcPos=2` gives
                        // `Object(2)`).
                        //
                        // Removing the overlap fixes both consumers at
                        // once, which is why it is done here rather than by
                        // teaching the snapshot about the scratch homes.
                        let mut scratch_base = self.next_spill_offset;
                        for slot in [src_slot, src_pos_slot, dst_slot, dst_pos_slot, len_slot] {
                            if let crate::x64::StackSlot::Frame(off) = slot {
                                scratch_base = scratch_base.max(off.saturating_add(8));
                            }
                        }
                        if !self.spill_range_fits(scratch_base, 5) {
                            // Named here rather than inside the probe: this
                            // is the arraycopy intrinsic's five scratch
                            // homes, and a bail site of
                            // `spill-range-exhausted` said only that some
                            // range somewhere did not fit.
                            self.fail("singlepass-codegen/arraycopy-scratch-spill-exhausted");
                            return WalkStep::Return(false);
                        }
                        let s_src = scratch_base;
                        let s_src_pos = scratch_base + 8;
                        let s_dst = scratch_base + 16;
                        let s_dst_pos = scratch_base + 24;
                        let s_len = scratch_base + 32;
                        // ALIASING HAZARD: these scratch homes can overlap
                        // the operands' OWN frame homes. `flush_scratch_
                        // registers()` above spills any CalleeSaved *oop*
                        // operand (here src and dst) to frame slots taken
                        // from `next_spill_offset`; the five pops then rewind
                        // `next_spill_offset` back over those very slots, so
                        // e.g. `src_slot`/`dst_slot` may be `Frame(s_src)` /
                        // `Frame(s_src_pos)`. Writing the scratch homes in
                        // operand order would corrupt a not-yet-read operand:
                        // storing s_src_pos (=srcPos) overwrites dst's spilled
                        // home BEFORE s_dst reads it, leaving s_dst = srcPos
                        // (a small int) — guard-2 then bails to native when
                        // srcPos==0, or dereferences the bogus pointer and
                        // SIGSEGVs when srcPos!=0. Defeat the aliasing by
                        // loading ALL five operands into distinct scratch
                        // GPRs FIRST, then storing. RAX/RCX/RDX/R10/R11 are
                        // never local-mapped (LOCAL_REGS is RBX/R12..R15
                        // [+RSI/RDI on SysV]) and hold no deferred-Scratch
                        // value after the flush, so no load can clobber an
                        // operand still pending a read.
                        self.load_slot_to_reg(RAX, src_slot);
                        self.load_slot_to_reg(RCX, src_pos_slot);
                        self.load_slot_to_reg(RDX, dst_slot);
                        self.load_slot_to_reg(R10, dst_pos_slot);
                        self.load_slot_to_reg(R11, len_slot);
                        self.emit_store_local(s_src, RAX);
                        self.emit_store_local(s_src_pos, RCX);
                        self.emit_store_local(s_dst, RDX);
                        self.emit_store_local(s_dst_pos, R10);
                        self.emit_store_local(s_len, R11);

                        // Collect every "bail to native" branch patch
                        // here; they are all wired to one shared deopt
                        // stub (reason 2) after the inline body.
                        let mut bail_patches: Vec<usize> = Vec::new();

                        // --- Guard 1: src != null ---
                        self.emit_load_local(RAX, s_src);
                        self.emit_test_r64_r64(RAX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                        // --- Guard 2: dst != null ---
                        self.emit_load_local(RCX, s_dst);
                        self.emit_test_r64_r64(RCX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                        // --- Guards 3 and 4: both are arrays, of the SAME
                        // PRIMITIVE element kind ---
                        //
                        // `kind` and `element_type` are BOTH in the one
                        // byte at `KIND_TAGS_BYTE_OFFSET` — `kind` in bits
                        // 0..2 (`KIND_TAG_BYTE_MASK`), `element_type` in
                        // bits 2..6. They used to be separate bytes at
                        // offsets 4 and 5, and this code still read those
                        // two literals after the header shrank 24 -> 16 on
                        // 2026-08-07 and the quartet moved into the mark
                        // word.
                        //
                        // Offset 4 is now `shape` — an ARRAY'S LENGTH. The
                        // same function reads the length from that very
                        // offset (via `ARRAY_LENGTH_OFFSET`) forty lines
                        // below, so the "is this an array" guard was
                        // testing the length's low byte and the "element
                        // type" was the next length byte. That does not
                        // fail safe: for a length whose low byte is 1 and
                        // whose second byte is >= 4 — 1025 = 0x0401 — both
                        // tests PASS and the element width comes out as
                        // `1 << ((4 - 4) & 3)` = one byte. `arraycopy` on a
                        // `long[1025]` moved 1025 bytes instead of 8200 and
                        // returned normally: no exception, no crash, a
                        // silently truncated copy.
                        // `probes/ArraycopyHeaderOffsetProbe.java` is the
                        // repro — mismatch at index 128 for `long[1025]`
                        // and 256 for `int[1025]`, while 1024 / 300 / 257
                        // pass, which is why this hid.
                        //
                        // Read the constants, as every other header access
                        // in this file already does.
                        const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;

                        // MOVZX EDX, BYTE [RAX + KIND_TAGS]  (src tags)
                        self.buf.emit(&[0x0F, 0xB6, 0x50, KIND_TAGS]);
                        // MOV R10D, EDX — keep the whole byte; EDX is about
                        // to be masked down to the kind bits.
                        self.buf.emit(&[0x41, 0x89, 0xD2]);
                        // AND EDX, KIND_TAG_BYTE_MASK
                        self.buf
                            .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]);
                        // CMP EDX, ObjectKind::Array
                        self.buf
                            .emit(&[0x83, 0xFA, cratonvm_types::ObjectKind::Array as u8]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                        // MOVZX EDX, BYTE [RCX + KIND_TAGS]  (dst tags)
                        self.buf.emit(&[0x0F, 0xB6, 0x51, KIND_TAGS]);
                        // MOV R11D, EDX
                        self.buf.emit(&[0x41, 0x89, 0xD3]);
                        self.buf
                            .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]);
                        self.buf
                            .emit(&[0x83, 0xFA, cratonvm_types::ObjectKind::Array as u8]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                        // element_type = (tags >> 2) & 0xF. ArrayElementType:
                        // Reference=0, Boolean=4, Char=5, Float=6, Double=7,
                        // Byte=8, Short=9, Int=10, Long=11 — four bits.
                        // SHR R10D, 2 ; AND R10D, 0xF   (src)
                        self.buf.emit(&[0x41, 0xC1, 0xEA, 0x02]);
                        self.buf.emit(&[0x41, 0x83, 0xE2, 0x0F]);
                        // SHR R11D, 2 ; AND R11D, 0xF   (dst)
                        self.buf.emit(&[0x41, 0xC1, 0xEB, 0x02]);
                        self.buf.emit(&[0x41, 0x83, 0xE3, 0x0F]);
                        // CMP R10D, R11D → element kinds must be equal
                        self.buf.emit(&[0x45, 0x39, 0xDA]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                                            // CMP R10D, 4 → primitive kinds are 4..=11; a
                                                                            // value < 4 means Reference (0) — bail (the GC
                                                                            // store barrier / ArrayStoreException make
                                                                            // reference copies unsafe to inline).
                        self.buf.emit(&[0x41, 0x83, 0xFA, 0x04]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x82)); // JB (unsigned <)
                                                                            // MOV EDX, R10D — the shift math below operates
                                                                            // on EDX, as it did when EDX held the element
                                                                            // type directly.
                        self.buf.emit(&[0x44, 0x89, 0xD2]);

                        // shift = (element_type - 4) & 3, where
                        //   width == 1 << shift  for every primitive
                        //   kind (verified: 4→0,5→1,6→2,7→3,8→0,9→1,
                        //   10→2,11→3). Stash the shift in R11 (scratch,
                        //   untouched until the copy below).
                        // SUB EDX, 4
                        self.buf.emit(&[0x83, 0xEA, 0x04]);
                        // AND EDX, 3
                        self.buf.emit(&[0x83, 0xE2, 0x03]);
                        // MOV R11D, EDX
                        self.buf.emit(&[0x41, 0x89, 0xD3]);

                        // --- Guard 5: bounds checks ---
                        // Load srcPos, dstPos, len sign-extended to
                        // 64-bit so the pos+len additions cannot
                        // overflow.
                        // srcPos → RAX
                        self.emit_load_local(RAX, s_src_pos);
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                                                            // dstPos → RDX
                        self.emit_load_local(RDX, s_dst_pos);
                        self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
                                                            // len → RCX
                        self.emit_load_local(RCX, s_len);
                        self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX

                        // srcPos < 0 ?  TEST RAX,RAX; JS bail
                        self.emit_test_r64_r64(RAX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                            // dstPos < 0 ?  TEST RDX,RDX; JS bail
                        self.emit_test_r64_r64(RDX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                            // len < 0 ?  TEST RCX,RCX; JS bail
                        self.emit_test_r64_r64(RCX);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS

                        // srcPos + len > src.length ?
                        // R10 = srcPos + len
                        self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
                        self.buf.emit(&[0x49, 0x01, 0xCA]); // ADD R10, RCX
                                                            // RAX = src ptr; EAX = src.length (zero-extended,
                                                            // so the 64-bit value is non-negative).
                        self.emit_load_local(RAX, s_src);
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RAX+12]
                                                                                 // CMP R10, RAX  (srcPos+len vs src.length)
                        self.buf.emit(&[0x49, 0x39, 0xC2]);
                        bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG (signed >)

                        // dstPos + len > dst.length ?
                        self.buf.emit(&[0x49, 0x89, 0xD2]); // MOV R10, RDX
                        self.buf.emit(&[0x49, 0x01, 0xCA]); // ADD R10, RCX
                        self.emit_load_local(RAX, s_dst);
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RAX+12]
                        self.buf.emit(&[0x49, 0x39, 0xC2]); // CMP R10, RAX
                        bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG

                        // --- len == 0 fast exit ---
                        // All bounds are validated; an empty copy is a
                        // no-op. RCX still holds the sign-extended len.
                        self.emit_test_r64_r64(RCX);
                        let zero_len_skip = self.emit_jcc_rel32_patch(0x84); // JZ → done

                        // --- compute byte addresses & count ---
                        // Preserve RSI / RDI: on Windows they are
                        // callee-saved AND used by the local register
                        // allocator (LOCAL_REGS), so they may hold live
                        // locals. PUSH/POP brackets the REP MOVSB; no
                        // CALL occurs in between, so RSP stays balanced.
                        // PUSH RSI ; PUSH RDI
                        self.buf.emit(&[0x56, 0x57]);

                        // shift → CL
                        // MOV ECX, R11D
                        self.buf.emit(&[0x44, 0x89, 0xD9]);

                        // srcAddr = src + HEADER_SIZE + (srcPos << shift)
                        self.emit_load_local(RSI, s_src_pos);
                        self.buf.emit(&[0x48, 0x63, 0xF6]); // MOVSXD RSI, ESI
                        self.buf.emit(&[0x48, 0xD3, 0xE6]); // SHL RSI, CL
                        self.emit_load_local(RAX, s_src);
                        self.buf.emit(&[0x48, 0x01, 0xC6]); // ADD RSI, RAX
                                                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x48, 0x83, 0xC6, HEADER_SIZE as u8]); // ADD RSI, HEADER_SIZE

                        // dstAddr = dst + HEADER_SIZE + (dstPos << shift)
                        self.emit_load_local(RDI, s_dst_pos);
                        self.buf.emit(&[0x48, 0x63, 0xFF]); // MOVSXD RDI, EDI
                        self.buf.emit(&[0x48, 0xD3, 0xE7]); // SHL RDI, CL
                        self.emit_load_local(RAX, s_dst);
                        self.buf.emit(&[0x48, 0x01, 0xC7]); // ADD RDI, RAX
                                                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x48, 0x83, 0xC7, HEADER_SIZE as u8]); // ADD RDI, HEADER_SIZE

                        // byteCount = len << shift  → RDX (kept for the
                        // backward-copy adjust, then copied into RCX
                        // for REP).
                        self.emit_load_local(RDX, s_len);
                        self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
                        self.buf.emit(&[0x48, 0xD3, 0xE2]); // SHL RDX, CL

                        // --- direction select ---
                        // Overlap is possible only within the SAME
                        // array; distinct arrays are distinct heap
                        // allocations. Copy backward iff
                        //   src_ptr == dst_ptr  &&  dstPos > srcPos.
                        // Otherwise forward is always memmove-correct.
                        self.emit_load_local(RAX, s_src);
                        self.emit_load_local(RCX, s_dst);
                        // CMP RAX, RCX
                        self.buf.emit(&[0x48, 0x39, 0xC8]);
                        let fwd_if_diff = self.emit_jcc_rel32_patch(0x85); // JNE → forward
                                                                           // same array: compare dstPos vs srcPos (signed
                                                                           // 32-bit; both already validated >= 0).
                        self.emit_load_local(RAX, s_dst_pos);
                        self.emit_load_local(RCX, s_src_pos);
                        // CMP EAX, ECX  (dstPos vs srcPos)
                        self.buf.emit(&[0x39, 0xC8]);
                        let fwd_if_le = self.emit_jcc_rel32_patch(0x8E); // JLE → forward

                        // --- backward copy (STD) ---
                        // Point RSI/RDI at the LAST byte of each region:
                        //   addr += byteCount - 1.
                        // LEA RSI, [RSI + RDX - 1]
                        self.buf.emit(&[0x48, 0x8D, 0x74, 0x16, 0xFF]);
                        // LEA RDI, [RDI + RDX - 1]
                        self.buf.emit(&[0x48, 0x8D, 0x7C, 0x17, 0xFF]);
                        // MOV RCX, RDX  (byte count)
                        self.buf.emit(&[0x48, 0x89, 0xD1]);
                        // STD ; REP MOVSB ; CLD
                        self.buf.emit(&[0xFD, 0xF3, 0xA4, 0xFC]);
                        let backward_done = self.emit_jmp_rel32_patch();

                        // --- forward copy (CLD) ---
                        self.patch_rel32_to_here(fwd_if_diff);
                        self.patch_rel32_to_here(fwd_if_le);
                        // MOV RCX, RDX  (byte count)
                        self.buf.emit(&[0x48, 0x89, 0xD1]);
                        // CLD ; REP MOVSB
                        self.buf.emit(&[0xFC, 0xF3, 0xA4]);

                        // Both copy paths converge here.
                        self.patch_rel32_to_here(backward_done);
                        // POP RDI ; POP RSI
                        self.buf.emit(&[0x5F, 0x5E]);

                        // --- done ---
                        self.patch_rel32_to_here(zero_len_skip);

                        // The GPU input-residency barrier, for the
                        // DESTINATION array.
                        //
                        // This intrinsic writes a primitive array with
                        // `REP MOVSB` and no `*astore` opcode anywhere in
                        // the method, so `offload_jit_gate`'s bytecode
                        // scan -- which looks only for those seven
                        // opcodes -- never saw it. A method whose only
                        // array write is a `System.arraycopy` was
                        // therefore admitted to the JIT even while the
                        // gate was refusing every ordinary array writer,
                        // and its inline copy left the device mirror
                        // stale with nothing to notice. That predates the
                        // barrier and is not what the gate was widened
                        // for; it is the same root cause reached by a
                        // path the gate could not see.
                        //
                        // Reloaded into RAX from the pinned frame slot
                        // rather than kept in a register: RSI/RDI were
                        // just popped and RAX/RCX/RDX are the copy's own
                        // scratch. Emitted on the zero-length path too --
                        // it costs one over-eviction in a case that wrote
                        // nothing, and keeping the label single-exit is
                        // worth more than the branch that would avoid it.
                        self.emit_load_local(RAX, s_dst);
                        self.emit_gpu_input_cache_barrier();

                        // Every bail branch (null, non-array,
                        // reference-element array, mismatched kind, or
                        // out-of-bounds) is a NORMAL, valid outcome for
                        // `System.arraycopy` — either a real exception or
                        // a successful reference-array copy — not an
                        // "uncommon" condition that should re-run the
                        // method. Route them through the SAME safe
                        // native-dispatch CALL an ordinary
                        // non-intrinsic `invokestatic` site uses
                        // (registered for this exact pc alongside the
                        // intrinsic — see the `ArraycopyPrimitive` match
                        // arm in `jit_scan`'s caller), so a guard failure
                        // throws/succeeds via normal call semantics
                        // instead of a deopt trap whose only resume
                        // strategy (whole-method re-run) can
                        // DOUBLE-EXECUTE a side effect the method already
                        // performed before reaching this call (e.g. a
                        // `stack[ptr--]` decrement already committed to
                        // the heap) — the mechanism behind the JDT
                        // `Parser` stack-corruption bug (jasper-jdt-parser-arrayindexoutofbounds.md).
                        // Falls back to the historical deopt trap only if
                        // the dispatch info wasn't registered (defensive;
                        // should not happen for this intrinsic).
                        let dispatch_info = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        match dispatch_info.map(|info| {
                            (info, self.reserve_spill_slots(5, SpillReason::HelperArgs))
                        }) {
                            Some((info, Some(args_base))) => {
                                let skip_dispatch = self.emit_jmp_rel32_patch();
                                for &patch in &bail_patches {
                                    self.patch_rel32_to_here(patch);
                                }
                                // Reload the five original operands from
                                // their pinned scratch homes (untouched
                                // by the guard sequence above) into the
                                // freshly reserved, contiguous args
                                // buffer in the layout `jit_invoke_
                                // dispatch` expects: arg[0] at the
                                // highest offset (lowest address),
                                // arg[n-1] at the lowest offset —
                                // mirrors the generic invokestatic
                                // dispatch site's buffer construction.
                                // ALIASING HAZARD: `args_base` reuses the
                                // SAME frame region as `s_src..s_len`
                                // (arraycopy's scratch homes never
                                // advanced `next_spill_offset` — see the
                                // "arraycopy pushes nothing" comment
                                // above), and the target buffer order is
                                // the REVERSE of the scratch-home order,
                                // so a naive per-index load-then-store
                                // would overwrite a not-yet-read home
                                // (e.g. writing arg[0] to `s_len`'s
                                // address before arg[4] has read `s_len`).
                                // Defeat it exactly like the fast-path
                                // guard setup above: load ALL five
                                // operands into distinct registers FIRST,
                                // then store.
                                self.emit_load_local(RAX, s_src);
                                self.emit_load_local(RCX, s_src_pos);
                                self.emit_load_local(RDX, s_dst);
                                self.emit_load_local(R10, s_dst_pos);
                                self.emit_load_local(R11, s_len);
                                self.emit_store_local(args_base + 4 * 8, RAX); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base + 3 * 8, RCX); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base + 2 * 8, RDX); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base + 1 * 8, R10); // Cast: x86-64 immediate encoding
                                self.emit_store_local(args_base, R11);
                                // SAFETY: info comes from self.invoke_info, which holds
                                // pointers to JitInvokeInfo structs kept alive by the
                                // caller for the duration of compilation.
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                                let buf_start = args_base + 4 * 8; // Cast: x86-64 immediate encoding
                                self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                                self.emit_mov_imm32_sx(ARG_REGS[3], 5);
                                self.emit_pre_safepoint_spill();
                                self.emit_call_absolute(self.helpers.invoke_dispatch);
                                self.emit_oop_map_for_safepoint();
                                self.emit_post_invoke_exception_check(b'V');
                                // arraycopy returns void: nothing pushed;
                                // the args buffer is dead, reclaim it.
                                self.next_spill_offset = args_base;
                                self.patch_rel32_to_here(skip_dispatch);
                            }
                            Some((_, None)) => {
                                // Spill region exhausted — bail the whole
                                // compile (always safe: the method falls
                                // back to the interpreter).
                                self.fail("singlepass-codegen/arraycopy-args-spill-exhausted");
                                return WalkStep::Return(false);
                            }
                            None => {
                                // Wire every bail branch to a shared deopt stub
                                // (reason 2 = DEOPT_REASON_BOUNDS_CHECK). The
                                // emit_deopt_stubs pass coalesces equal
                                // (bci, reason) pairs into one stub, so all the
                                // bail edges share a single trap.
                                for patch in bail_patches {
                                    self.deopt_stubs.push((patch, pc, 2));
                                }
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: ARRAYCOPY =====

                    // ===== INTRINSIC REGION BEGIN: ARRAYS_OPS =====
                    // java.util.Arrays.fill / Arrays.equals — Phase 4a.
                    //
                    // Array layout (cratonvm_types): a 40-byte object
                    // header with the i32 element count at offset 12
                    // (ARRAY_LENGTH_OFFSET); element data begins at
                    // HEADER_SIZE (40). Primitive elements are packed at
                    // their natural width (byte=1, char/short=2, int=4,
                    // long=8).
                    //
                    // Register discipline: `flush_scratch_registers()`
                    // ran at the top of the 0xb8 handler, so no Java
                    // local lives in a scratch GPR — RAX/RCX/RDX/R8/R9/
                    // R10/R11 are all free to clobber. RDI and RSI ARE
                    // in `LOCAL_REGS` (Windows callee-saved), so the
                    // REP STOS/CMPS sequences PUSH/POP them to keep the
                    // owning locals intact. There is no CALL and no
                    // safepoint between the PUSH and the POP, so the
                    // transient RSP adjustment is invisible to the GC.
                    else if callee_entry == crate::JitIntrinsic::ArraysFill1.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFill2.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFill4.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysFill8.as_entry()
                    {
                        // Arrays.fill(array, value) : void
                        //
                        // Operand stack (deepest first): [array, value].
                        // Pop value, then array.
                        let value_slot = self.pop_stack();
                        let array_slot = self.pop_stack();

                        // Array pointer → RAX for the null check.
                        self.load_slot_to_reg(RAX, array_slot);
                        // Null array → JDK throws NullPointerException.
                        // Reuse the shared null-check stub (sets
                        // JIT_PENDING_NPE, deopts out): TEST RAX,RAX +
                        // JZ stub. This is an intrinsic, not a plain
                        // array opcode, so no precise JEP-358 array action
                        // applies — record NONE (unmessaged NPE), matching
                        // the prior behaviour.
                        let trap_key = crate::x64::inlining::record_npe_trap_site(pc);
                        self.emit_null_check_array_load(npe_action::NONE, trap_key);

                        // Save array base in R8 (RAX is needed as the
                        // STOS source register).
                        // MOV R8, RAX  (49 89 C0)
                        self.buf.emit(&[0x49, 0x89, 0xC0]);
                        // Element count → ECX (zero-extends to RCX, the
                        // REP counter). MOV ECX, [RAX+12]  (8B 48 0C)
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x8B, 0x48, ARRAY_LENGTH_OFFSET as u8]);

                        // Fill value → RAX. STOSB/W/D/Q use AL/AX/EAX/
                        // RAX, all sub-registers of RAX, so a single
                        // 64-bit load serves every width.
                        self.load_slot_to_reg(RAX, value_slot);

                        // Save RDI (a Java local may live there), point
                        // it at the element data, run REP STOS, restore.
                        // PUSH RDI  (57)
                        self.buf.emit_byte(0x57);
                        // LEA RDI, [R8 + HEADER_SIZE]  (49 8D 78 28)
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x78, HEADER_SIZE as u8]);
                        if callee_entry == crate::JitIntrinsic::ArraysFill1.as_entry() {
                            // REP STOSB  (F3 AA)
                            self.buf.emit(&[0xF3, 0xAA]);
                        } else if callee_entry == crate::JitIntrinsic::ArraysFill2.as_entry() {
                            // REP STOSW  (66 F3 AB)
                            self.buf.emit(&[0x66, 0xF3, 0xAB]);
                        } else if callee_entry == crate::JitIntrinsic::ArraysFill4.as_entry() {
                            // REP STOSD  (F3 AB)
                            self.buf.emit(&[0xF3, 0xAB]);
                        } else {
                            // REP STOSQ  (F3 48 AB)
                            self.buf.emit(&[0xF3, 0x48, 0xAB]);
                        }
                        // POP RDI  (5F)
                        self.buf.emit_byte(0x5F);

                        // The GPU input-residency barrier. Same story as
                        // the `System.arraycopy` intrinsic above: this
                        // writes the whole array with `REP STOS` and no
                        // `*astore` opcode, so the gate's scan never saw
                        // the method as an array writer at all.
                        //
                        // The array base is still in R8, where this arm
                        // parked it before RAX became the STOS source.
                        // MOV RAX, R8  (4C 89 C0)
                        self.buf.emit(&[0x4C, 0x89, 0xC0]);
                        self.emit_gpu_input_cache_barrier();

                        // void return — nothing pushed onto the operand
                        // stack.
                    } else if callee_entry == crate::JitIntrinsic::ArraysEquals1.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysEquals2.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysEquals4.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysEquals8.as_entry()
                    {
                        // Arrays.equals(a, b) : boolean
                        //
                        // JDK semantics (java.util.Arrays):
                        //   a == b               -> true   (both null, or same ref)
                        //   a == null || b==null -> false
                        //   a.length != b.length -> false
                        //   else element-wise equality.
                        // `equals` never throws — it is a pure compare,
                        // so no null-check stub is needed.
                        //
                        // The element-wise compare is a raw byte compare
                        // of `length * elem_size` bytes via REP CMPSB.
                        // boolean[] stores 0/1 per byte, so a byte
                        // compare is exact for `equals([Z[Z)Z`.
                        let b_slot = self.pop_stack();
                        let a_slot = self.pop_stack();
                        // a → R8, b → R9.
                        self.load_slot_to_reg(R8, a_slot);
                        self.load_slot_to_reg(R9, b_slot);

                        let shift: u8 = if callee_entry
                            == crate::JitIntrinsic::ArraysEquals1.as_entry()
                        {
                            0
                        } else if callee_entry == crate::JitIntrinsic::ArraysEquals2.as_entry() {
                            1
                        } else if callee_entry == crate::JitIntrinsic::ArraysEquals4.as_entry() {
                            2
                        } else {
                            3
                        };

                        // CMP R8, R9  (4D 39 C8) — same reference?
                        self.buf.emit(&[0x4D, 0x39, 0xC8]);
                        // JE -> true  (74 rel8) — covers both-null and
                        // identical-reference.
                        self.buf.emit_byte(0x74);
                        let je_true_1 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // TEST R8, R8  (4D 85 C0) — a null (b not)?
                        self.buf.emit(&[0x4D, 0x85, 0xC0]);
                        // JZ -> false  (74 rel8)
                        self.buf.emit_byte(0x74);
                        let jz_false_1 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // TEST R9, R9  (4D 85 C9) — b null (a not)?
                        self.buf.emit(&[0x4D, 0x85, 0xC9]);
                        // JZ -> false  (74 rel8)
                        self.buf.emit_byte(0x74);
                        let jz_false_2 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // Lengths: EAX = a.length, EDX = b.length.
                        // MOV EAX, [R8+12]  (41 8B 40 0C)
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]);
                        // MOV EDX, [R9+12]  (41 8B 51 0C)
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x8B, 0x51, ARRAY_LENGTH_OFFSET as u8]);
                        // CMP EAX, EDX  (39 D0)
                        self.buf.emit(&[0x39, 0xD0]);
                        // JNE -> false  (75 rel8)
                        self.buf.emit_byte(0x75);
                        let jne_false_1 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // Byte count = length << shift  → RCX (REP
                        // counter). MOVZX is unnecessary: array lengths
                        // are non-negative i32, so a 32-bit MOV
                        // zero-extends cleanly into RCX.
                        // MOV ECX, EAX  (89 C1)
                        self.buf.emit(&[0x89, 0xC1]);
                        if shift != 0 {
                            // SHL RCX, shift  (48 C1 E1 ib)
                            self.buf.emit(&[0x48, 0xC1, 0xE1, shift]);
                        }
                        // Empty arrays (count == 0): equal. Also avoids
                        // running REP CMPSB with RCX==0, whose ZF would
                        // otherwise carry over from the SHL above.
                        // TEST RCX, RCX  (48 85 C9)
                        self.buf.emit(&[0x48, 0x85, 0xC9]);
                        // JZ -> true  (74 rel8)
                        self.buf.emit_byte(0x74);
                        let jz_true_2 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // Save RSI/RDI (Java locals may live there),
                        // point them at the two element-data regions,
                        // run REP CMPSB, restore. POP does not touch
                        // flags, so ZF from CMPSB survives the restore.
                        // PUSH RSI (56) ; PUSH RDI (57)
                        self.buf.emit(&[0x56, 0x57]);
                        // LEA RSI, [R8 + HEADER_SIZE]  (49 8D 70 28)
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x70, HEADER_SIZE as u8]);
                        // LEA RDI, [R9 + HEADER_SIZE]  (49 8D 79 28)
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x79, HEADER_SIZE as u8]);
                        // REP CMPSB  (F3 A6) — compares RCX bytes;
                        // stops early on the first mismatch. ZF=1 iff
                        // every byte matched.
                        self.buf.emit(&[0xF3, 0xA6]);
                        // POP RDI (5F) ; POP RSI (5E)
                        self.buf.emit(&[0x5F, 0x5E]);
                        // SETE AL  (0F 94 C0) — AL = ZF.
                        self.buf.emit(&[0x0F, 0x94, 0xC0]);
                        // MOVZX EAX, AL  (0F B6 C0) — boolean result.
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                        // JMP -> done  (EB rel8)
                        self.buf.emit_byte(0xEB);
                        let jmp_done_1 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // --- true label ---
                        let true_label = self.buf.pos();
                        // MOV EAX, 1  (B8 01 00 00 00)
                        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
                        // JMP -> done  (EB rel8)
                        self.buf.emit_byte(0xEB);
                        let jmp_done_2 = self.buf.pos();
                        self.buf.emit_byte(0x00);

                        // --- false label ---
                        let false_label = self.buf.pos();
                        // XOR EAX, EAX  (31 C0)
                        self.buf.emit(&[0x31, 0xC0]);

                        // --- done label ---
                        let done_label = self.buf.pos();

                        // Patch all rel8 displacements. Every span here
                        // is a few dozen bytes — comfortably inside the
                        // signed-rel8 range; debug_assert guards it.
                        for (patch, target) in [
                            (je_true_1, true_label),
                            (jz_true_2, true_label),
                            (jz_false_1, false_label),
                            (jz_false_2, false_label),
                            (jne_false_1, false_label),
                            (jmp_done_1, done_label),
                            (jmp_done_2, done_label),
                        ] {
                            // Cast: signed offset to isize for pointer/index arithmetic
                            let rel = target as isize - (patch as isize + 1);
                            debug_assert!(
                                (-128..=127).contains(&rel),
                                "Arrays.equals intrinsic rel8 out of range: {rel}"
                            );
                            // Widening: isize -> i64 (no truncation)
                            Self::patch_rel8_or_bail(&mut self.buf, patch, rel as i64);
                        }

                        // boolean result in RAX → operand stack.
                        self.push_from_rax();
                    }
                    // ===== INTRINSIC REGION END: ARRAYS_OPS =====

                    // ===== INTRINSIC REGION BEGIN: ARRAYS_SORT =====
                    else if callee_entry == crate::JitIntrinsic::ArraysSortInt.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortLong.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortChar.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortShort.as_entry()
                        || callee_entry == crate::JitIntrinsic::ArraysSortByte.as_entry()
                    {
                        // Phase 4b — java.util.Arrays.sort(prim[]) inline
                        // sort. Void return: nothing is pushed.
                        //
                        // The matcher (try_resolve_intrinsic ARRAYS_SORT
                        // region) registers only the five integral
                        // single-arg overloads. Up to 47 elements — the
                        // JDK's own insertion-sort cut-over — this is an
                        // insertion sort; past that, an in-place heapsort,
                        // O(n log n) and still allocation-free. The
                        // insertion sort used to run for EVERY length:
                        // O(n^2), so a compiled `Arrays.sort` of a million
                        // ints took minutes instead of tens of milliseconds
                        // and stalled every thread waiting on a safepoint
                        // for all of it.
                        //
                        // Neither loop polls for a safepoint, and neither
                        // may: the array base lives in R8, which no oop map
                        // names, so a moving collection inside the loop
                        // would leave it pointing at from-space. Heapsort
                        // bounds that window to O(n log n) work, which is
                        // what a native sort costs anyway.
                        //
                        // All work uses caller-saved scratch only
                        // (RAX/RCX/RDX/R8/R9/R10/R11) — `flush_scratch_
                        // registers()` already spilled them and Java
                        // locals live in callee-saved registers, so the
                        // loop never clobbers live state.
                        //
                        // Register file for the emitted routine:
                        //   R8  = array base pointer
                        //   R9  = n (element count)
                        //   R10 = i (outer index)
                        //   R11 = j (inner index)
                        //   RAX = key (= a[i])
                        //   RCX = a[j] scratch
                        //   RDX = j+1 (store index)
                        //
                        // Every element is loaded sign-/zero-extended to
                        // a full 64-bit register, so a single signed
                        // 64-bit CMP orders all five element kinds
                        // correctly (char is unsigned 0..=65535, which is
                        // non-negative, so signed compare still works).

                        // SIB scale bits + load/store encodings per width.
                        let is_int = callee_entry == crate::JitIntrinsic::ArraysSortInt.as_entry();
                        let is_long =
                            callee_entry == crate::JitIntrinsic::ArraysSortLong.as_entry();
                        let is_char =
                            callee_entry == crate::JitIntrinsic::ArraysSortChar.as_entry();
                        let is_short =
                            callee_entry == crate::JitIntrinsic::ArraysSortShort.as_entry();
                        // is_byte is the remaining case.
                        let scale_ss: u8 = if is_long {
                            0b11 // *8
                        } else if is_int {
                            0b10 // *4
                        } else if is_char || is_short {
                            0b01 // *2
                        } else {
                            0b00 // *1 (byte)
                        };
                        // HEADER_SIZE / ARRAY_LENGTH_OFFSET both fit in a
                        // signed disp8 (asserted in cratonvm_types).
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        let hdr = HEADER_SIZE as u8;
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        let len_off = ARRAY_LENGTH_OFFSET as u8;

                        // Pop the array reference, null-check it (reusing
                        // the shared NPE stub), then park it in R8.
                        let arr_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, arr_slot);
                        // TEST RAX,RAX / JZ -> shared null-check stub.
                        // Intrinsic array access — no precise array opcode,
                        // so record NONE (unmessaged NPE).
                        let trap_key = crate::x64::inlining::record_npe_trap_site(pc);
                        self.emit_null_check_array_load(npe_action::NONE, trap_key);
                        // MOV R8, RAX  (49 89 C0)
                        self.buf.emit(&[0x49, 0x89, 0xC0]);
                        // MOV R9D, [R8 + ARRAY_LENGTH_OFFSET]  (45 8B 48 dd)
                        // (32-bit load zero-extends n into R9.)
                        self.buf.emit(&[0x45, 0x8B, 0x48, len_off]);
                        // CMP R9, 47  (49 83 F9 2F) ; JG .heapsort
                        // n is a zero-extended array length, so the signed
                        // compare is exact.
                        self.buf.emit(&[0x49, 0x83, 0xF9, 0x2F]);
                        let heapsort_patch = self.emit_jcc_rel32_patch(0x8F);
                        // MOV R10D, 1   (41 BA 01 00 00 00) — i = 1
                        self.buf.emit(&[0x41, 0xBA, 0x01, 0x00, 0x00, 0x00]);

                        // --- inline emitters for width-specific access ---
                        // Access form: [R8 + idx*scale + hdr] with a
                        // ModRM.reg operand `r` (the GPR load dest /
                        // store source). REX bits MUST be derived per
                        // register: REX.R from `r`, REX.X from `idx`,
                        // REX.B is always 1 (base is R8). REX.W comes
                        // from the caller (`w`). A prior version hard-
                        // coded REX.X=1, which silently re-routed a
                        // store through a non-extended index register
                        // (RDX -> R10) and left the array unsorted.
                        let rex = |w: u8, r: u8, idx: u8| -> u8 {
                            0x40 | (w << 3)
                                // Truncation: wider int -> u8 (low 8 bits, intentional)
                                | (((r >= 8) as u8) << 2)
                                // Truncation: wider int -> u8 (low 8 bits, intentional)
                                | (((idx >= 8) as u8) << 1)
                                | 1 // REX.B: base = R8
                        };
                        // load r <- [R8 + idx*scale + hdr]
                        let emit_load = |buf: &mut crate::ExecutableBuffer, r: u8, idx: u8| {
                            let sib = (scale_ss << 6) | ((idx & 7) << 3);
                            let modrm = 0x40 | ((r & 7) << 3) | 0x04;
                            if is_long {
                                // MOV r64,[..]  REX.W, 8B
                                buf.emit(&[rex(1, r, idx), 0x8B, modrm, sib, hdr]);
                            } else if is_int {
                                // MOVSXD r64,[..]  REX.W, 63
                                buf.emit(&[rex(1, r, idx), 0x63, modrm, sib, hdr]);
                            } else if is_char {
                                // MOVZX r32,m16  0F B7. char is
                                // unsigned 0..=65535; zero-extending
                                // to 32 bits also clears RAX[63:32],
                                // so the 64-bit signed CMP is correct.
                                buf.emit(&[rex(0, r, idx), 0x0F, 0xB7, modrm, sib, hdr]);
                            } else if is_short {
                                // MOVSX r64,m16  REX.W 0F BF — MUST
                                // sign-extend to the FULL 64-bit
                                // register: the inner-loop CMP is
                                // 64-bit, and a 32-bit MOVSX would
                                // leave a negative short looking like
                                // a large positive (0x0000_0000_FFFF…).
                                buf.emit(&[rex(1, r, idx), 0x0F, 0xBF, modrm, sib, hdr]);
                            } else {
                                // MOVSX r64,m8  REX.W 0F BE — sign-
                                // extend a signed byte to 64 bits
                                // (same rationale as short above).
                                buf.emit(&[rex(1, r, idx), 0x0F, 0xBE, modrm, sib, hdr]);
                            }
                        };
                        // store [R8 + idx*scale + hdr] <- r
                        let emit_store = |buf: &mut crate::ExecutableBuffer, r: u8, idx: u8| {
                            let sib = (scale_ss << 6) | ((idx & 7) << 3);
                            let modrm = 0x40 | ((r & 7) << 3) | 0x04;
                            if is_long {
                                // MOV [..],r64  REX.W, 89
                                buf.emit(&[rex(1, r, idx), 0x89, modrm, sib, hdr]);
                            } else if is_int {
                                // MOV [..],r32  89
                                buf.emit(&[rex(0, r, idx), 0x89, modrm, sib, hdr]);
                            } else if is_char || is_short {
                                // MOV [..],r16  66 prefix, 89
                                buf.emit(&[0x66, rex(0, r, idx), 0x89, modrm, sib, hdr]);
                            } else {
                                // MOV [..],r8   88
                                buf.emit(&[rex(0, r, idx), 0x88, modrm, sib, hdr]);
                            }
                        };

                        // .outer:
                        let outer_label = self.buf.pos();
                        // CMP R10, R9   (4D 39 CA) — i vs n
                        self.buf.emit(&[0x4D, 0x39, 0xCA]);
                        // JGE .done  (0F 8D rel32) — signed: i >= n
                        let done_patch = self.emit_jcc_rel32_patch(0x8D);
                        // key = a[i]
                        emit_load(&mut self.buf, RAX, R10);
                        // MOV R11, R10  (4D 89 D3) — j = i
                        self.buf.emit(&[0x4D, 0x89, 0xD3]);
                        // DEC R11       (49 FF CB) — j = i - 1
                        self.buf.emit(&[0x49, 0xFF, 0xCB]);

                        // .inner:
                        let inner_label = self.buf.pos();
                        // TEST R11,R11  (4D 85 DB)
                        self.buf.emit(&[0x4D, 0x85, 0xDB]);
                        // JS .insert    (0F 88 rel32) — j < 0 -> stop
                        let insert_patch = self.emit_jcc_rel32_patch(0x88);
                        // RCX = a[j]
                        emit_load(&mut self.buf, RCX, R11);
                        // CMP RCX, RAX  (48 39 C1) — a[j] vs key, signed64
                        self.buf.emit(&[0x48, 0x39, 0xC1]);
                        // JLE .insert   (0F 8E rel32) — a[j] <= key -> stop
                        //   (stable: equal keys are never shifted past.)
                        let insert_patch2 = self.emit_jcc_rel32_patch(0x8E);
                        // a[j+1] = a[j]
                        // LEA RDX, [R11 + 1]  (49 8D 53 01)
                        self.buf.emit(&[0x49, 0x8D, 0x53, 0x01]);
                        emit_store(&mut self.buf, RCX, RDX);
                        // DEC R11  (49 FF CB)  — j--
                        self.buf.emit(&[0x49, 0xFF, 0xCB]);
                        // JMP .inner  (E9 rel32) — backward branch.
                        self.buf.emit_byte(0xE9);
                        {
                            let here = self.buf.pos();
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (inner_label as i64) - (here as i64 + 4);
                            // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                            self.buf.emit(&(rel as i32).to_le_bytes());
                        }

                        // .insert: both JS and JLE land here.
                        self.patch_rel32_to_here(insert_patch);
                        self.patch_rel32_to_here(insert_patch2);
                        // a[j+1] = key
                        // LEA RDX, [R11 + 1]  (49 8D 53 01)
                        self.buf.emit(&[0x49, 0x8D, 0x53, 0x01]);
                        emit_store(&mut self.buf, RAX, RDX);
                        // INC R10  (49 FF C2)  — i++
                        self.buf.emit(&[0x49, 0xFF, 0xC2]);
                        // JMP .outer  (E9 rel32) — backward branch.
                        self.buf.emit_byte(0xE9);
                        {
                            let here = self.buf.pos();
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (outer_label as i64) - (here as i64 + 4);
                            // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                            self.buf.emit(&(rel as i32).to_le_bytes());
                        }

                        // .done: the top-of-loop JGE lands here.
                        self.patch_rel32_to_here(done_patch);
                        // JMP .end — skip the heapsort.
                        self.buf.emit_byte(0xE9);
                        let end_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        // .heapsort: n > 47. Floyd's bottom-up heap
                        // construction, then repeated extraction of the
                        // maximum. Same register file as above, plus:
                        //   R10 = heap build cursor / extraction end
                        //   R11 = sift root
                        //   RDX = sift child
                        // RAX/RCX hold element values, loaded to 64 bits by
                        // `emit_load`, so the signed 64-bit CMPs order every
                        // element kind exactly as the insertion sort does.
                        self.patch_rel32_to_here(heapsort_patch);
                        macro_rules! jmp_back {
                            ($s:ident, $label:expr) => {{
                                $s.buf.emit_byte(0xE9);
                                let here = $s.buf.pos();
                                // Widening: usize offset -> i64 for rel math
                                let rel = ($label as i64) - (here as i64 + 4);
                                // Truncation: i64 -> i32 (rel32 within one method)
                                $s.buf.emit(&(rel as i32).to_le_bytes());
                            }};
                        }
                        // Sift a[R11] down within [0, limit). `$limit_rm` is
                        // the ModRM byte of `CMP RDX, limit` (limit = R9 → CA,
                        // limit = R10 → D2) under REX 4C.
                        macro_rules! sift_down {
                            ($s:ident, $limit_rm:expr) => {{
                                let sift_loop = $s.buf.pos();
                                // LEA RDX, [R11 + R11 + 1] — child = 2*root + 1
                                $s.buf.emit(&[0x4B, 0x8D, 0x54, 0x1B, 0x01]);
                                $s.buf.emit(&[0x4C, 0x39, $limit_rm]); // CMP RDX, limit
                                let sift_done = $s.emit_jcc_rel32_patch(0x8D); // JGE
                                emit_load(&mut $s.buf, RAX, RDX); // RAX = a[child]
                                $s.buf.emit(&[0x48, 0xFF, 0xC2]); // INC RDX
                                $s.buf.emit(&[0x4C, 0x39, $limit_rm]); // CMP RDX, limit
                                let no_right = $s.emit_jcc_rel32_patch(0x8D); // JGE .left
                                emit_load(&mut $s.buf, RCX, RDX); // RCX = a[child + 1]
                                $s.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                                let left_not_smaller = $s.emit_jcc_rel32_patch(0x8D); // JGE .left
                                $s.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                $s.buf.emit_byte(0xE9); // JMP .have_child
                                let have_child = $s.buf.pos();
                                $s.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                                // .left: the left child is the larger one.
                                $s.patch_rel32_to_here(no_right);
                                $s.patch_rel32_to_here(left_not_smaller);
                                $s.buf.emit(&[0x48, 0xFF, 0xCA]); // DEC RDX
                                // .have_child: RDX = larger child, RAX = its value.
                                $s.patch_rel32_to_here(have_child);
                                emit_load(&mut $s.buf, RCX, R11); // RCX = a[root]
                                $s.buf.emit(&[0x48, 0x39, 0xC1]); // CMP RCX, RAX
                                let in_order = $s.emit_jcc_rel32_patch(0x8D); // JGE .sift_done
                                emit_store(&mut $s.buf, RAX, R11); // a[root] = a[child]
                                emit_store(&mut $s.buf, RCX, RDX); // a[child] = old a[root]
                                $s.buf.emit(&[0x49, 0x89, 0xD3]); // MOV R11, RDX
                                jmp_back!($s, sift_loop);
                                $s.patch_rel32_to_here(sift_done);
                                $s.patch_rel32_to_here(in_order);
                            }};
                        }
                        // Build: for start in (0..n/2).rev() { sift(start, n) }
                        self.buf.emit(&[0x4D, 0x89, 0xCA]); // MOV R10, R9
                        self.buf.emit(&[0x49, 0xD1, 0xEA]); // SHR R10, 1
                        let build_loop = self.buf.pos();
                        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                        let build_done = self.emit_jcc_rel32_patch(0x84); // JZ
                        self.buf.emit(&[0x49, 0xFF, 0xCA]); // DEC R10
                        self.buf.emit(&[0x4D, 0x89, 0xD3]); // MOV R11, R10
                        sift_down!(self, 0xCA);
                        jmp_back!(self, build_loop);
                        self.patch_rel32_to_here(build_done);
                        // Extract: for end in (1..n).rev() { swap(0, end); sift(0, end) }
                        self.buf.emit(&[0x4D, 0x89, 0xCA]); // MOV R10, R9
                        let extract_loop = self.buf.pos();
                        self.buf.emit(&[0x49, 0xFF, 0xCA]); // DEC R10
                        self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                        let sorted = self.emit_jcc_rel32_patch(0x8E); // JLE
                        self.buf.emit(&[0x45, 0x31, 0xDB]); // XOR R11D, R11D
                        emit_load(&mut self.buf, RAX, R11); // RAX = a[0]
                        emit_load(&mut self.buf, RCX, R10); // RCX = a[end]
                        emit_store(&mut self.buf, RCX, R11); // a[0] = a[end]
                        emit_store(&mut self.buf, RAX, R10); // a[end] = old a[0]
                        sift_down!(self, 0xD2);
                        jmp_back!(self, extract_loop);
                        self.patch_rel32_to_here(sorted);

                        // .end
                        self.patch_rel32_to_here(end_patch);
                        // Void method — nothing pushed; `ret_type` is 'V'.
                        let _ = ret_type;
                    }
                    // ===== INTRINSIC REGION END: ARRAYS_SORT =====
                    else {
                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the pre-pop operand stack (see the
                        // matching invokevirtual/interface fix below) so a
                        // post-invoke exception/deopt guard's `Reinterpret`
                        // resume at this bci has the args this invokestatic
                        // needs, instead of underflowing on an empty stack.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        // Direct call to a JIT-compiled callee
                        let n = callee_params;
                        // `pop_stack` rewinds `next_spill_offset` when it pops a
                        // top-of-stack `Frame` slot, but it still HANDS THE SLOT
                        // BACK, and every `arg_slots` entry stays live until
                        // `emit_stack_arg_setup` marshals it into the entry ABI far
                        // below. Anything that reserves spill space in between is
                        // therefore handed the argument slots themselves. Remember
                        // the pre-pop top so such a reservation can be placed above
                        // them. See
                        // jit-direct-call-arg1-clobbered-by-arg0-FIXED.md.
                        let args_frame_top = self.next_spill_offset;
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        // A reference staged where no oop map can name it fails the
                        // safepoint closed. DEFERRED to just after the service-range
                        // reservation below, because whether that is true here is
                        // exactly what the reservation decides: when it succeeds it
                        // copies every argument into a contiguous frame range, and a
                        // frame range IS nameable. See `direct_call_arg_maps_enabled`.
                        // Spill cursor as the bytecode's operand stack sees it
                        // now that this invoke's arguments are popped. The
                        // return value belongs HERE, not wherever the
                        // service-argument reservation below leaves the cursor.
                        let post_pop_spill = self.next_spill_offset;

                        // T5.2.16 — Sibling tail-call optimization.
                        //
                        // When the caller's immediate next bytecode
                        // is an `xreturn` of the same type the
                        // callee produces, we can tear down our
                        // frame and `JMP` into the callee so the
                        // callee returns straight to OUR caller.
                        // Gate on:
                        //   1. pc+3 is an xreturn whose type tag
                        //      matches ret_type, or the callee is
                        //      void AND pc+3 is `return` (0xB1).
                        //   2. callee_needs_ctx == self.needs_heap
                        //      (we have a VM context iff the
                        //      callee wants one) — otherwise the
                        //      ABI shift wouldn't match.
                        //   3. RET intrinsics (MATH_*_INTRINSIC)
                        //      are NOT targeted (already branched
                        //      above), so the callee is a normal
                        //      JIT-compiled method.
                        //   4. `pc + 3` is NOT a branch target. The
                        //      tail form CONSUMES the `xreturn` — it emits
                        //      no code for that PC and leaves
                        //      `pc_to_native[pc + 3]` unset — so any other
                        //      edge into it becomes unresolvable and
                        //      `patch_branches` rejects the whole method
                        //      with `branch-target-not-an-instruction-
                        //      boundary`, a reason whose message blames
                        //      malformed bytecode. It is the ordinary
                        //      shape `return (x != null ? x : missing())`:
                        //      the `else` arm's call sits immediately
                        //      before the shared `areturn`, and the `then`
                        //      arm's `goto` lands on it. Fusing would also
                        //      be wrong on its own terms — the other edge
                        //      arrives with its own value on the operand
                        //      stack and expects a plain return, not "load
                        //      args and JMP to the callee". This is the
                        //      same precondition the const-arith peepholes
                        //      state: never fuse across a merge point.
                        let tail_op_matches = pc + 3 < code_len
                            && !branch_targets[pc + 3]
                            && match (ret_type, code[pc + 3]) {
                                (b'I' | b'Z' | b'B' | b'S' | b'C', 0xAC) => true,
                                (b'J', 0xAD) => true,
                                (b'F', 0xAE) => true,
                                (b'D', 0xAF) => true,
                                (b'L' | b'[', 0xB0) => true,
                                (b'V', 0xB1) => true,
                                _ => false,
                            };
                        // A tail-call inside a try region would tear this
                        // frame down before the callee runs, so anything it
                        // throws escapes the handler that covers this pc
                        // (see `pc_is_protected`). Demote to a normal CALL.
                        let is_sibling_tail = tail_op_matches
                            && callee_needs_ctx == self.needs_heap
                            && !self.pc_is_protected(pc);

                        // Round-8 wave-3: sibling-tail demotion.
                        // Tail-calling with stack args is non-trivial
                        // — args would have to be re-materialized
                        // *after* the epilogue restores RSP, which
                        // requires an additional shuffle buffer.
                        // Simpler and still correct: demote to a
                        // non-tail CALL when the arg count would
                        // require stack passing. The fall-through
                        // below handles that case with the proper
                        // stack-arg setup helper.
                        let sibling_reg_limit = if callee_needs_ctx {
                            ARG_REGS.len() - 1
                        } else {
                            ARG_REGS.len()
                        };
                        // 5. The callee cannot stash a deopt frame.
                        //
                        // A sibling tail call REPLACES this frame, so the
                        // callee returns straight to OUR caller — and if it
                        // traps, the `i64::MIN` sentinel and the frame it
                        // stashed under the CALLEE's key arrive at a call
                        // site that invoked US. That site's identity gate
                        // (`try_resume_trapped_callee`) correctly refuses a
                        // stash naming a method it did not call, and the
                        // frame becomes an orphan nobody can attribute.
                        // A real CALL keeps this frame alive long enough
                        // for `emit_inline_callee_deopt_check` below to
                        // service the trap at the site that made it.
                        //
                        // `info_ptr.is_some()` IS the "can stash" test:
                        // a `JitInvokeInfo` is registered for exactly the
                        // sites whose callee is a compiled Java artifact
                        // (plus `ArraycopyPrimitive`, the one intrinsic
                        // that deopts). Inline-machine-code intrinsics and
                        // the thin native helpers have no info and no way
                        // to stash, so they keep the tail form.
                        let sibling_tail_ok = is_sibling_tail
                            && arg_slots.len() <= sibling_reg_limit
                            && info_ptr.is_none()
                            && sp_tailcall_enabled();
                        if sibling_tail_ok {
                            // Load args into ABI registers, tear
                            // down our frame, then JMP.
                            if callee_needs_ctx {
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    if i + 1 < ARG_REGS.len() {
                                        self.load_slot_to_reg(ARG_REGS[i + 1], *slot);
                                    }
                                }
                            } else {
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    if i < ARG_REGS.len() {
                                        self.load_slot_to_reg(ARG_REGS[i], *slot);
                                    }
                                }
                            }
                            self.emit_epilogue_without_ret();
                            self.emit_jmp_absolute(callee_entry);
                            // Consume the invokestatic (3) and the
                            // xreturn (1) — no fall-through.
                            pc += 3; // invokestatic
                            pc += 1; // xreturn
                            self.reset_spills();
                            return WalkStep::Next(pc);
                        }

                        // Preserve Java arguments in a contiguous frame range for
                        // the cold callee-sentinel service. A baked direct call has no
                        // dispatch helper frame to recover them from when its own
                        // exception table must run.
                        let service_args_base = info_ptr.and_then(|_| {
                            // See `reserve_direct_call_service_slots`: this range MUST
                            // sit above the argument slots `pop_stack` just handed
                            // back, or the copy below reverses the arguments into
                            // themselves and the callee gets arg0 in every slot.
                            let base =
                                self.reserve_direct_call_service_slots(args_frame_top, &arg_slots)?;
                            for (i, slot) in arg_slots.iter().enumerate() {
                                self.load_slot_to_reg(R11, *slot);
                                let off = base + ((arg_slots.len() - 1 - i) as i32) * 8;
                                self.emit_store_local(off, R11);
                                // THE SAME CHANNEL THE DISPATCH SITE USES. Its args
                                // buffer pushes each oop among the arguments to
                                // `pending_staged_arg_oops`, so the safepoint map
                                // NAMES it and `collect_live_oop_homes` publishes it
                                // on the shadow stack. This copy is the same shape --
                                // a contiguous frame range, written before the CALL,
                                // still live after it (`emit_inline_callee_deopt_check`
                                // reads it) -- and it named nothing.
                                if direct_call_arg_maps_enabled() && arg_oops[i] {
                                    self.pending_staged_arg_oops.push(off);
                                }
                            }
                            Some(base)
                        });
                        // Only an argument oop with NO named home fails the
                        // safepoint closed now. With the service range reserved
                        // every one of them has one.
                        if arg_oops.iter().any(|&o| o)
                            && (!direct_call_arg_maps_enabled() || service_args_base.is_none())
                        {
                            self.pending_staged_args_unmapped = true;
                        }
                        // Round-8 wave-3 HIGH fix: stack-arg setup
                        // for direct calls whose total arg count
                        // exceeds ARG_REGS. Uses platform ABI
                        // (Win64 32-byte shadow + stack; SysV pure
                        // stack), 16-byte aligned at the CALL site.
                        let total_sub = self.emit_stack_arg_setup(&arg_slots, callee_needs_ctx);
                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering CALL -- unless the frame
                        // is provably oop-clean here, in which case the
                        // 14-store blind copy publishes nothing and only the
                        // safepoint id is needed. See
                        // `can_elide_direct_call_register_spill`.
                        // `args_frame_resident` is what lets mode 2 admit a
                        // reference argument: the service range makes it
                        // frame-resident for the CONSERVATIVE walk. That is no
                        // longer the whole obligation -- naming the argument in
                        // the map means a moving cycle will rewrite it, and for
                        // that it must also be PUBLISHED, which only the real
                        // spill path emits. So a call that names its argument
                        // oops declines the elision and pays the spill again;
                        // `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS=0` restores the
                        // cheaper, unrelocatable arrangement.
                        let names_arg_oops = direct_call_arg_maps_enabled()
                            && service_args_base.is_some()
                            && arg_oops.iter().any(|&o| o);
                        if self.can_elide_direct_call_register_spill(
                            &arg_oops,
                            service_args_base.is_some() && !names_arg_oops,
                            1,
                        ) {
                            self.emit_safepoint_metadata_only();
                        } else {
                            // ARG_REGS only, and only if the service slots
                            // were actually reserved: the copy above stages
                            // through R11, so RAX is untouched here and its
                            // contents are unpublished.
                            self.emit_pre_safepoint_spill_args_published(
                                service_args_base.is_some(),
                                false,
                            );
                        }
                        // Emit direct CALL to callee entry point
                        self.emit_call_absolute(callee_entry);
                        self.emit_post_call_rbp_republish();
                        // T1.1.2 — direct call to a JIT-compiled
                        // callee is still a safepoint: the callee
                        // may allocate and trigger GC transitively.
                        self.emit_oop_map_for_safepoint();
                        self.emit_stack_arg_cleanup(total_sub);
                        // 2026-09-02: one sentinel compare on the hot
                        // path; the callee-deopt check and the exception
                        // check below keep their own compares on the cold
                        // side (`merged_call_sentinel_enabled`).
                        let merged_keep = merged_call_sentinel_enabled()
                            .then(|| self.emit_call_sentinel_fast_skip());
                        if let (Some(info), Some(args_base)) = (info_ptr, service_args_base) {
                            self.emit_inline_callee_deopt_check(
                                info as *const crate::JitInvokeInfo,
                                arg_slots.len(),
                                args_base,
                            );
                        } else {
                            self.dbg_unserviced_direct_call(
                                "invokestatic",
                                pc,
                                info_ptr.is_some(),
                                service_args_base.is_some(),
                            );
                            self.fail_unserviced_java_direct_call(info_ptr, service_args_base);
                        }

                        // A directly-called compiled callee that throws
                        // (or deopts) returns the `i64::MIN` sentinel.
                        // Without this guard the JIT would push the
                        // sentinel as the return value — for an L/[
                        // return it would then be tagged as an oop
                        // (`mark_top_as_oop` below) and the next deref
                        // of that `0x8000_0000_0000_0000` wild pointer
                        // segfaults. Deopt out so the interpreter routes
                        // the stashed exception through the exception
                        // table instead.
                        self.emit_post_invoke_exception_check(ret_type);
                        if let Some(keep) = merged_keep {
                            self.patch_rel32_to_here(keep);
                        }

                        // Reclaim the spill cursor to the popped-args depth
                        // before the result is pushed, exactly as the
                        // dispatch-helper arm below does with its own
                        // `post_pop_spill`.
                        //
                        // `reserve_direct_call_service_slots` parks the cold
                        // deopt-service copy of the arguments ABOVE the argument
                        // slots (it has to: the slots `pop_stack` handed back are
                        // still live sources for `emit_stack_arg_setup`), which
                        // leaves `next_spill_offset` n slots past the pre-pop top.
                        // Pushing the return value from there parks it above its
                        // semantic operand-stack depth, and every later push in
                        // this basic block inherits the shift. The linear walk
                        // stays self-consistent, so nothing looks wrong -- until
                        // the first branch target after the call, whose depth is
                        // re-established from the bytecode. Writer and reader then
                        // address different slots and the method computes with a
                        // stale one. Measured on ECJ's
                        // `OperandStack.pop(OperandCategory)`, whose `if_icmpeq`
                        // (a tableswitch merge point) compared `TypeBinding.id`
                        // against the expected category instead of
                        // `TypeIds.getCategory(id)`: every JSP compiled after that
                        // method tiered up threw `AssertionError: Unexpected
                        // operand at stack top` (tomcat/ecj-operandstack-*.md).
                        //
                        // Safe to hand the reserved range back: its only consumer
                        // is `emit_inline_callee_deopt_check`, emitted just above.
                        self.next_spill_offset = post_pop_spill;

                        if ret_type != b'V' {
                            if matches!(ret_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                            if matches!(ret_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                    }
                } else if let Some(info) = info_ptr {
                    // Non-self invokestatic without a direct target — use dispatch helper
                    // SAFETY: info comes from self.invoke_info, which holds pointers to
                    // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                    let info_ref = unsafe { &*info };
                    let n = info_ref.num_jit_args;

                    // value-stack-usize-underflow-nio-worker-panic fix:
                    // snapshot the pre-pop operand stack (see the matching
                    // invokevirtual/interface fix below) so a post-invoke
                    // exception/deopt guard's `Reinterpret` resume at this
                    // bci has the args this invokestatic needs, instead of
                    // underflowing on an empty stack.
                    if crate::deopt_real_enabled() {
                        self.snapshot_pre_intrinsic_call(
                            pc,
                            crate::deopt::DeoptReason::ReceiverTypeChanged,
                        );
                    }

                    // Capture spill offset BEFORE popping to prevent
                    // the args buffer from overlapping source Frame slots.
                    let pre_pop_spill = self.next_spill_offset;
                    let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                    // Cursor at the popped-args depth — the reclaim after
                    // the call restores THIS level (not `pre_pop_spill`).
                    // Restoring to pre_pop left the return value parked
                    // n slots above its semantic depth, permanently
                    // inflating the cursor by n per non-void dispatch
                    // site; across a long method the creep walked the
                    // args buffer past `sub rsp, frame_size` into the
                    // callee's stack (Bug 4: testAdHocData's FFT receiver
                    // zeroed by the next helper call's frame).
                    let post_pop_spill = self.next_spill_offset;

                    let args_base_offset = pre_pop_spill;
                    if n > 0 {
                        let Some(args_end) = self.checked_spill_range_end(args_base_offset, n)
                        else {
                            return WalkStep::Return(false);
                        };
                        self.next_spill_offset = args_end;
                        // Store args in reverse offset order so they form
                        // a contiguous ascending-address buffer:
                        //   arg[0] at [rbp - highest_offset] (lowest addr)
                        //   arg[n-1] at [rbp - args_base_offset] (highest addr)
                        // This is necessary because modrm_rbp_disp negates
                        // the offset, so higher offsets map to lower addresses.
                        for (i, slot) in arg_slots.iter().enumerate() {
                            let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(RAX, *slot);
                            self.emit_store_local(buf_offset, RAX);
                            // This argument leaves the simulated operand
                            // stack here; if it is a reference, the
                            // safepoint map below is the only thing that
                            // can still name it.
                            if arg_oops[i] {
                                self.pending_staged_arg_oops.push(buf_offset);
                            }
                        }
                    }
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                    if n > 0 {
                        // LEA to the highest offset = lowest address = start of buffer
                        let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                        self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                    } else {
                        self.emit_xor_reg_self(ARG_REGS[2]);
                    }
                    self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding
                                                                   // Round-8 wave-3: defensive callee-saved spill before any
                                                                   // GC-triggering CALL -- unless the caller frame is
                                                                   // provably oop-clean here. Every argument of this site,
                                                                   // reference or not, was just stored into the helper's
                                                                   // args buffer at `args_base_offset`, and each oop among
                                                                   // them was pushed to `pending_staged_arg_oops` so the map
                                                                   // below NAMES it: `args_frame_resident` is
                                                                   // unconditionally true here, which is a stronger
                                                                   // guarantee than the direct sites' service slots.
                    if self.can_elide_direct_call_register_spill(&arg_oops, true, 2) {
                        self.emit_safepoint_metadata_only();
                    } else {
                        // Every argument is in the helper's args buffer and
                        // the oops among them are named in the map below;
                        // ARG_REGS carry the helper ABI (heap/info/buf/count),
                        // never a Java oop. RAX is published only when the
                        // staging loop -- which writes through RAX -- ran.
                        self.emit_pre_safepoint_spill_args_published(true, n > 0);
                    }
                    self.emit_call_absolute(self.helpers.invoke_dispatch);
                    // T1.1.2 — invoke dispatch is a full safepoint:
                    // the callee may allocate, trigger GC, or throw.
                    // Record an oop map for the operand stack state
                    // that survives the call (args are already popped,
                    // return value not yet pushed).
                    self.emit_oop_map_for_safepoint();

                    // After the dispatch returns, check whether the
                    // static callee threw a Java exception. `jit_invoke_
                    // dispatch` returns `i64::MIN` (and stashes the
                    // exception in `JIT_PENDING_EXCEPTION`) when the
                    // callee throws. Without this guard — which the
                    // invokevirtual/invokespecial paths already have —
                    // the JIT pushes the `i64::MIN` sentinel as the
                    // return value; for an L/[ static method it is then
                    // tagged as an oop (`mark_top_as_oop` below) and the
                    // next deref of that wild `0x8000_0000_0000_0000`
                    // pointer segfaults (the Tomcat boot regression).
                    // Deopt out so the interpreter routes the stashed
                    // exception through the method's exception table.
                    self.emit_post_invoke_exception_check(info_ref.return_type);

                    // Reclaim spill slots used for invoke args AND the
                    // popped arg values — see `post_pop_spill` above.
                    self.next_spill_offset = post_pop_spill;

                    if info_ref.return_type != b'V' {
                        if matches!(info_ref.return_type, b'D' | b'F') {
                            self.push_from_rax_as_xmm0();
                        } else {
                            self.push_from_rax();
                        }
                        // T1.1.2 — the return value is an object
                        // reference iff the descriptor ends in `L`
                        // or `[`. Tag it so the next safepoint
                        // records it as a live oop.
                        if matches!(info_ref.return_type, b'L' | b'[') {
                            self.mark_top_as_oop();
                        }
                    }
                } else {
                    // Self-recursive call (no invoke_info, no direct_call)
                    let n = self.num_params;

                    // Check for tail call: invokestatic self at PC, xreturn at PC+3.
                    // Never inside a try region — the tail form tears this
                    // frame down, so a throw from the self-recursive callee
                    // would bypass the handler covering this pc
                    // (see `pc_is_protected`).
                    // Never when the `xreturn` is a branch target: the
                    // tail form consumes that PC without emitting it, so
                    // another edge into it has no native offset to be
                    // patched to (see the sibling-tail arm above for the
                    // full argument).
                    let is_tail_call = pc + 3 < code_len
                        && matches!(code[pc + 3], 0xac..=0xb0) // ireturn..areturn
                        && !branch_targets[pc + 3]
                        && !self.pc_is_protected(pc)
                        // `-self-tailcall` demotes this to the raw
                        // self-recursive CALL below, restoring one native
                        // frame per activation. Off is the HotSpot-faithful
                        // answer; on is the default.
                        && self_tailcall_enabled();

                    // jit-invokedynamic-groovy-regression fix: a method
                    // containing a live invokedynamic site (compiled as an
                    // unconditional reason-8 trap) must NEVER machine-CALL
                    // its own entry: a trap in the INNER recursive
                    // invocation stashes a frame whose method identity
                    // equals this method's, so the dispatch-helper resume
                    // above this frame could not distinguish the inner
                    // invocation's frame from the outer's and would resume
                    // the wrong one (dropping the outer continuation). The
                    // tail-JMP form below is exempt (it reuses the SAME
                    // frame, so the stash genuinely describes the one live
                    // invocation). For the non-tail raw CALL, bail the
                    // whole compile — correctness first; a self-recursive
                    // method that also contains an invokedynamic is rare
                    // enough that staying interpreted is acceptable.
                    if !self.indy_info.is_empty() && !(is_tail_call && self.body_entry_offset > 0) {
                        return WalkStep::Return(false);
                    }

                    // value-stack-usize-underflow-nio-worker-panic fix:
                    // snapshot the pre-pop operand stack for the non-tail
                    // path below (see the matching invokevirtual/interface
                    // fix elsewhere in this match arm) — the tail-call form
                    // JMPs and never reaches `emit_post_invoke_exception_check`,
                    // so this is a no-op for it beyond the idempotent
                    // `deopt_box_ptr_by_bci` insert.
                    if crate::deopt_real_enabled() {
                        self.snapshot_pre_intrinsic_call(
                            pc,
                            crate::deopt::DeoptReason::ReceiverTypeChanged,
                        );
                    }
                    let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                    // THE SAME CHANNEL THE TWO DIRECT-CALL SITES USE, for the one
                    // invoke arm `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS` did not reach.
                    //
                    // This arm used to raise `pending_staged_args_unmapped` for any
                    // reference argument, on the stated grounds that the value had
                    // been staged "into an area no oop map can name". At the
                    // safepoint that consumes the flag it has not: `pop_invoke_args`
                    // hands back the arguments' FRAME slots, the non-tail form's
                    // stack-guard safepoint and recursive CALL are emitted below
                    // this point, and `emit_stack_arg_setup` — the step that does
                    // move them into the un-nameable outgoing-ABI area — runs later
                    // still. So the slots are live, frame-resident and nameable
                    // exactly where the refusal was being raised.
                    //
                    // Naming them is also what PUBLISHES them:
                    // `collect_live_oop_homes` reads `pending_staged_arg_oops`, so
                    // the shadow stack carries them and the band verifier stops
                    // finding a movable word nothing published. See
                    // `self_call_arg_maps_enabled` for the measurement.
                    //
                    // An argument whose home is NOT a frame slot still fails
                    // closed: a register/scratch/xmm-resident reference is precisely
                    // what a frame-slot map cannot describe.
                    let staged_self_args_mark = self.pending_staged_arg_oops.len();
                    if self_call_arg_maps_enabled() {
                        let mut unnameable = false;
                        for (i, slot) in arg_slots.iter().enumerate() {
                            if !arg_oops.get(i).copied().unwrap_or(false) {
                                continue;
                            }
                            match slot {
                                StackSlot::Frame(off) => {
                                    self.pending_staged_arg_oops.push(*off);
                                }
                                _ => unnameable = true,
                            }
                        }
                        if unnameable {
                            self.pending_staged_args_unmapped = true;
                        }
                    } else if arg_oops.iter().any(|&o| o) {
                        self.pending_staged_args_unmapped = true;
                    }

                    if is_tail_call && self.body_entry_offset > 0 {
                        // Tail-call optimization: load args into parameter locals
                        // and JMP back to body entry (skip prologue)
                        let local_assignments = self.local_assignments.clone();
                        for (i, slot) in arg_slots.iter().enumerate() {
                            if i < n {
                                if let Some(reg) = local_assignments.get(i).copied().flatten() {
                                    self.load_slot_to_reg(reg, *slot);
                                } else {
                                    self.load_slot_to_reg(RAX, *slot);
                                    self.emit_store_local(self.local_offset(i), RAX);
                                }
                            }
                        }
                        // JMP rel32 back to body entry
                        self.buf.emit_byte(0xE9); // JMP rel32
                        let jmp_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        // Patch: target = body_entry_offset
                        let rel = self.body_entry_offset as i32 - (jmp_offset as i32 + 4); // Cast: x86-64 rel32 displacement
                        let pos = self.buf.pos();
                        self.buf.try_patch_i32(jmp_offset, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                        let _ = pos;

                        // THE TAIL FORM EMITS NO SAFEPOINT. The arguments were
                        // just loaded into this method's own parameter locals,
                        // which `local_oop_masks` names from here on, and
                        // `reset_spills` below hands their old slots straight back
                        // to the spill allocator. Anything left pending would be
                        // consumed by a LATER, unrelated safepoint and would name a
                        // slot the cursor has already given to something else —
                        // the mirror of the defect this staging fixes. Truncating
                        // to the mark drops exactly what this site pushed; no
                        // safepoint can have run in between to take them.
                        self.pending_staged_arg_oops.truncate(staged_self_args_mark);

                        // Skip the following xreturn — we already jumped
                        pc += 3; // invokestatic
                        pc += 1; // xreturn
                        self.reset_spills();
                        return WalkStep::Next(pc);
                    }

                    // BUG-1 companion — native-stack headroom guard for the
                    // direct self-recursive CALL below. Historically every
                    // NON-tail self-recursive site was routed through
                    // `jit_invoke_dispatch` purely so the dispatch depth
                    // guard could convert runaway compiled recursion into a
                    // catchable StackOverflowError; that made each recursive
                    // call pay the full dispatch-helper round trip (the
                    // dominant cost of fib/binarytrees-style recursion).
                    // With the dedicated guard helper wired, the site stays
                    // a direct CALL and pays one cheap leaf helper call:
                    //   MOV  ARG0, [RBP - heap_local]   ; vm_ptr
                    //   CALL self_call_stack_guard      ; 0 = ok
                    //   TEST RAX, RAX
                    //   JNZ  merge                      ; RAX = i64::MIN →
                    //                                   ; post-invoke check
                    //                                   ; routes the stashed
                    //                                   ; StackOverflowError
                    // The guard's overflow arm allocates (SOE construction),
                    // so it is bracketed like a call safepoint: defensive
                    // callee-saved spill before, and — under precise/shadow
                    // modes — an oop map (whose shadow RELOAD pairs with the
                    // spill's PUSH) at its return PC. The JNZ target sits
                    // AFTER `emit_stack_arg_cleanup`, so the overflow path
                    // skips arg setup + CALL + cleanup as one balanced unit
                    // (no RSP adjustment happens on that path). `arg_slots`
                    // are call-safe across the guard CALL: the
                    // `flush_scratch_registers()` at the 0xb8 arm entry
                    // moved scratch GPR/XMM stack slots into frame slots.
                    // Requires `needs_heap` (the routing in `try_compile`
                    // sets it for every raw-routed self-call site) — the
                    // vm_ptr frame slot is what the guard is called with.
                    // Unwired helper (tests, historical callers): emits
                    // nothing, byte-identical legacy code.
                    let guard_skip_patch =
                        if self.helpers.self_call_stack_guard != 0 && self.needs_heap {
                            // perf/throughput-20260710 -- INLINE floor fast
                            // path: the prologue cached this thread's
                            // native-stack floor in a frame slot, so the
                            // common case is:
                            //   CMP RSP, [rbp - floor_slot]
                            //   JA  <skip helper guard>   (headroom ok)
                            // OSR-entered frames have the slot initialised
                            // to usize::MAX by the trampoline (`RSP > MAX`
                            // is unsatisfiable), so they always take the
                            // helper. The skipped block is the guard CALL
                            // plus its safepoint spill and shadow
                            // push/reload bracketing (skipped TOGETHER, so
                            // the shadow stack stays balanced); the
                            // recursive CALL below still emits its own
                            // spill + oop map, so GC coverage of the
                            // actual recursion is unchanged. This constant
                            // was the dominant per-level cost of
                            // fib/binarytrees-style recursion.
                            let fast_skip = if self.stack_floor_slot_off != 0 {
                                self.emit_cmp_r64_rbp_local(RSP, self.stack_floor_slot_off);
                                Some(self.emit_jcc_rel32_patch(0x87)) // JA
                            } else {
                                None
                            };
                            if self.gc_inert_selfrec {
                                self.emit_pre_safepoint_spill_without_shadow();
                            } else {
                                self.emit_pre_safepoint_spill();
                            }
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.emit_call_absolute(self.helpers.self_call_stack_guard);
                            if self.precise_maps || self.shadow_enabled {
                                self.emit_oop_map_for_safepoint();
                            }
                            self.emit_test_r64_r64(RAX);
                            let jne_merge = Some(self.emit_jcc_rel32_patch(0x85)); // JNE merge
                            if let Some(skip) = fast_skip {
                                self.patch_rel32_to_here(skip);
                            }
                            jne_merge
                        } else {
                            None
                        };
                    // Round-8 wave-3 HIGH fix: stack-arg setup for
                    // self-recursive direct calls past ARG_REGS.
                    let total_sub = self.emit_stack_arg_setup(&arg_slots, self.needs_heap);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before the recursive CALL (which transitively
                    // can allocate and reach a GC safepoint).
                    if self.gc_inert_selfrec {
                        // The callee is this same poll-free,
                        // allocation-free method. The direct edge is not a
                        // safepoint, so publishing roots here would be pure
                        // overhead. The overflow helper above retains its
                        // cold safepoint protocol.
                    } else if self.can_elide_self_call_register_spill() {
                        self.emit_safepoint_metadata_only();
                    } else {
                        self.emit_pre_safepoint_spill();
                    }
                    // Normal self-call via CALL (rel32, patched
                    // post-emission).
                    self.buf.emit_byte(0xE8);
                    let call_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.self_call_patches.push(call_patch);
                    self.emit_post_call_rbp_republish();
                    // Stage A (precise oop maps, B-K fix) — a self-recursive
                    // compiled call IS a GC-capable safepoint (the callee
                    // allocates: this is exactly bintrees18's recursive
                    // `make()`). The neighbour direct/dispatch invoke sites
                    // already pair `emit_pre_safepoint_spill` with an oop map
                    // at the return PC; this site historically spilled but
                    // recorded NO map, so the precise relocation path could
                    // not remap this frame (`remap_one_jit_frame` found no
                    // entry for the active sp-id) — the documented gap #1.
                    // Gated behind `precise_maps`: emits zero bytes and no
                    // metadata on the default path, so it is byte-identical
                    // gate-OFF; gate-ON it records the map AND the paired
                    // post-safepoint register reload (Stage 4 / G5).
                    // Also required under `shadow_enabled`: the shadow-stack
                    // PUSH happened in `emit_pre_safepoint_spill`, so its
                    // paired RELOAD (inside `emit_oop_map_for_safepoint`) must
                    // run here too, else the shadow stack grows unbalanced
                    // through this recursive call → unbounded pinning → OOM.
                    if !self.gc_inert_selfrec && (self.precise_maps || self.shadow_enabled) {
                        self.emit_oop_map_for_safepoint();
                    }
                    self.emit_stack_arg_cleanup(total_sub);
                    // Stack-guard merge point: the overflow JNE lands here
                    // with RAX = i64::MIN, flowing straight into the
                    // sentinel check below (exactly as if the callee threw).
                    if let Some(p) = guard_skip_patch {
                        self.patch_rel32_to_here(p);
                    }
                    // A self-recursive compiled call that throws (or
                    // deopts) returns the `i64::MIN` sentinel — same
                    // hazard as the direct/dispatch invokestatic paths
                    // above. Guard it so the sentinel is never pushed
                    // (and never tagged as an oop) as a return value.
                    //
                    // The callee here IS this method, so its return type is
                    // the method's own — but single-pass does not thread the
                    // method descriptor into the Compiler, so we pass `b'I'`
                    // (the plain `CMP; JE` check, byte-identical to before).
                    // Consequence: a self-recursive `long`/`double` method
                    // that *legitimately* returns `Long.MIN_VALUE` retains
                    // the pre-existing `i64::MIN` collision at THIS site
                    // only. That is a rare corner (cross-method J/D calls —
                    // the real unblock — go through the dispatch/direct sites
                    // above, which ARE disambiguated). Left as a documented
                    // follow-up rather than threading a new descriptor param
                    // through every `x64::compile` caller.
                    self.emit_post_invoke_exception_check(b'I');
                    // VOID self-recursive fix (ES SortingDigestTests -Jit
                    // on): this push was unconditional, so a `void`
                    // self-recursive callee (DualPivotQuicksort.sort) left
                    // a PHANTOM entry on the simulated operand stack after
                    // every non-tail self-call. The extra entry shifts the
                    // canonical spill-slot layout for everything downstream
                    // of the next merge point, so later loads read
                    // neighbouring slots (an array ref read as a double →
                    // heap addresses stored into double[] elements,
                    // deterministic mis-sorts and garbage AIOOBE indices —
                    // no GC involved). The method's own return type IS
                    // available via `method_key` ("Class.name:descriptor"),
                    // so only push a return value when there is one. When
                    // `method_key` is empty (unit-test compiles) keep the
                    // historical push — those callers never compile void
                    // self-recursive methods.
                    let self_ret_ty = self
                        .method_key
                        .rfind(')')
                        .and_then(|i| self.method_key.as_bytes().get(i + 1))
                        .copied();
                    if self_ret_ty != Some(b'V') {
                        self.push_from_rax();
                        // The callee IS this method, so its return type is
                        // the method's own. A reference result MUST be
                        // tagged: `collect_live_oop_homes` publishes only
                        // marked operand entries, so an untagged reference
                        // left on the operand stack across a later
                        // GC-capable call is invisible to the shadow stack
                        // — while `moving_young_safepoint_coverage_complete`
                        // still certifies the frame, because it only checks
                        // that MARKED entries have frame/register homes.
                        //
                        // That combination is the measured heap corruption
                        // in `moving-young-gen-drops-jit-held-oops-FIXED.md`:
                        // `BinTreesClassic.bottomUpTree` keeps the result of
                        // its first recursive call — an entire subtree — on
                        // the operand stack across its second, and a moving
                        // young cycle neither marked nor rewrote it.
                        match self_ret_ty {
                            Some(b'L') | Some(b'[') => self.mark_top_as_oop(),
                            // No descriptor (the legacy `compile` test
                            // wrapper passes an empty `method_key`): we
                            // cannot tell whether this is a reference, so
                            // the mark vector is no longer exact and this
                            // frame must not certify moving-young coverage.
                            // Fail-closed costs a non-moving cycle; guessing
                            // costs the heap.
                            None => self.stack_oop_marks_exact = false,
                            _ => {}
                        }
                    }
                }
                pc += 3;
            }
            _ => {
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }

    /// Lower `invokevirtual`, `invokespecial` and `invokeinterface` at `pc`.
    #[allow(clippy::too_many_arguments)]
    fn walk_invoke_instance(
        &mut self,
        _code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // invokevirtual / invokespecial / invokeinterface — direct call or dispatch helper
            0xb6 | 0xb7 | 0xb9 => {
                // Scalar replacement: skip <init>()V on scalar-replaced objects
                if op == 0xb7 && self.scalar_init_skips.contains(&pc) {
                    let _ = self.pop_stack(); // discard dup'd receiver
                    pc += 3;
                    return WalkStep::Next(pc);
                }
                // Elide `invokespecial java/lang/Object.<init>()V` — the
                // terminal of every constructor chain. The method body is
                // a bare `return` and the VM-side registration is
                // `native_noop_with_this`, so the call has no observable
                // effect. The site can never become a direct call or an
                // inline site (`<init>` + native-shadow compile gates), so
                // without this it falls to `jit_invoke_dispatch`'s
                // interpreter slow path once per object allocation —
                // dominant on allocation-heavy code (bintrees18: ~69M
                // dispatches of an empty method).
                if op == 0xb7 {
                    if let Some(&idx) = self.invoke_info_idx.get(&pc) {
                        // SAFETY: invoke_info pointers are owned by the
                        // enclosing try_compile scope and outlive codegen.
                        let info = unsafe { &*self.invoke_info[idx].1 };
                        if info.invoke_kind == 1
                            && info.descriptor == "()V"
                            && info.method_name == "<init>"
                            && info.class_name == "java/lang/Object"
                        {
                            let _ = self.pop_stack(); // discard receiver
                            pc += 3;
                            return WalkStep::Next(pc);
                        }
                    }
                }
                self.flush_scratch_registers();

                // Check for inline site (invokespecial only — virtual/interface not eligible)
                if op == 0xb7 && self.inline_sites.contains_key(&pc) {
                    if self.try_emit_inline(pc) {
                        pc += 3;
                        return WalkStep::Next(pc);
                    }
                }

                // PGO-02 (docs/feature-designs/profile-guided-inlining.md):
                // guarded MONOMORPHIC virtual/interface inline. `inline_sites`
                // + `inline_guard_variants` are populated TOGETHER, only for
                // an admitted `InlineVerdict::Monomorphic` plan, only when
                // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is on (see
                // `InlineBackendCaps` in jit/src/lib.rs) — with the flag off
                // `inline_guard_variants` is always empty and this whole
                // block costs one HashMap probe. Splices the callee body via
                // the SAME `try_emit_inline` the invokespecial check above
                // already uses, behind a receiver class-id guard; the miss
                // edge falls through UNCHANGED to the normal dispatch code
                // below (MIC/PIC/`jit_invoke_dispatch`) — never a deopt (see
                // the design doc's §3 deopt-safety argument: no caller
                // scopes are populated, so this relies on — and does not
                // change — the existing guarantee that nothing inside an
                // inlined body publishes a deopt point).
                let mut guarded_virtual_done_patches: Vec<usize> = Vec::new();
                if op != 0xb7 {
                    if let Some(variants) = self.inline_guard_variants.get(&pc).cloned() {
                        // Every variant is the SAME call site, so every
                        // body pops the same operand shape. A disagreement
                        // means the two were resolved from different
                        // descriptors, which this lowering has no model
                        // for — refuse the whole site rather than emit two
                        // guards over two different stack effects.
                        let recv_depth = variants
                            .first()
                            .map(|(_, site)| site.callee_num_args)
                            .unwrap_or(0);
                        let uniform = !variants.is_empty()
                            && variants
                                .iter()
                                .all(|(_, site)| site.callee_num_args == recv_depth);
                        if uniform && recv_depth >= 1 && self.stack.len() >= recv_depth {
                            let recv_slot = self.stack[self.stack.len() - recv_depth];

                            // Full state snapshot from BEFORE any guard byte
                            // is emitted — mirrors try_emit_inline's own
                            // checkpoint set exactly (see its comment on the
                            // groovyjarjarasm-asm-handler-getexceptiontablesize
                            // fix for why every one of these fields matters),
                            // so a rewind on either failure path below is
                            // indistinguishable from never having attempted
                            // this guard at all.
                            let buf_checkpoint = self.buf.pos();
                            let stack_checkpoint = self.stack.clone();
                            let oop_marks_checkpoint = self.stack_oop_marks.clone();
                            let spill_checkpoint = self.next_spill_offset;
                            let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
                            let deopt_stubs_checkpoint = self.deopt_stubs.len();
                            let forward_patches_checkpoint = self.forward_patches.len();
                            let jump_table_patches_checkpoint = self.jump_table_patches.len();
                            let self_call_patches_checkpoint = self.self_call_patches.len();
                            let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
                            let null_check_store_stubs_checkpoint =
                                self.null_check_store_stubs.len();

                            // The receiver is loaded and null-checked ONCE,
                            // ahead of the guard chain: `null` fails every
                            // guard, and re-testing it per variant would be
                            // pure code size. Peeked, not popped —
                            // try_emit_inline_site does its own popping on
                            // each hit path, and the miss tail needs the
                            // receiver+args untouched for the normal-dispatch
                            // code that runs next.
                            self.load_slot_to_reg(RAX, recv_slot);
                            self.emit_test_r64_r64(RAX);
                            let null_miss_patch = self.emit_jcc_rel32_patch(0x84); // JZ

                            // The guard chain. Variant k's mismatch edge
                            // lands at variant k+1's `CMP`; the last one's
                            // lands at the normal-dispatch code below. A hit
                            // jumps PAST that code entirely.
                            let mut pending_miss: Option<usize> = None;
                            let mut spliced_any = false;
                            for (guard_class_id, site) in &variants {
                                // Per-variant checkpoint: if THIS body cannot
                                // be spliced, only this variant's bytes are
                                // rewound — the ones already emitted for
                                // earlier variants stay.
                                let variant_buf_checkpoint = self.buf.pos();
                                let variant_exception_stubs = self.exception_check_stubs.len();
                                let variant_deopt_stubs = self.deopt_stubs.len();
                                let variant_forward_patches = self.forward_patches.len();
                                let variant_jump_table_patches = self.jump_table_patches.len();
                                let variant_self_call_patches = self.self_call_patches.len();
                                let variant_bounds_stubs = self.bounds_check_stubs.len();
                                let variant_null_store_stubs = self.null_check_store_stubs.len();

                                // Land the previous variant's mismatch edge
                                // exactly here. If this variant then fails and
                                // rewinds, the same offset becomes the start of
                                // whatever is emitted next — the following
                                // variant's `CMP`, or the normal-dispatch code
                                // — which is the correct landing spot either
                                // way.
                                if let Some(prev) = pending_miss.take() {
                                    self.patch_rel32_to_here(prev);
                                }

                                // CMP DWORD [RAX+0], guard_class_id —
                                // identical encoding to the String/CRC32
                                // intrinsic guard above (81 /7 id, ModRM 0x78
                                // = mod00 /7 rm=RAX).
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                let this_miss = self.emit_jcc_rel32_patch(0x85); // JNE

                                if self.try_emit_inline_site(pc, site) {
                                    // Hit: skip every later guard AND the
                                    // normal-dispatch bytes entirely.
                                    guarded_virtual_done_patches.push(self.emit_jmp_rel32_patch());
                                    spliced_any = true;
                                    pending_miss = Some(this_miss);
                                    // The inline body consumed the
                                    // receiver+args and pushed its result via
                                    // the same push_from_rax /
                                    // push_from_rax_as_xmm0 convention the
                                    // normal-dispatch code below also uses.
                                    // Restore the compiler's SYMBOLIC state
                                    // (not the already-emitted bytes) to
                                    // exactly what it was before the guard
                                    // chain, so the NEXT variant sees the same
                                    // operand stack this one did, and so the
                                    // dispatch code — the only Rust-level
                                    // continuation from here, run
                                    // unconditionally — pops the SAME
                                    // receiver+args positions and pushes a
                                    // canonically-shaped result regardless of
                                    // which machine-code path a given
                                    // execution actually takes at runtime.
                                    self.stack = stack_checkpoint.clone();
                                    self.stack_oop_marks = oop_marks_checkpoint.clone();
                                    self.next_spill_offset = spill_checkpoint;
                                } else {
                                    // try_emit_inline_site already rolled back
                                    // its OWN side effects; rewind this
                                    // variant's guard bytes too, so the site is
                                    // byte-identical to never having offered
                                    // this variant.
                                    self.buf.rewind_to(variant_buf_checkpoint);
                                    self.stack = stack_checkpoint.clone();
                                    self.stack_oop_marks = oop_marks_checkpoint.clone();
                                    self.next_spill_offset = spill_checkpoint;
                                    self.exception_check_stubs.truncate(variant_exception_stubs);
                                    self.deopt_stubs.truncate(variant_deopt_stubs);
                                    self.forward_patches.truncate(variant_forward_patches);
                                    self.jump_table_patches.truncate(variant_jump_table_patches);
                                    self.self_call_patches.truncate(variant_self_call_patches);
                                    self.bounds_check_stubs.truncate(variant_bounds_stubs);
                                    self.null_check_store_stubs
                                        .truncate(variant_null_store_stubs);
                                }
                            }

                            if spliced_any {
                                // Every remaining miss edge — the null check
                                // and the last guard — lands at the exact
                                // start of the UNCHANGED normal-dispatch code
                                // that runs next.
                                self.patch_rel32_to_here(null_miss_patch);
                                if let Some(last) = pending_miss {
                                    self.patch_rel32_to_here(last);
                                }
                            } else {
                                // No variant could be spliced: rewind the
                                // shared receiver load and null check too, so
                                // the fall-through is byte-identical to never
                                // having attempted a guard.
                                self.buf.rewind_to(buf_checkpoint);
                                self.stack = stack_checkpoint;
                                self.stack_oop_marks = oop_marks_checkpoint;
                                self.next_spill_offset = spill_checkpoint;
                                self.exception_check_stubs
                                    .truncate(exception_check_stubs_checkpoint);
                                self.deopt_stubs.truncate(deopt_stubs_checkpoint);
                                self.forward_patches.truncate(forward_patches_checkpoint);
                                self.jump_table_patches
                                    .truncate(jump_table_patches_checkpoint);
                                self.self_call_patches
                                    .truncate(self_call_patches_checkpoint);
                                self.bounds_check_stubs
                                    .truncate(bounds_check_stubs_checkpoint);
                                self.null_check_store_stubs
                                    .truncate(null_check_store_stubs_checkpoint);
                            }
                        }
                    }
                }

                // The direct exceptional-return service needs the same
                // invoke metadata as the normal dispatch fallback.
                let info_ptr = self
                    .invoke_info_idx
                    .get(&pc)
                    .map(|&i| self.invoke_info[i].1);
                // Check for direct call target (invokespecial with compiled callee)
                // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                let direct = self.direct_calls_idx.get(&pc).map(|&i| {
                    let dc = &self.direct_calls[i].1;
                    note_emit_direct(&self.method_key, pc, dc.entry);
                    (
                        dc.entry,
                        dc.needs_context,
                        dc.num_params,
                        dc.return_type,
                        dc.guard_class_id,
                    )
                });

                // Receiver-type speculation consults the per-bci de-spec
                // registry. A call-site intrinsic with `guard_class_id != 0`
                // (the `java/lang/CharSequence` String family, the CRC32
                // family, the atomic-field family) is a SPECULATION: the
                // inline body it emits is valid for exactly that one
                // receiver class and every other receiver takes the
                // `ReceiverTypeChanged` deopt edge. On a site whose receiver
                // is NEVER that class the guard fails on every single call,
                // so the method deopts per invocation, is recompiled with
                // the identical guard, and is finally barred from
                // compilation altogether by `recommend_action`'s
                // `MakeNotCompilable` escalation -- for a speculation the
                // compiler chose, about a program that never satisfied it.
                // netty's `HttpHeaderValidationUtil.validateValidHeaderValue`
                // is that shape: its `CharSequence.length()` at bci 1 is
                // reached only with `AsciiString` and an anonymous
                // `CharSequence`, and the class measured 1.67 deopts per
                // loop iteration (docs/known-issues/netty/httpheader-
                // validationutiltest-exhaustive-loop-timeout-20260816.md).
                //
                // `real_frame_deopt_resume_and_despeculate` already records
                // such a bci in the de-spec registry after
                // `PER_BCI_DESPEC_LIMIT` deopts and prints "speculation
                // suppressed on next compile" -- but until now nothing on
                // the receiver-guard path READ that registry (only the two
                // loop-hoist gates in `driver.rs` and the
                // `ArraycopyPrimitive` intrinsic below did), so the claim
                // was false and the next compile emitted the same guard.
                // The consult belongs at the RESOLVER, not here, and this
                // block counts rather than declines. Measured 2026-08-24,
                // and it cost most of a session: declining a registered
                // intrinsic in THIS filter does not send the site to the
                // `else` arm's MIC/PIC dispatch, because that arm needs
                // `invoke_info` at this pc and there is none. A site the
                // resolver registered as an intrinsic took
                // `direct_calls.push(..); continue;` in `try_compile_inner`
                // BEFORE the `invoke_info.push` below it, so the dispatch
                // metadata was never built. With both `direct` and
                // `info_ptr` `None` the emitter falls through to the
                // unconditional `UnreachedCode` trap at the bottom of this
                // arm -- so the "declined" site deopts on EVERY execution
                // instead of dispatching. The de-spec consult read as inert
                // (deopts 3502 -> 3055 on `HeaderValidationLoopRate`) while
                // its own `sites-declined` counter said it had fired 51
                // times; only correlating the decline trace against the
                // deopt stream separated the two -- 33 declines at
                // `oldHeaderValueValidationAlgorithm pc=6` and 1466 deopts
                // at that same bci AFTERWARDS.
                //
                // The pre-existing `ArraycopyPrimitive` and
                // `StringIndexOfChar` filters in the invokestatic ladder
                // above have the identical shape and therefore the identical
                // defect; they are left alone here because each needs its
                // own A/B, and are named on the known-issue page.
                let direct = direct.filter(|&(_, _, _, _, guard_class_id)| {
                    if guard_class_id == 0 {
                        crate::metrics::note_receiver_despec(
                            crate::metrics::RECEIVER_DESPEC_UNGUARDED,
                        );
                        return true;
                    }
                    crate::metrics::note_receiver_despec(
                        crate::metrics::RECEIVER_DESPEC_GUARD_EMITTED,
                    );
                    true
                });

                if let Some((
                    callee_entry,
                    callee_needs_ctx,
                    callee_params,
                    ret_type,
                    guard_class_id,
                )) = direct
                {
                    // --- invokevirtual/special/interface intrinsic ladder ---
                    // Instance-method call-site intrinsics (String / CRC32)
                    // are dispatched here BEFORE the plain direct-call
                    // handling below. Unlike the invokestatic ladder,
                    // instance intrinsics treat the deepest stack operand
                    // as the receiver (`this`): the JLS argument count is
                    // `callee_params`, and total operands popped is
                    // `callee_params + 1`.
                    //
                    // A follow-up agent for family <TAG> fills exactly one
                    // region with `if callee_entry ==
                    // crate::JitIntrinsic::Foo.as_entry() {
                    //     <emit inline code>; <handled = true>; }`.
                    // When every region is empty `intrinsic_handled`
                    // stays false and control falls through to the
                    // unchanged plain direct-call path.
                    #[allow(unused_mut)]
                    let mut intrinsic_handled = false;

                    // ===== INTRINSIC REGION BEGIN: FFM_SEGMENT =====
                    // `MemorySegment.getAtIndex`/`setAtIndex`, lowered to a
                    // CALL of the FFM element fast-path helper with a
                    // DECLINE edge that runs the site's ordinary native
                    // dispatch.
                    //
                    // Through that ordinary dispatch these measure ~1158
                    // ns/element against ~0.8 ns for a `short[]` element,
                    // and they are per-ELEMENT: any segment-backed array
                    // drives one per element.
                    //
                    // Not inline machine code, deliberately. The carrier's
                    // liveness model spans two synthetic classes owned by
                    // two different files, and a second copy of one of its
                    // slot indices has already made that check silently
                    // DEAD once (the W7-89 note on `PE_ARENA_CLASS`). The
                    // helper asks the NATIVE for a verdict instead of
                    // re-deriving one — see
                    // `cratonvm_native_builtins::ffm_fast`.
                    //
                    // The decline edge is what makes this safe to be wrong
                    // about: helper returns 0 and control falls into the
                    // same `invoke_dispatch` the site would have used
                    // anyway, so a heap-backed carrier, a closed scope, an
                    // out-of-bounds index or an unrecognised shape all keep
                    // today's behaviour AND today's exceptions. Nothing
                    // here has to reproduce an exception.
                    if !intrinsic_handled
                        && (callee_entry == crate::JitIntrinsic::FfmSegmentGetAtIndex.as_entry()
                            || callee_entry == crate::JitIntrinsic::FfmSegmentSetAtIndex.as_entry())
                    {
                        let is_get =
                            callee_entry == crate::JitIntrinsic::FfmSegmentGetAtIndex.as_entry();
                        let helper = if is_get {
                            self.helpers.ffm_segment_get
                        } else {
                            self.helpers.ffm_segment_set
                        };
                        // The site's own dispatch info: the decline edge's
                        // call target, and the source of the element kind
                        // (the descriptor NAMES the `ValueLayout` subtype,
                        // so the width is a compile-time constant).
                        let info_ptr = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        // SAFETY: the pointee is owned by this compile's
                        // `_jit_invoke_infos` arena and outlives the code.
                        let kind = info_ptr.and_then(|p| {
                            if p.is_null() {
                                None
                            } else {
                                // SAFETY: `p` is non-null here and owned by this compile's `_jit_invoke_infos` arena, which outlives the code.
                                crate::ffm_kind_for_descriptor(unsafe { (*p).descriptor })
                            }
                        });
                        match (info_ptr, kind) {
                            (Some(info), Some(kind)) if helper != 0 && !info.is_null() => {
                                // SAFETY: the pointee is owned by this
                                // compile's `_jit_invoke_infos` arena and
                                // outlives the code being emitted.
                                let ret_tag = unsafe { (*info).return_type };
                                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_FFM") {
                                    eprintln!("[ffm] EMITTED pc={pc} kind={kind} is_get={is_get}");
                                }
                                self.flush_scratch_registers();
                                // Operands, deepest first: receiver, layout,
                                // index, and (set only) the value.
                                let value_slot = if is_get { None } else { Some(self.pop_stack()) };
                                let index_slot = self.pop_stack();
                                let layout_slot = self.pop_stack();
                                let recv_slot = self.pop_stack();

                                // One slot for the helper's out-parameter
                                // (get only). Reserved BEFORE the decline
                                // edge's argument buffer so reclaiming that
                                // buffer cannot free this.
                                let out_base = if is_get {
                                    match self.reserve_spill_slots(1, SpillReason::HelperArgs) {
                                        Some(b) => Some(b),
                                        None => {
                                            self.fail("singlepass-codegen/ffm-out-spill-exhausted");
                                            return WalkStep::Return(false);
                                        }
                                    }
                                } else {
                                    None
                                };

                                // ---- fast path -------------------------
                                // No safepoint spill and no oop map: the
                                // helper neither allocates nor blocks, so no
                                // GC can run inside it and no oop it is
                                // handed can move.
                                self.load_slot_to_reg(ARG_REGS[0], recv_slot);
                                self.load_slot_to_reg(ARG_REGS[1], index_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], kind as i32);
                                // arg3 is the out-pointer for a get and the
                                // value for a set. `is_get` already pinned
                                // which of the two is `Some`, but this file
                                // routes every recoverable case through a
                                // bail rather than a panic (see the
                                // `deny(...)` header) — so a shape that
                                // cannot arise fails the COMPILE, which
                                // drops the method to the interpreter.
                                match (out_base, value_slot) {
                                    (Some(out), _) => self.emit_lea_frame_slot(ARG_REGS[3], out),
                                    (None, Some(v)) => self.load_slot_to_reg(ARG_REGS[3], v),
                                    (None, None) => {
                                        self.fail("singlepass-codegen/ffm-missing-arg3-operand");
                                        return WalkStep::Return(false);
                                    }
                                }
                                self.emit_call_absolute(helper);
                                self.emit_test_r64_r64(RAX);
                                // RAX == 0 -> declined.
                                let declined = self.emit_jcc_rel32_patch(0x84);
                                if let Some(out) = out_base {
                                    self.emit_load_local(RAX, out);
                                }
                                let done = self.emit_jmp_rel32_patch();

                                // ---- decline edge: the unchanged dispatch
                                self.patch_rel32_to_here(declined);
                                let nargs = if is_get { 3 } else { 4 };
                                let args_base = match self
                                    .reserve_spill_slots(nargs, SpillReason::HelperArgs)
                                {
                                    Some(b) => b,
                                    None => {
                                        self.fail("singlepass-codegen/ffm-args-spill-exhausted");
                                        return WalkStep::Return(false);
                                    }
                                };
                                // `jit_invoke_dispatch`'s buffer runs
                                // arg[0] at the HIGHEST offset down to
                                // arg[n-1] at the lowest — the same layout
                                // the generic dispatch site builds. Load
                                // every operand into a distinct register
                                // before storing any of them: the buffer can
                                // overlap the operand homes, and a
                                // load-then-store per index would overwrite
                                // a home not yet read.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.load_slot_to_reg(RCX, layout_slot);
                                self.load_slot_to_reg(RDX, index_slot);
                                if let Some(v) = value_slot {
                                    self.load_slot_to_reg(R10, v);
                                }
                                let top = args_base + (nargs as i32 - 1) * 8;
                                self.emit_store_local(top, RAX);
                                self.emit_store_local(top - 8, RCX);
                                self.emit_store_local(top - 16, RDX);
                                if value_slot.is_some() {
                                    self.emit_store_local(top - 24, R10);
                                }
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64);
                                self.emit_lea_frame_slot(ARG_REGS[2], top);
                                self.emit_mov_imm32_sx(ARG_REGS[3], nargs as i32);
                                self.emit_pre_safepoint_spill();
                                self.emit_call_absolute(self.helpers.invoke_dispatch);
                                self.emit_oop_map_for_safepoint();
                                // The site's REAL return tag, not `b'I'`.
                                // `emit_post_invoke_exception_check` has a
                                // separate arm for `J`/`D`/`F` because the
                                // pending-exception sentinel is `i64::MIN`,
                                // which is also a LEGITIMATE value for those
                                // widths — `-0.0` as a double is exactly
                                // that word. Passing `b'I'` took the plain
                                // `CMP RAX, i64::MIN; JE bail` arm and would
                                // have mistaken such a value for a throw.
                                self.emit_post_invoke_exception_check(if is_get {
                                    ret_tag
                                } else {
                                    b'V'
                                });

                                // ---- join ------------------------------
                                self.patch_rel32_to_here(done);
                                // Reclaim both the argument buffer and the
                                // out slot; RAX already carries whichever
                                // path ran.
                                self.next_spill_offset =
                                    out_base.unwrap_or(args_base).min(args_base);
                                if is_get {
                                    // Both arms converge with the value's
                                    // RAW BITS in RAX — the helper returns
                                    // them that way and `invoke_dispatch`
                                    // already did — so one push serves the
                                    // fast path and the decline edge. A
                                    // float/double has to reach an XMM
                                    // stack slot, which is the same
                                    // `MOVQ XMM0, RAX` the generic dispatch
                                    // emits for those return types.
                                    if matches!(ret_tag, b'F' | b'D') {
                                        self.push_from_rax_as_xmm0();
                                    } else {
                                        self.push_from_rax();
                                    }
                                }
                                intrinsic_handled = true;
                            }
                            // Falling through here is NOT safe: the site
                            // carries an intrinsic SENTINEL as its
                            // `JitDirectCall::entry`, and the ordinary
                            // direct-call path would CALL that sentinel
                            // (measured: `EXCEPTION_ACCESS_VIOLATION at
                            // pc=0xFFFFFFFFFFFFFFB1`). Registration and
                            // emission are gated on the same
                            // `ffm_kind_for_descriptor`, so reaching this
                            // arm means they disagreed — bail the whole
                            // compile, which drops the method to the
                            // interpreter and is always safe.
                            _ => {
                                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_FFM") {
                                    eprintln!(
                                        "[ffm] UNHANDLED pc={pc} info={} kind={:?} helper={}",
                                        info_ptr.is_some(),
                                        kind,
                                        helper != 0
                                    );
                                }
                                self.fail("singlepass-codegen/ffm-unhandled-sentinel");
                                return WalkStep::Return(false);
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: FFM_SEGMENT =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
                    // `AtomicInteger` RMW family, emitted as ONE
                    // `LOCK XADD [value], ECX`.
                    //
                    // `XADD` atomically adds the source register to the
                    // destination and leaves the PRE-add value in the
                    // source, which is exactly `getAndAdd` semantics; the
                    // `*AndGet` forms add the delta back afterwards. That
                    // replaces a full native dispatch (~250 ns/op measured)
                    // with a single locked instruction.
                    //
                    // Soundness rests on three things:
                    //   * the registered native keeps its state in the SAME
                    //     memory (`get_field_volatile(this, 0)` /
                    //     `compare_and_swap_field(this, 0, ..)`), so an
                    //     interpreted caller and a compiled caller still
                    //     agree on one location;
                    //   * the receiver class-id guard below — AtomicInteger
                    //     is not final, so a subclass override must NOT take
                    //     this path;
                    //   * the per-object COMPACT/LEGACY branch, the same one
                    //     `emit_load_string_i32_field` uses, because a class
                    //     with a registered `CompactLayout` may still have
                    //     legacy-laid-out instances.
                    // Every uncertain case (null receiver, class mismatch)
                    // goes to the shared uncommon-trap stub and re-runs in
                    // the interpreter, which reproduces the NPE exactly.
                    if !intrinsic_handled {
                        // (delta_imm, return_post_add, delta_is_arg)
                        //
                        // `AtomicIntGet` is the one arm with no delta at
                        // all: it reads the same slot the RMW forms address
                        // and returns it. Everything before the final
                        // instruction -- null check, class guard, the
                        // per-object COMPACT/LEGACY branch, the deopt stub
                        // -- is shared, which is the whole reason it belongs
                        // in this block rather than beside it.
                        let is_load = callee_entry == crate::JitIntrinsic::AtomicIntGet.as_entry();
                        let plan: Option<(i32, bool, bool)> = if is_load {
                            Some((0, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntGetAndIncrement.as_entry()
                        {
                            Some((1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntGetAndDecrement.as_entry()
                        {
                            Some((-1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntIncrementAndGet.as_entry()
                        {
                            Some((1, true, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicIntDecrementAndGet.as_entry()
                        {
                            Some((-1, true, false))
                        } else if callee_entry == crate::JitIntrinsic::AtomicIntGetAndAdd.as_entry()
                        {
                            Some((0, false, true))
                        } else if callee_entry == crate::JitIntrinsic::AtomicIntAddAndGet.as_entry()
                        {
                            Some((0, true, true))
                        } else {
                            None
                        };
                        if let Some((delta_imm, return_post_add, delta_is_arg)) = plan {
                            // Recomputed from the same two inputs the
                            // matcher used; `None` here cannot happen for a
                            // registered site, and bailing keeps the plain
                            // direct-call path rather than emitting a CALL
                            // to an intrinsic sentinel.
                            if let Some(layout) =
                                crate::AtomicIntFieldLayout::new(0, guard_class_id)
                            {
                                self.flush_scratch_registers();
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::ReceiverTypeChanged,
                                    );
                                }
                                let mut bail: Vec<usize> = Vec::new();
                                // Operands: delta (if any) is shallower,
                                // the receiver is deepest.
                                let delta_slot = if delta_is_arg {
                                    Some(self.pop_stack())
                                } else {
                                    None
                                };
                                let recv_slot = self.pop_stack();

                                // RAX = receiver; null → deopt.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // Exact receiver class guard:
                                // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // EDX = delta (kept for the *AndGet fixup,
                                // since XADD overwrites its source with the
                                // pre-add value).
                                if !is_load {
                                    match delta_slot {
                                        Some(slot) => self.load_slot_to_reg(RDX, slot),
                                        None => {
                                            self.buf.emit(&[0xBA]); // MOV EDX, imm32
                                            self.buf.emit(&delta_imm.to_le_bytes());
                                        }
                                    }
                                    self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
                                }

                                // Per-object layout branch.
                                self.emit_test_mem8_imm8(
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    cratonvm_types::GC_FLAG_COMPACT,
                                );
                                let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                if is_load {
                                    // MOV ECX, [RAX + compact]. A plain load
                                    // is the correct volatile/acquire read on
                                    // x86-64: loads are not reordered with
                                    // older loads, so nothing is owed here.
                                    self.buf.emit(&[0x8B, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + compact], ECX
                                    self.buf.emit(&[0xF0, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                }
                                let done = self.emit_jmp_rel32_patch();
                                self.patch_rel32_to_here(legacy);
                                if is_load {
                                    // MOV ECX, [RAX + legacy]
                                    self.buf.emit(&[0x8B, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + legacy], ECX
                                    self.buf.emit(&[0xF0, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                }
                                self.patch_rel32_to_here(done);

                                // ECX now holds the PRE-add value.
                                if return_post_add {
                                    self.buf.emit(&[0x01, 0xD1]); // ADD ECX, EDX
                                }
                                self.buf.emit(&[0x48, 0x63, 0xC1]); // MOVSXD RAX, ECX
                                self.push_from_rax();
                                // Counted HERE, where the site is emitted,
                                // not in the matcher (review #80).
                                crate::ATOMIC_INTRINSIC_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                                for p in bail {
                                    self.deopt_stubs.push((p, pc, 6));
                                }
                                intrinsic_handled = true;
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_INT =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_INT_CAS =====
                    // `compareAndSet` / `weakCompareAndSet` as one
                    // `LOCK CMPXCHG [value], RDX`.
                    //
                    // Operand placement differs from the `XADD` arms above
                    // and has to: `CMPXCHG` compares the memory operand
                    // against RAX IMPLICITLY, so RAX belongs to the
                    // EXPECTED value and the receiver moves to RCX. Getting
                    // that backwards compiles and silently compares the
                    // object pointer against the field.
                    //
                    // The result is ZF, not the field, so the arm ends
                    // `SETZ AL` / `MOVZX EAX, AL` — and on failure RAX
                    // holds the value CMPXCHG read, which is deliberately
                    // discarded: `compareAndSet` returns only the boolean
                    // (`compareAndExchange`, which returns the witness, is
                    // NOT claimed here and keeps its native).
                    if !intrinsic_handled
                        && callee_entry == crate::JitIntrinsic::AtomicIntCompareAndSet.as_entry()
                    {
                        if let Some(layout) = crate::AtomicIntFieldLayout::new(0, guard_class_id) {
                            self.flush_scratch_registers();
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();
                            // Deepest first on the operand stack: receiver,
                            // expected, update. Popped in reverse.
                            let update_slot = self.pop_stack();
                            let expect_slot = self.pop_stack();
                            let recv_slot = self.pop_stack();

                            // RCX = receiver; null -> deopt.
                            self.load_slot_to_reg(RCX, recv_slot);
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // Exact receiver class guard:
                            // CMP DWORD [RCX + 0], guard_class_id ; JNE
                            self.buf.emit(&[0x81, 0x79, 0x00]);
                            self.buf.emit(&guard_class_id.to_le_bytes());
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // RAX = expected (the implicit comparand),
                            // RDX = update. Loaded AFTER the guard so a
                            // deopt path does not depend on them.
                            self.load_slot_to_reg(RAX, expect_slot);
                            self.load_slot_to_reg(RDX, update_slot);

                            // Per-object layout branch, as the arms above.
                            self.emit_test_mem8_imm8(
                                RCX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                cratonvm_types::GC_FLAG_COMPACT,
                            );
                            let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                            self.buf.emit(&[0xF0, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                            let done = self.emit_jmp_rel32_patch();
                            self.patch_rel32_to_here(legacy);
                            self.buf.emit(&[0xF0, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                            self.patch_rel32_to_here(done);

                            // ZF = "the swap happened".
                            self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETZ AL
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                            self.push_from_rax();
                            crate::ATOMIC_INTRINSIC_SITES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_INT_CAS =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
                    // `AtomicLong`, emitted as ONE REX.W `LOCK XADD
                    // [value], RCX`. Structurally identical to the 32-bit
                    // region above -- same null check, same exact class-id
                    // guard, same per-object COMPACT/LEGACY branch, same
                    // deopt stub for every uncertain case -- and different
                    // only in operand width.
                    //
                    // The width differences are the whole risk surface, so
                    // they are spelled out:
                    //   * every access carries REX.W (0x48), so it reads
                    //     and writes 8 bytes;
                    //   * the layout's LEGACY offset is the 64-bit payload
                    //     offset inside the `Value` cell, not the 32-bit
                    //     one, and a compact storage width other than 8 is
                    //     refused by `AtomicLongFieldLayout::new`;
                    //   * the delta immediate is `MOV RDX, imm32`
                    //     sign-extended, which is exact for the only
                    //     immediates this region uses, +1 and -1;
                    //   * no `MOVSXD` at the end -- the value is already
                    //     64-bit in RCX, and sign-extending it would be
                    //     both wrong and unnecessary.
                    //
                    // An aligned 8-byte `MOV` is atomic on x86-64 and is a
                    // correct volatile/acquire load, so the `get` arm owes
                    // no `LOCK` and no fence.
                    if !intrinsic_handled {
                        let is_load = callee_entry == crate::JitIntrinsic::AtomicLongGet.as_entry();
                        // (delta_imm, return_post_add, delta_is_arg)
                        let plan: Option<(i32, bool, bool)> = if is_load {
                            Some((0, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongGetAndIncrement.as_entry()
                        {
                            Some((1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongGetAndDecrement.as_entry()
                        {
                            Some((-1, false, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongIncrementAndGet.as_entry()
                        {
                            Some((1, true, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongDecrementAndGet.as_entry()
                        {
                            Some((-1, true, false))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongGetAndAdd.as_entry()
                        {
                            Some((0, false, true))
                        } else if callee_entry
                            == crate::JitIntrinsic::AtomicLongAddAndGet.as_entry()
                        {
                            Some((0, true, true))
                        } else {
                            None
                        };
                        if let Some((delta_imm, return_post_add, delta_is_arg)) = plan {
                            // Recomputed from the same two inputs the
                            // matcher used; `None` cannot happen for a
                            // registered site, and bailing keeps the plain
                            // direct-call path rather than emitting a CALL
                            // to an intrinsic sentinel.
                            if let Some(layout) =
                                crate::AtomicLongFieldLayout::new(0, guard_class_id)
                            {
                                self.flush_scratch_registers();
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::ReceiverTypeChanged,
                                    );
                                }
                                let mut bail: Vec<usize> = Vec::new();
                                // Operands: delta (if any) is shallower,
                                // the receiver is deepest.
                                let delta_slot = if delta_is_arg {
                                    Some(self.pop_stack())
                                } else {
                                    None
                                };
                                let recv_slot = self.pop_stack();

                                // RAX = receiver; null -> deopt.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // Exact receiver class guard:
                                // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // RDX = delta (kept for the *AndGet fixup,
                                // since XADD overwrites its source with the
                                // pre-add value).
                                if !is_load {
                                    match delta_slot {
                                        Some(slot) => self.load_slot_to_reg(RDX, slot),
                                        None => {
                                            // MOV RDX, imm32 (sign-extended)
                                            self.buf.emit(&[0x48, 0xC7, 0xC2]);
                                            self.buf.emit(&delta_imm.to_le_bytes());
                                        }
                                    }
                                    self.buf.emit(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
                                }

                                // Per-object layout branch.
                                self.emit_test_mem8_imm8(
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    cratonvm_types::GC_FLAG_COMPACT,
                                );
                                let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                if is_load {
                                    // MOV RCX, [RAX + compact]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + compact], RCX
                                    self.buf.emit(&[0xF0, 0x48, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                }
                                let done = self.emit_jmp_rel32_patch();
                                self.patch_rel32_to_here(legacy);
                                if is_load {
                                    // MOV RCX, [RAX + legacy]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                } else {
                                    // LOCK XADD [RAX + legacy], RCX
                                    self.buf.emit(&[0xF0, 0x48, 0x0F, 0xC1, 0x88]);
                                    self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                }
                                self.patch_rel32_to_here(done);

                                // RCX now holds the PRE-add value.
                                if return_post_add {
                                    self.buf.emit(&[0x48, 0x01, 0xD1]); // ADD RCX, RDX
                                }
                                self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                self.push_from_rax();
                                // Counted at emission, not in the matcher
                                // (review #80).
                                crate::ATOMIC_LONG_INTRINSIC_SITES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                                for p in bail {
                                    self.deopt_stubs.push((p, pc, 6));
                                }
                                intrinsic_handled = true;
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_LONG =====

                    // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG_CAS =====
                    // `compareAndSet` / `weakCompareAndSet` as one
                    // REX.W `LOCK CMPXCHG [value], RDX`.
                    //
                    // Operand placement differs from the `XADD` arms above
                    // and has to: `CMPXCHG` compares the memory operand
                    // against RAX IMPLICITLY, so RAX belongs to the
                    // EXPECTED value and the receiver moves to RCX. Getting
                    // that backwards compiles and silently compares the
                    // object pointer against the field.
                    //
                    // The result is ZF, not the field, so the arm ends
                    // `SETZ AL` / `MOVZX EAX, AL` — and on failure RAX
                    // holds the value CMPXCHG read, which is deliberately
                    // discarded: `compareAndSet` returns only the boolean
                    // (`compareAndExchange`, which returns the witness, is
                    // NOT claimed here and keeps its native).
                    if !intrinsic_handled
                        && callee_entry == crate::JitIntrinsic::AtomicLongCompareAndSet.as_entry()
                    {
                        if let Some(layout) = crate::AtomicLongFieldLayout::new(0, guard_class_id) {
                            self.flush_scratch_registers();
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();
                            // Deepest first on the operand stack: receiver,
                            // expected, update. Popped in reverse.
                            let update_slot = self.pop_stack();
                            let expect_slot = self.pop_stack();
                            let recv_slot = self.pop_stack();

                            // RCX = receiver; null -> deopt.
                            self.load_slot_to_reg(RCX, recv_slot);
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // Exact receiver class guard:
                            // CMP DWORD [RCX + 0], guard_class_id ; JNE
                            self.buf.emit(&[0x81, 0x79, 0x00]);
                            self.buf.emit(&guard_class_id.to_le_bytes());
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // RAX = expected (the implicit comparand),
                            // RDX = update. Loaded AFTER the guard so a
                            // deopt path does not depend on them.
                            self.load_slot_to_reg(RAX, expect_slot);
                            self.load_slot_to_reg(RDX, update_slot);

                            // Per-object layout branch, as the arms above.
                            self.emit_test_mem8_imm8(
                                RCX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                cratonvm_types::GC_FLAG_COMPACT,
                            );
                            let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                            self.buf.emit(&[0xF0, 0x48, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                            let done = self.emit_jmp_rel32_patch();
                            self.patch_rel32_to_here(legacy);
                            self.buf.emit(&[0xF0, 0x48, 0x0F, 0xB1, 0x91]);
                            self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                            self.patch_rel32_to_here(done);

                            // ZF = "the swap happened".
                            self.buf.emit(&[0x0F, 0x94, 0xC0]); // SETZ AL
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                            self.push_from_rax();
                            crate::ATOMIC_LONG_INTRINSIC_SITES
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: ATOMIC_LONG_CAS =====

                    // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
                    // `Long.longValue()J` and `Integer.intValue()I` — the
                    // UNBOX half of autoboxing, emitted as the same aligned
                    // `MOV` the two `*Get` arms above emit. `Long.value` /
                    // `Integer.value` are `private final` at field slot 0:
                    // a plain load, no `LOCK`, no fence, nothing to order.
                    //
                    // Structurally this is the `is_load` path of the two
                    // regions above with a different class guard, and it is
                    // written out rather than shared with them because the
                    // two differ in exactly one thing an abstraction would
                    // have to re-introduce anyway — the operand width, and
                    // with it the final sign-extension (`MOVSXD` for `I`,
                    // none for `J`).
                    //
                    // Null receiver deopts to the interpreter (bail reason
                    // 6), which re-runs the unbox and raises the same NPE
                    // `Long.longValue` on `null` raises today. Nothing is
                    // CALLed on the inline path.
                    if !intrinsic_handled {
                        let is_long = callee_entry == crate::JitIntrinsic::LongLongValue.as_entry();
                        let is_int =
                            callee_entry == crate::JitIntrinsic::IntegerIntValue.as_entry();
                        if is_long || is_int {
                            // Recomputed from the same two inputs the
                            // matcher used. `None` cannot happen for a
                            // registered site; bailing keeps the plain
                            // direct-call path rather than emitting a CALL
                            // to an intrinsic sentinel.
                            let offsets = if is_long {
                                crate::AtomicLongFieldLayout::new(0, guard_class_id)
                                    .map(|l| (l.value_compact_offset, l.value_legacy_offset))
                            } else {
                                crate::AtomicIntFieldLayout::new(0, guard_class_id)
                                    .map(|l| (l.value_compact_offset, l.value_legacy_offset))
                            };
                            if let Some((compact_off, legacy_off)) = offsets {
                                self.flush_scratch_registers();
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::ReceiverTypeChanged,
                                    );
                                }
                                let mut bail: Vec<usize> = Vec::new();
                                let recv_slot = self.pop_stack();

                                // RAX = receiver; null -> deopt.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // Exact receiver class guard:
                                // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // Per-object layout branch, exactly as the
                                // `Atomic*` arms do it: a COMPACT instance
                                // stores the payload at the registered body
                                // offset, a LEGACY one inside its 16-byte
                                // `Value` cell.
                                self.emit_test_mem8_imm8(
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    cratonvm_types::GC_FLAG_COMPACT,
                                );
                                let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                if is_long {
                                    // MOV RCX, [RAX + compact]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                } else {
                                    // MOV ECX, [RAX + compact]
                                    self.buf.emit(&[0x8B, 0x88]);
                                }
                                self.buf.emit(&compact_off.to_le_bytes());
                                let done = self.emit_jmp_rel32_patch();
                                self.patch_rel32_to_here(legacy);
                                if is_long {
                                    // MOV RCX, [RAX + legacy]
                                    self.buf.emit(&[0x48, 0x8B, 0x88]);
                                } else {
                                    // MOV ECX, [RAX + legacy]
                                    self.buf.emit(&[0x8B, 0x88]);
                                }
                                self.buf.emit(&legacy_off.to_le_bytes());
                                self.patch_rel32_to_here(done);

                                if is_long {
                                    self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                } else {
                                    // MOVSXD RAX, ECX — an `int` is kept
                                    // sign-extended in the 64-bit operand
                                    // slot, the same way the `AtomicInt`
                                    // arm above ends.
                                    self.buf.emit(&[0x48, 0x63, 0xC1]);
                                }
                                self.push_from_rax();
                                // Counted at emission, not in the matcher
                                // (review #80).
                                if is_long {
                                    crate::LONG_LONG_VALUE_INTRINSIC_SITES
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                } else {
                                    crate::INTEGER_INT_VALUE_INTRINSIC_SITES
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }

                                for p in bail {
                                    self.deopt_stubs.push((p, pc, 6));
                                }
                                intrinsic_handled = true;
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: BOX_UNBOX =====

                    // ===== INTRINSIC REGION BEGIN: STRINGBUILDER_ACCESS =====
                    // java.lang.StringBuilder: length()I and append(C).
                    //
                    // MEASURED on this tree, and the reason this region
                    // exists: `StringBuilder.length()` is 194 ns/op and
                    // `append(char)` 349 ns, while `String.length()` — the
                    // same shape, already intrinsified two regions below —
                    // is 2 ns. `System.identityHashCode`, a trivial native
                    // with one object argument, is 120 ns, which is what
                    // the boundary alone costs. The builder was paying it
                    // on every call.
                    //
                    // # The slow edge is a CALL, not an uncommon trap
                    //
                    // Every other intrinsic in this file deopts on a failed
                    // guard, because its guards fail on genuinely uncommon
                    // things (a null receiver, an out-of-bounds index).
                    // `append`'s do not: a full payload is what every
                    // growing builder reaches O(log n) times, and a UTF16
                    // builder fails the coder guard on EVERY call. Deopting
                    // there would re-run the whole method in the
                    // interpreter each time, so these edges go to the same
                    // `invoke_dispatch` the site would have used anyway —
                    // the decline-edge shape the FFM region above
                    // established. Nothing here has to reproduce an
                    // exception, a growth, or a coder inflation: the native
                    // does all three, unchanged.
                    if !intrinsic_handled
                        && (callee_entry == crate::JitIntrinsic::StringBuilderLength.as_entry()
                            || callee_entry
                                == crate::JitIntrinsic::StringBuilderAppendChar.as_entry())
                    {
                        let is_append =
                            callee_entry == crate::JitIntrinsic::StringBuilderAppendChar.as_entry();
                        let info_ptr = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        // The narrow-`value` refusal lives in
                        // `StringBuilderFieldLayout::new`, not here, so
                        // that registration and emission cannot disagree
                        // about it — see that constructor.
                        let layout = self.string_layout.and_then(|l| l.builder);
                        match (info_ptr, layout) {
                            (Some(info), Some(b)) if !info.is_null() => {
                                self.flush_scratch_registers();
                                // Operands, deepest first: receiver, then
                                // (append only) the char.
                                let ch_slot = if is_append {
                                    Some(self.pop_stack())
                                } else {
                                    None
                                };
                                let recv_slot = self.pop_stack();

                                let mut decline: Vec<usize> = Vec::new();

                                // RAX = receiver; null takes the call.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                decline.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // The receiver guard, and it is exact:
                                // `StringBuilder` is final, so a header
                                // class-id match IS that class. This is
                                // what keeps `StringBuffer` — synchronized
                                // methods, a `toStringCache` to invalidate
                                // on every mutation — off a path that
                                // emits neither obligation.
                                //   CMP DWORD [RAX + 0], class_id
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&b.class_id.to_le_bytes());
                                decline.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                let done = if let Some(ch) = ch_slot {
                                    // RCX = the char. LATIN1 only: a wider
                                    // one inflates the payload to UTF16,
                                    // which is the native's job.
                                    //
                                    // Destructured rather than `expect`ed:
                                    // this file denies production panics
                                    // (`hot_files_have_no_production_panics`),
                                    // and `is_append` and `ch_slot.is_some()`
                                    // are the same fact — the operand is
                                    // popped under exactly that condition.
                                    self.load_slot_to_reg(RCX, ch);
                                    self.buf.emit(&[0x81, 0xF9]); // CMP ECX, imm32
                                    self.buf.emit(&0xFFi32.to_le_bytes());
                                    decline.push(self.emit_jcc_rel32_patch(0x87)); // JA

                                    // One compact/legacy branch for the
                                    // whole body rather than one per field:
                                    // the body is ten instructions, and a
                                    // per-field test would emit four
                                    // branches over the same header bit.
                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    self.emit_sb_append_char_body(
                                        b.count_compact_offset,
                                        b.value_compact_offset,
                                        b.coder_compact_offset,
                                        b.coder_compact_is_byte,
                                        &mut decline,
                                    );
                                    let joined = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    self.emit_sb_append_char_body(
                                        b.count_legacy_offset,
                                        b.value_legacy_offset,
                                        b.coder_legacy_offset,
                                        false,
                                        &mut decline,
                                    );
                                    self.patch_rel32_to_here(joined);
                                    // `append` returns its receiver, which
                                    // is still in RAX and was never moved:
                                    // this path allocates nothing.
                                    self.emit_jmp_rel32_patch()
                                } else {
                                    // length() is `count`, sign-extended
                                    // into the 64-bit operand slot the same
                                    // way `String.length()` ends.
                                    self.emit_load_string_i32_field(
                                        RAX,
                                        RAX,
                                        b.count_compact_offset,
                                        false,
                                        b.count_legacy_offset,
                                    );
                                    self.emit_jmp_rel32_patch()
                                };

                                // ---- decline edge: the unchanged dispatch
                                for p in decline {
                                    self.patch_rel32_to_here(p);
                                }
                                let nargs = if is_append { 2 } else { 1 };
                                let args_base = match self
                                    .reserve_spill_slots(nargs, SpillReason::HelperArgs)
                                {
                                    Some(base) => base,
                                    None => {
                                        self.fail("singlepass-codegen/sb-args-spill-exhausted");
                                        return WalkStep::Return(false);
                                    }
                                };
                                // `jit_invoke_dispatch`'s buffer runs
                                // arg[0] at the HIGHEST offset down. Both
                                // operands are loaded before either is
                                // stored: the buffer can overlap the
                                // operand homes.
                                self.load_slot_to_reg(RAX, recv_slot);
                                if let Some(ch) = ch_slot {
                                    self.load_slot_to_reg(RCX, ch);
                                }
                                // Cast: an argument count of 1 or 2.
                                let top = args_base + (nargs as i32 - 1) * 8;
                                self.emit_store_local(top, RAX);
                                if ch_slot.is_some() {
                                    self.emit_store_local(top - 8, RCX);
                                }
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64);
                                self.emit_lea_frame_slot(ARG_REGS[2], top);
                                self.emit_mov_imm32_sx(ARG_REGS[3], nargs as i32);
                                self.emit_pre_safepoint_spill();
                                self.emit_call_absolute(self.helpers.invoke_dispatch);
                                self.emit_oop_map_for_safepoint();
                                // `append` returns a reference, `length` an
                                // int; neither shares the `i64::MIN`
                                // pending-exception sentinel with a
                                // legitimate value, so both take the plain
                                // arm.
                                self.emit_post_invoke_exception_check(if is_append {
                                    b'L'
                                } else {
                                    b'I'
                                });

                                // ---- join -----------------------------
                                self.patch_rel32_to_here(done);
                                self.next_spill_offset = args_base;
                                self.push_from_rax();
                                if is_append {
                                    // The builder it returns is the builder
                                    // it was handed.
                                    self.mark_top_as_oop();
                                }
                                intrinsic_handled = true;
                            }
                            // Falling through is NOT safe: the site carries
                            // an intrinsic SENTINEL as its
                            // `JitDirectCall::entry`, and the ordinary
                            // direct-call path would CALL that sentinel.
                            // Registration and emission must agree, so a
                            // shape that cannot be emitted fails the
                            // compile and drops the method to the
                            // interpreter.
                            _ => {
                                self.fail("singlepass-codegen/sb-intrinsic-unemittable");
                                return WalkStep::Return(false);
                            }
                        }
                    }
                    // ===== INTRINSIC REGION END: STRINGBUILDER_ACCESS =====

                    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
                    // java.lang.String access intrinsics (Phase 3a):
                    // length()I, isEmpty()Z, charAt(I)C, hashCode()I.
                    //
                    // These are registered by `try_resolve_string_intrinsic`
                    // ONLY when a `StringFieldLayout` with a `coder` field
                    // resolved for this compilation; that same layout is in
                    // `self.string_layout`. The defensive `if let Some` below
                    // therefore always matches when a String sentinel is
                    // seen — but if it somehow does not (layout unexpectedly
                    // absent), `intrinsic_handled` stays false and control
                    // falls through to the normal direct-call path, so the
                    // intrinsic sentinel is never mis-`CALL`ed.
                    //
                    // String representation (compact): `value` is a `byte[]`,
                    // `coder` is 0 (LATIN1, 1 byte/char) or 1 (UTF16, 2 LE
                    // bytes/char). `length() == value.length >> coder`.
                    //
                    // Every uncertain case — null receiver, null `value`
                    // array, charAt index out of bounds — branches to a
                    // shared uncommon-trap deopt stub (reason 6) which
                    // re-runs the whole method in the interpreter; the
                    // native `lang_string.rs` impl then reproduces the
                    // exact NPE / StringIndexOutOfBoundsException / value
                    // semantics. No `CALL` is emitted on the inline path.
                    if let Some(layout) = self.string_layout {
                        let acc = if callee_entry == crate::JitIntrinsic::StringLength.as_entry() {
                            Some(0u8)
                        } else if callee_entry == crate::JitIntrinsic::StringIsEmpty.as_entry() {
                            Some(1)
                        } else if callee_entry == crate::JitIntrinsic::StringCharAt.as_entry() {
                            Some(2)
                        } else if callee_entry == crate::JitIntrinsic::StringHashCode.as_entry() {
                            Some(3)
                        } else {
                            None
                        };
                        if let Some(kind) = acc {
                            self.flush_scratch_registers();
                            // Step 6: snapshot before arg pops so a null-
                            // receiver / class-id guard bail resumes at the
                            // invokevirtual bci (receiver [+ index] on stack).
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();

                            // Pop operands. charAt has an index arg
                            // (shallower); the receiver is always deepest.
                            let index_slot = if kind == 2 {
                                Some(self.pop_stack())
                            } else {
                                None
                            };
                            let recv_slot = self.pop_stack();

                            // RAX = receiver. Null receiver → deopt.
                            self.load_slot_to_reg(RAX, recv_slot);
                            self.emit_test_r64_r64(RAX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // Receiver class-id guard. For a
                            // `java/lang/String` call site (final →
                            // monomorphic) `guard_class_id == 0` and no
                            // guard is emitted. For a `java/lang/CharSequence`
                            // site the receiver may be any CharSequence, so
                            // this inline String-layout decode is valid only
                            // when the receiver is actually a String: compare
                            // the ObjectHeader class id at [recv+0] against
                            // the String class id and deopt (→ native
                            // dispatch) on a mismatch (e.g. a StringBuilder /
                            // StringBuffer receiver). Same guard the CRC32
                            // family uses; see its STRING_SEARCH-adjacent
                            // region below.
                            if guard_class_id != 0 {
                                // CMP DWORD [RAX + 0], guard_class_id
                                //   81 /7 id, ModRM 0x78 = mod00 /7 rm=RAX.
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                            }

                            // RCX = value (byte[]) ref. Null → deopt.
                            self.emit_load_string_value_ptr(
                                RCX,
                                RAX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.emit_test_r64_r64(RCX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // R10D = coder (0 LATIN1 / 1 UTF16).
                            self.emit_load_string_i32_field(
                                R10,
                                RAX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );

                            if kind == 3 {
                                // hashCode(): first read the cached `hash`
                                // int. A non-zero cache is the result —
                                // matches the native lazy cache. (A zero
                                // cache, or an empty string, recomputes;
                                // the inline path does NOT write the cache
                                // back — the returned value is identical
                                // either way, the cache is a
                                // non-observable optimisation.)
                                self.emit_load_string_i32_field(
                                    RAX,
                                    RAX,
                                    layout.hash_compact_offset,
                                    false,
                                    layout.hash_legacy_offset,
                                );
                                // TEST EAX,EAX ; JNZ cached_done
                                self.buf.emit(&[0x85, 0xC0]);
                                let cached_done = self.emit_jcc_rel32_patch(0x85); // JNZ

                                // Recompute: char_count = value.len >> coder.
                                // R11D = value.length (zero-extended).
                                self.buf
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    .emit(&[0x44, 0x8B, 0x59, ARRAY_LENGTH_OFFSET as u8]); // MOV R11D,[RCX+12]
                                                                                           // MOV ECX, R10D ; SHR R11D, CL
                                self.buf.emit(&[0x44, 0x89, 0xD1]);
                                self.buf.emit(&[0x41, 0xD3, 0xEB]);
                                // Reload value ptr into RDX (RCX is now CL
                                // scratch). value cell is still in [RAX..]
                                // — but RAX now holds h(=0 region); reload
                                // from the receiver. Receiver was clobbered:
                                // re-pop is not possible. Instead keep value
                                // ptr safe: recompute from recv_slot.
                                self.load_slot_to_reg(RDX, recv_slot);
                                self.emit_load_string_value_ptr(
                                    RDX,
                                    RDX,
                                    layout.value_compact_offset,
                                    layout.value_legacy_offset,
                                );
                                // h = 0 (EAX) ; i = 0 (R8D).
                                self.emit_xor_reg_self(RAX);
                                self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                                                                    // loop: CMP R8D,R11D ; JGE done
                                let loop_top = self.buf.pos();
                                self.buf.emit(&[0x45, 0x39, 0xD8]); // CMP R8D,R11D
                                let loop_done = self.emit_jcc_rel32_patch(0x8D); // JGE
                                                                                 // decode char into ECX: coder branch.
                                                                                 // TEST R10D,R10D ; JNZ utf16
                                self.buf.emit(&[0x45, 0x85, 0xD2]);
                                let utf16 = self.emit_jcc_rel32_patch(0x85);
                                // LATIN1: MOVZX ECX, BYTE [RDX+R8*1+40]
                                self.buf.emit(&[
                                    0x42,
                                    0x0F,
                                    0xB6,
                                    0x4C,
                                    0x02,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                let dec_done = self.emit_jmp_rel32_patch();
                                // UTF16: MOVZX ECX, WORD [RDX+R8*2+40]
                                self.patch_rel32_to_here(utf16);
                                self.buf.emit(&[
                                    0x42,
                                    0x0F,
                                    0xB7,
                                    0x4C,
                                    0x42,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                self.patch_rel32_to_here(dec_done);
                                // h = h*31 + c  ==  (h<<5) - h + c.
                                // MOV R9D,EAX ; SHL EAX,5 ; SUB EAX,R9D ;
                                // ADD EAX,ECX
                                self.buf.emit(&[0x41, 0x89, 0xC1]); // MOV R9D,EAX
                                self.buf.emit(&[0xC1, 0xE0, 0x05]); // SHL EAX,5
                                self.buf.emit(&[0x44, 0x29, 0xC8]); // SUB EAX,R9D
                                self.buf.emit(&[0x01, 0xC8]); // ADD EAX,ECX
                                                              // INC R8D ; JMP loop
                                self.buf.emit(&[0x41, 0xFF, 0xC0]);
                                let back = self.emit_jmp_rel32_patch();
                                // Cast: value to i32 (encoding immediate/displacement)
                                let rel = loop_top as i32 - (back as i32 + 4);
                                self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                self.patch_rel32_to_here(loop_done);
                                self.patch_rel32_to_here(cached_done);
                                // Result (EAX) is sign-extended on push.
                                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                self.push_from_rax();
                            } else if kind == 2 {
                                // charAt(I)C. Register plan:
                                //   R8  = value ptr   (RCX freed for CL)
                                //   R9  = index       (survives SHR)
                                //   R11 = char_count
                                //   R10 = coder
                                // R9 = index.
                                self.load_slot_to_reg(R9, index_slot.unwrap());
                                // R11D = value.length (zero-extended) —
                                // read BEFORE freeing RCX.
                                self.buf
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    .emit(&[0x44, 0x8B, 0x59, ARRAY_LENGTH_OFFSET as u8]); // MOV R11D,[RCX+12]
                                                                                           // R8 = value ptr (so CL can use RCX).
                                self.buf.emit(&[0x49, 0x89, 0xC8]); // MOV R8,RCX
                                                                    // char_count = value.length >> coder.
                                                                    // MOV ECX,R10D ; SHR R11D,CL
                                self.buf.emit(&[0x44, 0x89, 0xD1]);
                                self.buf.emit(&[0x41, 0xD3, 0xEB]);
                                // Bounds: (unsigned) index >= char_count
                                // → deopt (also catches negative index).
                                // CMP R9D, R11D ; JAE deopt
                                self.buf.emit(&[0x45, 0x39, 0xD9]);
                                bail.push(self.emit_jcc_rel32_patch(0x83));
                                // Decode: coder branch. R10D = coder,
                                // R9 = index, R8 = value ptr.
                                self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
                                let utf16 = self.emit_jcc_rel32_patch(0x85);
                                // LATIN1: MOVZX EAX, BYTE [R8+R9*1+40]
                                self.buf.emit(&[
                                    0x43,
                                    0x0F,
                                    0xB6,
                                    0x44,
                                    0x08,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                let dec_done = self.emit_jmp_rel32_patch();
                                // UTF16: MOVZX EAX, WORD [R8+R9*2+40]
                                self.patch_rel32_to_here(utf16);
                                self.buf.emit(&[
                                    0x43,
                                    0x0F,
                                    0xB7,
                                    0x44,
                                    0x48,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                self.patch_rel32_to_here(dec_done);
                                // char result already zero-extended in EAX.
                                self.push_from_rax();
                            } else {
                                // length()I (kind 0) / isEmpty()Z (kind 1).
                                // EAX = value.length (zero-extended).
                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                self.buf.emit(&[0x8B, 0x41, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RCX+12]
                                                                                         // MOV ECX,R10D ; SHR EAX,CL  → char count.
                                self.buf.emit(&[0x44, 0x89, 0xD1]);
                                self.buf.emit(&[0xD3, 0xE8]);
                                if kind == 1 {
                                    // isEmpty: EAX = (char_count == 0).
                                    // TEST EAX,EAX ; SETE AL ; MOVZX EAX,AL
                                    self.buf.emit(&[0x85, 0xC0]);
                                    self.buf.emit(&[0x0F, 0x94, 0xC0]);
                                    self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                                } else {
                                    // length: sign-extend the int result.
                                    self.buf.emit(&[0x48, 0x63, 0xC0]);
                                }
                                self.push_from_rax();
                            }

                            // Wire every bail edge to one shared
                            // uncommon-trap stub (reason 6).
                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: STRING_ACCESS =====

                    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
                    // java.lang.String search intrinsics (Phase 3b):
                    // equals(Ljava/lang/Object;)Z, compareTo(String)I,
                    // indexOf(I)I and indexOf(String)I.
                    //
                    // `equals` strategy. The receiver is a `java/lang/String`
                    // (monomorphic — String is final). The emitted code:
                    //   * other == null            → result 0 (false)
                    //   * this.ptr == other.ptr    → result 1 (true)
                    //   * other's ObjectHeader class id != this's
                    //                              → deopt (non-String
                    //                                argument: native
                    //                                equals returns false)
                    //   * this.value/other.value null   → deopt
                    //   * this.coder != other.coder     → deopt (rare;
                    //                                native compares the
                    //                                decoded char slices)
                    //   * value-array lengths differ    → result 0
                    //   * else REP CMPSB over the bytes → 1 iff identical
                    // Same coder + identical backing bytes ⇒ identical
                    // decoded strings, so the raw byte compare is exact.
                    // No `CALL` on the inline path; every deopt edge re-runs
                    // the method in the interpreter (native `equals`).
                    if self.string_layout.is_some()
                        && callee_entry == crate::JitIntrinsic::StringEquals.as_entry()
                    {
                        let layout = self.string_layout.unwrap();
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, other) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, other.
                        let other_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // RAX = this, RDX = other.
                        self.load_slot_to_reg(RAX, this_slot);
                        self.load_slot_to_reg(RDX, other_slot);

                        // other == null → result 0.
                        self.emit_test_r64_r64(RDX);
                        let other_null = self.emit_jcc_rel32_patch(0x84); // JZ

                        // this.ptr == other.ptr → result 1.
                        // CMP RAX,RDX
                        self.buf.emit(&[0x48, 0x39, 0xD0]);
                        let same_ref = self.emit_jcc_rel32_patch(0x84); // JZ

                        // Class-id check: ObjectHeader.class_id is the i32
                        // at offset 0. `this` is a String, so [RAX] is
                        // String's class id; a differing [RDX] means a
                        // non-String argument → deopt.
                        // MOV ECX,[RAX] ; CMP ECX,[RDX]
                        self.buf.emit(&[0x8B, 0x08]);
                        self.buf.emit(&[0x3B, 0x0A]);
                        bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                        // R8 = this.value, R9 = other.value (byte[] refs).
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.emit_load_string_value_ptr(
                            R9,
                            RDX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        // Null value array on either side → deopt.
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                        bail.push(self.emit_jcc_rel32_patch(0x84));

                        // coder mismatch → deopt.
                        // MOV ECX,[RAX+coder] ; CMP ECX,[RDX+coder]
                        self.emit_load_string_i32_field(
                            RCX,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        // other.coder may come from a legacy-laid-out `other`
                        // independently of `this` -- load it through the
                        // same compact/legacy-aware helper (R11 is free
                        // here) instead of a raw CMP-with-memory-operand,
                        // then compare register-to-register.
                        self.emit_load_string_i32_field(
                            R11,
                            RDX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        self.emit_alu_r32_r32(0x39, RCX, R11); // CMP ECX,R11D
                        bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                        // value-array length mismatch → result 0.
                        // MOV ECX,[R8+12] ; CMP ECX,[R9+12]
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x8B, 0x48, ARRAY_LENGTH_OFFSET as u8]);
                        self.buf
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            .emit(&[0x41, 0x3B, 0x49, ARRAY_LENGTH_OFFSET as u8]);
                        let len_diff = self.emit_jcc_rel32_patch(0x85); // JNE

                        // Byte compare of ECX bytes from
                        // [R8+HEADER] vs [R9+HEADER] via REP CMPSB.
                        // RSI/RDI are callee-saved + may hold locals —
                        // bracket with PUSH/POP (no CALL in between).
                        // PUSH RSI ; PUSH RDI
                        self.buf.emit(&[0x56, 0x57]);
                        // RSI = R8 + HEADER ; RDI = R9 + HEADER
                        // LEA RSI,[R8+40]
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x70, HEADER_SIZE as u8]);
                        // LEA RDI,[R9+40]
                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                        self.buf.emit(&[0x49, 0x8D, 0x79, HEADER_SIZE as u8]);
                        // RCX = length (ECX already holds it,
                        // zero-extended into RCX).
                        // REPE CMPSB  (F3 A6)
                        self.buf.emit(&[0xF3, 0xA6]);
                        // POP RDI ; POP RSI
                        self.buf.emit(&[0x5F, 0x5E]);
                        // SETE AL ; MOVZX EAX,AL → 1 iff all bytes equal
                        // (REPE stops on the first mismatch with ZF clear).
                        self.buf.emit(&[0x0F, 0x94, 0xC0]);
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                        let eq_done = self.emit_jmp_rel32_patch();

                        // result 0 path (other null / length mismatch).
                        self.patch_rel32_to_here(other_null);
                        self.patch_rel32_to_here(len_diff);
                        self.emit_xor_reg_self(RAX);
                        let false_done = self.emit_jmp_rel32_patch();

                        // result 1 path (same reference).
                        self.patch_rel32_to_here(same_ref);
                        // MOV EAX,1
                        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);

                        // join.
                        self.patch_rel32_to_here(eq_done);
                        self.patch_rel32_to_here(false_done);
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }

                    // --- compareTo(Ljava/lang/String;)I ---------------
                    // Lexicographic decoded-char compare. Each side is
                    // decoded through ITS OWN `coder` byte, so every
                    // LATIN1/UTF16 combination (including mixed) is
                    // handled inline — no coder-mismatch deopt. The deopt
                    // stub is reached only for a null receiver, a null
                    // String argument (native throws NPE on the re-run),
                    // or a null backing `value` array. After those checks
                    // the result is fully determined: the unsigned-char
                    // difference at the first mismatch, else len1-len2.
                    // Identical to `native_string_compare_to`.
                    if self.string_layout.is_some()
                        && callee_entry == crate::JitIntrinsic::StringCompareTo.as_entry()
                    {
                        let layout = self.string_layout.unwrap();
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, other) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, other.
                        let other_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // --- deopt checks + field reads happen BEFORE
                        // any PUSH, into CALLER-saved registers only.
                        // Two reasons: (1) the deopt stub's epilogue
                        // assumes RSP is at the post-prologue value, so
                        // the stack must stay balanced on every path that
                        // can reach a bail; (2) a `this`/`other` operand
                        // slot may itself be a callee-saved register that
                        // the loop is about to overwrite — reading it
                        // before the loop's registers are clobbered (and
                        // before the PUSH, while it is still the live
                        // local) is the only sound order.
                        //
                        // RAX = this; null receiver → deopt.
                        self.load_slot_to_reg(RAX, this_slot);
                        self.emit_test_r64_r64(RAX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                    // RDX = other; null argument → deopt.
                        self.load_slot_to_reg(RDX, other_slot);
                        self.emit_test_r64_r64(RDX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                    // R8 = this.value, R9 = other.value; null → deopt.
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        self.emit_load_string_value_ptr(
                            R9,
                            RDX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        // R10 = this.coder, R11 = other.coder.
                        self.emit_load_string_i32_field(
                            R10,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        self.emit_load_string_i32_field(
                            R11,
                            RDX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );

                        // --- past every deopt edge: save the callee-
                        // saved registers the loop uses, then park the
                        // pre-computed caller-saved values into them.
                        // PUSH RBX,RSI,RDI,R12,R13,R14,R15.
                        self.buf.emit(&[0x53, 0x56, 0x57]);
                        self.buf.emit(&[0x41, 0x54, 0x41, 0x55]);
                        self.buf.emit(&[0x41, 0x56, 0x41, 0x57]);
                        // RSI=this.value, RDI=other.value, R12=coder1,
                        // R13=coder2 (moves out of the caller-saved regs;
                        // the original callee-saved values are safely on
                        // the machine stack).
                        self.emit_mov_r64_r64(RSI, R8);
                        self.emit_mov_r64_r64(RDI, R9);
                        self.emit_mov_r64_r64(R12, R10);
                        self.emit_mov_r64_r64(R13, R11);
                        // len1 = this.value.length >> coder1  → R14D.
                        // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RSI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R12); // MOV ECX,R12D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R14, RAX); // MOV R14D,EAX
                                                               // len2 = other.value.length >> coder2 → R15D.
                                                               // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RDI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R13); // MOV ECX,R13D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R15, RAX); // MOV R15D,EAX
                                                               // min_len = min(len1,len2) → EBX.
                        self.emit_alu_r32_r32(0x89, RBX, R14); // MOV EBX,R14D
                        self.emit_alu_r32_r32(0x39, RBX, R15); // CMP EBX,R15D
                                                               // CMOVG EBX,R15D (EBX > R15D ⇒ keep R15D as min).
                        self.buf.emit(&[0x41, 0x0F, 0x4F, 0xDF]);

                        // i = 0 (R8D).
                        self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                        let loop_top = self.buf.pos();
                        // CMP R8D,EBX ; JGE loop_done (i >= min_len).
                        self.emit_alu_r32_r32(0x39, R8, RBX); // CMP R8D,EBX
                        let loop_done = self.emit_jcc_rel32_patch(0x8D);
                        // char_a = this[i]  → R9D ; char_b = other[i] → R10D.
                        self.emit_string_decode_char(R9, RSI, R8, R12);
                        self.emit_string_decode_char(R10, RDI, R8, R13);
                        // diff = char_a - char_b ; JNZ mismatch.
                        self.emit_alu_r32_r32(0x29, R9, R10); // SUB R9D,R10D
                        let mismatch = self.emit_jcc_rel32_patch(0x85);
                        // INC R8D ; JMP loop_top.
                        self.buf.emit(&[0x41, 0xFF, 0xC0]);
                        let back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = loop_top as i32 - (back as i32 + 4);
                        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                // loop_done: result = len1 - len2.
                        self.patch_rel32_to_here(loop_done);
                        self.emit_alu_r32_r32(0x89, RAX, R14); // MOV EAX,R14D
                        self.emit_alu_r32_r32(0x29, RAX, R15); // SUB EAX,R15D
                        let cmp_join = self.emit_jmp_rel32_patch();
                        // mismatch: result = diff (R9D).
                        self.patch_rel32_to_here(mismatch);
                        self.emit_alu_r32_r32(0x89, RAX, R9); // MOV EAX,R9D
                        self.patch_rel32_to_here(cmp_join);
                        // Sign-extend the int result for the push ABI.
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                                            // POP R15,R14,R13,R12,RDI,RSI,RBX.
                        self.buf.emit(&[0x41, 0x5F, 0x41, 0x5E]);
                        self.buf.emit(&[0x41, 0x5D, 0x41, 0x5C]);
                        self.buf.emit(&[0x5F, 0x5E, 0x5B]);
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }

                    // --- indexOf(I)I ----------------------------------
                    // LIVE again as of E27-1 N2b (2026-08-18), for constant
                    // BMP needles only. The `direct.filter` far above is
                    // what enforces that; by the time control reaches here
                    // the needle is known to be a compile-time constant in
                    // `0..=0xFFFF`.
                    //
                    // Scan the receiver for the first code unit equal to
                    // `(ch & 0xFFFF)`, from index 0. This comment used to
                    // call that "bit-identical to `native_string_index_of`,
                    // which likewise masks the argument". BOTH HALVES WERE
                    // FALSE — the JDK gates on `Character.isValidCodePoint`
                    // BEFORE any narrowing and matches a supplementary `ch`
                    // as a surrogate PAIR, and the native side stopped
                    // masking at E18-1, which put the rule in one place
                    // (`lang_string.rs`'s `code_point_needle`). It is
                    // spelled out rather than deleted because E27-1's
                    // finding is that code reading as a working
                    // implementation is how four copies of this rule
                    // survived.
                    //
                    // What makes the scan correct now is the RANGE, not the
                    // mask: on `0..=0xFFFF` the JDK scans for exactly one
                    // code unit, lone surrogates included, so the mask is
                    // the identity and this loop is `code_point_needle`'s
                    // answer. It is left in place rather than folded into a
                    // baked immediate to keep this change a gate change and
                    // nothing else — the emitted bytes here are unchanged.
                    //
                    // The deopt stub is reached only for a null receiver or
                    // a null backing `value` array, both genuinely
                    // once-per-program. It is NOT reached for an
                    // out-of-range needle: such a site is never
                    // intrinsified in the first place. That distinction is
                    // the whole of N2b — see `prev_insn_int_const`.
                    //
                    // Uses only caller-saved registers, so no PUSH/POP is
                    // needed.
                    if self.string_layout.is_some()
                        && callee_entry == crate::JitIntrinsic::StringIndexOfChar.as_entry()
                    {
                        let layout = self.string_layout.unwrap();
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, ch) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, ch.
                        let ch_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // RAX = this; null receiver → deopt.
                        self.load_slot_to_reg(RAX, this_slot);
                        self.emit_test_r64_r64(RAX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                    // R8 = this.value; null → deopt.
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        // R10 = this.coder.
                        self.emit_load_string_i32_field(
                            R10,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        // R9D = needle = ch & 0xFFFF.
                        self.load_slot_to_reg(R9, ch_slot);
                        // AND R9D, 0xFFFF  (REX.B + 81 /4 id).
                        self.buf.emit(&[0x41, 0x81, 0xE1]);
                        self.buf.emit(&0xFFFFu32.to_le_bytes());
                        // len = this.value.length >> coder → R11D.
                        // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, R8, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R10); // MOV ECX,R10D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R11, RAX); // MOV R11D,EAX
                                                               // i = 0 (EDX).
                        self.emit_xor_reg_self(RDX);
                        let loop_top = self.buf.pos();
                        // CMP EDX,R11D ; JGE not_found.
                        self.emit_alu_r32_r32(0x39, RDX, R11);
                        let not_found = self.emit_jcc_rel32_patch(0x8D);
                        // c = this[i] → ECX ; CMP ECX,R9D ; JE found.
                        self.emit_string_decode_char(RCX, R8, RDX, R10);
                        self.emit_alu_r32_r32(0x39, RCX, R9); // CMP ECX,R9D
                        let found = self.emit_jcc_rel32_patch(0x84);
                        // INC EDX ; JMP loop_top.
                        self.buf.emit(&[0xFF, 0xC2]);
                        let back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = loop_top as i32 - (back as i32 + 4);
                        self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                // found: result = i.
                        self.patch_rel32_to_here(found);
                        self.emit_alu_r32_r32(0x89, RAX, RDX); // MOV EAX,EDX
                        let join = self.emit_jmp_rel32_patch();
                        // not_found: result = -1.
                        self.patch_rel32_to_here(not_found);
                        self.buf.emit(&[0xB8]);
                        self.buf.emit(&(-1i32).to_le_bytes());
                        self.patch_rel32_to_here(join);
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }

                    // --- indexOf(Ljava/lang/String;)I -----------------
                    // Naive O(n*m) substring search from index 0; an
                    // empty needle returns 0. Each haystack/needle char is
                    // decoded through its own `coder`, so all coder combos
                    // are handled inline. Bit-identical to
                    // `native_string_index_of_str`. The deopt stub is
                    // reached only for a null receiver or a null backing
                    // `value` array on either side; a null String argument
                    // also deopts — the native re-run then returns -1,
                    // which is the same answer.
                    if self.string_layout.is_some()
                        && callee_entry == crate::JitIntrinsic::StringIndexOfStr.as_entry()
                    {
                        let layout = self.string_layout.unwrap();
                        self.flush_scratch_registers();
                        // Step 6: snapshot (this, needle) before pops.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let mut bail: Vec<usize> = Vec::new();

                        // Operand stack (deepest first): this, needle.
                        let needle_slot = self.pop_stack();
                        let this_slot = self.pop_stack();

                        // --- deopt checks + field reads BEFORE any PUSH,
                        // into CALLER-saved registers only (see the
                        // compareTo block for the rationale: balanced
                        // stack on deopt edges, and an operand slot may
                        // itself be a callee-saved register the loop is
                        // about to clobber).
                        self.load_slot_to_reg(RAX, this_slot);
                        self.emit_test_r64_r64(RAX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                        self.load_slot_to_reg(RDX, needle_slot);
                        self.emit_test_r64_r64(RDX);
                        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                        self.emit_load_string_value_ptr(
                            R8,
                            RAX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        self.emit_load_string_value_ptr(
                            R9,
                            RDX,
                            layout.value_compact_offset,
                            layout.value_legacy_offset,
                        );
                        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                        bail.push(self.emit_jcc_rel32_patch(0x84));
                        // R10 = haystack coder, R11 = needle coder.
                        self.emit_load_string_i32_field(
                            R10,
                            RAX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );
                        self.emit_load_string_i32_field(
                            R11,
                            RDX,
                            layout.coder_compact_offset,
                            layout.coder_compact_is_byte,
                            layout.coder_legacy_offset,
                        );

                        // PUSH RBX,RSI,RDI,R12,R13,R14,R15.
                        self.buf.emit(&[0x53, 0x56, 0x57]);
                        self.buf.emit(&[0x41, 0x54, 0x41, 0x55]);
                        self.buf.emit(&[0x41, 0x56, 0x41, 0x57]);
                        // RSI=haystack value, RDI=needle value,
                        // R12=haystack coder, R13=needle coder.
                        self.emit_mov_r64_r64(RSI, R8);
                        self.emit_mov_r64_r64(RDI, R9);
                        self.emit_mov_r64_r64(R12, R10);
                        self.emit_mov_r64_r64(R13, R11);
                        // hlen → R14D, nlen → R15D.
                        // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RSI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R12); // MOV ECX,R12D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R14, RAX); // MOV R14D,EAX
                                                               // Cast: fixed struct/layout offset to i32 instruction displacement
                        self.emit_mov_r32_mem_disp32(RAX, RDI, ARRAY_LENGTH_OFFSET as i32);
                        self.emit_alu_r32_r32(0x89, RCX, R13); // MOV ECX,R13D
                        self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                        self.emit_alu_r32_r32(0x89, R15, RAX); // MOV R15D,EAX

                        // empty needle (nlen == 0) → result 0.
                        self.buf.emit(&[0x45, 0x85, 0xFF]); // TEST R15D,R15D
                        let needle_empty = self.emit_jcc_rel32_patch(0x84); // JZ
                                                                            // nlen > hlen → not_found.
                        self.emit_alu_r32_r32(0x39, R15, R14); // CMP R15D,R14D
                        let too_long = self.emit_jcc_rel32_patch(0x8F); // JG
                                                                        // max_start = hlen - nlen → EBX (inclusive bound).
                        self.emit_alu_r32_r32(0x89, RBX, R14); // MOV EBX,R14D
                        self.emit_alu_r32_r32(0x29, RBX, R15); // SUB EBX,R15D

                        // outer: i = 0 (R8D).
                        self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                        let outer_top = self.buf.pos();
                        // CMP R8D,EBX ; JG not_found (i > max_start).
                        self.emit_alu_r32_r32(0x39, R8, RBX);
                        let outer_done = self.emit_jcc_rel32_patch(0x8F);
                        // inner: j = 0 (R9D).
                        self.buf.emit(&[0x45, 0x31, 0xC9]); // XOR R9D,R9D
                        let inner_top = self.buf.pos();
                        // CMP R9D,R15D ; JGE match_found (j >= nlen).
                        self.emit_alu_r32_r32(0x39, R9, R15);
                        let match_found = self.emit_jcc_rel32_patch(0x8D);
                        // h = haystack[i+j]: R10D = i+j, decode → R11D.
                        self.emit_alu_r32_r32(0x89, R10, R8); // MOV R10D,R8D
                        self.emit_alu_r32_r32(0x01, R10, R9); // ADD R10D,R9D
                        self.emit_string_decode_char(R11, RSI, R10, R12);
                        // n = needle[j]: decode → R10D.
                        self.emit_string_decode_char(R10, RDI, R9, R13);
                        // CMP R11D,R10D ; JNE inner_break.
                        self.emit_alu_r32_r32(0x39, R11, R10);
                        let inner_break = self.emit_jcc_rel32_patch(0x85);
                        // INC R9D ; JMP inner_top.
                        self.buf.emit(&[0x41, 0xFF, 0xC1]);
                        let inner_back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = inner_top as i32 - (inner_back as i32 + 4);
                        self.buf.try_patch_i32(inner_back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                      // inner_break: INC R8D ; JMP outer_top.
                        self.patch_rel32_to_here(inner_break);
                        self.buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D
                        let outer_back = self.emit_jmp_rel32_patch();
                        // Cast: value to i32 (encoding immediate/displacement)
                        let rel = outer_top as i32 - (outer_back as i32 + 4);
                        self.buf.try_patch_i32(outer_back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                      // match_found: result = i (R8D).
                        self.patch_rel32_to_here(match_found);
                        self.emit_alu_r32_r32(0x89, RAX, R8); // MOV EAX,R8D
                        let join = self.emit_jmp_rel32_patch();
                        // not_found (nlen>hlen or outer exhausted): -1.
                        self.patch_rel32_to_here(too_long);
                        self.patch_rel32_to_here(outer_done);
                        self.buf.emit(&[0xB8]);
                        self.buf.emit(&(-1i32).to_le_bytes());
                        let join2 = self.emit_jmp_rel32_patch();
                        // needle_empty: result = 0.
                        self.patch_rel32_to_here(needle_empty);
                        self.emit_xor_reg_self(RAX);
                        // join.
                        self.patch_rel32_to_here(join);
                        self.patch_rel32_to_here(join2);
                        self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                                            // POP R15,R14,R13,R12,RDI,RSI,RBX.
                        self.buf.emit(&[0x41, 0x5F, 0x41, 0x5E]);
                        self.buf.emit(&[0x41, 0x5D, 0x41, 0x5C]);
                        self.buf.emit(&[0x5F, 0x5E, 0x5B]);
                        self.push_from_rax();

                        for p in bail {
                            self.deopt_stubs.push((p, pc, 6));
                        }
                        intrinsic_handled = true;
                    }
                    // ===== INTRINSIC REGION END: STRING_SEARCH =====

                    // ===== INTRINSIC REGION BEGIN: CRC32 =====
                    // java.util.zip.CRC32 / CRC32C `update` call-site
                    // intrinsics (Phase 4c). Both classes hold a single
                    // `private int crc` at instance field slot 0 — the
                    // running (uncomplemented) CRC state — see
                    // crc_layout_contract.md. The two
                    // sentinels handled here (the IEEE `CRC32` variants
                    // were never registered and were deleted 2026-09-12):
                    //
                    //   Crc32cUpdateByte  : CRC32C.update(I)V
                    //   Crc32cUpdateBytes : CRC32C.update([BII)V
                    //
                    // Every variant:
                    //   1. pops the operand stack (receiver is the
                    //      deepest operand; `callee_params + 1` total),
                    //   2. emits a receiver class-id guard — `CMP
                    //      DWORD [recv+0], guard_class_id` — and deopts
                    //      to normal dispatch on a null receiver or a
                    //      class-id mismatch (a subclass could override
                    //      `update`),
                    //   3. loads the running crc from field cell slot 0
                    //      (`HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET`),
                    //   4. folds the input byte(s) into it,
                    //   5. writes the result back to the same cell
                    //      (Int tag word + payload word).
                    //
                    // CRC32C folds with the hardware `CRC32` instruction
                    // (it computes exactly the Castagnoli polynomial);
                    // CRC32 folds with an inline reflected-CRC bit loop
                    // (IEEE poly 0xEDB88320 — the hardware instruction is
                    // the wrong polynomial). Neither path emits a `CALL`.
                    //
                    // `update([BII)V` preserves null-array NPE and
                    // out-of-bounds AIOOBE by deopting on a null array or
                    // a range outside `[0, array.length]` — the
                    // interpreter then re-runs `update` via the native
                    // override, which raises the exact exception.
                    {
                        let crc32c_byte = crate::JitIntrinsic::Crc32cUpdateByte.as_entry();
                        let crc32c_bytes = crate::JitIntrinsic::Crc32cUpdateBytes.as_entry();
                        let is_crc32c = callee_entry == crc32c_byte || callee_entry == crc32c_bytes;
                        let is_byte_form = callee_entry == crc32c_byte;
                        let is_bytes_form = callee_entry == crc32c_bytes;

                        if is_crc32c {
                            // The matcher only registers a CRC32 family
                            // intrinsic with a resolved class id (it
                            // skips registration when guard_class_id
                            // would be 0), so this is always non-zero
                            // here; assert the invariant defensively.
                            debug_assert!(
                                guard_class_id != 0,
                                "CRC32 intrinsic reached codegen without a guard class id",
                            );

                            // Instance field cell for the `int crc` at
                            // slot 0: HEADER_SIZE + 0*SLOT_SIZE, then the
                            // tag word at +0 and the 32-bit payload at
                            // +FIELD_CELL_PAYLOAD32_OFFSET (see the
                            // inline-getfield codegen for opcode 0xb4).
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            let cell_off = HEADER_SIZE as i32;
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            let tag_off = cell_off + FIELD_CELL_TAG_OFFSET as i32;
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            let pay_off = cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32;

                            self.flush_scratch_registers();
                            // Step 6: snapshot (receiver [, arr, off, len])
                            // before pops so null/bounds guard bails resume
                            // at the invokevirtual CRC32.update bci.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::BoundsCheck,
                                );
                            }

                            // --- pop operands (deepest = receiver) ---
                            // update(I)V    : [receiver, b]
                            // update([BII)V : [receiver, arr, off, len]
                            let (b_or_len_slot, off_slot, arr_slot, recv_slot);
                            if is_byte_form {
                                let b = self.pop_stack();
                                let r = self.pop_stack();
                                b_or_len_slot = b;
                                off_slot = b; // unused
                                arr_slot = b; // unused
                                recv_slot = r;
                            } else {
                                let len = self.pop_stack();
                                let off = self.pop_stack();
                                let arr = self.pop_stack();
                                let r = self.pop_stack();
                                b_or_len_slot = len;
                                off_slot = off;
                                arr_slot = arr;
                                recv_slot = r;
                            }

                            // Pin operands into owned frame scratch slots
                            // below `next_spill_offset`. The call site
                            // had >= (callee_params+1) operand-stack
                            // entries, so these offsets are in-frame. The
                            // intrinsic pushes nothing (void return), so
                            // the next bytecode re-allocates spill slots
                            // from the same base.
                            let scratch_slots = if is_byte_form { 2 } else { 4 };
                            if !self.spill_range_fits(self.next_spill_offset, scratch_slots) {
                                self.fail("singlepass-codegen/intrinsic-pin-spill-exhausted");
                                return WalkStep::Return(false);
                            }
                            let s_recv = self.next_spill_offset;
                            let s_a = self.next_spill_offset + 8;
                            let s_b = self.next_spill_offset + 16;
                            let s_c = self.next_spill_offset + 24;
                            self.load_slot_to_reg(RAX, recv_slot);
                            self.emit_store_local(s_recv, RAX);
                            if is_byte_form {
                                self.load_slot_to_reg(RAX, b_or_len_slot);
                                self.emit_store_local(s_a, RAX);
                            } else {
                                self.load_slot_to_reg(RAX, arr_slot);
                                self.emit_store_local(s_a, RAX);
                                self.load_slot_to_reg(RAX, off_slot);
                                self.emit_store_local(s_b, RAX);
                                self.load_slot_to_reg(RAX, b_or_len_slot);
                                self.emit_store_local(s_c, RAX);
                            }

                            // Every "bail to interpreter" edge is wired
                            // to a shared uncommon-trap deopt stub
                            // (reason 2). The interpreter re-runs the
                            // method and dispatches `update` normally —
                            // preserving NPE / AIOOBE and any overriding
                            // subclass `update` exactly.
                            let mut bail_patches: Vec<usize> = Vec::new();

                            // --- receiver class-id guard ---
                            // RAX = receiver. A null receiver bails
                            // (the interpreter NPEs on the virtual
                            // dispatch). Then CMP the class id at
                            // ObjectHeader+0 against the declared class.
                            self.emit_load_local(RAX, s_recv);
                            self.emit_test_r64_r64(RAX);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                                // CMP DWORD [RAX + 0], guard_class_id
                                                                                //   81 /7 ib? — use the imm32 form: 81 /7.
                                                                                //   ModRM 0x78 = mod00 reg=7(/7=CMP) rm=RAX.
                            self.buf.emit(&[0x81, 0x78, 0x00]);
                            self.buf.emit(&guard_class_id.to_le_bytes());
                            bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // --- load CRC state → ECX ---
                            // MOV ECX, DWORD [RAX + pay_off]. The slot
                            // holds a `Value::Int`. CRC32C stores the
                            // running (complemented) state, while real
                            // JDK CRC32 stores the public value. The IEEE
                            // folding helper consumes the former, so the
                            // CRC32 path complements on either side.
                            self.emit_mov_r32_mem_disp32(RCX, RAX, pay_off);

                            if is_byte_form {
                                // --- update(I)V: fold one byte ---
                                // EDX = arg byte & 0xFF.
                                self.emit_load_local(RDX, s_a);
                                // MOVZX EDX, DL  (0F B6 D2) — low 8 bits.
                                self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                                // CRC32 ECX, DL — hardware Castagnoli
                                // fold of one byte. F2 0F 38 F0 /r,
                                // ModRM 0xCA = reg=ECX rm=EDX(=DL).
                                self.buf.emit(&[0xF2, 0x0F, 0x38, 0xF0, 0xCA]);
                            } else {
                                // --- update([BII)V: fold a range ---
                                // Guards (all bail to the deopt stub,
                                // matching the native override's NPE /
                                // AIOOBE semantics):
                                //   arr != null
                                //   off >= 0, len >= 0
                                //   off + len <= arr.length
                                //
                                // Register file held live across the
                                // guards into the fold loop:
                                //   R8  = array base pointer
                                //   R9  = current index (starts at off)
                                //   R11 = end index = off + len
                                //   RCX = running crc (already loaded)
                                // RDX/RAX are loop-body scratch (the
                                // IEEE helper consumes EDX and clobbers
                                // EAX), so they must NOT carry the index.
                                //
                                // R8 = array ptr; null-array → bail.
                                self.emit_load_local(R8, s_a);
                                self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                                bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ → null array
                                                                                    // RDX = off, sign-extended to 64-bit so
                                                                                    // the range arithmetic cannot overflow.
                                self.emit_load_local(RDX, s_b);
                                self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX,EDX
                                                                    // off < 0 ? TEST RDX,RDX; JS bail.
                                self.emit_test_r64_r64(RDX);
                                bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                    // R11 = len, sign-extended.
                                self.emit_load_local(R11, s_c);
                                self.buf.emit(&[0x4D, 0x63, 0xDB]); // MOVSXD R11,R11D
                                                                    // len < 0 ? TEST R11,R11; JS bail.
                                self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11,R11
                                bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                    // R11 = off + len  (the end index).
                                self.buf.emit(&[0x49, 0x01, 0xD3]); // ADD R11,RDX
                                                                    // RAX = arr.length (zero-extended 32-bit
                                                                    // load → non-negative 64-bit value).
                                self.buf
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    .emit(&[0x41, 0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[R8+ARRAY_LENGTH_OFFSET]
                                                                                           // off + len > arr.length ? CMP R11,RAX;
                                                                                           // JG bail (signed >).
                                self.buf.emit(&[0x49, 0x39, 0xC3]); // CMP R11,RAX
                                bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG
                                                                                    // R9 = current index = off (RDX).
                                self.buf.emit(&[0x49, 0x89, 0xD1]); // MOV R9,RDX

                                // --- fold loop ---
                                // .loop: CMP R9,R11 ; JGE .done
                                let loop_label = self.buf.pos();
                                self.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9,R11
                                let done_patch = self.emit_jcc_rel32_patch(0x8D); // JGE
                                                                                  // EAX = byte = arr[R9].
                                                                                  // MOVZX EAX, BYTE [R8 + R9 + HDR]
                                                                                  //   43 0F B6 44 08 dd
                                                                                  //   (REX.X for R9 index, REX.B for
                                                                                  //    R8 base → 0x43; SIB scale=1).
                                self.buf.emit(&[
                                    0x43,
                                    0x0F,
                                    0xB6,
                                    0x44,
                                    0x08,
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    HEADER_SIZE as u8,
                                ]);
                                // CRC32 ECX, AL — hardware Castagnoli
                                // fold. F2 0F 38 F0 /r, ModRM 0xC8 =
                                // reg=ECX rm=EAX(=AL).
                                self.buf.emit(&[0xF2, 0x0F, 0x38, 0xF0, 0xC8]);
                                // INC R9 ; JMP .loop
                                self.buf.emit(&[0x49, 0xFF, 0xC1]); // INC R9
                                self.buf.emit_byte(0xE9); // JMP rel32
                                {
                                    let here = self.buf.pos();
                                    // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                    let rel = (loop_label as i64) - (here as i64 + 4);
                                    // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                                    self.buf.emit(&(rel as i32).to_le_bytes());
                                }
                                // .done:
                                self.patch_rel32_to_here(done_patch);
                            }

                            // --- write CRC state back to slot 0 ---
                            // RAX = receiver again (reload — RAX was
                            // clobbered by the array-length load / loop).
                            self.emit_load_local(RAX, s_recv);
                            // Tag word := 0  (Value::Int discriminant —
                            // pinned by `field_cell_layout_matches_
                            // value_enum` in cratonvm-types). Keeps the
                            // cell a well-formed Int even if a prior
                            // write left a stale tag.
                            self.emit_mov_dword_mem_disp32_imm32(RAX, tag_off, 0);
                            // Payload word := ECX (the class-specific
                            // state representation described above).
                            // MOV DWORD [RAX + pay_off], ECX  (89 88 dd).
                            self.buf.emit_byte(0x89);
                            self.buf.emit_byte(0x88);
                            self.buf.emit(&pay_off.to_le_bytes());

                            // Wire every bail edge to the shared
                            // uncommon-trap stub (reason 2). Equal
                            // (bci, reason) pairs are coalesced by
                            // `emit_deopt_stubs`.
                            for patch in bail_patches {
                                self.deopt_stubs.push((patch, pc, 2));
                            }
                            // `update` is void — nothing is pushed.
                            let _ = ret_type;
                            intrinsic_handled = true;
                        }
                    }
                    // ===== INTRINSIC REGION END: CRC32 =====

                    if !intrinsic_handled {
                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the pre-pop operand stack here too (see
                        // the matching fix + comment on the MIC/PIC helper
                        // dispatch path below) — a direct call's callee can
                        // still throw/deopt, and `emit_post_invoke_exception_check`
                        // would otherwise be the first (and only) snapshot
                        // for this bci, taken AFTER the receiver/args are
                        // popped, which underflows on a `Reinterpret` resume.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        // Direct call: pop receiver + params, call compiled entry
                        // invokespecial has a receiver, so total args = callee_params + 1
                        let n = callee_params + 1; // receiver + params
                                                   // `pop_stack` rewinds `next_spill_offset` when it pops a
                                                   // top-of-stack `Frame` slot, but it still HANDS THE SLOT
                                                   // BACK, and every `arg_slots` entry stays live until
                                                   // `emit_stack_arg_setup` marshals it into the entry ABI far
                                                   // below. Anything that reserves spill space in between is
                                                   // therefore handed the argument slots themselves. Remember
                                                   // the pre-pop top so such a reservation can be placed above
                                                   // them. See
                                                   // jit-direct-call-arg1-clobbered-by-arg0-FIXED.md.
                        let args_frame_top = self.next_spill_offset;
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        // JVMS 6.5: a null `objectref` raises NPE AT THE INVOKE,
                        // before the callee's first instruction. A baked direct
                        // call jumps straight into the compiled callee, so the
                        // only thing that ever raised it here was the callee
                        // body faulting on its own — which it does only if it
                        // dereferences `this`. MEASURED on three private callees
                        // behind one 50 000-call warming loop (HotSpot raises NPE
                        // for all three):
                        //
                        //     return 3;          NPE interpreted, NO-THROW(3) jit
                        //     return this.x;     NPE both
                        //     return helper();   NPE interpreted, NO-THROW(5) jit
                        //
                        // `invokevirtual`/`invokeinterface` are correct only
                        // incidentally: their inline cache tests the receiver's
                        // class, and a null fails every guard. `invokespecial`
                        // is statically bound, has no guard, and reaches here.
                        // This is the mechanism behind the bogus `Cannot read
                        // field "interfaces" because "rd" is null` at
                        // Class.java:1217 that `vm/tests/
                        // null_receiver_cached_invoke.rs` was written for.
                        //
                        // The receiver is argument 0 of every invoke that
                        // reaches this arm — 0xb6/0xb7/0xb9 all have one, which
                        // is why `n` above is `callee_params + 1`.
                        if let Some(receiver) = arg_slots.first() {
                            self.load_slot_to_reg(RAX, *receiver);
                            self.emit_precise_null_check_field_store();
                        }
                        // A reference staged where no oop map can name it fails the
                        // safepoint closed. DEFERRED to just after the service-range
                        // reservation below, because whether that is true here is
                        // exactly what the reservation decides: when it succeeds it
                        // copies every argument into a contiguous frame range, and a
                        // frame range IS nameable. See `direct_call_arg_maps_enabled`.
                        // Spill cursor as the bytecode's operand stack sees it
                        // now that this invoke's arguments are popped. The
                        // return value belongs HERE, not wherever the
                        // service-argument reservation below leaves the cursor.
                        let post_pop_spill = self.next_spill_offset;

                        // Preserve Java arguments for the cold direct-callee
                        // exception-table service before call marshalling.
                        let service_args_base = info_ptr.and_then(|_| {
                            // See `reserve_direct_call_service_slots`: this range MUST
                            // sit above the argument slots `pop_stack` just handed
                            // back, or the copy below reverses the arguments into
                            // themselves and the callee gets arg0 in every slot.
                            let base =
                                self.reserve_direct_call_service_slots(args_frame_top, &arg_slots)?;
                            for (i, slot) in arg_slots.iter().enumerate() {
                                self.load_slot_to_reg(R11, *slot);
                                let off = base + ((arg_slots.len() - 1 - i) as i32) * 8;
                                self.emit_store_local(off, R11);
                                // THE SAME CHANNEL THE DISPATCH SITE USES. Its args
                                // buffer pushes each oop among the arguments to
                                // `pending_staged_arg_oops`, so the safepoint map
                                // NAMES it and `collect_live_oop_homes` publishes it
                                // on the shadow stack. This copy is the same shape --
                                // a contiguous frame range, written before the CALL,
                                // still live after it (`emit_inline_callee_deopt_check`
                                // reads it) -- and it named nothing.
                                if direct_call_arg_maps_enabled() && arg_oops[i] {
                                    self.pending_staged_arg_oops.push(off);
                                }
                            }
                            Some(base)
                        });
                        // Only an argument oop with NO named home fails the
                        // safepoint closed now. With the service range reserved
                        // every one of them has one.
                        if arg_oops.iter().any(|&o| o)
                            && (!direct_call_arg_maps_enabled() || service_args_base.is_none())
                        {
                            self.pending_staged_args_unmapped = true;
                        }
                        // Round-8 wave-3 HIGH fix: stack-arg setup for
                        // invokespecial/virtual direct calls whose
                        // receiver+params exceed ARG_REGS.
                        let total_sub = self.emit_stack_arg_setup(&arg_slots, callee_needs_ctx);
                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering CALL -- see the
                        // invokestatic site above for why an oop-clean frame
                        // can publish the safepoint id alone.
                        // `args_frame_resident` is what lets mode 2 admit a
                        // reference argument: the service range makes it
                        // frame-resident for the CONSERVATIVE walk. That is no
                        // longer the whole obligation -- naming the argument in
                        // the map means a moving cycle will rewrite it, and for
                        // that it must also be PUBLISHED, which only the real
                        // spill path emits. So a call that names its argument
                        // oops declines the elision and pays the spill again;
                        // `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS=0` restores the
                        // cheaper, unrelocatable arrangement.
                        let names_arg_oops = direct_call_arg_maps_enabled()
                            && service_args_base.is_some()
                            && arg_oops.iter().any(|&o| o);
                        if self.can_elide_direct_call_register_spill(
                            &arg_oops,
                            service_args_base.is_some() && !names_arg_oops,
                            1,
                        ) {
                            self.emit_safepoint_metadata_only();
                        } else {
                            // See the invokestatic twin: R11 stages, so only
                            // ARG_REGS are published, and only with slots.
                            self.emit_pre_safepoint_spill_args_published(
                                service_args_base.is_some(),
                                false,
                            );
                        }
                        self.emit_call_absolute(callee_entry);
                        self.emit_post_call_rbp_republish();
                        // Stage A (precise oop maps, B-K fix) — a direct
                        // invokespecial/virtual call to a compiled callee is a
                        // GC-capable safepoint (the callee may allocate). Like
                        // the self-recursive site above, it historically spilled
                        // but recorded NO oop map (gap #1), so the precise path
                        // could not remap this frame. Gated behind `precise_maps`
                        // → byte-identical gate-OFF; gate-ON records the map and
                        // the paired post-safepoint register reload.
                        // Also under `shadow_enabled` (balance the shadow push/
                        // reload across this direct call — see the self-recursive
                        // site above).
                        if self.precise_maps || self.shadow_enabled {
                            self.emit_oop_map_for_safepoint();
                        }
                        self.emit_stack_arg_cleanup(total_sub);
                        if let (Some(info), Some(args_base)) = (info_ptr, service_args_base) {
                            self.emit_inline_callee_deopt_check(
                                info as *const crate::JitInvokeInfo,
                                arg_slots.len(),
                                args_base,
                            );
                        } else {
                            self.dbg_unserviced_direct_call(
                                "invokespecial/virtual",
                                pc,
                                info_ptr.is_some(),
                                service_args_base.is_some(),
                            );
                            self.fail_unserviced_java_direct_call(info_ptr, service_args_base);
                        }

                        // A directly-called compiled callee that throws (or
                        // deopts) returns the `i64::MIN` sentinel. Propagate
                        // the deopt instead of running on with a bogus value.
                        self.emit_post_invoke_exception_check(ret_type);

                        // Reclaim the spill cursor to the popped-args depth
                        // before the result is pushed, exactly as the
                        // dispatch-helper arm below does with its own
                        // `post_pop_spill`.
                        //
                        // `reserve_direct_call_service_slots` parks the cold
                        // deopt-service copy of the arguments ABOVE the argument
                        // slots (it has to: the slots `pop_stack` handed back are
                        // still live sources for `emit_stack_arg_setup`), which
                        // leaves `next_spill_offset` n slots past the pre-pop top.
                        // Pushing the return value from there parks it above its
                        // semantic operand-stack depth, and every later push in
                        // this basic block inherits the shift. The linear walk
                        // stays self-consistent, so nothing looks wrong -- until
                        // the first branch target after the call, whose depth is
                        // re-established from the bytecode. Writer and reader then
                        // address different slots and the method computes with a
                        // stale one. Measured on ECJ's
                        // `OperandStack.pop(OperandCategory)`, whose `if_icmpeq`
                        // (a tableswitch merge point) compared `TypeBinding.id`
                        // against the expected category instead of
                        // `TypeIds.getCategory(id)`: every JSP compiled after that
                        // method tiered up threw `AssertionError: Unexpected
                        // operand at stack top` (tomcat/ecj-operandstack-*.md).
                        //
                        // Safe to hand the reserved range back: its only consumer
                        // is `emit_inline_callee_deopt_check`, emitted just above.
                        self.next_spill_offset = post_pop_spill;

                        if ret_type != b'V' {
                            if matches!(ret_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                            // Same obligation as the direct INVOKESTATIC arm
                            // (which has always tagged it) and the dispatch
                            // arm below: a reference return is a live oop and
                            // must not keep `push_from_rax`'s default `false`
                            // mark. Missing here, the reference is published
                            // to neither the precise oop map nor the shadow
                            // stack — see the self-recursive site above.
                            if matches!(ret_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                    } // end `if !intrinsic_handled` (plain direct call)
                } else {
                    // Dispatch via helper (MIC-optimized for virtual/interface, plain for others)
                    // MED-4 / Fix 3 — O(1) pc-indexed lookups for invoke/MIC/PIC.
                    let info_ptr = self
                        .invoke_info_idx
                        .get(&pc)
                        .map(|&i| self.invoke_info[i].1);
                    if let Some(info) = info_ptr {
                        // SAFETY: info comes from self.invoke_info, which holds pointers to
                        // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                        let info_ref = unsafe { &*info };
                        let n = info_ref.num_jit_args;

                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the operand stack BEFORE popping this
                        // invoke's receiver/args, mirroring
                        // `snapshot_pre_intrinsic_call`'s "Step 6" pattern
                        // used by the String-intrinsic ladder above. Without
                        // this, the ONLY deopt point available at this bci is
                        // the one `emit_post_invoke_exception_check` builds
                        // AFTER the args are already popped (it only builds
                        // one when `deopt_box_ptr_by_bci` has no entry yet) —
                        // that snapshot is fine for "resume after the call
                        // with an exception pending", but every such point is
                        // tagged `DeoptAction::Reinterpret` at THIS bci, which
                        // means "re-execute this same invoke bytecode from
                        // scratch" and therefore needs the receiver (+ args)
                        // still live on the operand stack. An empty
                        // post-pop snapshot underflows the moment the
                        // resumed interpreter re-fetches the receiver —
                        // reproduced as a `value_stack.rs` panic on a
                        // background NIO worker thread resuming
                        // `LinkedBlockingQueue.take()`'s `Condition.await()`
                        // interface dispatch after an inline-cache miss.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }

                        // Check for MIC slot at this PC
                        let mic_ptr = self.mic_slots_idx.get(&pc).map(|&i| self.mic_slots[i].1);
                        // Check for PIC slot at this PC. When both PIC
                        // and MIC are present (the adaptive recompiler
                        // promotes MIC → PIC and leaves the old MIC
                        // slot live as a fallback), PIC takes
                        // precedence: it caches a 4-entry superset.
                        let pic_ptr = self.pic_slots_idx.get(&pc).map(|&i| self.pic_slots[i].1);
                        if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_GEN") {
                            eprintln!(
                                "[JIT_GEN_INVOKE_VS] pc={} op=0x{:02x} info_kind={} mic_present={} pic_present={} {}.{}{}",
                                pc, op, info_ref.invoke_kind, mic_ptr.is_some(), pic_ptr.is_some(),
                                info_ref.class_name, info_ref.method_name, info_ref.descriptor,
                            );
                        }

                        // Capture spill offset BEFORE popping to prevent
                        // the args buffer from overlapping source Frame slots.
                        let pre_pop_spill = self.next_spill_offset;
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        // Post-pop cursor — the restore point after the
                        // dispatch (see the invokestatic twin above for
                        // the Bug-4 frame-creep rationale).
                        let post_pop_spill = self.next_spill_offset;

                        let args_base_offset = pre_pop_spill;
                        if n > 0 {
                            let Some(args_end) = self.checked_spill_range_end(args_base_offset, n)
                            else {
                                return WalkStep::Return(false);
                            };
                            self.next_spill_offset = args_end;
                            // Store args in reverse offset order (same fix
                            // as invokestatic): higher offsets → lower addresses,
                            // so arg[0] at highest offset = lowest address.
                            for (i, slot) in arg_slots.iter().enumerate() {
                                let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(RAX, *slot);
                                self.emit_store_local(buf_offset, RAX);
                                // See the invokestatic twin: the map below
                                // is the only remaining namer of this oop.
                                if arg_oops[i] {
                                    self.pending_staged_arg_oops.push(buf_offset);
                                }
                            }
                        }

                        // PRECISE-MAPS FIX (bug-03 layer C / B-K): the inline
                        // MIC/PIC cascade below calls the resolved compiled
                        // callee directly (`call r11`) on a class-id hit and
                        // then `jmp`s to the shared `.done` site, where
                        // `emit_oop_map_for_safepoint` emits the precise
                        // post-safepoint RELOAD of oop register-locals from
                        // their canonical frame slots. But the inline-hit path
                        // never reaches the slow-path `emit_pre_safepoint_spill`
                        // below — so under `precise_maps` the reload would load
                        // an UN-spilled (stale) slot back into a live oop
                        // register (e.g. the receiver `this`), which then
                        // faults on the next `getfield` (observed: compiled
                        // `String.codePointAt` → `this.isLatin1()` inline hit →
                        // reload corrupts `this` → SIGSEGV). Spill HERE, before
                        // the cascade, so the spill dominates BOTH the
                        // inline-hit and the slow/miss paths and pairs with the
                        // single shared reload. Gated on `precise_maps`, and the
                        // slow-path spill below is made `!precise_maps`, so the
                        // gate-OFF default path is byte-identical (exactly one
                        // conservative spill, on the slow path, as before).
                        //
                        // SB-CRASH-04: `safepoint_reg_spill` joins this hoist for
                        // the SAME reason — the inline-hit virtual dispatch is a
                        // GC-capable safepoint (the callee allocates), so the
                        // caller's register-only oops must be spilled BEFORE the
                        // cascade to be visible to the conservative scan. Hoisting
                        // here (vs the .miss slow path) also keeps the inline-hit
                        // `jmp .done` rel8 span from being widened by the spill.
                        if self.precise_maps || self.safepoint_reg_spill {
                            // The hoisted spill dominates the inline-hit and
                            // the miss path and pairs with ONE shared reload
                            // at `.done`. Both halves of that pairing survive
                            // the elision: the predicate requires
                            // `precise_maps` and refuses any register-homed
                            // reference local, so the shared
                            // `emit_post_safepoint_reload` -- which walks
                            // `local_oop_masks[pc]`, oops only -- has nothing
                            // to reload and emits nothing. Every argument,
                            // receiver included, is already in the args
                            // buffer with its oops named in the map above.
                            if self.can_elide_direct_call_register_spill(&arg_oops, true, 3) {
                                self.emit_safepoint_metadata_only();
                            } else {
                                // Args are in the buffer and their oops are
                                // named in the map at `.done`; RAX only when
                                // the staging loop ran. See the dispatch twin.
                                self.emit_pre_safepoint_spill_args_published(true, n > 0);
                            }
                        }

                        // CRIT-8 — Inline MIC fast-path guard.
                        //
                        // Layout (verified by `test_jit_mic_slot_offsets` in
                        // jit/src/lib.rs; struct is `#[repr(C)]`):
                        //   offset  0  AtomicU32  cached_class_id
                        //   offset  8  AtomicU64  cached_entry_word
                        //              (entry | needs-context in bit 0)
                        //
                        // HIGH-7 follow-up — Inline 4-way PIC fast-path
                        // guard. `JitPICSlot` is now `#[repr(C)]` with
                        // hot atomic fields at the front (see
                        // `jit/src/lib.rs:1305` and the layout assertion
                        // `test_jit_pic_slot_offsets`):
                        //
                        //   CLASS_ID_OFFSETS      = [0, 4, 8, 12]
                        //   ENTRY_PTR_OFFSETS     = [16, 24, 32, 40]
                        //
                        // Each entry offset holds a TAGGED word: the
                        // target address with bit 0 set when it needs
                        // the VM context (`JIT_IC_NEEDS_CONTEXT_TAG`).
                        //
                        // The Mutex<Option<String>> array (`class_names`)
                        // is moved to the tail so its unstable layout
                        // cannot disturb these offsets.
                        //
                        // When a PIC slot is allocated at this PC, we
                        // emit a 4-way cascade in place of the MIC probe.
                        // PIC supersedes MIC (it is a 4-entry superset)
                        // so we do not emit BOTH guards.
                        //
                        // Hot-path sequence (PIC, ≈5 cycles on slot-0 hit):
                        //   mov   r10, imm64(pic)
                        //   mov   rax, [rbp - receiver_spill]
                        //   mov   eax, [rax]                       ; class_id @ ObjectHeader+0
                        //   ; --- per slot i in 0..4 ---
                        //   cmp   eax, [r10 + CLASS_ID_OFFSETS[i]]
                        //   jne   .try_{i+1}  (or .miss for the last)
                        //   mov   r11, [r10 + ENTRY_PTR_OFFSETS[i]] ; ONE load
                        //   test  r11, r11
                        //   jz    .miss
                        //   btr   r11, 0          ; CF = needs-context tag
                        //   jnc   .noctx
                        //   jmp   .call           ; context ABI already live
                        // .noctx:
                        //   <load context-free ABI: arg_slots[0..n]>
                        // .call:
                        //   call  r11
                        //   jmp   .done
                        //   ; --- end per-slot ---
                        // .miss:
                        //   <existing helper-ABI setup>
                        //   call  jit_invoke_virtual_mic            ; same helper —
                        //                                            ; it consults the
                        //                                            ; underlying cache
                        //                                            ; (MIC or PIC via the
                        //                                            ; adaptive recompiler).
                        // .done:
                        //
                        // Raw memory loads of the atomics are equivalent
                        // to `Ordering::Relaxed` reads (no fences). A
                        // torn class_id or stale entry pointer at worst
                        // causes a miss → slow path; the helper
                        // revalidates and re-resolves authoritatively.
                        // Empty PIC entries hold class_id == 0, which the
                        // doc reserves for `java.lang.Object` (never a
                        // dispatch target here), so an empty slot
                        // naturally fails its CMP and falls through.
                        //
                        // Fast-path eligibility (same as MIC):
                        //   1. pic_ptr OR mic_ptr is Some.
                        //   2. The receiver exists (n >= 1).
                        //   3. The cached entry's `needs_context` bit is
                        //      checked inline and selects the matching
                        //      compiled-entry ABI.
                        //   4. Total callee-ABI arg count (1 vm_ptr + n)
                        //      fits in ARG_REGS.
                        let needs_ctx_arg_count = n + 1; // vm_ptr + n receiver/params
                        let args_fit = n >= 1 && needs_ctx_arg_count <= ARG_REGS.len();
                        // The inline MIC/PIC path emits a raw CALL into
                        // another compiled body.  That bypasses the
                        // interpreter-owned JitEntryGuard, leaving the
                        // active-RBP mirror pointing at the callee while
                        // the root-chain metadata still names the caller.
                        // That is NOT a reclaim hazard, which is what this
                        // comment used to claim: `chain_entry_rbp_is_foreign`
                        // detects it, the moving-young coverage proof comes
                        // back incomplete, and the cycle falls back to the
                        // non-moving sweep. It costs precision, not
                        // correctness — measured, nothing is reclaimed.
                        // `direct_jit_callee_calls_enabled()` gates the
                        // inline MIC path (default-ON — see its doc
                        // comment for the closing fixes and the
                        // regression this default avoids; opt out with
                        // `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`). The MIC
                        // and PIC both publish entry pointer + ABI before
                        // the release-store of class id. Generated guards
                        // also reject a zero entry pointer, covering
                        // class-only profile seeds.
                        //
                        // Spring SpEL's flawed-pattern threshold test drives
                        // catastrophic regex backtracking through the mutually
                        // recursive BmpCharPropertyGreedy/GroupHead pair. The
                        // raw inline IC direct-call path skips the dispatch
                        // helper's frame bookkeeping for that recursion shape
                        // and short-circuits the search after only ~2k
                        // CharSequence accesses. Keep just this pair on the
                        // helper path even when the flag is set.
                        let regex_backtracking_frame = self
                            .method_label
                            .starts_with("java/util/regex/Pattern$BmpCharPropertyGreedy.match")
                            || self
                                .method_label
                                .starts_with("java/util/regex/Pattern$GroupHead.match");
                        // Cached direct entries cannot publish the caller's
                        // precise exception frame. A protected call in a method
                        // whose handler reads locals must take the dispatch path,
                        // where `emit_post_invoke_exception_check` records that
                        // complete caller state before entering its handler.
                        let protected_precise_handler_call =
                            self.precise_exception_frames && self.pc_is_protected(pc);
                        // Per-site bisect levers (`CRATONVM_JIT_SP_IC_ONLY`
                        // / `_DENY`). Inert unless one is set: the whole
                        // cascade is a program-wide switch otherwise, which
                        // localises a defect to this edge but not to a site.
                        let site_allowed = sp_ic_site_allowed(
                            &self.method_label,
                            info_ref.class_name,
                            info_ref.method_name,
                        );
                        let inline_virtual_ic_allowed = crate::direct_jit_callee_calls_enabled()
                            && sp_inline_ic_enabled()
                            && site_allowed
                            && !regex_backtracking_frame
                            && !protected_precise_handler_call;
                        let pic_inline = inline_virtual_ic_allowed
                            && sp_inline_pic_enabled()
                            && pic_ptr.is_some()
                            && args_fit;
                        let mic_inline = inline_virtual_ic_allowed
                            && sp_inline_mic_enabled()
                            && !pic_inline
                            && mic_ptr.is_some()
                            && args_fit;
                        if sp_ic_site_trace() {
                            eprintln!(
                                "[SP_IC_SITE] {}||{}.{}{} pc={} pic={} mic={}",
                                self.method_label,
                                info_ref.class_name,
                                info_ref.method_name,
                                info_ref.descriptor,
                                pc,
                                pic_inline,
                                mic_inline,
                            );
                        }
                        // `.done` patches collected from each emitted
                        // fast-path. Multiple in PIC's case (one per
                        // slot), one in MIC's, none if neither inline
                        // fires. All are JMP rel32 (5 bytes) so the
                        // patch records a 4-byte signed displacement at
                        // `patch_pos`.
                        let mut done_patches32: Vec<usize> = Vec::new();
                        // `.done` patches that are JMP rel8 (single
                        // byte); MIC and the last PIC slot use these
                        // when the skip distance is small enough.
                        let mut done_patch: Option<usize> = None;
                        let mut mic_miss_patches32: Vec<usize> = Vec::new();

                        if pic_inline {
                            let pic = pic_ptr.expect("pic_inline ⇒ pic_ptr Some");

                            // Cache the layout constants locally so a
                            // future const-rename in lib.rs surfaces as
                            // a compile error here.
                            const CLASS_ID_OFFS: [u8; 4] = [0, 4, 8, 12];
                            const ENTRY_PTR_OFFS: [u8; 4] = [16, 24, 32, 40];

                            // Compile-time sanity: the byte offsets we
                            // hardcode in the encodings below must
                            // match the public constants exported by
                            // `JitPICSlot`. A mismatch here would
                            // silently dispatch to a stale entry_ptr.
                            const _: () = assert!(
                                crate::JitPICSlot::CLASS_ID_OFFSETS[0] == 0
                                    && crate::JitPICSlot::CLASS_ID_OFFSETS[1] == 4
                                    && crate::JitPICSlot::CLASS_ID_OFFSETS[2] == 8
                                    && crate::JitPICSlot::CLASS_ID_OFFSETS[3] == 12
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[0] == 16
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[1] == 24
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[2] == 32
                                    && crate::JitPICSlot::ENTRY_PTR_OFFSETS[3] == 40
                                    // `BTR R11, 0` below strips exactly bit 0.
                                    && crate::JIT_IC_NEEDS_CONTEXT_TAG == 1
                            );

                            // R10 = pic_ptr (imm64, fixed 10-byte form).
                            // Task #60: use the fixed-length form so the
                            // unroll duplicator can locate the imm64 at a
                            // deterministic offset (+2 from the MOV start)
                            // and patch it to a freshly-allocated PIC slot
                            // per unrolled copy. Without this, all copies
                            // would share the original slot — per-iteration
                            // cache hits would collide and miss across
                            // copies for any receiver-type-varying loop.
                            let ic_imm64_off = self.buf.pos() + 2;
                            self.emit_mov_imm64_full(R10, pic as *const _ as i64); // Cast: function pointer for JIT call target
                            self.ic_patches
                                .push((ic_imm64_off, 1, pic as *const _ as usize)); // 1 = PIC
                                                                                    // SECURITY FIX (V1) INVARIANT: R10 holds the
                                                                                    // PIC slot base pointer from here until each
                                                                                    // per-slot `MOV R11,[R10+disp]; CALL R11`
                                                                                    // below. Code emitted in this window (receiver
                                                                                    // load, NPE guard, class_id load, the per-slot
                                                                                    // CMP/JNE cascade) must touch only RAX and R10
                                                                                    // itself — it must NOT route through any helper
                                                                                    // that clobbers R10 (e.g. emit_bounds_check,
                                                                                    // SIMD lowering). The ABI marshalling below
                                                                                    // targets ARG_REGS only (RCX/RDX/RSI/R8/R9/RDI
                                                                                    // — never R10), so R10 stays the trusted base.

                            // ---- Hoist callee ABI marshalling out of
                            // the 4-way cascade. Previously each slot
                            // re-loaded `vm_ptr + n args` into
                            // ARG_REGS[0..=n] (~26 bytes per slot on
                            // x86-64 SysV with n=4), tripling the
                            // marshalling cost. Hoisting once:
                            //
                            //   * Cuts ~78 bytes per PIC site (≈26B
                            //     × 2 redundant copies).
                            //   * Keeps the per-slot body to: type
                            //     CMP + JNE, needs-ctx CMP + JE,
                            //     CALL [R10+disp], JMP rel32 .done.
                            //   * Args remain live across the inter-
                            //     slot CMP/JNE pairs because those
                            //     instructions touch only RAX and
                            //     R10 (neither is in ARG_REGS).
                            //   * On a successful CALL, ARG_REGS are
                            //     caller-saved and may be clobbered
                            //     by the callee — but we JMP to
                            //     .done immediately afterwards, so
                            //     no other slot's CALL needs them.
                            //   * On the miss path, the slow-path
                            //     prelude (~line 10709) overwrites
                            //     ARG_REGS with its helper-ABI
                            //     arguments (vm_ptr, info, buf,
                            //     n[, mic, pic]) before the helper
                            //     call. The hoisted values are
                            //     already dead at that point.
                            //
                            // We must materialize the receiver-load
                            // *first* (its source spill could alias
                            // ARG_REGS[0] in degenerate frames) and
                            // RAX holds the class_id used by every
                            // per-slot CMP, so RAX is loaded last.
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            for j in 0..n {
                                let spill_off = args_base_offset + ((n - 1 - j) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.emit_load_local(ARG_REGS[j + 1], spill_off);
                            }
                            // ARG_REGS[1] now holds the receiver
                            // (arg_slots[0]). Reuse it as the source
                            // of the class_id load so we avoid an
                            // extra reload from the receiver spill —
                            // this saves an additional ~5 bytes vs
                            // the prior `emit_load_local(RAX, ...)`.
                            // MOV EAX, dword [ARG_REGS[1]] — load
                            // class_id (ObjectHeader+0). Encoding
                            // depends on whether the receiver reg is
                            // an extended register (R8+).
                            // round-7 audit (bug 4): ARG_REGS[1]
                            // here is the receiver register — the
                            // loop above (`emit_load_local(ARG_REGS[j+1], …)`)
                            // wrote `arg_slots[0]` (= the receiver
                            // by JVM invokevirtual/interface
                            // calling convention) into ARG_REGS[0+1].
                            // The debug_assert below is therefore
                            // checking the right register; verified.
                            let recv_reg = ARG_REGS[1];
                            // NPE guard: a null receiver must not be
                            // dereferenced by the class_id load below
                            // (`MOV EAX, [recv_reg]`) — that faults at
                            // address 0 and SIGSEGVs the VM (observed
                            // in Tomcat: JIT-compiled `Locale.hashCode`
                            // dispatching `BaseLocale.hashCode` on a
                            // null receiver). Route a null receiver to
                            // the `.miss` slow path, which decodes args
                            // and bails to the interpreter where the
                            // invokevirtual receiver null check raises
                            // a proper NullPointerException.
                            //   TEST recv_reg, recv_reg
                            if recv_reg >= 8 {
                                self.buf.emit(&[
                                    0x4D,
                                    0x85,
                                    0xC0 | ((recv_reg & 7) << 3) | (recv_reg & 7),
                                ]);
                            } else {
                                self.buf.emit(&[
                                    0x48,
                                    0x85,
                                    0xC0 | ((recv_reg & 7) << 3) | (recv_reg & 7),
                                ]);
                            }
                            //   JZ rel32 → .miss (patched at miss_off)
                            self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                            let pic_null_miss_patch = self.buf.pos() - 4;
                            // ARRAY-RECEIVER GUARD. `ObjectHeader.class_id` sits at offset 0 for objects AND for
                            // arrays, and a reference array stores its
                            // COMPONENT class id there — a `Foo[]` and a
                            // `Foo` present the SAME 4-byte guard word. A
                            // class-id-only guard therefore lets a site
                            // warmed on a `Foo` receiver dispatch a later
                            // `Foo[]` receiver straight into `Foo`'s own
                            // method body, where the first `checkcast Foo`
                            // throws `class [LFoo; cannot be cast to class
                            // Foo`. (The helper never INSTALLS an entry for
                            // an array receiver — `cacheable_receiver` is
                            // false for `ObjectKind::Array` — so only the
                            // consumption guard was ever wrong.)
                            // `ObjectHeader.kind` (offset 4) separates the
                            // two; anything that is not a plain object goes
                            // to the miss path, which resolves on the real
                            // receiver.
                            //   CMP BYTE [recv_reg + KIND_TAGS_BYTE_OFFSET], Object
                            // mod=01 (disp8) / reg=/7 (CMP imm8) / rm=recv.
                            // `recv_reg & 7 != 4` is already asserted below
                            // (mod=00 would need a SIB there), and mod=01
                            // makes rm==5 an ordinary [reg+disp8].
                            if recv_reg >= 8 {
                                self.buf.emit_byte(0x41); // REX.B
                            }
                            self.buf.emit(&[
                                0x80,
                                0x78 | (recv_reg & 7),
                                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                                cratonvm_types::ObjectKind::Object as u8,
                            ]);
                            //   JNE rel32 → .miss
                            self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                            let pic_kind_miss_patch = self.buf.pos() - 4;
                            // MED (round-5 review): mod=00 encoding
                            // reuses the low-3 bits of the register as
                            // r/m, where r/m==4 (RSP/R12) means
                            // SIB-follows and r/m==5 (RBP/R13) means
                            // RIP-relative — both would mis-encode this
                            // displacement-free load. Safe today
                            // because RCX(.1)/RDX(.2)/RSI(.6) are all
                            // outside {4,5}, but any future ARG_REGS
                            // shuffle would silently corrupt this PIC
                            // slot. Trap brittleness in debug builds.
                            debug_assert!(
                                (recv_reg & 7) != 4 && (recv_reg & 7) != 5,
                                "PIC slot-0 mod=00 requires ARG_REGS[1] low3 not in {{4,5}} (got {})",
                                recv_reg,
                            );
                            if recv_reg >= 8 {
                                // REX.B + 8B /r, modrm = mod(00) reg(EAX=0) rm(recv&7)
                                self.buf.emit(&[0x41, 0x8B, recv_reg & 7]);
                            } else {
                                // 8B /r, modrm = mod(00) reg(EAX=0) rm(recv)
                                self.buf.emit(&[0x8B, recv_reg]);
                            }

                            // Per-slot cascade. Inter-slot `jne` jumps
                            // are rel8 and patched once we know the
                            // start of the next slot. Final slot's
                            // `jne` and every `je needs_ctx → .miss`
                            // jump to the shared `.miss` label.
                            //
                            // `slot_starts[i]` is the byte position of
                            // slot i's first emitted byte (the CMP
                            // opcode); used to resolve inter-slot
                            // `jne` rel8 patches once all slots are
                            // emitted.
                            let mut slot_starts: [usize; 4] = [0; 4];
                            // (patch_pos, target_slot_index) for each
                            // inter-slot `jne` rel8 that needs to land
                            // at the start of slot `target_slot_index`.
                            let mut next_slot_patches: Vec<(usize, usize)> = Vec::new();
                            // CRIT-3 — miss branches use rel32 form
                            // unconditionally. With n>=5 args on Linux
                            // the cumulative body across all three
                            // slots + slow-path prelude can exceed 127
                            // bytes, overflowing the previous rel8
                            // encoding (`0x74`/`0x75`). The rel32
                            // forms (`0x0F 0x84`/`0x0F 0x85` + 4-byte
                            // disp) always fit. `miss_patches_rel32`
                            // stores the byte offset of the 4-byte
                            // displacement immediate, patched at the
                            // shared `.miss` label below.
                            // Seeded with the receiver-null-check JZ
                            // emitted above the cascade so it is patched
                            // to the same shared `.miss` target.
                            let mut miss_patches_rel32: Vec<usize> =
                                vec![pic_null_miss_patch, pic_kind_miss_patch];

                            for i in 0..crate::JIT_PIC_ENTRIES {
                                slot_starts[i] = self.buf.pos();

                                // CMP EAX, dword [R10 + CLASS_ID_OFFS[i]]
                                // For disp == 0 (slot 0 today) emit the
                                // mod=00 form with no displacement byte:
                                // saves 1 byte per JIT site on the hot
                                // slot-0 cascade. For disp != 0 use the
                                // mod=01 (disp8) form. R10 in mod=00 is
                                // ModRM 00_000_010 = 0x02.
                                if CLASS_ID_OFFS[i] == 0 {
                                    // 3 bytes: REX.B + 3B /r + ModRM(00,000,010).
                                    self.buf.emit(&[0x41, 0x3B, 0x02]);
                                } else {
                                    // 4 bytes: REX.B + 3B /r + ModRM(01,000,010) + disp8.
                                    self.buf.emit(&[0x41, 0x3B, 0x42, CLASS_ID_OFFS[i]]);
                                }

                                if i + 1 < crate::JIT_PIC_ENTRIES {
                                    // JNE rel32 → start of slot i+1
                                    // (patched below once slot i+1's
                                    // start position is known).
                                    //
                                    // This was rel8 on the claim that "a
                                    // single slot body is ~30 bytes for
                                    // n<=5". That stopped being true: a
                                    // slot body now also carries the
                                    // post-call innermost-RBP republish and
                                    // the callee-deopt service check, which
                                    // together put it well past 127 bytes.
                                    // The patch truncated the displacement
                                    // (`rel as u8`) behind a
                                    // `debug_assert!`, so release builds
                                    // silently got `JNE -128` — a branch
                                    // into the middle of the pre-call
                                    // shadow-stack push, which then ran as
                                    // an unguarded infinite push loop and
                                    // walked off the end of the thread's
                                    // shadow buffer (SIGSEGV ~170 MiB
                                    // later, at the arena end). Only
                                    // reachable with raw JIT-to-JIT calls
                                    // enabled, which is why the closed gate
                                    // hid it. rel32 has no such cliff.
                                    self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                    let patch = self.buf.pos() - 4;
                                    next_slot_patches.push((patch, i + 1));
                                } else {
                                    // Final slot: JNE rel32 → .miss
                                    // (6 bytes: 0x0F 0x85 + i32 disp).
                                    // The final slot's miss target sits past
                                    // the whole cascade and may be reachable
                                    // in rel8 but we keep rel32 for
                                    // consistency with slot 0/1 and
                                    // because the slow-path prelude
                                    // following the cascade can push
                                    // the distance over 127 bytes.
                                    self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                    miss_patches_rel32.push(self.buf.pos() - 4);
                                }

                                // Capture the way's target ONCE. The word
                                // carries the entry AND its calling
                                // convention, so the ABI chosen below and
                                // the CALL can never come from two
                                // different publications of this way.
                                // MOV R11, qword [R10 + ENTRY_PTR_OFFS[i]]
                                // 4 bytes: REX.WRB + 8B /r + ModRM(01,R11,R10) + disp8
                                self.buf.emit(&[0x4D, 0x8B, 0x5A, ENTRY_PTR_OFFS[i]]);
                                // A receiver profile may pre-populate only
                                // class_id, and a retired way's word is
                                // zeroed: never CALL a zero target.
                                // TEST R11, R11 (3 bytes)
                                self.buf.emit(&[0x4D, 0x85, 0xDB]);
                                // JZ rel32 → .miss
                                self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                miss_patches_rel32.push(self.buf.pos() - 4);

                                // BTR R11, 0 — CF := the needs-context
                                // tag, R11 := the bare entry address.
                                // 5 bytes: REX.WB + 0F BA /6 + ModRM(11,110,R11) + imm8
                                self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);

                                // JNC rel32 → .noctx. Both ABI shapes are
                                // valid cache hits; small interface
                                // implementations are commonly
                                // context-free.
                                self.buf.emit(&[0x0F, 0x83, 0x00, 0x00, 0x00, 0x00]);
                                let noctx_patch = self.buf.pos() - 4;

                                // Callee ABI args (vm_ptr + n) have
                                // already been materialised once in
                                // ARG_REGS[0..=n] above the cascade
                                // (HIGH-perf hoist; see comment at
                                // the top of the PIC body). Slot
                                // bodies must NOT touch ARG_REGS.

                                // Context ABI is already live. Skip the
                                // alternate marshalling block.
                                self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                let ctx_call_patch = self.buf.pos() - 4;

                                // .noctx: Java args start at ARG_REGS[0].
                                let noctx_off = self.buf.pos();
                                let noctx_rel = (noctx_off as i64) - (noctx_patch as i64 + 4);
                                debug_assert!(
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&noctx_rel)
                                );
                                self.buf.try_patch_i32(noctx_patch, noctx_rel as i32).ok();
                                for j in 0..n {
                                    let spill_off = args_base_offset + ((n - 1 - j) as i32) * 8;
                                    self.emit_load_local(ARG_REGS[j], spill_off);
                                }

                                // .call
                                let call_off = self.buf.pos();
                                let call_rel = (call_off as i64) - (ctx_call_patch as i64 + 4);
                                debug_assert!(
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&call_rel)
                                );
                                self.buf.try_patch_i32(ctx_call_patch, call_rel as i32).ok();

                                // SECURITY FIX (V1): do NOT keep the call
                                // target live in memory addressed through
                                // R10 across an indirect CALL — R10 is the
                                // shared bounds-check / SIMD scratch
                                // register. The target was captured into
                                // R11 above (a caller-saved scratch reg
                                // that is NOT in ARG_REGS / SCRATCH_REGS /
                                // LOCAL_REGS and is clobbered by the call
                                // anyway) and R10 is not read again.
                                // Between that capture and this CALL only
                                // the `emit_load_local` loads of the
                                // no-context ABI run, and they write
                                // ARG_REGS alone. INVARIANT: nothing that
                                // writes R11 may be emitted between the
                                // entry-word capture and this `CALL R11`.
                                //
                                // CALL R11  (3 bytes: REX.B + FF /2 + ModRM(11,/2,R11))
                                self.buf.emit(&[0x41, 0xFF, 0xD3]);
                                // A compiled callee's prologue publishes
                                // ITS rbp into the innermost-RBP mirror the
                                // GC reads. Nothing on the return path of a
                                // raw JIT-to-JIT call restores this
                                // caller's, so without this the mirror
                                // names a DEAD frame from here on and the
                                // next GC applies THIS method's oop map to
                                // whatever has since reused that stack
                                // memory. The MIC arm below and the hashed
                                // stub have always republished; this
                                // cascade did not, and since a PIC slot is
                                // allocated eagerly at every eligible site
                                // (`pic_inline` wins over `mic_inline`
                                // whenever `pic_ptr` is Some) it is the arm
                                // that actually runs. See
                                // `conservative_roots::top_rbp_mirror_write`
                                // for the Rust-side analogue of the same
                                // contract.
                                self.emit_post_call_rbp_republish();
                                self.emit_inline_callee_deopt_check(info, n, args_base_offset);

                                // JMP rel32 → .done. Use rel32 because
                                // for slots 0 and 1 the skip distance
                                // (remaining slot bodies + slow path)
                                // routinely exceeds 127 bytes. Slot 2's
                                // .done jump is short but we keep rel32
                                // uniform to keep patching simple.
                                // branching logic.
                                // E9 cd: JMP rel32 (5 bytes).
                                self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                done_patches32.push(self.buf.pos() - 4);
                            }

                            // Resolve inter-slot `jne` rel8 patches now
                            // that every slot's start is known.
                            for (jne_patch, target_slot) in &next_slot_patches {
                                let slot_start = slot_starts[*target_slot];
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (slot_start as i64) - (*jne_patch as i64 + 4);
                                debug_assert!(
                                    // Widening: i32 bound -> i64 (range comparison)
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline PIC inter-slot jne overflowed rel32 ({} bytes)",
                                    rel
                                );
                                // Cast: rel32 displacement. On Err,
                                // try_patch_i32 marks the buffer
                                // overflowed and the compile bails.
                                self.buf.try_patch_i32(*jne_patch, rel as i32).ok();
                            }

                            // .miss: patch all `je needs_ctx → .miss`
                            // and the final slot's `jne → .miss` to land
                            // HERE — the start of the slow-path block
                            // emitted below.  CRIT-3: these are all
                            // rel32 form, so the patch site holds a
                            // 4-byte signed displacement computed
                            // from the byte AFTER the immediate
                            // (patch + 4) to the target.
                            let miss_off = self.buf.pos();
                            for patch in &miss_patches_rel32 {
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (miss_off as i64) - (*patch as i64 + 4);
                                debug_assert!(
                                    // Widening: i32 bound -> i64 (range comparison)
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline PIC miss branch overflowed rel32 ({} bytes)",
                                    rel
                                );
                                self.buf
                                    .try_patch_i32(*patch, rel as i32) // Cast: rel32 displacement
                                    .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                            }
                            // The MIC arm maintains its own rel32 miss
                            // patches; PIC uses the vector above.
                        } else if mic_inline {
                            let mic = mic_ptr.expect("mic_inline ⇒ mic_ptr Some");
                            // R10 = mic_ptr (imm64, fixed 10-byte form).
                            // Task #60: same rationale as the PIC arm —
                            // fixed-length encoding gives the unroll
                            // duplicator a deterministic imm64 location
                            // to overwrite with a per-copy fresh MIC slot.
                            let ic_imm64_off = self.buf.pos() + 2;
                            self.emit_mov_imm64_full(R10, mic as *const _ as i64); // Cast: function pointer for JIT call target
                            self.ic_patches
                                .push((ic_imm64_off, 0, mic as *const _ as usize)); // 0 = MIC
                                                                                    // SECURITY FIX (V1) INVARIANT: R10 holds the
                                                                                    // MIC slot base pointer from here until the
                                                                                    // `MOV R11,[R10+8]; CALL R11` below. Every
                                                                                    // instruction emitted in this window (receiver
                                                                                    // load into RAX, NPE guard, class_id/needs-ctx
                                                                                    // checks, and the ARG_REGS marshalling loop)
                                                                                    // touches only RAX, R10, and ARG_REGS — never
                                                                                    // R10 as a destination — so the call target
                                                                                    // base cannot be perturbed before the CALL.

                            // Load receiver pointer into RAX. Receiver is
                            // arg_slots[0], spilled at the *highest* offset
                            // (lowest address) in the args buffer.
                            let receiver_spill = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                            self.emit_load_local(RAX, receiver_spill);

                            // NPE guard: a null receiver must not reach
                            // the `MOV EAX,[RAX]` class_id load below —
                            // dereferencing address 0 SIGSEGVs the VM.
                            // Route null to `.miss` (slow helper →
                            // interpreter), which raises a proper
                            // NullPointerException per JVMS invokevirtual.
                            //   TEST RAX, RAX  (48 85 C0)
                            self.buf.emit(&[0x48, 0x85, 0xC0]);
                            //   JZ rel32 → .miss. The dual-ABI
                            // marshalling blocks make the miss span larger
                            // than rel8 for otherwise tiny callees.
                            self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                            mic_miss_patches32.push(self.buf.pos() - 4);

                            // ARRAY-RECEIVER GUARD. `ObjectHeader.class_id` sits at offset 0 for objects AND for
                            // arrays, and a reference array stores its
                            // COMPONENT class id there — a `Foo[]` and a
                            // `Foo` present the SAME 4-byte guard word. A
                            // class-id-only guard therefore lets a site
                            // warmed on a `Foo` receiver dispatch a later
                            // `Foo[]` receiver straight into `Foo`'s own
                            // method body, where the first `checkcast Foo`
                            // throws `class [LFoo; cannot be cast to class
                            // Foo`. (The helper never INSTALLS an entry for
                            // an array receiver — `cacheable_receiver` is
                            // false for `ObjectKind::Array` — so only the
                            // consumption guard was ever wrong.)
                            // `ObjectHeader.kind` (offset 4) separates the
                            // two; anything that is not a plain object goes
                            // to the miss path, which resolves on the real
                            // receiver.
                            //   CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], Object
                            self.buf.emit(&[
                                0x80,
                                0x78,
                                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                                cratonvm_types::ObjectKind::Object as u8,
                            ]);
                            //   JNE rel32 → .miss
                            self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                            mic_miss_patches32.push(self.buf.pos() - 4);

                            // MOV EAX, dword [RAX]  — load class_id (ObjectHeader+0).
                            // 2 bytes: 8B 00
                            self.buf.emit(&[0x8B, 0x00]);

                            // CMP EAX, dword [R10 + 0]  — vs cached_class_id.
                            // 3 bytes: REX.B (0x41) + 3B /r + modrm(00 000 010)
                            self.buf.emit(&[0x41, 0x3B, 0x02]);

                            // JNE rel32 → .miss.
                            self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                            mic_miss_patches32.push(self.buf.pos() - 4);

                            // Capture the tagged target word ONCE: the
                            // entry address with the needs-context flag in
                            // bit 0, so the ABI and the call target come
                            // from the same publication.
                            // MOV R11, qword [R10 + 8]
                            // 4 bytes: REX.WRB + 8B /r + ModRM(01,R11,R10) + disp8
                            self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x08]);
                            // A profiled MIC can publish the receiver class
                            // before the helper has installed a compiled
                            // target, and a withdrawal zeroes the word.
                            // Never CALL a zero target.
                            // TEST R11, R11 (3 bytes)
                            self.buf.emit(&[0x4D, 0x85, 0xDB]);
                            // JZ rel32 → .miss
                            self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                            mic_miss_patches32.push(self.buf.pos() - 4);

                            // BTR R11, 0 — CF := needs-context, R11 := entry.
                            // 5 bytes: REX.WB + 0F BA /6 + ModRM(11,110,R11) + imm8
                            self.buf.emit(&[0x49, 0x0F, 0xBA, 0xF3, 0x00]);

                            // JNC rel32 → .noctx. Context-free compiled
                            // methods use Java arg0 in ARG_REGS[0], while
                            // context-using methods reserve that register
                            // for vm_ptr. The cache publishes this ABI bit;
                            // honoring both shapes is essential because
                            // small interface implementations almost
                            // always compile context-free.
                            self.buf.emit(&[0x0F, 0x83, 0x00, 0x00, 0x00, 0x00]);
                            let noctx_patch = self.buf.pos() - 4;

                            // ---- Set up callee ABI: (vm_ptr, arg_slots[0..n]) ----
                            // vm_ptr → ARG_REGS[0]
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            // arg_slots[i] → ARG_REGS[i + 1]
                            for i in 0..n {
                                let spill_off = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.emit_load_local(ARG_REGS[i + 1], spill_off);
                            }

                            // JMP rel32 → .call, skipping the no-context
                            // marshalling block.
                            self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                            let ctx_call_patch = self.buf.pos() - 4;

                            // .noctx: Java args begin at ARG_REGS[0].
                            let noctx_off = self.buf.pos();
                            let noctx_rel = (noctx_off as i64) - (noctx_patch as i64 + 4);
                            debug_assert!((i32::MIN as i64..=i32::MAX as i64).contains(&noctx_rel));
                            self.buf.try_patch_i32(noctx_patch, noctx_rel as i32).ok();
                            for i in 0..n {
                                let spill_off = args_base_offset + ((n - 1 - i) as i32) * 8;
                                self.emit_load_local(ARG_REGS[i], spill_off);
                            }

                            // .call
                            let call_off = self.buf.pos();
                            let call_rel = (call_off as i64) - (ctx_call_patch as i64 + 4);
                            debug_assert!((i32::MIN as i64..=i32::MAX as i64).contains(&call_rel));
                            self.buf.try_patch_i32(ctx_call_patch, call_rel as i32).ok();

                            // SECURITY FIX (V1): same hardening as the
                            // PIC arm — the target was captured into R11
                            // (caller-saved scratch, not in ARG_REGS /
                            // SCRATCH_REGS / LOCAL_REGS) before the ABI
                            // marshalling, which loads into ARG_REGS only,
                            // and R10 is not read again. INVARIANT: nothing
                            // that writes R11 may be emitted between the
                            // entry-word capture and this `CALL R11`.
                            //
                            // CALL R11  (3 bytes: REX.B + FF /2 + ModRM(11,/2,R11))
                            self.buf.emit(&[0x41, 0xFF, 0xD3]);
                            self.emit_post_call_rbp_republish();
                            self.emit_inline_callee_deopt_check(info, n, args_base_offset);

                            // JMP rel32 → .done. The root-frame
                            // republish above makes the distance exceed
                            // the old rel8 budget.
                            self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                            done_patches32.push(self.buf.pos() - 4);

                            // .miss: patch both rel32 sites here.
                            let miss_off = self.buf.pos();
                            for patch in &mic_miss_patches32 {
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (miss_off as i64) - (*patch as i64 + 4);
                                debug_assert!(
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline MIC miss branch overflowed rel32 ({} bytes)",
                                    rel
                                );
                                self.buf.try_patch_i32(*patch, rel as i32).ok();
                                // on Err try_patch_byte set buf.overflowed; compile bails
                            }
                        }

                        // ---- Shared compact hashed/vtable stub ----
                        // Both the baseline and optimizing tiers lower
                        // megamorphic misses through this exact library.
                        // It reloads arg0, performs two lock-free probes,
                        // and falls through here only on a real miss.
                        if let Some(pic) = pic_ptr
                            .filter(|_| inline_virtual_ic_allowed && sp_inline_mega_enabled())
                        {
                            let arg_offsets: Vec<i32> = (0..n)
                                .map(|i| args_base_offset + ((n - 1 - i) as i32) * 8)
                                .collect();
                            done_patches32.extend(
                                crate::runtime_lowering::emit_hashed_vtable_stub(
                                    &mut self.buf,
                                    pic as usize,
                                    self.heap_local_offset,
                                    &arg_offsets,
                                    if self.precise_maps {
                                        self.helpers.frame_record
                                    } else {
                                        0
                                    },
                                    self.helpers.service_callee_deopt,
                                    info as usize,
                                    // This backend stages its outgoing
                                    // arguments into one descending block,
                                    // so element 0 already names it.
                                    arg_offsets[0],
                                ),
                            );
                        }

                        // ---- Slow path: helper ABI setup + call ----
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                        if n > 0 {
                            let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                        } else {
                            self.emit_xor_reg_self(ARG_REGS[2]);
                        }
                        self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding

                        // Round-8 wave-3: defensive callee-saved spill
                        // before any GC-triggering dispatch CALL. Under
                        // `precise_maps` (or SB-CRASH-04 `safepoint_reg_spill`)
                        // the spill was already emitted before the inline
                        // cascade (so it dominates the inline-hit path too — see
                        // the PRECISE-MAPS FIX comment above); emitting it again
                        // here would be a redundant double-spill (and, with the
                        // shadow stack, an unbalanced double push). Gate-OFF:
                        // unchanged — this is the single conservative spill on
                        // the slow path.
                        if !self.precise_maps && !self.safepoint_reg_spill {
                            self.emit_pre_safepoint_spill();
                        }

                        if let Some(mic) = mic_ptr {
                            // MIC-optimized dispatch: pass MIC slot as 5th
                            // arg and (CRIT-1) PIC slot as 6th arg so the
                            // helper can populate the inline 4-way cascade
                            // via `JitPICSlot::install` on every successful
                            // resolution. A `pic_ptr == 0` tells the helper
                            // no PIC is installed for this site.
                            //
                            // On Windows x64, args 5 and 6 go on the stack
                            // at [RSP+32] and [RSP+40] (shadow + spill).
                            // RAX is caller-saved so we stage each
                            // pointer through it before storing — safe
                            // regardless of which call encoding
                            // `emit_call_absolute` picks (rel32 leaves
                            // RAX alone; the imm64-via-RAX fallback
                            // would overwrite RAX anyway, but that
                            // happens after we've already stored).
                            let pic_arg: i64 = pic_ptr.map(|p| p as *const _ as i64).unwrap_or(0); // Cast: function pointer for JIT call target
                            #[cfg(target_os = "windows")]
                            {
                                // 5th arg at [RSP + 32]
                                let mic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(RAX, mic as *const _ as i64);
                                self.ic_patches.push((
                                    mic_arg_imm64_off,
                                    0,
                                    mic as *const _ as usize,
                                ));
                                // MOV [RSP + 32], RAX
                                self.rex_w();
                                self.buf.emit(&[0x89, 0x44, 0x24, 0x20]);
                                // 6th arg at [RSP + 40]
                                let pic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(RAX, pic_arg);
                                if let Some(pic) = pic_ptr {
                                    self.ic_patches.push((
                                        pic_arg_imm64_off,
                                        1,
                                        pic as *const _ as usize,
                                    ));
                                }
                                // MOV [RSP + 40], RAX
                                self.rex_w();
                                self.buf.emit(&[0x89, 0x44, 0x24, 0x28]);
                            }
                            #[cfg(not(target_os = "windows"))]
                            {
                                // SysV: 5th arg in R8, 6th in R9.
                                let mic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R8, mic as *const _ as i64);
                                self.ic_patches.push((
                                    mic_arg_imm64_off,
                                    0,
                                    mic as *const _ as usize,
                                ));
                                let pic_arg_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R9, pic_arg);
                                if let Some(pic) = pic_ptr {
                                    self.ic_patches.push((
                                        pic_arg_imm64_off,
                                        1,
                                        pic as *const _ as usize,
                                    ));
                                }
                            }
                            self.emit_call_absolute(self.helpers.invoke_virtual_mic);
                        } else {
                            // Plain dispatch without MIC
                            self.emit_call_absolute(self.helpers.invoke_dispatch);
                        }

                        // .done: patch the fast-path forward JMP(s).
                        //   - `done_patch`     (rel8, MIC inline)
                        //   - `done_patches32` (rel32, PIC inline — one
                        //                       per cache slot)
                        let done_off = self.buf.pos();
                        if let Some(patch) = done_patch {
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (done_off as i64) - (patch as i64 + 1);
                            debug_assert!(
                                (-128..=127).contains(&rel),
                                "inline MIC done jump overflowed rel8 ({} bytes)",
                                rel
                            );
                            Self::patch_rel8_or_bail(&mut self.buf, patch, rel);
                        }
                        for patch in &done_patches32 {
                            // `patch` points at the start of the rel32
                            // immediate (4 bytes); the JMP opcode (E9)
                            // precedes it by 1 byte. The displacement
                            // is computed from the byte AFTER the
                            // immediate (patch + 4) to the target.
                            // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                            let rel = (done_off as i64) - (*patch as i64 + 4);
                            debug_assert!(
                                // Widening: i32 bound -> i64 (range comparison)
                                (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                "inline PIC done jump overflowed rel32 ({} bytes)",
                                rel
                            );
                            self.buf
                                .try_patch_i32(*patch, rel as i32) // Cast: rel32 displacement
                                .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                        }
                        // T1.1.2 — every virtual/interface dispatch is
                        // a full safepoint: the callee may allocate,
                        // throw, or block. Record the oop map before
                        // the return value is pushed so the GC root
                        // walker has precise coverage at the return PC.
                        self.emit_oop_map_for_safepoint();

                        // After the dispatch returns, check whether the
                        // callee threw a Java exception. The dispatch
                        // helpers return `i64::MIN` (and stash the
                        // exception object in `JIT_PENDING_EXCEPTION`)
                        // when the callee throws; without this guard the
                        // JIT would push the bogus return value and keep
                        // running, masking the real exception with a
                        // downstream secondary failure. The guard deopts
                        // out so the interpreter routes the exception
                        // through this method's exception table.
                        self.emit_post_invoke_exception_check(info_ref.return_type);

                        // Reclaim spill slots used for invoke args AND the
                        // popped arg values — restoring to `pre_pop_spill`
                        // here was the Bug-4 cursor creep (n slots per
                        // non-void dispatch).
                        self.next_spill_offset = post_pop_spill;

                        if info_ref.return_type != b'V' {
                            if matches!(info_ref.return_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                            // Tag the return as an oop when the
                            // descriptor is L... or [....
                            if matches!(info_ref.return_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                    }
                }
                for done in guarded_virtual_done_patches {
                    self.patch_rel32_to_here(done);
                }
                if op == 0xb9 {
                    pc += 5;
                } else {
                    pc += 3;
                }
            }
            _ => {
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }

    /// Lower `invokedynamic` at `pc`.
    #[allow(clippy::too_many_arguments)]
    fn walk_invokedynamic(
        &mut self,
        _code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // invokedynamic — unconditional deopt to the interpreter.
            //
            // This instruction is never actually JIT-executed: rather than
            // building call-site machinery for MethodHandle/CallSite
            // dispatch, the codegen jumps straight to the EXISTING shared
            // uncommon-trap deopt stub (reason 8 = `UnreachedCode`), whose
            // pre-existing policy (`DeoptimizationController::recommend_action`,
            // `jit/src/deopt.rs`) gives up immediately on first occurrence —
            // exactly the right fail-safe: if this exact program point is
            // ever actually reached at runtime (assertions enabled, or a
            // genuinely live indy), the method permanently reverts to
            // interpreter-only execution for the rest of the process (i.e.
            // today's status quo for that one method), while the
            // overwhelmingly common case — a dead `assert cond : "msg" +
            // var;` branch — never takes the trap and the surrounding hot
            // method compiles and runs at full JIT speed.
            //
            // Only the STACK EFFECT is modeled here (pop the call's args,
            // push a placeholder result of the correct kind) so the
            // compiler's simulated operand stack stays consistent for
            // whatever bytecode follows the (unreachable, but still
            // compiled) invokedynamic — e.g. the assert-message pattern's
            // `invokespecial AssertionError.<init>` + `athrow`.
            0xba => {
                // O(1) pc-indexed lookup — see `indy_info` field doc.
                let info = self
                    .indy_info_idx
                    .get(&pc)
                    .map(|&i| self.indy_info[i].clone());
                let Some((_pc, arg_slots, ret_type, arg_type_tags, bridge_site)) = info else {
                    // No resolver, or this site couldn't be resolved at
                    // compile time: fail safe and bail the whole method,
                    // exactly like every other CP-resolved metadata miss
                    // in this backend (see 0x12/0x13 above) — never emit
                    // unsound code for an unresolvable call site.
                    return WalkStep::Return(false);
                };

                self.flush_scratch_registers();

                // A BRIDGED site has a resolved, process-lifetime
                // handler, so call it directly instead of taking the
                // uncommon trap below. This is what removes RBC.7's premise
                // for the common `println("..." + x)`-after-a-loop shape:
                // with no trap at the indy bci there is nothing for an OSR
                // frame to resume imprecisely, so the OSR artifact keeps
                // running.
                //
                // The bridged set is `StringConcatFactory` and, since
                // 2026-08-23, `LambdaMetafactory`, `SwitchBootstraps` and
                // `ObjectMethods` — every bootstrap whose implementation
                // reaches the frame only through its operand stack and its
                // class id, which is all a bridge can offer. Between them
                // they cover a lambda or method reference, a
                // pattern-matching `switch`, and a record's
                // `equals`/`hashCode`/`toString`. The lambda one is what
                // lets a method that CREATES a lambda stay compiled at all:
                // before it, such a method took this trap on its first
                // execution and was retired with `MakeNotCompilable`, which
                // on Reactor/WebFlux assembly means essentially nothing is
                // ever compiled. The site pointer is self-describing (its
                // `kind` tag), so ONE call sequence and one entry serve
                // both. Every other bootstrap kind still falls through to
                // the trap.
                let bridge_entry = self.direct_helpers.indy_bridge;
                if bridge_site != 0 && bridge_entry != 0 && ret_type != b'V' {
                    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                        eprintln!("[cratonvm-jitc] indy bridge pc={} args={}", pc, arg_slots);
                    }
                    let pre_pop_spill = self.next_spill_offset;
                    // `pop_invoke_args`, not a bare `pop_stack` loop: it
                    // also hands back the per-argument OOP MARKS, and the
                    // staged buffer below is the only thing holding those
                    // references across a call that runs a bootstrap and
                    // allocates. Without `pending_staged_arg_oops` the
                    // safepoint map does not name them, which for a
                    // capturing lambda means every captured object is
                    // invisible to a collection that happens inside its own
                    // creation. The concat bridge this arm grew out of had
                    // the same gap and never showed it, because a
                    // `StringConcatFactory` argument is read into a Rust
                    // `String` before anything can allocate.
                    let (arg_slots_vec, arg_oops) = self.pop_invoke_args(arg_slots);
                    let post_pop_spill = self.next_spill_offset;
                    if arg_slots > 0 {
                        let Some(args_end) = self.checked_spill_range_end(pre_pop_spill, arg_slots)
                        else {
                            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                                eprintln!(
                                    "[cratonvm-jitc] indy bridge spill overflow pc={} base={} args={}",
                                    pc, pre_pop_spill, arg_slots
                                );
                            }
                            return WalkStep::Return(false);
                        };
                        self.next_spill_offset = args_end;
                        for (i, slot) in arg_slots_vec.iter().enumerate() {
                            let offset = pre_pop_spill + ((arg_slots - 1 - i) as i32) * 8;
                            self.load_slot_to_reg(RAX, *slot);
                            self.emit_store_local(offset, RAX);
                            if arg_oops[i] {
                                self.pending_staged_arg_oops.push(offset);
                            }
                        }
                    }
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm64(ARG_REGS[1], bridge_site as i64);
                    if arg_slots > 0 {
                        self.emit_lea_frame_slot(
                            ARG_REGS[2],
                            pre_pop_spill + ((arg_slots as i32) - 1) * 8,
                        );
                    } else {
                        self.emit_xor_reg_self(ARG_REGS[2]);
                    }
                    self.emit_mov_imm32_sx(ARG_REGS[3], arg_slots as i32);
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(bridge_entry);
                    self.emit_oop_map_for_safepoint();
                    self.next_spill_offset = post_pop_spill;
                    // The bridge is FALLIBLE, and the `LambdaMetafactory`
                    // half is fallible in a way the concat half is not: a
                    // bootstrap can raise `BootstrapMethodError` /
                    // `LambdaConversionError`, and the VM-side entry stashes
                    // that and returns the `i64::MIN` deopt sentinel. Route
                    // it through the same shared stub every other fallible
                    // helper call uses; without it the sentinel bits would
                    // be pushed AS AN OBJECT REFERENCE and dereferenced by
                    // whatever consumes the result.
                    //
                    // Added with the generic bridge rather than before it
                    // because the concat entry's only failure answer is `0`,
                    // i.e. a null `String` — wrong, but not a wild pointer.
                    self.emit_post_invoke_exception_check(ret_type);
                    // Pushed by the DESCRIPTOR, the same three-way choice
                    // the invoke lowering above makes: `xmm0` for `D`/`F`,
                    // a plain slot otherwise, oop-marked for `L`/`[`. A
                    // `V` site never reaches here — it is refused at
                    // admission, because a void bridge has no push for
                    // this to model.
                    if matches!(ret_type, b'D' | b'F') {
                        self.push_from_rax_as_xmm0();
                    } else {
                        self.push_from_rax();
                    }
                    if matches!(ret_type, b'L' | b'[') {
                        self.mark_top_as_oop();
                    }
                    pc += 5;
                    return WalkStep::Next(pc);
                }

                // Unconditional JMP to the shared deopt stub. Mirrors the
                // conditional String-intrinsic bail edges elsewhere in
                // this file (`emit_jcc_rel32_patch` + `deopt_stubs.push`),
                // but unconditional (`emit_jmp_rel32_patch`) since this
                // instruction is NEVER taken on the JIT-compiled path.
                // FIX (silent data corruption, HHH-15895 InPredicateTest /
                // AccumRepro3 residual): the fallback for this trap when no
                // precise snapshot exists ("safe reject" in the VM's
                // `try_osr()`) can only rewind execution to the method's OSR
                // entry bci, discarding every side effect committed by
                // JIT-compiled code between OSR entry and this trap — this
                // instruction can be reached arbitrarily late in a method
                // (e.g. inside a `println` well after earlier loops/mutations
                // already ran to completion), so "rewind to entry" silently
                // re-executes or drops already-committed work. Unlike the
                // experimental speculative-guard snapshots elsewhere in this
                // file (gated behind `deopt_real_enabled()`), this one is
                // unconditional: `emit_deopt_stubs` always uses it for reason
                // 8 (see the matching fix note there), because the imprecise
                // fallback here has PROVEN silent corruption risk, not just
                // performance cost. Reuses the existing OSR-exit snapshot
                // machinery (frame reconstruction from live registers/spill
                // slots) so the VM can resume precisely at THIS bci instead of
                // rewinding. Tagged `UnreachedCode` (the trap's true
                // reason) so the resume sinks' de-speculation applies the
                // give-up-immediately policy — see
                // `emit_osr_exit_map_at_reason`.
                //
                // deopt-osr indy-arg-types fix: record this call's own
                // per-argument type tags BEFORE the snapshot is built, so
                // the operand-stack loop can precisely type the top
                // `arg_type_tags.len()` stack entries instead of falling
                // back to the coarse `wide_fp` gate — see
                // `indy_stack_arg_types`'s doc comment.
                if !arg_type_tags.is_empty() {
                    self.indy_stack_arg_types.insert(pc, arg_type_tags.clone());
                }
                self.emit_osr_exit_map_at_reason(pc, crate::deopt::DeoptReason::UnreachedCode);

                // This trap is UNCONDITIONAL: every execution of this bci
                // deopts. So if the snapshot just built cannot be
                // materialised back into an interpreter frame, the method
                // is guaranteed to fail on its first compiled call --
                // `build_deopt_frame_inner` returns `None` and the resume
                // sink refuses with `precise deoptimization unavailable
                // ... refusing side-effecting replay`, a hard
                // `InternalError` rather than a slow path.
                //
                // The usual producer of an unmaterialisable slot here is
                // the coarse `wide_fp` gate in the snapshot's operand-stack
                // loop: in a method that touches any long/float/double, a
                // non-oop stack entry that is NOT one of this call site own
                // arguments has no per-entry width source and is recorded
                // `Unsupported`. javac `ClassReader.readInnerClasses` is
                // the canonical shape -- `optPoolEntry(int, IntFunction,
                // Object)` leaves an `int` underneath the lambda argument,
                // so the indy-arg tags type the top entry but not that one.
                //
                // Compiling such a method is strictly worse than
                // interpreting it, so bail the whole compile. This is what
                // the per-method SPRING-TESTCOMPILER / HIB-STOREDPROC-JIT
                // bans did by hand for the javac family; deciding it from
                // the snapshot itself covers every method with this shape
                // rather than the ones somebody happened to hit.
                let unresumable_trap = self
                    .deopt_points
                    .last()
                    .is_some_and(|p| !crate::deopt::frame_state_is_resumable(&p.frame_state));
                if unresumable_trap {
                    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                        eprintln!("[cratonvm-jitc] compile-bail unresumable-indy-trap bci={pc}");
                    }
                    self.buf.mark_codegen_unencodable("unresumable-indy-trap");
                }

                let patch = self.emit_jmp_rel32_patch();
                self.deopt_stubs.push((patch, pc, 8)); // 8 = DEOPT_REASON_UNREACHED_CODE

                // Stack-effect-only bookkeeping for subsequent (unreachable
                // but still-compiled) bytecode: pop the call's arguments —
                // popping is type-agnostic, so only the count matters —
                // then push a single placeholder of the correct STACK-SLOT
                // KIND for the descriptor's return type (control never
                // reaches past the trap above, so the placeholder's actual
                // bit-pattern is irrelevant; only its kind must match what
                // downstream codegen expects).
                for _ in 0..arg_slots {
                    self.pop_stack();
                }
                match ret_type {
                    b'V' => {}
                    b'F' | b'D' => {
                        self.emit_xor_reg_self(RAX);
                        self.push_from_rax_as_xmm0();
                    }
                    b'L' | b'[' => {
                        self.emit_xor_reg_self(RAX);
                        self.push_from_rax();
                        self.mark_top_as_oop();
                    }
                    _ => {
                        // int / long / short / byte / char / boolean
                        self.emit_xor_reg_self(RAX);
                        self.push_from_rax();
                    }
                }
                pc += 5;
            }
            _ => {
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }
}
