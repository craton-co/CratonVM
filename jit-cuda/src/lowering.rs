//! Java bytecode → PTX kernel lowering — entry point.
//!
//! The architecture is two-stage:
//!
//! 1. [`loop_recog`](self::loop_recog) recognizes the canonical
//!    counted-loop pattern. Anything else returns
//!    [`LoweringError::UnsupportedNode`] with a precise reason.
//! 2. [`emit`](self::emit) walks the bytecode, simulates the JVM
//!    operand stack with PTX virtual registers, and emits PTX text.
//!
//! The emitted kernel is a SIMT element-wise body:
//!
//!  * Compute `tid = ctaid.x * ntid.x + tid.x`.
//!  * Compare `tid >= bound`; if so, `ret`.
//!  * Emit the loop body once with `iload iv` rewritten to `tid` and
//!    `iinc iv` skipped.
//!  * Emit each `*aload`/`*astore` with a bounds check that jumps to
//!    a shared failure label on out-of-bounds, writing `1` to
//!    `*failure_flag` and `ret`-ing.

use crate::analyzer::ParamKind;
use crate::emitter::{LoweringError, PtxKernel, PtxModule, PtxParam, PtxParamKind, RegDecl, RegKind};
use crate::signature::KernelSignature;
use rustjvm_reader::method::ClassFileMethod;

mod emit;
mod loop_recog;

use emit::{locate_bound, BoundSource, Emitter};
use loop_recog::{detect_loop, LoopShape};

/// Lower a single eligible method into a one-kernel PTX module.
///
/// The kernel name is `<class>__<method>__<descriptor mangled>` so it
/// is unique across the program. The body honours the convention
/// established by [`build_param_list`]: array params are `(ptr, len)`
/// pairs, scalar params are single typed registers, and the trailing
/// `failure_flag` is a device-pointer used for deopt signalling.
pub fn lower_method(
    class_name: &str,
    method: &ClassFileMethod,
    sig: &KernelSignature,
    sm_major: u32,
    sm_minor: u32,
) -> Result<PtxModule, LoweringError> {
    let kernel_name = mangle(class_name, &method.name, &method.descriptor);
    let params = build_param_list(sig);

    let code = method.code().ok_or_else(|| {
        LoweringError::UnsupportedNode("method has no Code attribute".into())
    })?;
    let bytes = &code.code;
    let shape = detect_loop(bytes)?;

    let mut emitter = Emitter::new(bytes, sig);
    emitter.bind_param_locals()?;

    match shape {
        LoopShape::StraightLine => {
            // Single-thread kernel: every CUDA thread runs the body
            // identically. Marshalling pre-allocates a 1-element output
            // buffer; collision on `ret_ptr` is benign because every
            // thread writes the same value.
            emitter.emit_tid();
            // Optional: skip extra threads. We could check `tid != 0`
            // and ret, to avoid redundant work; harmless either way.
            // Walk the whole method.
            emitter.walk(0, bytes.len(), None)?;
        }
        LoopShape::Counted(li) => {
            // Pre-loop: emit straight-line.
            emitter.iv_slot = Some(li.iv_slot);
            emitter.emit_tid();
            let bound = locate_bound(bytes, &li, sig)?;
            emitter.walk(0, li.header_pc, Some(&li))?;
            // Drop the operand stack — javac emits `iload iv; iload bound`
            // just before the exit-if, but the body walker hasn't seen
            // them yet (we start at `body_start_pc`). The pre-loop walk
            // ran 0..header_pc so the stack should be empty.
            if emitter.stack_len() != 0 {
                return Err(LoweringError::Internal(format!(
                    "pre-loop walk left {} operand(s) on the stack",
                    emitter.stack_len()
                )));
            }
            // Emit guard.
            match bound {
                BoundSource::ParamLen(idx) => {
                    let bound_reg = emitter.materialise_param_len(idx);
                    emitter.emit_loop_guard(&bound_reg);
                }
                BoundSource::Literal(v) => {
                    let bound_reg = emitter.materialise_literal_s32(v);
                    emitter.emit_loop_guard(&bound_reg);
                }
            }
            // Body — walks until it hits the back-branch goto.
            emitter.walk(li.body_start_pc, li.back_branch_pc + 5, Some(&li))?;
            // Post-loop — anything from li.exit_pc on (returns).
            // Clear the hit_back_branch flag so the post-loop walk runs.
            emitter.clear_back_branch();
            emitter.walk(li.exit_pc, bytes.len(), None)?;
        }
    }

    emitter.finalize_epilogue();

    let reg_decls = emitter.emit_reg_decls();
    let body = emitter.into_body();

    let kernel = PtxKernel {
        name: kernel_name,
        params,
        body,
        reg_decls,
    };
    Ok(PtxModule {
        sm_major,
        sm_minor,
        kernels: vec![kernel],
    })
}

