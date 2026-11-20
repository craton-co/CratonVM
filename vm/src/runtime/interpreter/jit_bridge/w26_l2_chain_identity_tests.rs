//! Interpreter round i1 wave 26, lane L2
//! (`interpreter-L2-stash-identity-gates-name-the-innermost-scope-so-no-chain-resumes-FIXED-20260928`):
//! an inlined chain's stash is judged by its OUTERMOST scope at the two
//! sinks with a chain path, entered through the sinks themselves.

use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_jit::deopt::{FrameValue, ReconstructedFrame, ResumeSemantics};
use std::sync::Arc;

const OWNER: &str = "cratonvm/test/W26ChainOwner";

fn static_void(name: &str, code: Vec<u8>) -> cratonvm_reader::method::ClassFileMethod {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    cratonvm_reader::method::ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 2,
                max_locals: 2,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

fn scope(key: &str, bci: u32, semantics: ResumeSemantics, local: i32) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        locals: vec![FrameValue::Int(i64::from(local))],
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics,
        caller_frames: Vec::new(),
    }
}

/// `OWNER.outer()V` is `invokestatic inner; return` and `OWNER.inner()V`
/// is `nop; return`. Returns `outer`'s cached body and the frame a trap at
/// `inner`'s bci 0 publishes with `inner` spliced into `outer` at bci 0:
/// the trapping scope names `inner` (local 0 = 9), its one caller scope
/// `outer` (local 0 = 7).
fn chain_fixture(shared: &SharedVm) -> (Arc<CachedBytecodeMethod>, ReconstructedFrame) {
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        let cid = cm
            .try_ensure_synthetic_class(OWNER, 0)
            .expect("Compatible mode fabricates");
        let class = cm.class_store.get_mut(cid).expect("fabricated");
        class
            .methods
            .push(static_void("outer", vec![0xb8, 0x00, 0x01, 0xb1]));
        class.methods.push(static_void("inner", vec![0x00, 0xb1]));
        cid
    };
    let outer = super::super::deopt_resume::resolve_inlined_callee(
        shared,
        cid,
        &format!("{OWNER}.outer:()V"),
    )
    .expect("the outer method resolves");
    let mut frame = scope(
        &format!("{OWNER}.inner:()V"),
        0,
        ResumeSemantics::REEXECUTE,
        9,
    );
    frame.caller_frames.push(scope(
        &format!("{OWNER}.outer:()V"),
        0,
        ResumeSemantics::RESUME,
        7,
    ));
    (outer, frame)
}

fn body_with_point_at(bci: u32) -> cratonvm_jit::CompiledMethod {
    use cratonvm_jit::deopt::{DeoptAction, DeoptReason, DeoptimizationPoint, FrameState};
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("test buffer");
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.deopt_points.push(DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason: DeoptReason::NullCheck,
        action: DeoptAction::Reinterpret,
        semantics: ResumeSemantics::REEXECUTE,
        speculation_id: cratonvm_jit::deopt::speculation_id(bci, DeoptReason::NullCheck),
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
    });
    cm
}

/// The method-entry doors' sink resumes the chain as TWO frames — `outer`
/// parked after its invoke, `inner` at its trapping bci — instead of
/// reading the chain as `inner`'s stash, charging `inner` and re-running
/// `outer` from entry (`None`).
#[test]
fn the_door_sink_resumes_a_chain_whose_outermost_scope_is_its_method() {
    std::thread::spawn(|| {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let (outer, chain) = chain_fixture(&shared);
        let compiled = body_with_point_at(0);
        let resumed = resume_or_despeculate_stash(
            &shared,
            &mut thread,
            &compiled,
            &outer,
            &chain,
            None,
            0,
            "w26-chain",
        )
        .expect("no re-materialisation heap failure");
        assert!(
            matches!(resumed, Some(CachedCallResult::FramePushed)),
            "a chain of this door's method resumes"
        );
        assert_eq!(thread.frames.len(), 2);
        assert_eq!(thread.frames[0].method_name(), "outer");
        assert_eq!(
            thread.frames[0].pc, 3,
            "parked after the in-progress invoke"
        );
        assert_eq!(thread.frames[0].get_local(0), Value::Int(7));
        assert_eq!(thread.frames[1].method_name(), "inner");
        assert_eq!(thread.frames[1].pc, 0, "re-executes the trapping bytecode");
        assert_eq!(thread.frames[1].get_local(0), Value::Int(9));
    })
    .join()
    .expect("test thread");
}

/// The same chain reaching a door of ANOTHER method is still a foreign
/// stash: nothing is pushed and the door re-runs.
#[test]
fn the_door_sink_still_refuses_a_chain_of_another_method() {
    std::thread::spawn(|| {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let (outer, chain) = chain_fixture(&shared);
        let inner = super::super::deopt_resume::resolve_inlined_callee(
            &shared,
            outer.declaring_class_id,
            &format!("{OWNER}.inner:()V"),
        )
        .expect("the inner method resolves");
        let compiled = body_with_point_at(0);
        let resumed = resume_or_despeculate_stash(
            &shared,
            &mut thread,
            &compiled,
            &inner,
            &chain,
            None,
            0,
            "w26-chain",
        )
        .expect("no re-materialisation heap failure");
        assert!(resumed.is_none(), "the chain is not `inner`'s activation");
        assert!(thread.frames.is_empty());
    })
    .join()
    .expect("test thread");
}

/// The OSR sink's gate accepts the chain for the live frame's method (the
/// outermost scope) and not for the spliced callee's, and the transfer it
/// guards then writes `outer` in place and pushes `inner`.
#[test]
fn the_osr_sink_gate_admits_a_chain_of_the_osr_d_method() {
    std::thread::spawn(|| {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let (outer, chain) = chain_fixture(&shared);
        let compiled = body_with_point_at(0);
        assert!(osr_exit_stash_is_this_bodys(
            &chain, &compiled, 0, OWNER, "outer", "()V"
        ));
        assert!(!osr_exit_stash_is_this_bodys(
            &chain, &compiled, 0, OWNER, "inner", "()V"
        ));

        // The live frame the OSR door would have: `outer`, at bci 0.
        let live = scope(
            &format!("{OWNER}.outer:()V"),
            0,
            ResumeSemantics::REEXECUTE,
            1,
        );
        super::super::deopt_resume::resume_real_ir_deopt(&shared, &mut thread, &outer, &live)
            .expect("the live frame is seeded");
        let plan = cratonvm_jit::OsrEntryPlan {
            entry_pc: 0,
            resume_bci: 0,
            native_offset: 0,
            dead_mask: 0,
            contract: cratonvm_jit::OsrContractSource::RegisterHomes,
            exit_policy: cratonvm_jit::OsrExitPolicy::ExactTransfer,
            expected_locals: Vec::new(),
        };
        assert!(
            transfer_osr_exit_into_live_frame(
                &shared,
                &mut thread,
                0,
                &chain,
                &compiled,
                &plan
            )
            .is_some(),
            "the chain transfer behind the gate takes the chain"
        );
        assert_eq!(thread.frames.len(), 2);
        assert_eq!(thread.frames[0].pc, 3);
        assert_eq!(thread.frames[0].get_local(0), Value::Int(7));
        assert_eq!(thread.frames[1].method_name(), "inner");
    })
    .join()
    .expect("test thread");
}
