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
        code: ret_only_body(),
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
        code: ret_only_body(),
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

#[test]
fn ptx_module_uses_failure_flag_detects_ld_param_failure_flag() {
    let clean = PtxModule {
        sm_major: 7,
        sm_minor: 5,
        kernels: vec![PtxKernel {
            name: "clean".to_string(),
            params: vec![],
            code: ret_only_body(),
            reg_decls: vec![],
        }],
        writes_param_mask: 0,
        reads_param_mask: 0,
        work_bound: crate::emitter::WorkBound::Unknown,
    };
    assert!(!clean.uses_failure_flag());

    let mut body = crate::kir::Body::new();
    body.push(crate::kir::Instr::LdParam {
        ty: crate::kir::Ty::U64,
        dst: "%rd1".to_string(),
        param: crate::kir::ParamRef::FailureFlag,
    });
    body.push(crate::kir::Instr::Ret);
    let with_flag = PtxModule {
        sm_major: 7,
        sm_minor: 5,
        kernels: vec![PtxKernel {
            name: "flagged".to_string(),
            params: vec![],
            code: body,
            reg_decls: vec![],
        }],
        writes_param_mask: 0,
        reads_param_mask: 0,
        work_bound: crate::emitter::WorkBound::Unknown,
    };
    assert!(with_flag.uses_failure_flag());
}

#[test]
fn can_fail_after_store_distinguishes_uniform_prologue_from_body_traps() {
    use crate::kir::{Body, Cmp, Imm, Instr, Operand, ParamRef, PredUse, SpecialReg, Ty};

    // Pattern 1: uniform prologue check (`len_b < len_a`) before body -> safe (`false`).
    let mut prologue_only = Body::new();
    prologue_only.push(Instr::LdParam {
        dst: "%r1".into(),
        ty: Ty::S32,
        param: ParamRef::ArrayLen(0),
    });
    prologue_only.push(Instr::LdParam {
        dst: "%r2".into(),
        ty: Ty::S32,
        param: ParamRef::ArrayLen(1),
    });
    prologue_only.push(Instr::ReadSpecial {
        dst: "%r3".into(),
        which: SpecialReg::ThreadIdxX,
    });
    prologue_only.push(Instr::Setp {
        cmp: Cmp::Ge,
        ty: Ty::S32,
        dst: "%p1".into(),
        a: Operand::Reg("%r3".into()),
        b: Operand::Reg("%r1".into()),
        and: None,
    });
    prologue_only.push(Instr::Bra {
        target: "L_done".into(),
        pred: Some(PredUse { reg: "%p1".into(), negated: false }),
    });
    prologue_only.push(Instr::Setp {
        cmp: Cmp::Lt,
        ty: Ty::S32,
        dst: "%p2".into(),
        a: Operand::Reg("%r2".into()),
        b: Operand::Reg("%r1".into()),
        and: None,
    });
    prologue_only.push(Instr::Bra {
        target: "L_bounds_fail".into(),
        pred: Some(PredUse { reg: "%p2".into(), negated: false }),
    });
    prologue_only.push(Instr::StGlobal {
        ty: Ty::S32,
        addr: "%rd1".into(),
        value: Operand::Imm(Imm::Int(42)),
    });
    prologue_only.push(Instr::Label("L_done".into()));
    prologue_only.push(Instr::Ret);
    prologue_only.push(Instr::Label("L_bounds_fail".into()));
    prologue_only.push(Instr::LdParam {
        dst: "%rd2".into(),
        ty: Ty::U64,
        param: ParamRef::FailureFlag,
    });
    prologue_only.push(Instr::Ret);

    let m_prologue = PtxModule {
        sm_major: 7,
        sm_minor: 5,
        kernels: vec![PtxKernel {
            name: "vec_add".into(),
            params: vec![],
            code: prologue_only.clone(),
            reg_decls: vec![],
        }],
        writes_param_mask: 2,
        reads_param_mask: 1,
        work_bound: crate::emitter::WorkBound::ParamLen(0),
    };
    assert!(m_prologue.uses_failure_flag());
    assert!(!m_prologue.can_fail_after_store());

    // Pattern 2: thread-varying bounds check (`tid >= len_b`) -> unsafe (`true`).
    let mut thread_varying = Body::new();
    thread_varying.push(Instr::LdParam {
        dst: "%r1".into(),
        ty: Ty::S32,
        param: ParamRef::ArrayLen(1),
    });
    thread_varying.push(Instr::ReadSpecial {
        dst: "%r2".into(),
        which: SpecialReg::ThreadIdxX,
    });
    thread_varying.push(Instr::Setp {
        cmp: Cmp::Ge,
        ty: Ty::U32,
        dst: "%p1".into(),
        a: Operand::Reg("%r2".into()),
        b: Operand::Reg("%r1".into()),
        and: None,
    });
    thread_varying.push(Instr::Bra {
        target: "L_bounds_fail".into(),
        pred: Some(PredUse { reg: "%p1".into(), negated: false }),
    });
    thread_varying.push(Instr::Label("L_bounds_fail".into()));
    thread_varying.push(Instr::LdParam {
        dst: "%rd2".into(),
        ty: Ty::U64,
        param: ParamRef::FailureFlag,
    });
    thread_varying.push(Instr::Ret);

    let m_varying = PtxModule {
        sm_major: 7,
        sm_minor: 5,
        kernels: vec![PtxKernel {
            name: "varying".into(),
            params: vec![],
            code: thread_varying,
            reg_decls: vec![],
        }],
        writes_param_mask: 2,
        reads_param_mask: 1,
        work_bound: crate::emitter::WorkBound::Unknown,
    };
    assert!(m_varying.can_fail_after_store());
}
