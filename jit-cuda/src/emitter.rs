// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! PTX text emission.
//!
//! This module owns the **target side** of the lowering: given an
//! intermediate representation of a kernel body (whatever the
//! `lowering` module produces), it emits a valid PTX 7.5 module
//! string. Nothing in this file looks at JVM bytecode directly —
//! that's `lowering`'s job.
//!
//! The first cut is intentionally restricted; see [`PtxKernel`] for
//! what we can express today. Any IR node that doesn't have a
//! straight-line PTX equivalent must be rejected by the analyzer
//! (Part D) before reaching this stage.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoweringError {
    #[error("unsupported IR node: {0}")]
    UnsupportedNode(String),
    #[error("internal lowering error: {0}")]
    Internal(String),
}

/// How many work items the kernel's counted loop actually runs.
///
/// The launch grid used to be sized from the *largest array argument*,
/// which is right for an element-wise kernel (where the biggest array
/// is the iteration space) and badly wrong for anything whose inputs
/// are larger than its output. `out[i] = sum_j w[i*n + j] * x[j]`
/// iterates `out.length` times over a `w` that is `n` times bigger, so
/// a 2048-row matrix-vector product launched 4.2 million threads to do
/// 2048 threads' work — every extra one reaching the guard and exiting,
/// but only after the driver had created it.
///
/// Recording where the bound came from lets the host size the grid to
/// the loop instead. `Unknown` keeps the old largest-array behaviour,
/// which is what every shape that is not a single counted loop gets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkBound {
    /// Not a single counted loop, or the bound is not attributable to
    /// one parameter: fall back to the largest array argument.
    #[default]
    Unknown,
    /// The loop runs `p<N>.length` times.
    ParamLen(u32),
    /// The loop runs a compile-time-constant number of times.
    Literal(i32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtxModule {
    pub sm_major: u32,
    pub sm_minor: u32,
    pub kernels: Vec<PtxKernel>,
    /// Phase 10 #2 — bit-set of parameter indices written by the
    /// (single-kernel) module, populated by `lower_method` from the
    /// emitter's per-store tracking. Mirrors what will land on the
    /// `KernelSignature` once the module is wrapped in a
    /// `CompiledKernel`. Defaults to `0` (all params read-only) for
    /// hand-constructed unit-test modules — production code paths
    /// override it via the lowering pass.
    ///
    /// See `KernelSignature::writes_param_mask` for the consumer
    /// (the marshaller in `vm::runtime::offload`).
    pub writes_param_mask: u64,
    /// Bit-set of params the kernel READS element-wise. Mirror of
    /// `writes_param_mask`; see `KernelSignature::reads_param_mask` for
    /// why the chunked writeback needs it.
    pub reads_param_mask: u64,
    /// The counted loop's trip count, when it is attributable — see
    /// [`WorkBound`]. Read by the marshaller in `vm::runtime::offload`
    /// to size the launch grid.
    pub work_bound: WorkBound,
}

impl PtxModule {
    /// Render the module as a single PTX text blob suitable for
    /// `DeviceModule::from_ptx`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(".version 7.5\n");
        out.push_str(&format!(".target sm_{}{}\n", self.sm_major, self.sm_minor));
        out.push_str(".address_size 64\n\n");
        for k in &self.kernels {
            out.push_str(&k.render());
            out.push('\n');
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtxKernel {
    pub name: String,
    pub params: Vec<PtxParam>,
    /// Raw kernel body (the contents between `{` and `}`), already
    /// emitted by `lowering`. Each instruction terminated by `;\n`.
    pub body: String,
    pub reg_decls: Vec<RegDecl>,
}

impl PtxKernel {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(".visible .entry {}(\n", self.name));
        for (i, p) in self.params.iter().enumerate() {
            let comma = if i + 1 == self.params.len() { "" } else { "," };
            out.push_str(&format!("    .param {} {}{}\n", p.ptx_ty(), p.name, comma));
        }
        out.push_str(") {\n");
        for r in &self.reg_decls {
            out.push_str(&format!(
                "    .reg .{} %{}<{}>;\n",
                r.kind.suffix(),
                r.kind.prefix(),
                r.count
            ));
        }
        out.push('\n');
        out.push_str(&self.body);
        out.push_str("}\n");
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtxParam {
    pub name: String,
    pub kind: PtxParamKind,
}

impl PtxParam {
    fn ptx_ty(&self) -> &'static str {
        match self.kind {
            PtxParamKind::U64Ptr => ".u64",
            PtxParamKind::S32 => ".s32",
            PtxParamKind::S64 => ".s64",
            PtxParamKind::F32 => ".f32",
            PtxParamKind::F64 => ".f64",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtxParamKind {
    U64Ptr,
    S32,
    S64,
    F32,
    F64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegDecl {
    pub kind: RegKind,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegKind {
    /// 32-bit unsigned (predicates, indices).
    U32,
    /// 64-bit unsigned (pointers, indices).
    U64,
    /// 32-bit signed.
    S32,
    /// 64-bit signed.
    S64,
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
    /// Predicate register.
    Pred,
    /// Raw 16-bit. Not a JVM value type — a `short` on the operand
    /// stack is sign-extended into an `S32` register, same as the JVM
    /// does. This exists purely as the operand type PTX demands for
    /// `cvt.f32.f16`, which is the one instruction that reads half
    /// precision. Nothing is ever stored in a `B16` across a
    /// control-flow edge.
    B16,
}

impl RegKind {
    fn suffix(self) -> &'static str {
        match self {
            RegKind::U32 => "u32",
            RegKind::U64 => "u64",
            RegKind::S32 => "s32",
            RegKind::S64 => "s64",
            RegKind::F32 => "f32",
            RegKind::F64 => "f64",
            RegKind::Pred => "pred",
            RegKind::B16 => "b16",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            RegKind::U32 => "ru",
            RegKind::U64 => "rd",
            RegKind::S32 => "r",
            RegKind::S64 => "rl",
            RegKind::F32 => "f",
            RegKind::F64 => "fd",
            RegKind::Pred => "p",
            RegKind::B16 => "rs",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_module() {
        let m = PtxModule {
            sm_major: 7,
            sm_minor: 5,
            kernels: vec![],
            writes_param_mask: 0,
            reads_param_mask: 0,
            work_bound: crate::emitter::WorkBound::Unknown,
        };
        let s = m.render();
        assert!(s.contains(".version 7.5"));
        assert!(s.contains(".target sm_75"));
    }

    #[test]
    fn render_trivial_kernel() {
        let k = PtxKernel {
            name: "noop".to_string(),
            params: vec![],
            body: "    ret;\n".to_string(),
            reg_decls: vec![],
        };
        let s = k.render();
        assert!(s.contains(".visible .entry noop"));
        assert!(s.contains("    ret;\n"));
        assert!(s.ends_with("}\n"));
    }

    #[test]
    fn render_kernel_with_params_and_regs() {
        let k = PtxKernel {
            name: "vector_add".to_string(),
            params: vec![
                PtxParam {
                    name: "a".to_string(),
                    kind: PtxParamKind::U64Ptr,
                },
                PtxParam {
                    name: "n".to_string(),
                    kind: PtxParamKind::S32,
                },
            ],
            body: "    ret;\n".to_string(),
            reg_decls: vec![
                RegDecl {
                    kind: RegKind::S32,
                    count: 4,
                },
                RegDecl {
                    kind: RegKind::U64,
                    count: 2,
                },
            ],
        };
        let s = k.render();
        assert!(s.contains(".param .u64 a,"));
        assert!(s.contains(".param .s32 n"));
        assert!(s.contains(".reg .s32 %r<4>;"));
        assert!(s.contains(".reg .u64 %rd<2>;"));
    }
}
