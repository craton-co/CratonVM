//! Round 14 wave 3 (lane resume): CH3-4 (an OSR chain guard exit is
//! charged at its call site) and R14DP-1 (the call-site service takes a
//! never-redefined trapping body's own template).
use super::*;
use crate::config::VmConfig;
use cratonvm_jit::deopt::{
    speculation_id, DeoptAction, DeoptCause, DeoptEpochGuard, DeoptReason, DeoptimizationPoint,
    FrameState, FrameValue, ReconstructedFrame, ResumeSemantics, SavedRegisters,
    SPECULATION_ID_ANY,
};

const OUTER: &str = "r14w3/Loop.run:()I";

fn scope(key: &str, bci: u32, semantics: ResumeSemantics) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics,
        caller_frames: Vec::new(),
    }
}

fn cause(bci: u32, reason: DeoptReason) -> Option<DeoptCause> {
    Some(DeoptCause {
        reason,
        speculation_id: speculation_id(bci, reason),
    })
}



/// `elem` (bci 2) spliced into `k` at its call site bci 9, with the
/// innermost scope's semantics `semantics`.
fn r19_splice4_chain(
    semantics: cratonvm_jit::deopt::ResumeSemantics,
) -> cratonvm_jit::deopt::ReconstructedFrame {
    use cratonvm_jit::deopt::{ReconstructedFrame, ResumeSemantics};
    let frame = |key: &str, bci: u32, semantics: ResumeSemantics| ReconstructedFrame {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics,
        caller_frames: Vec::new(),
    };
    let mut chain = frame("r19s4/Outer.elem:([II)I", 2, semantics);
    chain
        .caller_frames
        .push(frame("r19s4/Outer.k:()V", 9, ResumeSemantics::RESUME));
    chain    }