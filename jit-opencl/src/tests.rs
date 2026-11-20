use super::*;
use cratonvm_jit_cuda::emitter::{PtxKernel, PtxModule, PtxParam, PtxParamKind, RegDecl, WorkBound};
use cratonvm_jit_cuda::kir::{Body, Instr, ParamRef, Ty};

#[test]
fn render_simple_elementwise() {
    let mut code = Body::new();
    code.push(Instr::ReadSpecial {
        dst: "%r0".into(),
        which: SpecialReg::ThreadIdxX,
    });
    code.push(Instr::LdParam {
        dst: "%rd0".into(),
        ty: Ty::U64,
        param: ParamRef::ArrayPtr(0),
    });
    code.push(Instr::Ret);

    let kernel = PtxKernel {
        name: "vector_add".into(),
        params: vec![
            PtxParam {
                name: "out".into(),
                kind: PtxParamKind::U64Ptr,
            },
            PtxParam {
                name: "a".into(),
                kind: PtxParamKind::U64Ptr,
            },
            PtxParam {
                name: "b".into(),
                kind: PtxParamKind::U64Ptr,
            },
            PtxParam {
                name: "n".into(),
                kind: PtxParamKind::S32,
            },
        ],
        code,
        reg_decls: vec![
            RegDecl {
                kind: RegKind::S32,
                count: 4,
            },
            RegDecl {
                kind: RegKind::U64,
                count: 4,
            },
            RegDecl {
                kind: RegKind::Pred,
                count: 1,
            },
        ],
    };

    let module = PtxModule {
        sm_major: 7,
        sm_minor: 0,
        kernels: vec![kernel],
        writes_param_mask: 0,
        reads_param_mask: 0,
        work_bound: WorkBound::Unknown,
    };

    let target = OpenClTarget::default();
    let rendered = render_module(&module, &target).expect("render succeeds");
    assert!(rendered.source.contains("__kernel void vector_add("));
    assert!(rendered.source.contains("__global void* out"));
    assert!(rendered.source.contains("get_local_id(0)"));
}
