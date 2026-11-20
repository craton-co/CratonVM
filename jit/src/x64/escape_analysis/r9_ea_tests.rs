use super::{
    analyze_escapes, analyze_escapes_keeping_locks, analyze_escapes_with_handlers,
    find_modified_locals,
    match_invariant_aaload, plan_scalar_replacement, InvokeSpecialShape,
};
use crate::JitInvokeInfo;
use rustc_hash::FxHashMap;

/// A leaked `<init>()V` info, as the existing plan tests build one.
fn init_void() -> *const JitInvokeInfo {
    // LEAK(intentional): test-only; plan_scalar_replacement reads it through a raw pointer.
    Box::leak(Box::new(JitInvokeInfo {
        class_name: "Foo",
        method_name: "<init>",
        descriptor: "()V",
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 1,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as *const JitInvokeInfo
}

fn trivial_init_at(pc: usize) -> FxHashMap<usize, InvokeSpecialShape> {
    let mut shapes = FxHashMap::default();
    shapes.insert(
        pc,
        InvokeSpecialShape {
            arg_slots: 1,
            is_trivial_void_init: true,
        },
    );
    shapes
}

fn non_escaping(pc: usize) -> std::collections::HashSet<usize> {
    let mut s = std::collections::HashSet::new();
    s.insert(pc);
    s
}

/// `Foo f = new Foo(); f.x = 1; String s = "" + n; return f.x;` — an
/// `invokedynamic` between two field accesses of a local scalar object.
/// The plan's catch-all used to drop the local, leaving the `getfield`
/// after the indy unmapped: a real field read on the dummy-null `new`.
#[test]
fn an_invokedynamic_does_not_unmap_a_local_scalar_objects_later_field_ops() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0xba, 0x00, 0x04, 0x00, 0x00, // 13: invokedynamic #4
        0x57, // 18: pop
        0x2b, // 19: aload_1
        0xb4, 0x00, 0x03, // 20: getfield #3
        0xac, // 23: ireturn
    ];
    let len = code.len();
    assert!(analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0));
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(plan.objects.contains_key(&0), "Foo is scalar-replaced");
    assert!(
        plan.field_ops.contains_key(&10),
        "putfield before the indy is mapped"
    );
    assert!(
        plan.field_ops.contains_key(&20),
        "getfield AFTER the indy must be mapped too, or it reads the dummy null"
    );
}

/// `p.x = a + 1` keeps `p` on the operand stack across the `iadd`. The
/// arithmetic used to hit `analyze_escapes`' catch-all and escape `p`.
#[test]
fn arithmetic_under_a_tracked_receiver_does_not_escape_it() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x1a, // 9: iload_0
        0x04, // 10: iconst_1
        0x60, // 11: iadd
        0xb5, 0x00, 0x03, // 12: putfield #3
        0x2b, // 15: aload_1
        0xb4, 0x00, 0x03, // 16: getfield #3
        0xac, // 19: ireturn
    ];
    let len = code.len();
    assert!(
        analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0),
        "an iadd reads and writes primitives only"
    );
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(plan.field_ops.contains_key(&12));
    assert!(plan.field_ops.contains_key(&16));
}

/// `{ Foo f = new Foo(); f.x = 1; } int y = 5; return y;` — javac reuses
/// slot 1. The dead object must stop being published in that slot.
#[test]
fn a_primitive_store_ends_the_objects_provenance_in_that_slot() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0x08, // 13: iconst_5
        0x3c, // 14: istore_1
        0x1b, // 15: iload_1
        0xac, // 16: ireturn
    ];
    let len = code.len();
    assert!(analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0));
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(plan.objects.contains_key(&0));
    assert_eq!(
        plan.local_prov_at.get(&14).map(|v| v.as_slice()),
        Some([(1usize, 0usize)].as_slice()),
        "slot 1 still holds the object when istore_1 starts"
    );
    assert!(
        !plan.local_prov_at.contains_key(&15),
        "after istore_1 slot 1 is an int, not a VirtualObject"
    );
}

