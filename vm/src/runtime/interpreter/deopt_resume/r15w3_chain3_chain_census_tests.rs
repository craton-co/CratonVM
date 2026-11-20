//! Round 15 wave 3 (lane chain3, CH2W3-1): the per-VM inlined-chain
//! resume census.
use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

fn frame(method_name: &str, local: i32) -> InlinedChainFrame {
    InlinedChainFrame {
        cached: Arc::new(CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: cratonvm_types::ClassId::new(0),
                class_name: Arc::from("T"),
                method_name: Arc::from(method_name),
                method_descriptor: Arc::from("()V"),
                source_file: None,
                code: Arc::from(&[0xb1u8][..]), // return
                exception_table: Arc::from(Vec::new().into_boxed_slice()),
                max_stack: 8,
                max_locals: 4,
                num_params: 0,
                is_synchronized: false,
                is_static: true,
            },
        )),
        locals: vec![Value::Int(local)],
        stack: Vec::new(),
        resume_pc: 0,
        executing_pc: 0,
        monitors: Vec::new(),
    }
}

/// A pushed chain counts once with its frames, an OSR-exit transfer's
/// outcome as in-place or refused, all in the VM that ran them.
#[test]
fn chain_resumes_and_refusals_are_counted_in_this_vm() {
    std::thread::spawn(|| {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let chain = DeoptFrameChain::ready(vec![frame("outer", 1), frame("inner", 2)]);
        let r = push_inlined_chain(&shared, &mut thread, chain, false);
        assert!(matches!(r, Some(CachedCallResult::FramePushed)));
        note_osr_chain_transfer(&shared, &Ok::<usize, String>(0));
        note_osr_chain_transfer(&shared, &Err::<usize, String>("refused".to_string()));
        let c = &shared.jit.bridge.inlined_chain_census;
        assert_eq!(c.pushed.load(Relaxed), 1);
        assert_eq!(c.frames.load(Relaxed), 2);
        assert_eq!(c.osr_in_place.load(Relaxed), 1);
        assert_eq!(c.refused.load(Relaxed), 1);
    })
    .join()
    .expect("test thread");
}
