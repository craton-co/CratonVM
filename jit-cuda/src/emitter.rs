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