/// `if (c) { Foo f = new Foo(); return f.x; } return 0;` — the `return`
/// ends `f`'s path, so the branch target after it must not escape `f`.
#[test]
fn an_early_return_does_not_hand_its_locals_to_the_next_branch_target() {
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x99, 0x00, 0x10, // 1: ifeq +16 -> 17
        0xbb, 0x00, 0x01, // 4: new #1
        0x59, // 7: dup
        0xb7, 0x00, 0x02, // 8: invokespecial <init>()V
        0x4c, // 11: astore_1
        0x2b, // 12: aload_1
        0xb4, 0x00, 0x03, // 13: getfield #3
        0xac, // 16: ireturn
        0x03, // 17: iconst_0   (branch target)
        0xac, // 18: ireturn
    ];
    let len = code.len();
    assert!(
        analyze_escapes(&code, len, &trivial_init_at(8)).contains(&4),
        "the object's whole life ends at the ireturn at 16"
    );
    let new_info = vec![(4usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(8usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(4), &new_info, &invoke_info, 0);
    assert!(plan.objects.contains_key(&4));
    assert!(plan.init_skips.contains(&8));
    assert!(plan.field_ops.contains_key(&13));
    assert!(!plan.local_prov_at.contains_key(&17));
}

/// `new; astore_1; aload_1; invokespecial <init>()V` — the init skip is
/// owned by the object the walk saw, not by `init_pc - 4`.
#[test]
fn a_non_canonical_init_keeps_its_skip() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x4c, // 3: astore_1
        0x2b, // 4: aload_1
        0xb7, 0x00, 0x02, // 5: invokespecial <init>()V
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0xb1, // 13: return
    ];
    let len = code.len();
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(5usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(plan.objects.contains_key(&0));
    assert!(
        plan.init_skips.contains(&5),
        "the <init> of a scalar object must be skipped, or it is called on the dummy null"
    );
}

/// Provenance lost while live (here: a `wide` the plan does not decode)
/// demotes the object to a real allocation — and takes its monitor
/// elision with it.
#[test]
fn lost_provenance_demotes_the_object_and_its_monitor_ops() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0xc2, // 9: monitorenter
        0xc4, 0x15, 0x00, 0x02, // 10: wide iload 2
        0x57, // 14: pop
        0x2b, // 15: aload_1
        0xc3, // 16: monitorexit
        0x2b, // 17: aload_1
        0xb4, 0x00, 0x03, // 18: getfield #3
        0xac, // 21: ireturn
    ];
    let len = code.len();
    assert!(
        !analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0),
        "a `wide` is a barrier for the escape walk too"
    );
    // Handed the object anyway (the two walks disagreeing is exactly the
    // case this guards), the plan must refuse it whole.
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(
        plan.objects.is_empty(),
        "a poisoned object is allocated for real"
    );
    assert!(plan.field_ops.is_empty());
    assert!(plan.init_skips.is_empty());
    assert!(
        !plan.monitor_scalar_ops.contains(&9),
        "the lock on a real allocation must not be elided without a relock record"
    );
}

/// `aload 70; iload_1; aaload` in a loop that re-assigns local 70. The
/// modified-locals mask saturates every local >= 63 onto bit 63, and the
/// hoist matcher used to ignore that bit for a local >= 64.
#[test]
fn a_high_local_the_loop_reassigns_is_not_hoisted() {
    // The loop body's store: `astore 70`.
    let body: Vec<u8> = vec![0x3a, 70];
    let modified = find_modified_locals(&body, 0, body.len());
    assert_ne!(modified & (1u64 << 63), 0, "local 70 saturates onto bit 63");
    let seq: Vec<u8> = vec![0x19, 70, 0x1b, 0x32]; // aload 70; iload_1; aaload
    assert!(
        match_invariant_aaload(&seq, 0, modified, seq.len()).is_none(),
        "a re-assigned array local must not be read once in the pre-header"
    );
    assert!(
        match_invariant_aaload(&seq, 0, 0, seq.len()).is_some(),
        "an unmodified one still is"
    );
}