/// Public alongside `lower_method`: the kernel parameter convention.
pub fn build_param_list(sig: &KernelSignature) -> Vec<PtxParam> {
    let mut out = Vec::new();
    for (i, k) in sig.param_kinds.iter().enumerate() {
        match k {
            ParamKind::I32 => out.push(PtxParam {
                name: format!("p{i}"),
                kind: PtxParamKind::S32,
            }),
            ParamKind::I64 => out.push(PtxParam {
                name: format!("p{i}"),
                kind: PtxParamKind::S64,
            }),
            ParamKind::F32 => out.push(PtxParam {
                name: format!("p{i}"),
                kind: PtxParamKind::F32,
            }),
            ParamKind::F64 => out.push(PtxParam {
                name: format!("p{i}"),
                kind: PtxParamKind::F64,
            }),
            ParamKind::I32Array
            | ParamKind::I64Array
            | ParamKind::F32Array
            | ParamKind::F64Array
            | ParamKind::I16Array
            | ParamKind::I8Array => {
                out.push(PtxParam {
                    name: format!("p{i}_ptr"),
                    kind: PtxParamKind::U64Ptr,
                });
                out.push(PtxParam {
                    name: format!("p{i}_len"),
                    kind: PtxParamKind::S32,
                });
            }
            ParamKind::Void => {}
        }
    }
    match sig.return_kind {
        ParamKind::Void => {}
        ParamKind::I32 | ParamKind::I64 | ParamKind::F32 | ParamKind::F64 => {
            out.push(PtxParam {
                name: "ret_ptr".to_string(),
                kind: PtxParamKind::U64Ptr,
            });
        }
        ParamKind::I32Array
        | ParamKind::I64Array
        | ParamKind::F32Array
        | ParamKind::F64Array
        | ParamKind::I16Array
        | ParamKind::I8Array => {
            out.push(PtxParam {
                name: "ret_ptr".to_string(),
                kind: PtxParamKind::U64Ptr,
            });
            out.push(PtxParam {
                name: "ret_len".to_string(),
                kind: PtxParamKind::S32,
            });
        }
    }
    out.push(PtxParam {
        name: "failure_flag".to_string(),
        kind: PtxParamKind::U64Ptr,
    });
    out
}

