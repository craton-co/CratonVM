// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! round-10 `deopt2` sweep: `OSR_REFUSE_INLINED_SCOPE` and
//! `OSR_REFUSE_UNDESCRIBABLE_SLOT` were missing from
//! `cratonvm_jit::OSR_COMPILE_STATE_REFUSAL_TAGS`, even though both refusals
//! are reachable ONLY through `CompiledMethod::deopt_points` /
//! `osr_entry_frame_state` — exactly the same profile/speculation-shaped
//! metadata `OSR_REFUSE_UNCONDITIONAL_TRAP`, `OSR_REFUSE_UNRESUMABLE_EXIT`,
//! `OSR_REFUSE_AMBIGUOUS_EXIT_IMAGE` and `OSR_REFUSE_CONTRACT_DISAGREEMENT`
//! were already listed for.
//!
//! The consequence of the gap: `mark_osr_entry_rejected_by`
//! (`vm/src/runtime/interpreter/jit_bridge.rs`) stamps a refusal's memo with
//! the JIT install epoch only when `osr_refusal_depends_on_compile_state`
//! answers `true`. Before this fix that function read `false` for these two
//! tags, so a method rejected under one compile's inlining decision (or one
//! compile's per-slot register-allocation classification) stayed refused at
//! that `(method, entry_pc)` for the life of the process — surviving a
//! code-cache flush that would otherwise have produced a fresh artifact
//! without the problem. See `jit/src/osr_entry.rs`'s
//! `OSR_COMPILE_STATE_REFUSAL_TAGS` doc for the full argument.
//!
//! This file characterizes the FIXED behaviour (the two tags now classified
//! correctly) rather than the gap, since the gap was closed in the same
//! change that added this test.

use cratonvm_jit::bailout::{Bailout, BailoutReason};
use cratonvm_jit::{
    osr_refusal_depends_on_compile_state, osr_refusal_is_permanent, OSR_COMPILE_STATE_REFUSAL_TAGS,
    OSR_PERMANENT_REFUSAL_TAGS, OSR_REFUSE_INLINED_SCOPE, OSR_REFUSE_PC_NOT_AN_ENTRY,
    OSR_REFUSE_UNDESCRIBABLE_SLOT,
};

fn bailout_for(tag: &'static str) -> Bailout {
    Bailout::with_context(BailoutReason::UnsupportedShape(tag), "probe")
}

/// `OSR_REFUSE_INLINED_SCOPE`'s admission-time guard
/// (`!inlined_methods.is_empty() && !deopt_points.is_empty() && ...`) is
/// vacuously false whenever `deopt_points` is empty, so it can only ever fire
/// under the same `deopt_real_enabled()`-gated, profile-shaped metadata the
/// four already-classified tags depend on. It must be memoed with an
/// expiring (install-epoch-stamped) verdict, not a forever one.
#[test]
fn inlined_scope_refusal_is_classified_as_compile_state_dependent() {
    assert!(
        OSR_COMPILE_STATE_REFUSAL_TAGS.contains(&OSR_REFUSE_INLINED_SCOPE),
        "OSR_REFUSE_INLINED_SCOPE depends on which callees this compile inlined \
         and whether their deopt points recorded a caller chain — both profile- \
         shaped facts a recompile can change, exactly like the four tags already \
         in this list"
    );
    assert!(osr_refusal_depends_on_compile_state(&bailout_for(
        OSR_REFUSE_INLINED_SCOPE
    )));
}

/// `OSR_REFUSE_UNDESCRIBABLE_SLOT` only fires when `osr_entry_frame_state`
/// returns a precise `FrameState` for the entry bci — i.e. only when a deopt
/// point was recorded there — and its verdict is a function of THIS compile's
/// register allocation and per-slot classification.
#[test]
fn undescribable_slot_refusal_is_classified_as_compile_state_dependent() {
    assert!(
        OSR_COMPILE_STATE_REFUSAL_TAGS.contains(&OSR_REFUSE_UNDESCRIBABLE_SLOT),
        "OSR_REFUSE_UNDESCRIBABLE_SLOT is read off a recorded deopt point's \
         FrameState, which a recompile's different register allocation or \
         speculation can change independently of the bytecode"
    );
    assert!(osr_refusal_depends_on_compile_state(&bailout_for(
        OSR_REFUSE_UNDESCRIBABLE_SLOT
    )));
}

/// Every compile-state-dependent tag must also be a memoable (permanent, i.e.
/// artifact-pure) one — a refusal that depends on the OFFERED interpreter
/// state (a slot's live type, the local count) must never be memoed at all,
/// whatever epoch it might be stamped with. This is the same precondition
/// `jit/src/tests.rs`'s `compile_state_dependent_osr_refusals_are_memoable_and_classified`
/// asserts for the whole list; restated here so the two tags this fix adds
/// are covered even if that test's enumeration is ever narrowed.
#[test]
fn every_compile_state_tag_is_also_permanent() {
    for tag in OSR_COMPILE_STATE_REFUSAL_TAGS {
        assert!(
            OSR_PERMANENT_REFUSAL_TAGS.contains(&tag),
            "{tag} is classified as compile-state-dependent but not as a pure \
             function of the artifact — mark_osr_entry_rejected_by would then \
             memo a refusal that must never be memoed at all"
        );
        assert!(osr_refusal_is_permanent(&bailout_for(tag)));
    }
}

/// Negative control: the two classifications are NOT the same set, and the fix
/// must not have widened the compile-state predicate to "anything permanent".
///
/// `OSR_REFUSE_PC_NOT_AN_ENTRY` is the witness. It IS permanent — that pc is not
/// an OSR entry in this artifact and no amount of re-running changes that — but it
/// is NOT compile-state-dependent, because it is a pure function of the bytecode
/// rather than of the speculation or inlining the artifact was built under. So it
/// is exactly the case that must keep a non-expiring memo across a code-cache
/// flush, unlike the two tags this round added to the compile-state set.
///
/// (An earlier draft of this test asserted the opposite — that the tag is not in
/// `OSR_PERMANENT_REFUSAL_TAGS` — which conflated "permanent" with
/// "compile-state-dependent" and failed on its first execution.)
#[test]
fn a_non_memoable_refusal_does_not_depend_on_compile_state() {
    assert!(
        OSR_PERMANENT_REFUSAL_TAGS.contains(&OSR_REFUSE_PC_NOT_AN_ENTRY),
        "the witness must be a permanent refusal, or it cannot separate the two sets"
    );
    assert!(
        !OSR_COMPILE_STATE_REFUSAL_TAGS.contains(&OSR_REFUSE_PC_NOT_AN_ENTRY),
        "and it must not be compile-state-dependent"
    );
    assert!(!osr_refusal_depends_on_compile_state(&bailout_for(
        OSR_REFUSE_PC_NOT_AN_ENTRY
    )));
}