/// `x += 1000` compiles to `wide iinc`. It used to reach both walks'
/// catch-alls — which disagreed (the escape walk kept the locals, the plan
/// dropped them), leaving `f.x` after the `iinc` a real read of the dummy
/// null. It is now modelled exactly in both.
#[test]
fn a_wide_iinc_neither_escapes_nor_unmaps_a_local_scalar_object() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0xc4, 0x84, 0x00, 0x02, 0x03, 0xe8, // 13: wide iinc 2, 1000
        0x2b, // 19: aload_1
        0xb4, 0x00, 0x03, // 20: getfield #3
        0xac, // 23: ireturn
    ];
    let len = code.len();
    assert!(analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0));
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(plan.objects.contains_key(&0));
    assert!(plan.field_ops.contains_key(&10));
    assert!(
        plan.field_ops.contains_key(&20),
        "the getfield after `wide iinc` must stay mapped to the frame slot"
    );
}

/// `Foo f = new Foo(); synchronized(f) { f.x = 1; }` — the exact bytecode
/// `s32_phase_c_plan_tracks_scalar_monitors` (`jit/src/x64/tests.rs`) uses
/// to exercise `plan_scalar_replacement`'s monitor-relock ("Phase C")
/// code. That test hands `plan_scalar_replacement` a HAND-BUILT
/// `non_escaping` set rather than this function's real output — it never
/// asks what `analyze_escapes` itself says about this bytecode. This test
/// closes that gap.
///
/// **THE ASSERTION FLIPPED ON 2026-09-22.** It was written as `!contains`,
/// with a doc comment saying it was "expected to FLIP the day
/// `analyze_escapes` gains precise `monitorenter`/`monitorexit` arms that
/// pop the receiver without escaping it". Those arms exist now, so the
/// answer is `contains`: `f` is confirmed NON-escaping, Phase C can receive
/// a candidate, and `plan_scalar_replacement`'s monitor-relock machinery —
/// real, tested, and consumed by `x64/op_object.rs`'s elision arm — is
/// reachable in production for the first time.
///
/// Kept rather than deleted, and kept under its original name, because a
/// flipped pin is the cheapest possible detector for the change being
/// reverted or walked back: if this reads `!contains` again, the whole
/// feature below it is inert again and nothing else in the tree would say
/// so. The three-line history is the point.
///
/// What it does NOT prove, and what its sibling tests are for: this is the
/// shape with no exception table, which javac never emits for a
/// `synchronized` block. `the_goto_over_javacs_monitor_handler_is_walked_through_when_the_handlers_are_known`
/// covers the real javac shape, and it needed EA-2 as well as these arms —
/// the arms alone were provably worth nothing on compiler output, which is
/// why they had not been written.
#[test]
fn monitorenter_on_an_otherwise_local_receiver_no_longer_escapes_it() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1            -> f in local 1
        0x2b, // 8: aload_1             -> [f]
        0x59, // 9: dup                 -> [f, f]
        0x4d, // 10: astore_2           -> local 2 = f, [f]
        0xc2, // 11: monitorenter       -> lock f
        0x2b, // 12: aload_1            -> [f]
        0x04, // 13: iconst_1           -> [f, 1]
        0xb5, 0x00, 0x03, // 14: putfield #3
        0x2c, // 17: aload_2            -> [f]
        0xc3, // 18: monitorexit        -> unlock
        0xb1, // 19: return
    ];
    let len = code.len();
    assert_eq!(
        len, 20,
        "sanity: this must match the bytecode s32_phase_c_plan_tracks_scalar_monitors uses"
    );
    assert!(
        analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0),
        "`monitorenter`/`monitorexit` have exact arms: they pop one slot \
         and do not escape what they popped. `analyze_escapes` returns the \
         CONFIRMED NON-ESCAPING set, so the receiver's `new` pc must be \
         PRESENT in it. A failure here means the arms were reverted and \
         Phase C is inert again -- see this test's own doc comment."
    );
}

