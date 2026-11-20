//! JIT round 16 wave 12 (lane throw3): the String access intrinsics' null
//! receiver under `CRATONVM_JIT_STRING_INTRINSIC_NPE=1` raises through the
//! reason-10 precise-frame stub at a protected site of a method compiled
//! with precise exceptional frames, where the wave-3 rule kept the deopt.

use super::{string_null_receiver_arm, StringNullReceiver};
use crate::deopt::DeoptReason;
use crate::x64::Compiler;
use cratonvm_jit_api::npe_action;

/// A bare `Compiler` with precise exceptional frames and one protected
/// range, shaped like `deopt_stubs.rs`'s `r9w2_spdeopt_tests` one.
fn precise_compiler(protected: Vec<(u32, u32)>) -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut helpers = cratonvm_jit_api::JitRuntimeHelpers::default();
    // Any non-zero addresses: the stubs only have to be emittable.
    helpers.set_throw_bci = precise_compiler as usize;
    helpers.jit_npe_with_action = precise_compiler as usize;
    Compiler::new(
        "r16w12-throw3-test".to_string(),
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
    )
}

#[test]
fn the_switch_off_or_a_splice_keeps_the_deopt() {
    for precise in [false, true] {
        assert_eq!(
            string_null_receiver_arm(false, false, precise),
            StringNullReceiver::Deopt
        );
        assert_eq!(
            string_null_receiver_arm(true, true, precise),
            StringNullReceiver::Deopt
        );
    }
}

#[test]
fn a_protected_precise_site_takes_the_precise_stub_and_others_the_shared_one() {
    assert_eq!(
        string_null_receiver_arm(true, false, true),
        StringNullReceiver::PreciseFrame
    );
    assert_eq!(
        string_null_receiver_arm(true, false, false),
        StringNullReceiver::SharedStub
    );
}

/// The precise arm files a `PendingException` point at the site, a
/// reason-10 stub, and the INVOKE_RECEIVER action the stub hands to
/// `jit_npe_with_action` (the JEP 358 text names the invoke).
#[test]
fn the_precise_arm_publishes_the_frame_with_the_invoke_action() {
    const SITE: usize = 13;
    let mut c = precise_compiler(vec![(0, 40)]);
    assert!(c.pc_is_protected(SITE));
    assert!(c.emit_precise_array_npe_check(SITE, npe_action::INVOKE_RECEIVER));
    assert_eq!(c.deopt_point_pcs, vec![SITE]);
    assert!(matches!(
        c.deopt_points[0].reason,
        DeoptReason::PendingException
    ));
    assert!(c.exc_frame_box_ptr_by_bci.contains_key(&SITE));
    assert_eq!(
        c.precise_npe_action_by_bci.get(&SITE).copied(),
        Some(u32::from(npe_action::INVOKE_RECEIVER))
    );
    assert_eq!(c.deopt_stubs.len(), 1);
    assert_eq!((c.deopt_stubs[0].1, c.deopt_stubs[0].2), (SITE, 10));
}

/// Wave 13 (w12b: `msg=null` under load): the String arm's trap-site key
/// rides above the action in the word the reason-10 stub hands
/// `jit_npe_with_action`, so the NPE's snapshot names the invoke's bci
/// exactly instead of the frame's stale safepoint-id slot.
#[test]
fn the_keyed_precise_arm_packs_the_trap_key_into_the_stub_word() {
    const SITE: usize = 13;
    const KEY: u32 = 0x0012_34;
    let packed = u32::from(npe_action::INVOKE_RECEIVER) | (KEY << 8);
    let mut c = precise_compiler(vec![(0, 40)]);
    assert!(c.emit_precise_npe_check_keyed(SITE, npe_action::INVOKE_RECEIVER, KEY));
    assert_eq!(
        c.precise_npe_action_by_bci.get(&SITE).copied(),
        Some(packed)
    );
    let stub_start = c.buf.pos();
    c.emit_deopt_stubs();
    let stub = &c.buf.as_slice()[stub_start..c.buf.pos()];
    assert!(
        stub.windows(4).any(|w| w == packed.to_le_bytes()),
        "the reason-10 stub must pass the packed action word"
    );
}

/// Without a key the word is the bare action: the array and `putfield`
/// sites' stubs are byte-identical to before.
#[test]
fn the_unkeyed_precise_arm_files_the_bare_action() {
    let mut c = precise_compiler(vec![(0, 40)]);
    assert!(c.emit_precise_array_npe_check(7, npe_action::INVOKE_RECEIVER));
    assert_eq!(
        c.precise_npe_action_by_bci.get(&7).copied(),
        Some(u32::from(npe_action::INVOKE_RECEIVER))
    );
}

/// Outside every protected range the precise arm emits nothing and
/// answers `false`, so the caller keeps its own check.
#[test]
fn an_unprotected_site_gets_no_precise_stub() {
    let mut c = precise_compiler(vec![(100, 140)]);
    let before = c.buf.pos();
    assert!(!c.emit_precise_array_npe_check(13, npe_action::INVOKE_RECEIVER));
    assert_eq!(c.buf.pos(), before);
    assert!(c.deopt_stubs.is_empty());
    assert!(c.precise_npe_action_by_bci.is_empty());
}
