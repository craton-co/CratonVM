use super::*;
use crate::compile_request::MhConstantFilterRow;

type Answer = Option<(String, String, String, u8, u32)>;

/// `invokevirtual` cp 9 is `MethodHandle.invokeExact(I)I`.
fn names(cp: u16) -> Option<(String, String, String)> {
    (cp == 9).then(|| {
        (
            "java/lang/invoke/MethodHandle".into(),
            "invokeExact".into(),
            "(I)I".into(),
        )
    })
}

fn twice(_invoke_cp: u16, _pc: usize, field_cp: u16) -> Answer {
    (field_cp == 3).then(|| ("p/Host".into(), "twice".into(), "(I)I".into(), 5, 0x7171))
}

fn never(_: usize) -> bool {
    false
}

fn neg_row() -> MhConstantFilterRow {
    MhConstantFilterRow {
        position: Some(0),
        main_kind: 3,
        pre_class: "p/F".into(),
        pre_name: "neg".into(),
        pre_desc: "(I)I".into(),
        pre_kind: 3,
        pre_class_id: 0x7272,
        target_slot: 0,
    }
}

fn row(
    invoke_cp: u16,
    pc: usize,
    field_cp: u16,
    getstatic_pc: usize,
) -> Option<MhConstantFilterRow> {
    (invoke_cp == 9 && pc == 4 && field_cp == 3 && getstatic_pc == 0).then(neg_row)
}

// 0: getstatic #3  3: iload_1  4: invokevirtual #9  7: ireturn
const CODE: [u8; 8] = [0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0xac];
const OPS: [(usize, u16, u8); 1] = [(4, 9, 0xb6)];
const OWN: (&str, &str, &str) = ("p/Caller", "loop", "(I)J");

fn plan(code: &[u8], ops: &[(usize, u16, u8)], gates: (bool, bool)) -> IrMhConstantPlan {
    ir_mh_constant_plan(
        Some(&twice),
        Some(&names),
        code,
        code.len(),
        &[],
        ops,
        gates,
        OWN,
        &never,
    )
}

#[test]
fn jr21_a_filter_species_site_plans_its_pre_call_at_the_getstatic() {
    let planned = plan(&CODE, &OPS, (true, true)).with_filter_rows(
        Some(&row),
        &CODE,
        &never,
        (true, true),
        OWN,
        &all_free,
    );
    assert_eq!(planned.site_count(), 1);
    assert_eq!(
        planned.rewritten_invoke_ops(&OPS),
        vec![(0, MH_FILTER_PRE_KEY, 0xb8), (4, 3, 0xb8)]
    );
    assert_eq!(
        planned.names(MH_FILTER_PRE_KEY),
        Some(("p/F".into(), "neg".into(), "(I)I".into()))
    );
    assert_eq!(planned.class_id(MH_FILTER_PRE_KEY), Some(0x7272));
    assert_eq!(
        planned.names(3),
        Some(("p/Host".into(), "twice".into(), "(I)I".into()))
    );
    assert_eq!(planned.class_context_at(4), Some(0x7171));
    assert_eq!(planned.class_context_at(0), Some(0x7272));
    assert!(planned.is_filter_pc(0) && planned.is_filter_pc(4) && !planned.is_filter_pc(3));
    assert_eq!(
        planned.filter_sites().get(&4),
        Some(&ir::IrMhFilterSite {
            pre_pc: 0,
            position: Some(0),
            main_virtual: false,
            pre_virtual: false,
            target_slot: 0,
        })
    );
    assert_eq!(planned.receiver_pcs().get(&4), Some(&0));
    assert!(planned.bound_slots().is_empty());
}

#[test]
fn jr21_a_filter_site_without_a_fitting_row_is_dropped() {
    let filter =
        |resolver: Option<&dyn Fn(u16, usize, u16, usize) -> Option<MhConstantFilterRow>>,
         protected: &dyn Fn(usize) -> bool,
         gates: (bool, bool)| {
            plan(&CODE, &OPS, gates)
                .with_filter_rows(resolver, &CODE, protected, gates, OWN, &all_free)
        };
    // No resolver, or no row.
    assert!(filter(None, &never, (true, true)).is_empty());
    let no_row = |_: u16, _: usize, _: u16, _: usize| -> Option<MhConstantFilterRow> { None };
    assert!(filter(Some(&no_row), &never, (true, true)).is_empty());
    // A handler over the window.
    assert!(filter(Some(&row), &|bci: usize| bci == 3, (true, true)).is_empty());
    // The static gate off (both calls are static).
    assert!(filter(Some(&row), &never, (false, true)).is_empty());
    // A row whose shape is not the site's: a filter of a long.
    let wide = |_: u16, _: usize, _: u16, _: usize| -> Option<MhConstantFilterRow> {
        Some(MhConstantFilterRow {
            pre_desc: "(J)I".into(),
            ..neg_row()
        })
    };
    assert!(filter(Some(&wide), &never, (true, true)).is_empty());
    // A PRE member of no known class.
    let no_class = |_: u16, _: usize, _: u16, _: usize| -> Option<MhConstantFilterRow> {
        Some(MhConstantFilterRow {
            pre_class_id: 0,
            ..neg_row()
        })
    };
    assert!(filter(Some(&no_class), &never, (true, true)).is_empty());
    // A local written between the getstatic and the invoke:
    // 0: getstatic #3  3: iinc 1 1  6: iload_1  7: invokevirtual #9  10: ireturn
    let code = [0xb2, 0, 3, 0x84, 1, 1, 0x1b, 0xb6, 0, 9, 0xac];
    let ops = [(7usize, 9u16, 0xb6u8)];
    let at7 = |_: u16, pc: usize, _: u16, _: usize| -> Option<MhConstantFilterRow> {
        (pc == 7).then(neg_row)
    };
    let planned = plan(&code, &ops, (true, true));
    assert_eq!(planned.site_count(), 1, "the walk plans it");
    assert!(planned
        .with_filter_rows(Some(&at7), &code, &never, (true, true), OWN, &all_free)
        .is_empty());
}