/// The second half of the round-10 monitor finding, KEPT AS A HISTORICAL
/// PIN after the fix: giving `analyze_escapes` the exact
/// `monitorenter`/`monitorexit` arms that page asked for would NOT, ON ITS
/// OWN, make the synchronized receiver non-escaping in the shape javac
/// actually emits.
///
/// That is still true and it is still worth executing, but read the
/// assertions below with the 2026-09-22 change in mind: the arms exist now,
/// so the `javac` array's receiver no longer escapes at its `monitorenter`
/// — it escapes at the `goto`, which is the finding. This test calls
/// `analyze_escapes`, i.e. the see-through DISABLED (`handler_pcs = None`),
/// which is why the answer is unchanged. Its sibling
/// `the_goto_over_javacs_monitor_handler_is_walked_through_when_the_handlers_are_known`
/// runs the same shape with the handlers supplied and gets the opposite
/// answer; the pair is what shows the two gates were independent.
///
/// The bytecode below is the real javac shape for
/// `void m() { Foo f = new Foo(); synchronized (f) { f.x = 1; } }` — the
/// one the sibling test above omits. javac must protect the body with a
/// catch-all handler that re-exits the monitor and rethrows, and it must
/// jump the normal path over that handler. That `goto` is the point of this
/// test.
///
/// # Why substituting `pop` is the right experiment
///
/// `analyze_escapes` has no `0xC2`/`0xC3` arm, so "what would the ideal arm
/// do?" cannot be asked of the function directly. But the ideal arm is
/// fully specified: `monitorenter` and `monitorexit` each pop exactly one
/// reference and push nothing, and the whole point of the proposed fix is
/// that the popped provenance must NOT be escaped. That is byte for byte
/// the contract of this function's `pop` (`0x57`) arm:
/// `abs_stack.pop(); pc += 1;`. So the `ideal` copy below — the same
/// bytecode with `0xC2`/`0xC3` rewritten to `0x57` — is an exact simulation
/// of a fixed `analyze_escapes` running on the javac shape, and it needs no
/// change to the function under test.
///
/// # What each assertion follows from
///
/// `analyze_escapes` returns the set of `new` PCs it CONFIRMED
/// non-escaping, so "the receiver escapes" is `!contains(&0)`.
///
/// 1. `plain` (the same allocation with the `synchronized` wrapper removed)
///    is non-escaping — `contains(&0)` — which is what stops the two
///    assertions below from being vacuously true about some unrelated
///    property of this `new; dup; <init>; astore; putfield` sequence.
/// 2. `javac` escapes it today, at the `monitorenter` catch-all (pc 11).
/// 3. `ideal` escapes it TOO, and this is the finding: with the monitor
///    opcodes modelled perfectly, the walk reaches pc 19 (`goto`) holding
///    `f` in locals 1 and 2, and the control-transfer barrier
///    (`0x99..=0xa9`) escapes every object held in a local. Fixing the
///    monitor arms alone therefore buys nothing for javac's output.
///
/// Read, not executed — this lane could not build.
#[test]
fn an_exact_non_escaping_monitor_arm_would_not_rescue_the_javac_synchronized_shape() {
    // `Foo f = new Foo(); f.x = 1;` with no `synchronized` at all.
    let plain: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0xb1, // 13: return
    ];
    assert!(
        analyze_escapes(&plain, plain.len(), &trivial_init_at(4)).contains(&0),
        "control: without the synchronized wrapper this allocation IS \
         confirmed non-escaping, so the two assertions below are about the \
         wrapper and nothing else"
    );

    // The full javac shape, including the monitor handler and the `goto`
    // that jumps the normal path over it.
    let javac: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1            -> f in local 1
        0x2b, // 8: aload_1
        0x59, // 9: dup
        0x4d, // 10: astore_2           -> the synthetic monitor temp
        0xc2, // 11: monitorenter
        0x2b, // 12: aload_1
        0x04, // 13: iconst_1
        0xb5, 0x00, 0x03, // 14: putfield #3
        0x2c, // 17: aload_2
        0xc3, // 18: monitorexit
        0xa7, 0x00, 0x08, // 19: goto 27 -- jumps over the handler
        0x4e, // 22: astore_3           -- handler: catch any
        0x2c, // 23: aload_2
        0xc3, // 24: monitorexit
        0x2d, // 25: aload_3
        0xbf, // 26: athrow
        0xb1, // 27: return
    ];
    assert_eq!(javac.len(), 28, "sanity: the goto at 19 must target pc 27");
    assert_eq!(javac[27], 0xb1, "sanity: pc 27 is the return");
    assert!(
        !analyze_escapes(&javac, javac.len(), &trivial_init_at(4)).contains(&0),
        "the receiver is absent from the confirmed non-escaping set. Until 2026-09-22 it was the catch-all at the `monitorenter` (pc 11) that put it there; since the exact monitor arms landed it is the `goto` at pc 19, which THIS entry point still treats as a hard barrier because it was given no handler pcs. The `ideal` run below separates the two causes. Escaped the \
         receiver, so its `new` pc is absent from the confirmed \
         non-escaping set"
    );

    // The same bytecode with both monitor opcodes rewritten to `pop` — an
    // exact stand-in for the precise, non-escaping arms the known-issue
    // page proposes.
    let ideal: Vec<u8> = javac
        .iter()
        .map(|&b| if b == 0xc2 || b == 0xc3 { 0x57 } else { b })
        .collect();
    assert_eq!(
        ideal.len(),
        javac.len(),
        "sanity: monitorenter, monitorexit and pop are all one byte, so the \
         substitution cannot move any pc or retarget the goto"
    );
    assert!(
        !analyze_escapes(&ideal, ideal.len(), &trivial_init_at(4)).contains(&0),
        "THE FINDING, and why the monitor arms had to land WITH EA-2 rather than before it: even with the monitor opcodes modelled as an exact, \
         non-escaping one-slot pop, the receiver still escapes -- the \
         `goto` at pc 19 that javac must emit to skip the monitor handler \
         is a hard control-transfer barrier, and it escapes every object \
         held in a local (here `f`, in locals 1 and 2). Adding \
         monitorenter/monitorexit arms to `analyze_escapes` is therefore \
         necessary but NOT sufficient to reach Phase C. Supply the handler pcs and the answer flips; that is the sibling test."
    );
}

