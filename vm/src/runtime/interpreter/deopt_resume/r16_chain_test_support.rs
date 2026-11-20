//! Round 16 lane chain: bodies with real chain points, and traps through
//! the real trampoline entry (a forced one through the tagged pad's
//! pointer, CH5W4-1), for the census tests.
use cratonvm_jit::deopt::{DeoptimizationPoint, FrameState};

/// A chain point `T.inner:()V@bci <- T.outer:()V@7`, plain values only.
pub(super) fn point(bci: u32) -> Box<DeoptimizationPoint> {
    let frame = |key: &str, bci: u32, caller: Option<Box<FrameState>>| FrameState {
        class_id: 0,
        method_key: key.to_string(),
        bci,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        caller,
    };
    Box::new(DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason: cratonvm_jit::deopt::DeoptReason::NullCheck,
        action: cratonvm_jit::deopt::DeoptAction::Reinterpret,
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        speculation_id: 0,
        frame_state: frame(
            "T.inner:()V",
            bci,
            Some(Box::new(frame("T.outer:()V", 7, None))),
        ),
    })
}

/// A body with one chain point (and forced-block cell) per bci.
pub(super) fn body(bcis: &[u32]) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    for &bci in bcis {
        let p = point(bci);
        let addr = &*p as *const DeoptimizationPoint as u64;
        cm._deopt_point_boxes.push(p);
        cm._forced_chain_deopt_points.push(addr);
        cm._forced_chain_deopt_counters
            .push(Box::new(cratonvm_jit::deopt::ForcedChainTripCell::default()));
    }
    cm
}

/// Trap at `cm`'s chain point `i` through `ir_deopt_entry` -- through the
/// forced pad's tagged pointer when `forced` -- and take the stash as a
/// sink does; the point address the sink holds.
pub(super) fn trap(cm: &cratonvm_jit::CompiledMethod, i: usize, forced: bool) -> usize {
    let addr = &*cm._deopt_point_boxes[i] as *const DeoptimizationPoint as usize;
    let tag = if forced {
        // u64 -> usize: bit 0 (64-bit targets).
        cratonvm_jit::deopt::FORCED_CHAIN_PAD_TAG as usize
    } else {
        0
    };
    let ptr = (addr | tag) as *const DeoptimizationPoint;
    assert_eq!(
        cratonvm_jit::deopt::ir_deopt_entry(ptr, 0, std::ptr::null()),
        i64::MIN
    );
    let (_, _, at) = cratonvm_jit::deopt::take_last_deopt_with_point().expect("stashed");
    assert_eq!(at, addr);
    at
}