/// JIT round j27 (lane MHINDY): the resolver is asked for EVERY site of
/// the field with that site's own `(invoke pc, getstatic pc)`, and one
/// refusal (the VM's split-line check) drops the field's every site.
#[test]
fn j27_every_filter_site_is_asked_with_its_own_getstatic_pc() {
    // 0: getstatic #3  3: iload_1  4: invokevirtual #9  7: pop
    // 8: getstatic #3  11: iload_1  12: invokevirtual #9  15: ireturn
    let code = [
        0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0x57, 0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0xac,
    ];
    let ops = [(4usize, 9u16, 0xb6u8), (12, 9, 0xb6)];
    let planned = plan(&code, &ops, (true, true));
    assert_eq!(planned.site_count(), 2, "the walk plans both");
    let asked = std::cell::RefCell::new(Vec::new());
    let both = |_: u16, pc: usize, _: u16, getstatic_pc: usize| {
        asked.borrow_mut().push((pc, getstatic_pc));
        Some(neg_row())
    };
    let kept = plan(&code, &ops, (true, true)).with_filter_rows(
        Some(&both),
        &code,
        &never,
        (true, true),
        OWN,
        &all_free,
    );
    assert_eq!(kept.site_count(), 2);
    assert_eq!(*asked.borrow(), vec![(4, 0), (12, 8)]);
    // The second site's two pcs on different lines: the VM answers None.
    let split = |_: u16, pc: usize, _: u16, _: usize| (pc != 12).then(neg_row);
    assert!(planned
        .with_filter_rows(Some(&split), &code, &never, (true, true), OWN, &all_free)
        .is_empty());
}

fn all_free(_: u16) -> bool {
    true
}

/// Two filter fields in one method, `#3` and `#4`, each its own species row:
///
/// ```text
///  0: getstatic #3  3: iload_1  4: invokevirtual #9  7: pop
///  8: getstatic #4  11: iload_1 12: invokevirtual #9  15: ireturn
/// ```
fn two_fields_code() -> ([u8; 16], [(usize, u16, u8); 2]) {
    (
        [
            0xb2, 0, 3, 0x1b, 0xb6, 0, 9, 0x57, 0xb2, 0, 4, 0x1b, 0xb6, 0, 9, 0xac,
        ],
        [(4, 9, 0xb6), (12, 9, 0xb6)],
    )
}

/// Both fields answer a MAIN `twice`; the second one's filter is `p/F.neg2`.
fn both_fields(_invoke_cp: u16, _pc: usize, field_cp: u16) -> Answer {
    matches!(field_cp, 3 | 4)
        .then(|| ("p/Host".into(), "twice".into(), "(I)I".into(), 5, 0x7171))
}

fn two_row(_: u16, pc: usize, _: u16, _: usize) -> Option<MhConstantFilterRow> {
    Some(if pc == 4 {
        neg_row()
    } else {
        MhConstantFilterRow {
            pre_name: "neg2".into(),
            pre_class_id: 0x7373,
            ..neg_row()
        }
    })
}

fn two_plan(code: &[u8], ops: &[(usize, u16, u8)]) -> IrMhConstantPlan {
    ir_mh_constant_plan(
        Some(&both_fields),
        Some(&names),
        code,
        code.len(),
        &[],
        ops,
        (true, true),
        OWN,
        &never,
    )
}