// -----------------------------------------------------------------------
// EA-2 (2026-09-22) — the `goto`-over-handler see-through.
//
// `docs/feature-designs/jit-ir-optimizations.md` §EA-2. The barrier
// these exercise is the one named in this file's own comment history for
// the bc `org.bouncycastle.math.ec` SEGV, so the loosening is pinned by
// FOUR refusals below, one per condition, and the two tests that do admit
// an object carry a control that fails if the admission came from anywhere
// else.
// -----------------------------------------------------------------------

/// The javac `synchronized` shape with the monitor opcodes rewritten to
/// `pop` — the same byte-exact simulation of the ideal monitor arm that
/// `an_exact_non_escaping_monitor_arm_would_not_rescue_the_javac_synchronized_shape`
/// uses. That test's third assertion is the finding EA-2 exists to remove:
/// with perfect monitor arms the receiver STILL escaped, at the `goto` that
/// jumps over the monitor handler.
///
/// With the handler pcs supplied, the `goto` is proven to be a straight-line
/// continuation and the receiver survives.
#[test]
fn the_goto_over_javacs_monitor_handler_is_walked_through_when_the_handlers_are_known() {
    // `void m() { Foo f = new Foo(); synchronized (f) { f.x = 1; } }`, with
    // monitorenter/monitorexit -> pop. Byte-for-byte the `ideal` array of
    // the sibling test.
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1            -> f in local 1
        0x2b, // 8: aload_1
        0x59, // 9: dup
        0x4d, // 10: astore_2           -> the synthetic monitor temp
        0x57, // 11: pop                (monitorenter, modelled exactly)
        0x2b, // 12: aload_1
        0x04, // 13: iconst_1
        0xb5, 0x00, 0x03, // 14: putfield #3
        0x2c, // 17: aload_2
        0x57, // 18: pop                (monitorexit, modelled exactly)
        0xa7, 0x00, 0x08, // 19: goto 27 -- jumps over the handler
        0x4e, // 22: astore_3           -- handler: catch any
        0x2c, // 23: aload_2
        0x57, // 24: pop
        0x2d, // 25: aload_3
        0xbf, // 26: athrow
        0xb1, // 27: return
    ];
    let len = code.len();
    // javac's own handler for this shape: one catch-all at pc 22, named by
    // both the body range and the handler's own range.
    let handlers = [22usize];

    // The control, and the reason the assertion below is about EA-2 rather
    // than about some other property of this byte sequence: told nothing
    // about handlers, the walk keeps the `goto` as a hard barrier and the
    // receiver still escapes — exactly as the sibling test pins.
    assert!(
        !analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), None).contains(&0),
        "control: with no handler pcs the see-through is disabled, which is \
         the pre-EA-2 behaviour -- so the assertion below measures the \
         see-through and not the bytecode"
    );

    assert!(
        analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), Some(&handlers))
            .contains(&0),
        "EA-2: the `goto` at 19 targets 27, whose only ordinary predecessor \
         it is; it skips only handler-only code (22..27); and 27 is not \
         itself a handler entry -- so the walk steps through it and `f` \
         survives in its locals"
    );
}