fn mangle(class_name: &str, method_name: &str, descriptor: &str) -> String {
    let mut out = String::with_capacity(class_name.len() + method_name.len() + descriptor.len());
    for ch in class_name
        .chars()
        .chain("__".chars())
        .chain(method_name.chars())
        .chain("__".chars())
        .chain(descriptor.chars())
    {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

impl<'a> Emitter<'a> {
    pub(crate) fn stack_len(&self) -> usize {
        self.stack.len()
    }

    pub(crate) fn clear_back_branch(&mut self) {
        self.hit_back_branch = false;
    }

    /// Materialise `pN_len` into a fresh s32 register.
    pub(crate) fn materialise_param_len(&mut self, idx: usize) -> emit::Reg {
        let r = self.regs.fresh_reg(RegKind::S32);
        use std::fmt::Write;
        writeln!(self.body, "    ld.param.s32 {}, [p{idx}_len];", r.name).unwrap();
        r
    }

    /// Materialise an integer literal into a fresh s32 register.
    pub(crate) fn materialise_literal_s32(&mut self, v: i32) -> emit::Reg {
        let r = self.regs.fresh_reg(RegKind::S32);
        use std::fmt::Write;
        writeln!(self.body, "    mov.s32 {}, {};", r.name, v).unwrap();
        r
    }

    pub(crate) fn into_body(self) -> String {
        self.body
    }

    pub(crate) fn emit_reg_decls(&self) -> Vec<RegDecl> {
        let mut out = Vec::new();
        if self.regs.u32_count > 0 {
            out.push(RegDecl { kind: RegKind::U32, count: self.regs.u32_count });
        }
        if self.regs.u64_count > 0 {
            out.push(RegDecl { kind: RegKind::U64, count: self.regs.u64_count });
        }
        if self.regs.s32_count > 0 {
            out.push(RegDecl { kind: RegKind::S32, count: self.regs.s32_count });
        }
        if self.regs.s64_count > 0 {
            out.push(RegDecl { kind: RegKind::S64, count: self.regs.s64_count });
        }
        if self.regs.f32_count > 0 {
            out.push(RegDecl { kind: RegKind::F32, count: self.regs.f32_count });
        }
        if self.regs.f64_count > 0 {
            out.push(RegDecl { kind: RegKind::F64, count: self.regs.f64_count });
        }
        if self.regs.pred_count > 0 {
            out.push(RegDecl { kind: RegKind::Pred, count: self.regs.pred_count });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangle_is_ptx_safe() {
        let m = mangle("com/example/Foo", "vectorAdd", "([I[I)[I");
        assert!(m.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(m.contains("vectorAdd"));
        assert!(m.contains("Foo"));
    }

    #[test]
    fn param_list_for_vector_add() {
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::I32Array, ParamKind::I32Array],
            return_kind: ParamKind::I32Array,
            estimated_work: 1 << 20,
        };
        let params = build_param_list(&sig);
        // (a_ptr, a_len, b_ptr, b_len, ret_ptr, ret_len, failure_flag) = 7
        assert_eq!(params.len(), 7);
        assert_eq!(params[0].name, "p0_ptr");
        assert_eq!(params[1].name, "p0_len");
        assert_eq!(params[2].name, "p1_ptr");
        assert_eq!(params[3].name, "p1_len");
        assert_eq!(params[4].name, "ret_ptr");
        assert_eq!(params[5].name, "ret_len");
        assert_eq!(params[6].name, "failure_flag");
    }

    #[test]
    fn param_list_scalar_return() {
        let sig = KernelSignature {
            param_kinds: vec![ParamKind::I32Array, ParamKind::I32Array],
            return_kind: ParamKind::I64,
            estimated_work: 1 << 20,
        };
        let params = build_param_list(&sig);
        // (a_ptr, a_len, b_ptr, b_len, ret_ptr, failure_flag) = 6
        assert_eq!(params.len(), 6);
        assert_eq!(params[4].name, "ret_ptr");
        assert_eq!(params[5].name, "failure_flag");
    }

    // ─────────────── real-class lowering tests ──────────────────────
    use crate::analyzer::{analyze, OffloadVerdict};
    use crate::test_support::load_method;

    fn lower_fixture(class: &str, method_name: &str, descriptor: &str) -> PtxModule {
        let method = load_method(class, method_name, descriptor);
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("fixture {class}.{method_name} not eligible: {v:?}"),
        };
        lower_method(class, &method, &sig, 7, 0)
            .unwrap_or_else(|e| panic!("lowering failed for {class}.{method_name}: {e}"))
    }

    #[test]
    fn vector_add_lowers_to_real_ptx() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();
        // Headers + entry name
        assert!(text.contains(".version 7.5"));
        assert!(text.contains(".target sm_70"));
        assert!(text.contains(".visible .entry EligibleVectorAdd__vectorAdd_"));
        // tid computation
        assert!(text.contains("%ctaid.x"));
        assert!(text.contains("%ntid.x"));
        assert!(text.contains("%tid.x"));
        assert!(text.contains("mad.lo.u32"));
        // Loop guard
        assert!(text.contains("setp.ge.s32"));
        assert!(text.contains("L_done"));
        // Two int loads, one int store, one int add, all in global mem.
        let n_int_loads = text.matches("ld.global.s32").count();
        let n_int_stores = text.matches("st.global.s32").count();
        let n_int_adds = text.matches("add.s32").count();
        assert!(n_int_loads >= 2, "expected ≥2 ld.global.s32, got {n_int_loads}\n{text}");
        assert!(n_int_stores >= 1, "expected ≥1 st.global.s32, got {n_int_stores}\n{text}");
        assert!(n_int_adds >= 1, "expected ≥1 add.s32, got {n_int_adds}\n{text}");
        // Bounds-fail label is emitted because we use array ops.
        assert!(text.contains("L_bounds_fail:"));
        assert!(text.contains("st.global.u32"));
        // The `failure_flag` param is referenced.
        assert!(text.contains("[failure_flag]"));
    }

    #[test]
    fn saxpy_lowers_with_float_ops() {
        let m = lower_fixture("EligibleSaxpy", "saxpy", "(F[F[F[F)V");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleSaxpy__saxpy_"));
        assert!(text.contains("ld.param.f32"));
        // Two float loads from arrays
        assert!(text.matches("ld.global.f32").count() >= 2);
        // One float multiply + one float add
        assert!(text.contains("mul.f32"));
        assert!(text.contains("add.f32"));
        // One float store
        assert!(text.contains("st.global.f32"));
        // Bounds fail
        assert!(text.contains("L_bounds_fail:"));
    }

    #[test]
    fn dot_product_lowers_with_long_math() {
        let m = lower_fixture("EligibleDotProduct", "dot", "([I[I)J");
        let text = m.render();
        assert!(text.contains(".visible .entry EligibleDotProduct__dot_"));
        // i2l conversions appear (a[i] is int, multiplied as long).
        assert!(text.contains("cvt.s64.s32"));
        // 64-bit multiply + add somewhere.
        assert!(text.contains("mul.lo.s64"));
        assert!(text.contains("add.s64"));
        // Scalar return → ret_ptr store of an s64.
        assert!(text.contains("[ret_ptr]"));
        assert!(text.contains("st.global.s64"));
        // Bounds-fail block present.
        assert!(text.contains("L_bounds_fail:"));
    }

    #[test]
    #[ignore = "diagnostic — prints PTX to stdout; run with --nocapture"]
    fn dump_vector_add_ptx() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        println!("\n----- PTX for EligibleVectorAdd::vectorAdd -----\n{}\n----- end -----", m.render());
    }

    /// `ptxas` round-trip — `#[ignore]` by default because most CI
    /// boxes have no CUDA toolkit. Set `PTXAS` env var to override
    /// the binary location; otherwise we look for `ptxas` on PATH.
    #[test]
    #[ignore = "requires NVIDIA CUDA toolkit (`ptxas`) — manual GPU verification"]
    fn ptxas_round_trip_vector_add() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();
        let tmpdir = std::env::temp_dir();
        let src_path = tmpdir.join("jit_cuda_vector_add.ptx");
        std::fs::write(&src_path, &text).expect("write ptx");
        let ptxas = std::env::var("PTXAS").unwrap_or_else(|_| "ptxas".to_string());
        let out = std::process::Command::new(&ptxas)
            .arg("-arch=sm_70")
            .arg(&src_path)
            .output()
            .expect("invoke ptxas");
        assert!(
            out.status.success(),
            "ptxas rejected the PTX:\nstdout: {}\nstderr: {}\nPTX:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            text,
        );
    }

    #[test]
    fn vector_add_kernel_has_correct_param_list_in_ptx() {
        let m = lower_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
        let text = m.render();
        assert!(text.contains(".param .u64 p0_ptr"));
        assert!(text.contains(".param .s32 p0_len"));
        assert!(text.contains(".param .u64 p1_ptr"));
        assert!(text.contains(".param .s32 p1_len"));
        assert!(text.contains(".param .u64 p2_ptr"));
        assert!(text.contains(".param .s32 p2_len"));
        assert!(text.contains(".param .u64 failure_flag"));
    }

    #[test]
    fn two_loops_method_is_rejected_by_lowering() {
        // The analyzer permits this method (no allocation, no invoke,
        // no field access) but the lowering refuses because the
        // canonical-pattern recognizer requires exactly one backward
        // branch.
        let method = load_method("TwoLoops", "clearTwice", "([I[I)V");
        let sig = match analyze(&method) {
            OffloadVerdict::Eligible(s) => s,
            v => panic!("expected TwoLoops.clearTwice to be eligible, got {v:?}"),
        };
        let err = lower_method("TwoLoops", &method, &sig, 7, 0)
            .expect_err("two-loop method must not lower");
        let msg = format!("{err}");
        assert!(
            msg.contains("multi-loop") || msg.contains("non-canonical"),
            "expected non-canonical error message, got: {msg}",
        );
    }

    #[test]
    fn straight_line_method_lowers_without_loop() {
        // Use the constructor `<init>` of EligibleVectorAdd: it has no
        // backward branch, just aload_0; invokespecial; return. The
        // invokespecial is rejected by the analyzer, so we can't take
        // the analyze-then-lower path. Instead, drive detect_loop
        // directly and verify StraightLine classification.
        //
        // Bytecode for "iconst_0; ireturn" — straight line.
        let bytes = vec![0x03, 0xAC];
        let shape = super::loop_recog::detect_loop(&bytes).unwrap();
        assert!(matches!(shape, super::loop_recog::LoopShape::StraightLine));
    }
}
