use crate::deopt::DeoptReason;
use crate::x64::Compiler;

fn compiler(protected: Vec<(u32, u32)>) -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = cratonvm_jit_api::JitRuntimeHelpers::default();
    helpers.throw_aioobe = compiler as usize;
    Compiler::new(
        "r22-sync-handover-test".to_string(),
        crate::ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        true,
        false,
        protected,
    )    }