/// t1 wave 2, lane M: the real javac `synchronized (new Foo())` shape with
/// its handler supplied is confirmed non-escaping (its lock elidable), and
/// `keep_locks` (a monitor observer may exist) escapes it at the
/// `monitorenter`, so the lock stays real. Read, not executed.
#[test]
fn t1m_keep_locks_escapes_the_locked_object() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x59, // 9: dup
        0x4d, // 10: astore_2
        0xc2, // 11: monitorenter
        0x2b, // 12: aload_1
        0x04, // 13: iconst_1
        0xb5, 0x00, 0x03, // 14: putfield #3
        0x2c, // 17: aload_2
        0xc3, // 18: monitorexit
        0xa7, 0x00, 0x08, // 19: goto 27
        0x4e, // 22: astore_3 -- handler: catch any
        0x2c, // 23: aload_2
        0xc3, // 24: monitorexit
        0x2d, // 25: aload_3
        0xbf, // 26: athrow
        0xb1, // 27: return
    ];
    let len = code.len();
    let handlers = [22usize];
    let shapes = trivial_init_at(4);
    assert!(
        analyze_escapes_keeping_locks(&code, len, &shapes, Some(&handlers), false)
            .contains(&0),
        "control: without an observer the locked object is non-escaping"
    );
    assert_eq!(
        analyze_escapes_keeping_locks(&code, len, &shapes, Some(&handlers), false),
        analyze_escapes_with_handlers(&code, len, &shapes, Some(&handlers)),
        "`false` is the old walk"
    );
    assert!(
        !analyze_escapes_keeping_locks(&code, len, &shapes, Some(&handlers), true)
            .contains(&0),
        "under an observer the monitorenter escapes it: no scalar \
         replacement, so no elided lock"
    );
}

/// The barrier EA-2 must NOT have removed: a CONDITIONAL branch.
///
/// This is the shape behind the miscompile this file's comments name. A
/// two-armed `if` is a real merge and the linear state at the join is not
/// the state on every incoming edge, so the receiver must still escape even
/// with the handler pcs supplied.
#[test]
fn a_conditional_branch_is_still_a_hard_barrier_under_ea2() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x1a, // 8: iload_0
        0x99, 0x00, 0x08, // 9: ifeq 17
        0x2b, // 12: aload_1
        0x04, // 13: iconst_1
        0xb5, 0x00, 0x03, // 14: putfield #3
        0xb1, // 17: return
    ];
    let len = code.len();
    assert!(
        !analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), Some(&[])).contains(&0),
        "a conditional branch is a merge; EA-2 admits only an unconditional \
         forward `goto` whose target has exactly one ordinary predecessor"
    );
}

/// Condition 1: a BACK edge is never a straight-line continuation.
#[test]
fn a_backward_goto_is_still_a_hard_barrier_under_ea2() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0xa7, 0xff, 0xfb, // 13: goto -5 -> 8   (a loop over the body)
    ];
    let len = code.len();
    assert!(
        !analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), Some(&[])).contains(&0),
        "a back edge makes its target a loop header, i.e. a real merge"
    );
}

