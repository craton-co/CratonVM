//! Round 15 wave 4 (lane chain4; `r15w3-chain3-forced-chain-exit-bit-patch`):
//! the census tells a forced chain exit from a natural one.
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

/// A claim counts `forced` only for a trip the forced pad marked, once per
/// trip; a chain carrying the claim counts `forced-resumed` at its push; a
/// natural trip's chain counts neither. (Round 16 lane chain: the mark is
/// the stash's, CH5W4-1.)
#[test]
fn forced_exits_are_claimed_and_their_resumes_counted() {
    use super::r16_chain_test_support::{body, trap};
    std::thread::spawn(|| {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let cm = body(&[3, 9]);
        let natural = trap(&cm, 1, false);
        assert!(
            !claim_forced_chain_trip(&shared, &cm, natural),
            "a natural trip"
        );
        let at = trap(&cm, 0, true);
        assert!(
            !claim_forced_chain_trip(&shared, &cm, natural),
            "another point"
        );
        assert!(claim_forced_chain_trip(&shared, &cm, at), "the marked trip");
        assert!(
            claim_forced_chain_trip(&shared, &cm, at),
            "a second sink still sees it forced (G2), counted once"
        );

        let natural = DeoptFrameChain::ready(vec![frame("outer", 1), frame("inner", 2)]);
        assert!(push_inlined_chain(&shared, &mut thread, natural, false).is_some());
        let forced = DeoptFrameChain::ready(vec![frame("outer", 3), frame("inner", 4)])
            .with_forced_trip(true);
        assert!(push_inlined_chain(&shared, &mut thread, forced, false).is_some());

        let c = &shared.jit.bridge.inlined_chain_census;
        assert_eq!(c.forced.load(Relaxed), 1);
        assert_eq!(c.forced_resumed.load(Relaxed), 1);
        assert_eq!(c.pushed.load(Relaxed), 2);
    })
    .join()
    .expect("test thread");
}