/// mh21 (MHFILTER21-1): a second filter field of a method is planned under
/// the key below the first one's, with its own names and class id.
#[test]
fn mh21_two_filter_fields_get_their_own_pre_keys() {
    let (code, ops) = two_fields_code();
    let planned = two_plan(&code, &ops);
    assert_eq!(planned.site_count(), 2, "the walk plans both fields");
    let kept =
        planned.with_filter_rows(Some(&two_row), &code, &never, (true, true), OWN, &all_free);
    assert_eq!(kept.site_count(), 2);
    let second_key = MH_FILTER_PRE_KEY - 1;
    assert_eq!(
        kept.rewritten_invoke_ops(&ops),
        vec![
            (0, MH_FILTER_PRE_KEY, 0xb8),
            (4, 3, 0xb8),
            (8, second_key, 0xb8),
            (12, 4, 0xb8)
        ]
    );
    assert_eq!(
        kept.names(MH_FILTER_PRE_KEY),
        Some(("p/F".into(), "neg".into(), "(I)I".into()))
    );
    assert_eq!(
        kept.names(second_key),
        Some(("p/F".into(), "neg2".into(), "(I)I".into()))
    );
    assert_eq!(kept.class_id(MH_FILTER_PRE_KEY), Some(0x7272));
    assert_eq!(kept.class_id(second_key), Some(0x7373));
    // The ordinary keys still answer their MAIN member.
    assert_eq!(
        kept.names(4),
        Some(("p/Host".into(), "twice".into(), "(I)I".into()))
    );
    // Neither key is answered for the other field, nor a third one.
    assert_eq!(kept.names(MH_FILTER_PRE_KEY - 2), None);
    assert_eq!(kept.class_id(MH_FILTER_PRE_KEY - 2), None);
    assert_eq!(kept.class_context_at(0), Some(0x7272));
    assert_eq!(kept.class_context_at(8), Some(0x7373));
    assert!(kept.is_filter_pc(0) && kept.is_filter_pc(12) && !kept.is_filter_pc(7));
    assert_eq!(kept.filter_sites().len(), 2);
}

/// mh21: a key some other row of the method answers is skipped, and a method
/// with no key left keeps the later field's chain.
#[test]
fn mh21_a_taken_pre_key_is_skipped_and_exhaustion_drops_the_field() {
    let (code, ops) = two_fields_code();
    // 0xFFFF is free but 0xFFFE is not: the second field has the key below.
    let not_fffe = |key: u16| key != MH_FILTER_PRE_KEY - 1;
    let kept = two_plan(&code, &ops).with_filter_rows(
        Some(&two_row),
        &code,
        &never,
        (true, true),
        OWN,
        &not_fffe,
    );
    assert_eq!(kept.site_count(), 2);
    assert_eq!(
        kept.names(MH_FILTER_PRE_KEY - 2),
        Some(("p/F".into(), "neg2".into(), "(I)I".into()))
    );
    assert_eq!(kept.names(MH_FILTER_PRE_KEY - 1), None);
    // Every key taken but the first: the second field loses its sites.
    let only_first = |key: u16| key == MH_FILTER_PRE_KEY;
    let kept = two_plan(&code, &ops).with_filter_rows(
        Some(&two_row),
        &code,
        &never,
        (true, true),
        OWN,
        &only_first,
    );
    assert_eq!(kept.site_count(), 1);
    assert_eq!(kept.rewritten_invoke_ops(&ops).len(), 3);
    assert_eq!(
        kept.names(MH_FILTER_PRE_KEY).map(|n| n.1),
        Some("neg".to_string())
    );
    // No key free at all: no field.
    let none_free = |_: u16| false;
    let kept = two_plan(&code, &ops).with_filter_rows(
        Some(&two_row),
        &code,
        &never,
        (true, true),
        OWN,
        &none_free,
    );
    assert!(kept.is_empty());
}

/// mh21: a window nested in another field's window keeps the chain (two PRE
/// calls inside one another's window are unproven), and at most
/// `MH_FILTER_MAX_FIELDS` fields are planned.
#[test]
fn mh21_nested_filter_windows_and_the_field_cap() {
    // 0: getstatic #3  3: getstatic #4  6: iload_1  7: invokevirtual #9
    // 10: invokevirtual #9  13: ireturn
    let nested = [0xb2, 0, 3, 0xb2, 0, 4, 0x1b, 0xb6, 0, 9, 0xb6, 0, 9, 0xac];
    let ops = [(7usize, 9u16, 0xb6u8), (10, 9, 0xb6)];
    let planned = two_plan(&nested, &ops);
    // The walk plans both (the inner invoke's value is the outer's
    // argument); no two kept fields may have overlapping windows.
    let kept = planned.with_filter_rows(
        Some(&two_row),
        &nested,
        &never,
        (true, true),
        OWN,
        &all_free,
    );
    let windows: Vec<(usize, usize)> = kept
        .by_cp
        .values()
        .flat_map(|s| s.pcs.iter().map(|&(at, getstatic_pc)| (getstatic_pc, at)))
        .collect();
    for (i, a) in windows.iter().enumerate() {
        for b in windows.iter().skip(i + 1) {
            assert!(a.1 < b.0 || b.1 < a.0, "overlap {a:?} {b:?}");
        }
    }
    // Field #3 (outer window 0..10) is kept, field #4 (inner window 3..7) is not.
    assert_eq!(kept.site_count(), 1);
    assert!(kept.names(3).is_some() && kept.names(4).is_none());
}