/// Condition 2: the target must have exactly ONE ordinary predecessor. Here
/// a conditional branch lands on the same instruction, so the target is a
/// join and the linear state is not the only state that reaches it.
#[test]
fn a_goto_whose_target_has_a_second_predecessor_is_still_a_barrier() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x1a, // 8: iload_0
        0x99, 0x00, 0x06, // 9: ifeq 15        -> second edge into 15
        0xa7, 0x00, 0x03, // 12: goto 15       -> the candidate
        0xb1, // 15: return
    ];
    let len = code.len();
    assert!(
        !analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), Some(&[])).contains(&0),
        "pc 15 has two ordinary predecessors (the `ifeq` at 9 and the `goto` \
         at 12), so it is a join and the barrier must stand"
    );
}

/// Condition 3: the region the `goto` skips must be reachable ONLY through
/// the exception table. Here the skipped instructions are on an ordinary
/// path, so stepping through the `goto` would jump over code that really
/// runs — including its `astore`, which kills the receiver's local.
#[test]
fn a_goto_over_ordinarily_reachable_code_is_still_a_barrier() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x1a, // 8: iload_0
        0x99, 0x00, 0x05, // 9: ifeq 14        -> into the "skipped" region
        0xa7, 0x00, 0x05, // 12: goto 17
        0x01, // 14: aconst_null   <- reachable from the ifeq
        0x4c, // 15: astore_1      <- and it clobbers the receiver's local
        0x00, // 16: nop
        0xb1, // 17: return
    ];
    let len = code.len();
    assert!(
        !analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), Some(&[])).contains(&0),
        "pcs 14..17 are ordinarily reachable, so this `goto` skips code that \
         runs -- which is exactly how a stale provenance would survive a \
         store that killed it"
    );
}

/// Condition 4: the target must not itself be an exception-handler entry,
/// which is reached on an edge carrying a different state.
///
/// The control is the SAME bytecode with an empty handler set, which does
/// qualify — without it this test would pass for any reason at all.
#[test]
fn a_goto_into_a_handler_entry_is_still_a_barrier() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0x04, // 9: iconst_1
        0xb5, 0x00, 0x03, // 10: putfield #3
        0xa7, 0x00, 0x03, // 13: goto 16
        0xb1, // 16: return
    ];
    let len = code.len();
    assert!(
        analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), Some(&[])).contains(&0),
        "control: with pc 16 an ordinary instruction the `goto` qualifies, \
         so the refusal below is about the handler set and nothing else"
    );
    assert!(
        !analyze_escapes_with_handlers(&code, len, &trivial_init_at(4), Some(&[16]))
            .contains(&0),
        "pc 16 is a handler entry, so control can arrive there on an \
         exception edge carrying a state this walk never modelled"
    );
}

/// Round 10 (`earelock`), Phase C hardening: a scalar object whose monitor
/// is held at a PC where NO local holds it must be demoted, not planned.
///
/// `x64/deopt_stubs.rs` turns each `monitor_at[bci]` entry into
/// `MonitorInfo { object: FrameValue::VirtualObjectRef(new_pc), .. }`, and a
/// `VirtualObjectRef` resolves only against the `VirtualObject(new_pc)` that
/// the frame state's LOCALS lane defines — and that lane is built from
/// `local_prov_at[bci]` alone. `new; dup; <init>()V; dup; monitorenter`
/// locks the receiver straight off the operand stack and never stores it, so
/// before this fix the plan published monitor entries at pcs 9..14 while
/// `local_prov_at` stayed empty: a monitor naming a shell nothing defines.
///
/// # What this asserts and why it follows
///
/// `plan_scalar_replacement` returns a plan whose `objects`, `field_ops`,
/// `init_skips`, `monitor_at` and `monitor_scalar_ops` have ALL been
/// filtered through `final_objects`, which excludes every `poisoned` object.
/// The fix poisons this receiver at the first monitor snapshot that finds no
/// local home (pc 9), so `final_objects` is empty and every one of those
/// five maps is empty with it. That is the demotion: the `new` is allocated
/// for real, `<init>` is really called, and the lock is really taken — which
/// is always correct, and is the same all-or-nothing coupling
/// `final_monitor_scalar_ops` already enforces from the other side.
///
/// Read, not executed.
#[test]
fn a_monitored_scalar_with_no_local_home_is_demoted_instead_of_dangling() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1          -> [f]
        0x59, // 3: dup                         -> [f, f]
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V -> [f]
        0x59, // 7: dup                         -> [f, f]
        0xc2, // 8: monitorenter                -> locks f, [f]
        0x59, // 9: dup                         -> [f, f]
        0x04, // 10: iconst_1                   -> [f, f, 1]
        0xb5, 0x00, 0x03, // 11: putfield #3    -> [f]
        0xc3, // 14: monitorexit                -> unlocks f, []
        0xb1, // 15: return
    ];
    let len = code.len();
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init_void())];
    // Handed the receiver as non-escaping on purpose: this test is about
    // what the PLAN does with such a candidate, not about whether
    // `analyze_escapes` can currently produce one (it cannot — see the two
    // sibling tests above).
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(
        plan.objects.is_empty(),
        "the receiver is locked off the operand stack and never stored, so \
         no deopt snapshot could define its VirtualObject shell -- it must \
         be demoted to a real allocation rather than scalar-replaced"
    );
    assert!(
        plan.monitor_at.is_empty() && plan.monitor_scalar_ops.is_empty(),
        "both Phase C maps are filtered through `final_objects`, which the \
         demotion just emptied, so neither may name a lock to elide"
    );
    assert!(
        plan.field_ops.is_empty() && plan.init_skips.is_empty(),
        "the same filter governs the field ops and the init skip: a \
         demoted object keeps its real putfield and its real <init> call"
    );
    assert_eq!(plan.total_slots, 0, "and reserves no frame slots");
}

