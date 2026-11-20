// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Unit tests for the x86-64 backend.

use super::*;
use cratonvm_types::Value;
use crate::JitInvokeInfo;
// -----------------------------------------------------------------------
// JIT review 2026-09-12 — regressions for the single-pass codegen findings
// -----------------------------------------------------------------------

/// `compile` with every table empty — the shape all the fixtures below need.
macro_rules! review_compile {
    ($code:expr, $len:expr, $params:expr, $locals:expr, $heap:expr) => {
        compile(
            &$code,
            $len,
            $params,
            $locals,
            $heap,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
            None,
        )
    };
}

mod r22_inline_splice_point_publication_tests {
    use super::super::{
        InlineTestPoint, InlineTestRollback, INLINE_TEST_PUBLISHES_DEOPT,
        INLINE_TEST_PUBLISHES_POINT, INLINE_TEST_ROLLBACKS,
    };
    use super::*;

    const CALLER_KEY: &str = "Caller.f:(I)I";
    const CALLEE_KEY: &str = "Test.inlined:(I)I";
    // 0: iload_0; 1: invokestatic #1; 4: ireturn
    const CALLER: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];

    fn sites() -> HashMap<usize, crate::InlineSite> {
        let mut sites = HashMap::new();
        sites.insert(1, make_inline_site(&[0x1a, 0xac], 1, 1, true, b'I'));
        sites
    }

    /// Compile the caller with `mode` live, and the rollbacks it caused.
    fn compile_under(mode: InlineTestPoint) -> (CompiledMethod, Vec<InlineTestRollback>) {
        INLINE_TEST_ROLLBACKS.with(|r| r.borrow_mut().clear());
        INLINE_TEST_PUBLISHES_POINT.with(|f| f.set(mode));
        let compiled = keyed_int_method_with_inlines(&CALLER, 5, 1, CALLER_KEY, sites());
        INLINE_TEST_PUBLISHES_POINT.with(|f| f.set(InlineTestPoint::Off));
        let rollbacks = INLINE_TEST_ROLLBACKS.with(|r| std::mem::take(&mut *r.borrow_mut()));
        (compiled.expect("the caller must compile under every mode"), rollbacks)
    }

    /// The code with every `mov r10, imm64` immediate zeroed: the entry
    /// counter's address is per compile, so two compiles of one method differ
    /// there and nowhere else.
    fn masked(m: &CompiledMethod) -> Vec<u8> {
        let mut c = m.code_bytes().to_vec();
        let mut i = 0;
        while i + 10 <= c.len() {
            if c[i] == 0x49 && c[i + 1] == 0xBA {
                c[i + 2..i + 10].fill(0);
                i += 10;
            } else {
                i += 1;
            }
        }
        c
    }

    fn names_the_callee(m: &CompiledMethod) -> bool {
        m.deopt_points.iter().any(|p| p.frame_state.method_key == CALLEE_KEY)
    }

    /// The baseline a refused splice must reproduce: the stub-injection
    /// refusal of `inline_publishing_a_deopt_point_is_refused`.
    fn refused_baseline() -> CompiledMethod {
        INLINE_TEST_PUBLISHES_DEOPT.with(|f| f.set(true));
        let m = keyed_int_method_with_inlines(&CALLER, 5, 1, CALLER_KEY, sites());
        INLINE_TEST_PUBLISHES_DEOPT.with(|f| f.set(false));
        m.expect("the stub-refused caller must compile")
    }

    /// A point the postcondition can read and that names the callee, under a
    /// caller scope, at a callee bci: admitted, and the splice stays.
    #[test]
    fn a_well_formed_point_inside_a_splice_is_admitted() {
        let (control, _) = compile_under(InlineTestPoint::Off);
        let (m, rollbacks) = compile_under(InlineTestPoint::WellFormed);
        assert!(rollbacks.is_empty(), "nothing to roll back: {rollbacks:?}");
        assert_eq!(masked(&m), masked(&control), "building a point emits no code");
        let p = m
            .deopt_points
            .iter()
            .find(|p| p.frame_state.method_key == CALLEE_KEY)
            .expect("the admitted point is in the artifact");
        assert_eq!(p.frame_state.bci, 0);
        let caller = p.frame_state.caller.as_ref().expect("a caller scope");
        assert_eq!(caller.method_key, CALLER_KEY);
        // SAFETY: JIT-compiled machine code produced by this compiler from
        // valid bytecode, in an executable mapping.
        unsafe {
            assert_eq!(m.try_call(&[42]).expect("test JIT call"), 42);
        }
    }

    /// Each misidentified point -- a callee bci past the callee's code, or
    /// the enclosing owner's name on a callee bci -- rolls the splice back:
    /// the buffer, the operand stack and the three lockstep vectors return to
    /// the checkpoint, no by-bci entry names a dropped box, and the artifact
    /// is the refused-splice one.
    #[test]
    fn a_misidentified_point_inside_a_splice_rolls_the_splice_back() {
        let baseline = refused_baseline();
        let (control, _) = compile_under(InlineTestPoint::Off);
        assert_ne!(masked(&baseline), masked(&control), "the control splices");
        for mode in [InlineTestPoint::BciPastCallee, InlineTestPoint::EnclosingOwner] {
            let (m, rollbacks) = compile_under(mode);
            assert!(!rollbacks.is_empty(), "{mode:?}: the postcondition must fire");
            for r in &rollbacks {
                assert_eq!(r.observed, r.expected, "{mode:?}: buffer / stack / points restored");
                assert_eq!(r.lockstep, (r.observed.2, r.observed.2), "{mode:?}: lockstep vectors");
                assert_eq!(r.dangling_by_bci, 0, "{mode:?}: the by-bci purge");
            }
            assert_eq!(masked(&m), masked(&baseline), "{mode:?}: the refused-splice artifact");
            assert!(!names_the_callee(&m), "{mode:?}: no point of the dropped splice survives");
            // Not called: the fall-through `invokestatic #1` has no dispatch
            // row in this hand-built compile (the refused baseline of
            // `inline_publishing_a_deopt_point_is_refused` is not called
            // either).
        }
    }
}


mod c1_branch_profile_tests;

mod marker_word;
#[allow(unused_imports)]
pub use marker_word::*;
#[allow(unused_imports)]
use marker_word::*;
mod compile_dsub_dmul_ddiv;
#[allow(unused_imports)]
pub use compile_dsub_dmul_ddiv::*;
#[allow(unused_imports)]
use compile_dsub_dmul_ddiv::*;
mod a_nonzero_conditional_back_edge;
#[allow(unused_imports)]
pub use a_nonzero_conditional_back_edge::*;
#[allow(unused_imports)]
use a_nonzero_conditional_back_edge::*;
mod if_acmpne;
#[allow(unused_imports)]
pub use if_acmpne::*;
#[allow(unused_imports)]
use if_acmpne::*;
mod p87_xmm_intermediate_chaining_float;
#[allow(unused_imports)]
pub use p87_xmm_intermediate_chaining_float::*;
#[allow(unused_imports)]
use p87_xmm_intermediate_chaining_float::*;
mod inline_iaload_out_of_bounds_throws;
#[allow(unused_imports)]
pub use inline_iaload_out_of_bounds_throws::*;
#[allow(unused_imports)]
use inline_iaload_out_of_bounds_throws::*;
mod executed;
#[allow(unused_imports)]
pub use executed::*;
#[allow(unused_imports)]
use executed::*;
