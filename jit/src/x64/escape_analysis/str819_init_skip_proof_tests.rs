//! STR819-2 (round 19 wave 9, lane str9): the single-pass plan records an
//! init skip only for a `<init>()V` the resolver proved empty -- the same
//! proof `analyze_escapes` consumes -- never from the descriptor alone.

use super::{analyze_escapes, plan_scalar_replacement_with_handlers, InvokeSpecialShape};
use rustc_hash::FxHashMap;
use std::collections::HashSet;

/// `Foo f = new Foo(); f.x = 1; return f.x;`
const CODE: [u8; 18] = [
    0xbb, 0x00, 0x01, // 0: new #1
    0x59, // 3: dup
    0xb7, 0x00, 0x02, // 4: invokespecial Foo.<init>()V
    0x4c, // 7: astore_1
    0x2b, // 8: aload_1
    0x04, // 9: iconst_1
    0xb5, 0x00, 0x03, // 10: putfield #3
    0x2b, // 13: aload_1
    0xb4, 0x00, 0x03, // 14: getfield #3
    0xac, // 17: ireturn
];
const INIT_PC: usize = 4;

fn init_void() -> *const crate::JitInvokeInfo {
    // LEAK(intentional): test-only; the plan reads it through a raw pointer.
    Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "Foo",
        method_name: "<init>",
        descriptor: "()V",
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 1,
        declaring_class_id: 0,
        owner_class_id: 0,
    }))
}

fn plan(
    non_escaping: &HashSet<usize>,
    proven: Option<&HashSet<usize>>,
) -> super::ScalarReplacementPlan {
    let new_info = vec![(0usize, 1u32, 1usize, true, true)];
    let invoke_info = vec![(INIT_PC, init_void())];
    plan_scalar_replacement_with_handlers(
        &CODE,
        CODE.len(),
        non_escaping,
        &new_info,
        &invoke_info,
        0,
        Some(&[]),
        proven,
    )
}

fn shapes(trivial: bool) -> FxHashMap<usize, InvokeSpecialShape> {
    let mut m = FxHashMap::default();
    m.insert(
        INIT_PC,
        InvokeSpecialShape {
            arg_slots: 1,
            is_trivial_void_init: trivial,
        },
    );
    m
}

/// Even if the object reached the plan as non-escaping (the two walks
/// drifting), a `()V` constructor that was NOT proven empty is never
/// skipped and its receiver stays a real allocation.
#[test]
fn a_non_empty_void_constructor_is_never_skipped() {
    let admitted: HashSet<usize> = [0usize].into_iter().collect();
    for proven in [
        None,
        Some(HashSet::new()),
        Some([99usize].into_iter().collect()),
    ] {
        let p = plan(&admitted, proven.as_ref());
        assert!(
            !p.init_skips.contains(&INIT_PC),
            "unproven ()V skipped with proof {proven:?}"
        );
        assert!(
            p.objects.is_empty() && p.field_ops.is_empty(),
            "receiver of an unproven ()V must stay allocated (proof {proven:?})"
        );
    }
}

/// Positive control: the proven-empty constructor is still skipped and
/// its receiver scalar-replaced, exactly as before.
#[test]
fn a_proven_empty_void_constructor_is_still_skipped() {
    let admitted: HashSet<usize> = [0usize].into_iter().collect();
    let proven: HashSet<usize> = [INIT_PC].into_iter().collect();
    let p = plan(&admitted, Some(&proven));
    assert!(p.init_skips.contains(&INIT_PC));
    assert!(p.objects.contains_key(&0));
    assert!(p.field_ops.contains_key(&10) && p.field_ops.contains_key(&14));
}

/// The pair end to end, as `x64/driver.rs` wires it: the same proof feeds
/// `analyze_escapes` and the plan, and the plan agrees in both directions.
#[test]
fn the_pair_agrees_on_the_same_proof() {
    let proven: HashSet<usize> = [INIT_PC].into_iter().collect();
    let ne = analyze_escapes(&CODE, CODE.len(), &shapes(true));
    assert!(ne.contains(&0), "proven-empty ctor: Foo does not escape");
    let p = plan(&ne, Some(&proven));
    assert!(p.init_skips.contains(&INIT_PC) && p.objects.contains_key(&0));

    let ne = analyze_escapes(&CODE, CODE.len(), &shapes(false));
    assert!(!ne.contains(&0), "unproven ctor: the receiver escapes");
    let p = plan(&ne, None);
    assert!(p.init_skips.is_empty() && p.objects.is_empty());    }