/// The control for the test above: the SAME Phase C machinery, on the shape
/// where the receiver does have a local home for the whole locked region.
/// Without this, the demotion fix could be "poison every monitored object"
/// and still pass.
///
/// This is the bytecode `x64/tests.rs::s32_phase_c_plan_tracks_scalar_monitors`
/// uses, fed the same hand-built non-escaping set for the same reason (see
/// `monitorenter_on_an_otherwise_local_receiver_still_escapes_it_today` for
/// why the real `analyze_escapes` cannot supply it).
///
/// The last assertion is the invariant the demotion fix establishes, stated
/// directly: every `(new_pc, depth)` a plan publishes in `monitor_at[bci]`
/// has a `(local, new_pc)` pair in `local_prov_at[bci]` to define its
/// `VirtualObject` shell. Read, not executed.
#[test]
fn a_monitored_scalar_kept_in_a_local_still_plans_its_relock() {
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial #2 <init>()V
        0x4c, // 7: astore_1            -> f in local 1
        0x2b, // 8: aload_1
        0x59, // 9: dup
        0x4d, // 10: astore_2           -> local 2 = f (the monitor temp)
        0xc2, // 11: monitorenter
        0x2b, // 12: aload_1
        0x04, // 13: iconst_1
        0xb5, 0x00, 0x03, // 14: putfield #3
        0x2c, // 17: aload_2
        0xc3, // 18: monitorexit
        0xb1, // 19: return
    ];
    let len = code.len();
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(4usize, init_void())];
    let plan =
        plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
    assert!(
        plan.objects.contains_key(&0),
        "locals 1 and 2 hold the receiver across the whole locked region, \
         so the demotion must NOT fire here"
    );
    assert!(
        plan.monitor_scalar_ops.contains(&11) && plan.monitor_scalar_ops.contains(&18),
        "both monitor ops name a surviving scalar object, so both are \
         elidable at `op_object.rs`'s 0xC2/0xC3 arm"
    );
    assert!(
        !plan.monitor_at.is_empty(),
        "the lock is held across pcs 12..18, so a deopt there has to record it"
    );
    for (bci, held) in &plan.monitor_at {
        for &(new_pc, depth) in held {
            assert!(
                depth >= 1,
                "a depth-0 entry is a lock nobody holds \
                 (deopt::DeoptMetadataError::UnbalancedMonitor)"
            );
            let defines_shell = plan
                .local_prov_at
                .get(bci)
                .is_some_and(|live| live.iter().any(|&(_, np)| np == new_pc));
            assert!(
                defines_shell,
                "monitor_at[{bci}] names VirtualObjectRef({new_pc}) but no \
                 local at that bci defines its VirtualObject shell"
            );
        }
    }
}
