use super::*;
use cratonvm_reader::constant_pool::ConstantPoolEntry as E;

fn utf8(s: &str) -> ConstantPoolEntry {
    E::Utf8(s.into())
}

/// `#1 "Holder"`, `#2 Class #1`, then `#3` / `#4` / `#5` as given.
fn pool(tail: Vec<ConstantPoolEntry>) -> ConstantPool {
    let mut entries = vec![
        E::Tombstone,
        utf8("Holder"),
        E::ClassReference { name_index: 1 },
    ];
    entries.extend(tail);
    ConstantPool::new(entries)
}

fn string_at(pool: &ConstantPool, index: u16) -> Option<&str> {
    match pool.get(index)? {
        E::StringReference { string_index } => pool.get_utf8(*string_index),
        _ => None,
    }
}

/// The case the old code breaks on: the same index names a different
/// string in the new pool. The old string is appended (with its UTF-8)
/// and the translation points the old index at it; the new index keeps
/// its new meaning.
#[test]
fn a_renumbered_string_is_appended_and_translated() {
    let old = pool(vec![utf8("old"), E::StringReference { string_index: 3 }]);
    let new = pool(vec![utf8("new"), E::StringReference { string_index: 3 }]);
    let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[4]).expect("fits");
    assert_eq!(string_at(&merged.constant_pool, 4), Some("new"));
    let moved = merged.translation[4];
    assert!(moved >= 5, "appended after the new pool, got #{moved}");
    assert_eq!(string_at(&merged.constant_pool, moved), Some("old"));
    // Unchanged entries map to themselves.
    assert_eq!(merged.translation[1], 1);
    assert_eq!(merged.translation[2], 2);
}

/// An identical pool (a retransform whose transformer returned the same
/// bytes) appends nothing.
#[test]
fn an_identical_pool_appends_nothing() {
    let old = pool(vec![utf8("same"), E::StringReference { string_index: 3 }]);
    let new = pool(vec![utf8("same"), E::StringReference { string_index: 3 }]);
    let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[]).expect("fits");
    assert_eq!(merged.constant_pool.len(), new.len());
    assert_eq!(merged.translation, vec![0, 1, 2, 3, 4]);
}

/// A constant that only moved is found at its new index, not appended.
#[test]
fn a_moved_constant_is_deduplicated() {
    let old = pool(vec![utf8("x"), E::StringReference { string_index: 3 }]);
    let new = pool(vec![
        utf8("pad"),
        utf8("x"),
        E::StringReference { string_index: 4 },
    ]);
    let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[4]).expect("fits");
    assert_eq!(merged.translation[4], 5);
    assert_eq!(merged.constant_pool.len(), new.len());
}

/// A `long` takes two slots when appended, and a member reference is
/// rebuilt from translated components.
#[test]
fn longs_and_member_references_keep_their_shape() {
    let old = pool(vec![
        E::Long(7.into()),
        E::Tombstone,
        utf8("f"),
        utf8("I"),
        E::NameAndType {
            name_index: 5,
            descriptor_index: 6,
        },
        E::FieldReference {
            class_index: 2,
            name_and_type_index: 7,
        },
    ]);
    let new = pool(vec![E::Long(8.into()), E::Tombstone]);
    let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[]).expect("fits");
    let long_at = merged.translation[3];
    assert!(matches!(
        merged.constant_pool.get(long_at),
        Some(E::Long(v)) if v.get() == 7
    ));
    assert!(matches!(
        merged.constant_pool.get(long_at + 1),
        Some(E::Tombstone)
    ));
    assert_eq!(
        merged.translation[4], 0,
        "a long's second slot names nothing"
    );
    let field_at = merged.translation[8];
    match merged.constant_pool.get(field_at) {
        Some(E::FieldReference {
            class_index,
            name_and_type_index,
        }) => {
            assert_eq!(
                merged.constant_pool.get_class_name(*class_index),
                Some("Holder")
            );
            assert_eq!(
                merged.constant_pool.get_name_and_type(*name_and_type_index),
                Some(("f", "I"))
            );
        }
        other => unreachable!("expected a field reference, got {other:?}"),
    }
}

/// An old `invokedynamic` brings its bootstrap method along, rebuilt
/// from translated arguments, when the new class has no equal one.
#[test]
fn an_old_call_site_brings_its_bootstrap_method() {
    let old = pool(vec![
        utf8("bsm"),
        utf8("()V"),
        E::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        E::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        },
        E::MethodHandle {
            reference_kind: 6,
            reference_index: 6,
        },
        utf8("arg-old"),
        E::StringReference { string_index: 8 },
        E::InvokeDynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 5,
        },
    ]);
    let old_bsms = vec![BootstrapMethod {
        bootstrap_method_ref: 7,
        bootstrap_arguments: vec![9],
    }];
    let new = pool(vec![utf8("unrelated")]);
    let merged = merge_for_obsolete_code(&old, &old_bsms, &new, &[], &[]).expect("fits");
    let site = merged.translation[10];
    let Some(E::InvokeDynamic {
        bootstrap_method_attr_index,
        ..
    }) = merged.constant_pool.get(site)
    else {
        unreachable!("the call site was appended");
    };
    let bsm = &merged.bootstrap_methods[usize::from(*bootstrap_method_attr_index)];
    assert_eq!(bsm.bootstrap_method_ref, merged.translation[7]);
    assert_eq!(bsm.bootstrap_arguments, vec![merged.translation[9]]);
    assert_eq!(
        string_at(&merged.constant_pool, merged.translation[9]),
        Some("arg-old")
    );
}

/// An `ldc` constant is placed before its UTF-8, so it gets the lower
/// index; the rewrite of `ldc` refuses an index above 255.
#[test]
fn ldc_constants_are_placed_first_and_must_fit_a_byte() {
    let old = pool(vec![utf8("gone"), E::StringReference { string_index: 3 }]);
    let new = pool(vec![utf8("fresh")]);
    let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[4]).expect("fits");
    assert_eq!(
        merged.translation[4], 4,
        "the String takes the first free slot"
    );
    assert_eq!(string_at(&merged.constant_pool, 4), Some("gone"));

    let map = |i: u16| Some(i + 1);
    assert_eq!(
        translate_code(&[0x12, 4, 0xb0], &map),
        Some(vec![0x12, 5, 0xb0])
    );
    let high = |_: u16| Some(300u16);
    assert_eq!(translate_code(&[0x12, 4, 0xb0], &high), None);
}

/// Every constant-pool operand is rewritten; nothing else moves,
/// including a `tableswitch` whose padding depends on its pc.
#[test]
fn translate_code_rewrites_operands_in_place() {
    let code = vec![
        0x13, 0x00, 0x10, // ldc_w #16
        0xb2, 0x00, 0x11, // getstatic #17
        0xb9, 0x00, 0x12, 0x02, 0x00, // invokeinterface #18 count 2
        0xba, 0x00, 0x13, 0x00, 0x00, // invokedynamic #19
        0x03, // iconst_0
        0xaa, 0x00, 0x00, // tableswitch, padded to pc 20
        0x00, 0x00, 0x00, 0x10, // default
        0x00, 0x00, 0x00, 0x00, // low
        0x00, 0x00, 0x00, 0x00, // high
        0x00, 0x00, 0x00, 0x10, // offset
        0xc0, 0x00, 0x14, // checkcast #20
        0xb1, // return
    ];
    let map = |i: u16| Some(i + 0x100);
    let out = translate_code(&code, &map).expect("translates");
    assert_eq!(out.len(), code.len());
    assert_eq!(&out[0..3], &[0x13, 0x01, 0x10]);
    assert_eq!(&out[3..6], &[0xb2, 0x01, 0x11]);
    assert_eq!(&out[6..11], &[0xb9, 0x01, 0x12, 0x02, 0x00]);
    assert_eq!(&out[11..16], &[0xba, 0x01, 0x13, 0x00, 0x00]);
    assert_eq!(&out[16..36], &code[16..36], "the switch is untouched");
    assert_eq!(&out[36..39], &[0xc0, 0x01, 0x14]);
    let unmapped = |_: u16| None;
    assert_eq!(translate_code(&code, &unmapped), None);
}

#[test]
fn exception_tables_translate_catch_types_but_not_finally() {
    let table = vec![
        ExceptionTableEntry {
            start_pc: 0,
            end_pc: 4,
            handler_pc: 5,
            catch_type: 7,
        },
        ExceptionTableEntry {
            start_pc: 0,
            end_pc: 4,
            handler_pc: 9,
            catch_type: 0,
        },
    ];
    let map = |i: u16| Some(i * 2);
    let out = translate_exception_table(&table, &map).expect("translates");
    assert_eq!(out[0].catch_type, 14);
    assert_eq!(out[1].catch_type, 0);
    assert_eq!(out[0].handler_pc, 5);
}

/// A frame is translated through every redefinition after its stamp,
/// in order; one older than the kept history, or than a redefinition
/// that recorded no translation, is not.
#[test]
fn the_history_composes_steps_after_the_stamp() {
    let mut history = RedefinitionHistory::default();
    assert!(!history.last_redefinition_moved_constants());
    history.record(5, Some(vec![0, 1, 2, 0]), Vec::new());
    assert!(
        !history.last_redefinition_moved_constants(),
        "every old index kept its constant"
    );
    history.record(10, Some(vec![0, 5, 6]), Vec::new());
    assert!(history.last_redefinition_moved_constants());
    history.record(20, Some(vec![0, 0, 0, 0, 0, 7, 8]), Vec::new());
    assert!(history.is_stale(9));
    assert!(history.is_stale(19));
    assert!(!history.is_stale(20));
    assert_eq!(history.translate(9, 1), Some(7), "both steps");
    assert_eq!(history.translate(15, 5), Some(7), "the second step only");
    assert_eq!(
        history.translate(25, 3),
        Some(3),
        "current code maps to itself"
    );
    assert_eq!(history.translate(9, 0), None);
    assert_eq!(history.latest_redefinition(), 20);

    history.record(30, None, Vec::new());
    assert!(
        !history.last_redefinition_moved_constants(),
        "nothing to repair with"
    );
    assert_eq!(
        history.translate(25, 3),
        None,
        "the pool was replaced without a map"
    );
    assert_eq!(history.translate(30, 3), Some(3));

    // Each step swaps #1 and #2, so an even number of them is the
    // identity. Each is also a different 2100-entry map (one more slot
    // names nothing) that swaps every later pair too, so no two
    // neighbours share a run (wave 24: translations are kept as runs)
    // and eight of them are over `HISTORY_BYTE_BUDGET`: a ninth drops
    // the first.
    let mut capped = RedefinitionHistory::default();
    for i in 0..=MAX_KEPT_REDEFINITIONS as u64 {
        let mut translation: Vec<u16> = (0..2100).collect();
        translation.swap(1, 2);
        for pair in (3..2099).step_by(2) {
            translation.swap(pair, pair + 1);
        }
        translation[10 + i as usize] = 0;
        capped.record(100 + i, Some(translation), Vec::new());
    }
    assert!(capped.distinct_bytes() > HISTORY_BYTE_BUDGET);
    assert_eq!(capped.translate(99, 1), None, "its first step was dropped");
    assert_eq!(capped.translate(100, 1), Some(1));
    assert_eq!(capped.translate(101, 1), Some(2));
}

/// Interpreter round i1 wave 22, lane L3 (i19-L3 page, case 2): steps
/// that repeat a translation share it, so a history of 200 such steps
/// fits the byte budget and a frame older than all of them is still
/// translated; the step cap still bounds the count.
#[test]
fn repeated_translations_are_shared_and_kept() {
    let mut toggled = RedefinitionHistory::default();
    for i in 0..200u64 {
        toggled.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
    }
    assert_eq!(toggled.steps.len(), 200);
    assert!(toggled.steps.iter().all(|step| {
        step.translation
            .as_ref()
            .zip(toggled.steps[0].translation.as_ref())
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
    }));
    // Three runs (#0 names nothing, #1 -> #2, #2 -> #1), kept once.
    assert_eq!(
        toggled.distinct_bytes(),
        3 * std::mem::size_of::<TranslationRun>()
    );
    assert_eq!(toggled.translate(99, 1), Some(1), "200 swaps");
    assert_eq!(toggled.translate(100, 1), Some(2), "199 swaps");

    let mut capped = RedefinitionHistory::default();
    for i in 0..=MAX_HISTORY_STEPS as u64 {
        capped.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
    }
    assert_eq!(capped.steps.len(), MAX_HISTORY_STEPS);
    assert_eq!(capped.translate(99, 1), None, "past the step cap");
    assert_eq!(capped.translate(100, 1), Some(1));
}

/// Interpreter round i1 wave 23, lane L3 (the i22-L3 census proposal):
/// once a stale-frame census has run, steps no frame needs are dropped
/// past the last eight whatever they cost, and steps a stale frame still
/// needs outlive the soft caps, up to the hard ones.
#[test]
fn the_census_drops_unneeded_steps_and_keeps_needed_ones() {
    // Each step swaps #1 and #2 and names nothing at one more slot, a
    // different one than the last four steps' (so none is shared); it
    // swaps every later pair too, so each of its 64 slots is a run of
    // its own (wave 24: translations are kept as runs).
    let distinct = |i: u64| {
        let mut translation: Vec<u16> = (0..64).collect();
        translation.swap(1, 2);
        for pair in (3..63).step_by(2) {
            translation.swap(pair, pair + 1);
        }
        translation[3 + (i % 50) as usize] = 0;
        translation
    };
    let mut kept = RedefinitionHistory::default();
    // A thread parked since count 50 still has its frames there.
    kept.prune(50);
    for i in 0..300u64 {
        kept.record(100 + i, Some(distinct(i)), Vec::new());
    }
    assert!(kept.distinct_bytes() > HISTORY_BYTE_BUDGET);
    assert_eq!(kept.steps.len(), 300, "needed: past both soft caps");
    assert_eq!(kept.translate(99, 1), Some(1), "300 swaps");

    // Every thread has moved its frames past count 350: the steps
    // retired up to it translate for nobody.
    kept.prune(350);
    assert_eq!(kept.steps.len(), 49, "retired 351..=399");
    assert_eq!(kept.translate(99, 1), None);
    assert_eq!(kept.translate(350, 1), Some(2), "49 swaps");

    kept.prune(1_000);
    assert_eq!(
        kept.steps.len(),
        MAX_KEPT_REDEFINITIONS,
        "the last eight stay whatever the census says"
    );

    let mut hard = RedefinitionHistory::default();
    hard.prune(0);
    for i in 0..=MAX_HISTORY_STEPS_HARD as u64 {
        hard.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
    }
    assert_eq!(hard.steps.len(), MAX_HISTORY_STEPS_HARD, "the hard cap");
    assert_eq!(hard.translate(99, 1), None);
}

/// Interpreter round i1 wave 24, lane L3: steps that replaced line tables
/// are what a stored backtrace reads its version's lines from, and a
/// backtrace is not a frame the census sees, so the census drops them
/// only past the soft caps; steps without line tables go at once.
#[test]
fn the_census_keeps_line_tables_within_the_soft_caps() {
    let mut history = RedefinitionHistory::default();
    history.prune(0);
    for i in 0..20u64 {
        let line = u16::try_from(i).unwrap_or(0);
        history.record(
            100 + i,
            Some(vec![0, 2, 1]),
            vec![replaced("run", &[(0, line)])],
        );
    }
    history.prune(1_000);
    assert_eq!(history.steps.len(), 20, "kept for stored backtraces");
    assert_eq!(
        history
            .line_table_at(99, "run", "()V")
            .map(|t| t.map(|t| line_number_at(&t, 0))),
        Some(Some(Some(0))),
        "the first version's lines"
    );

    let mut plain = RedefinitionHistory::default();
    plain.prune(0);
    for i in 0..20u64 {
        plain.record(100 + i, Some(vec![0, 2, 1]), Vec::new());
    }
    plain.prune(1_000);
    assert_eq!(
        plain.steps.len(),
        MAX_KEPT_REDEFINITIONS,
        "no lines: dropped"
    );
}

/// A redefinition records the count its swap began at: a frame stamped
/// in between is stale by its stamp, and `latest_redefinition_began`
/// tells the VM its body may be either one.
#[test]
fn a_redefinition_window_is_recorded() {
    let mut history = RedefinitionHistory::default();
    history.record_window(7, 9, Some(vec![0, 2, 1]), Vec::new(), None);
    assert_eq!(history.latest_redefinition_began(), 7);
    assert_eq!(history.latest_redefinition(), 9);
    assert!(history.is_stale(8));
    assert!(!history.is_stale(9));
    history.record(12, Some(vec![0, 2, 1]), Vec::new());
    assert_eq!(history.latest_redefinition_began(), 12, "no window");
}

/// A redefinition that moved no constant and changed no line table (an
/// identical retransform) records no step: nine of them after a moving
/// one leave that one reachable, where each used to take a slot and the
/// ninth dropped it (i19-L3 page, case 2). A frame built before any of
/// them is still stale, and translated through the moving step alone.
#[test]
fn identical_redefinitions_take_no_history_slot() {
    let mut history = RedefinitionHistory::default();
    history.record(10, Some(vec![0, 3, 2, 1]), Vec::new());
    for i in 0..=MAX_KEPT_REDEFINITIONS as u64 {
        history.record(20 + i, Some(vec![0, 1, 2, 3]), Vec::new());
    }
    assert_eq!(history.latest_redefinition(), 28);
    assert!(!history.last_redefinition_moved_constants());
    assert!(history.is_stale(5));
    assert_eq!(history.translate(5, 1), Some(3), "the moving step");
    assert_eq!(history.translate(15, 1), Some(1), "identities only");
    assert_eq!(history.translate(5, 0), None, "slot 0 names nothing");
}

fn lines(pairs: &[(u16, u16)]) -> Vec<LineNumberEntry> {
    pairs
        .iter()
        .map(|&(start_pc, line_number)| LineNumberEntry {
            start_pc,
            line_number,
        })
        .collect()
}

fn replaced(name: &str, pairs: &[(u16, u16)]) -> ReplacedLineTable {
    ReplacedLineTable {
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        lines: Arc::from(lines(pairs)),
    }
}

/// A frame's line table is the one the first step after its stamp that
/// changed the method replaced; with none, the current method's; older
/// than the kept history, unknown. A step that only changed lines is
/// kept even though its pool translation is the identity.
#[test]
fn line_tables_are_found_for_the_body_a_stamp_names() {
    let mut history = RedefinitionHistory::default();
    history.record(10, Some(vec![0, 1]), vec![replaced("run", &[(0, 5)])]);
    history.record(20, Some(vec![0, 1]), Vec::new());
    history.record(30, Some(vec![0, 1]), vec![replaced("run", &[(0, 7)])]);
    assert!(!history.last_redefinition_moved_constants());
    let first_line = |stamp: u64| {
        history
            .line_table_at(stamp, "run", "()V")
            .map(|t| t.map(|t| line_number_at(&t, 0)))
    };
    assert_eq!(first_line(5), Some(Some(Some(5))), "the original body");
    assert_eq!(first_line(15), Some(Some(Some(7))), "the second body");
    assert_eq!(first_line(25), Some(Some(Some(7))), "unchanged at 20");
    assert_eq!(first_line(35), Some(None), "the current body");
    assert_eq!(
        history
            .line_table_at(5, "other", "()V")
            .map(|t| t.is_some()),
        Some(false),
        "a method no step changed"
    );
    history.record(40, None, Vec::new());
    assert!(history.line_table_at(35, "run", "()V").is_none());
}

/// HotSpot's `Method::line_number_from_bci`: the first entry AT the bci
/// wins; else the largest start below it, the last of equals; a bci
/// before the first entry has no line. (j27 wave 5, lane STACKHUNT: bci 4
/// used to answer 12, the last of the two rows at 4.)
#[test]
fn line_number_at_follows_the_line_number_table_rule() {
    let table = lines(&[(0, 10), (4, 11), (4, 12), (9, 13)]);
    assert_eq!(line_number_at(&table, 0), Some(10));
    assert_eq!(line_number_at(&table, 3), Some(10));
    assert_eq!(line_number_at(&table, 4), Some(11));
    assert_eq!(line_number_at(&table, 5), Some(12));
    assert_eq!(line_number_at(&table, 8), Some(12));
    assert_eq!(line_number_at(&table, 100), Some(13));
    assert_eq!(line_number_at(&lines(&[(2, 1)]), 1), None);
    assert_eq!(line_number_at(&[], 0), None);
}

/// Only the methods whose table differs are kept, as they were.
#[test]
fn replaced_line_tables_keep_only_changed_methods() {
    use cratonvm_reader::attribute::{CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let method = |name: &str, pairs: &[(u16, u16)]| ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 0,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
                exception_table: Vec::new(),
                attributes: vec![Attribute::LineNumberTable(lines(pairs))],
            },
        )))],
    };
    let old = vec![method("same", &[(0, 3)]), method("moved", &[(0, 8)])];
    let new = vec![method("same", &[(0, 3)]), method("moved", &[(0, 9)])];
    let kept = replaced_line_tables(&old, &new);
    assert_eq!(kept.len(), 1);
    assert_eq!(&*kept[0].name, "moved");
    assert_eq!(line_number_at(&kept[0].lines, 0), Some(8));
}

/// An `ldc` whose constant maps above 255 keeps its byte and is
/// reported; a second translation reads a reported site's index, not
/// its stale byte, and writes the byte back once the index fits.
#[test]
fn translate_code_widening_reports_ldc_sites_above_a_byte() {
    let code = [0x12, 4, 0x12, 5, 0xb1];
    let map = |i: u16| Some(if i == 5 { 300 } else { i + 1 });
    let (out, widened) = translate_code_widening(&code, &map, &[]).expect("maps");
    assert_eq!(out, vec![0x12, 5, 0x12, 5, 0xb1]);
    assert_eq!(widened, vec![(2, 300)]);
    assert_eq!(translate_code(&code, &map), None);
    // The next redefinition moves #300 down to #7.
    let down = |i: u16| Some(if i == 300 { 7 } else { i });
    let (again, still) = translate_code_widening(&out, &down, &widened).expect("maps");
    assert_eq!(again, vec![0x12, 5, 0x12, 7, 0xb1]);
    assert!(still.is_empty());
}

/// A version-52 class `obsolete/Probe` whose one method `static value()I`
/// is `ldc #8; ireturn`, `#8` being `CONSTANT_Integer constant`.
fn ldc_class(constant: i32) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, "obsolete/Probe"); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&constant.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    let code = [0x12, 8, 0xac]; // ldc #8; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

/// `ClassManager::redefine_class` installs the merged pool and records
/// the step: the new code's `#8` keeps the new constant, and the old
/// body's `ldc #8` rewrites to the index of the old one.
#[test]
fn a_redefinition_keeps_the_old_constant_reachable_through_the_history() {
    use crate::{ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions};
    let mut cm = ClassManager::new(&[], &[], &[]);
    let Ok(cid) = cm.define_class_with_options(
        "obsolete/Probe",
        &ldc_class(70_000),
        ClassLoaderId::Application,
        DefineClassOptions::default(),
    ) else {
        // A manager that cannot define the probe class has nothing to
        // redefine; the pure merge tests above still cover the pool.
        return;
    };
    let stamp = crate::class_redefinition_count();
    assert!(cm.redefinition_history(cid).is_none(), "never redefined");
    cm.redefine_class(cid, ldc_class(80_000), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    let history = cm.redefinition_history(cid).expect("the step is recorded");
    assert!(history.last_redefinition_moved_constants());
    assert!(history.is_stale(stamp));
    assert!(!history.is_stale(crate::class_redefinition_count()));
    let moved = history
        .translate(stamp, 8)
        .expect("the old ldc operand maps");
    let cls = cm.class_store.get(cid).expect("still loaded");
    assert!(matches!(cls.constant_pool.get(8), Some(E::Integer(80_000))));
    assert!(matches!(
        cls.constant_pool.get(moved),
        Some(E::Integer(70_000))
    ));
    let map = |i: u16| history.translate(stamp, i);
    assert_eq!(
        translate_code(&[0x12, 8, 0xac], &map),
        Some(vec![0x12, u8::try_from(moved).unwrap_or(0), 0xac])
    );
}

/// Interpreter round i1 wave 24, lane L3: a translation is kept as runs
/// and answers every index as the dense map it was built from did.
#[test]
fn translations_are_kept_as_runs() {
    // #0 and #4 name nothing; #1..#3 keep their slots; the tail #5..#9
    // moved up by two as a block; #10 moved down.
    let dense: Vec<u16> = vec![0, 1, 2, 3, 0, 7, 8, 9, 10, 11, 4];
    let translation = Translation::from_dense(&dense);
    assert_eq!(translation.runs.len(), 5, "{translation:?}");
    for (index, &merged) in dense.iter().enumerate() {
        let index = u16::try_from(index).unwrap_or(u16::MAX);
        assert_eq!(translation.get(index), Some(merged), "#{index}");
    }
    assert_eq!(translation.get(11), None, "past the old pool");
    let mut image = Vec::new();
    translation.mark_image(None, &mut image);
    let named: Vec<usize> = (0..image.len()).filter(|&i| image[i]).collect();
    assert_eq!(named, vec![1, 2, 3, 4, 7, 8, 9, 10, 11]);
    let mut keep = vec![false; dense.len()];
    keep[6] = true;
    let mut image = Vec::new();
    translation.mark_image(Some(&keep), &mut image);
    let named: Vec<usize> = (0..image.len()).filter(|&i| image[i]).collect();
    assert_eq!(named, vec![8]);
}

/// Interpreter round i1 wave 24, lane L3 (the i22-L3 census proposal's
/// stage 3; the i19-L3 untranslatable-frames page, case 2): a class
/// retransformed with a fresh constant each time, with no stale frame
/// holding its history, used to carry every constant any version had
/// into every later merge -- 9 + 1536 slots after 1536 retransforms of
/// this 9-slot class, and a `u2` overflow after ~65,000 -- and its
/// translations grew with it. Now the dead tail is left out once it
/// reaches `COMPACT_TAIL_MIN`, and every frame the kept history can
/// translate still reads its own constant, across each compaction.
#[test]
fn a_merged_pool_drops_constants_no_kept_step_reaches() {
    use crate::{ClassLoaderId, ClassManager, DefineClassOptions, RedefineOptions};
    let mut cm = ClassManager::new(&[], &[], &[]);
    let Ok(cid) = cm.define_class_with_options(
        "obsolete/Probe",
        &ldc_class(70_000),
        ClassLoaderId::Application,
        DefineClassOptions::default(),
    ) else {
        return;
    };
    let rounds = 3 * COMPACT_TAIL_MIN as i32;
    // (stamp, the constant a frame built then loads)
    let mut stamps: Vec<(u64, i32)> = Vec::new();
    let mut longest = 0usize;
    let mut last_len = 0usize;
    let mut compactions = 0;
    for round in 1..=rounds {
        // The census finds no stale frame: only the last eight steps stay.
        let now = crate::class_redefinition_count();
        cm.prune_redefinition_histories(|_| Some(now));
        stamps.push((crate::class_redefinition_count(), 70_000 + round - 1));
        cm.redefine_class(cid, ldc_class(70_000 + round), RedefineOptions::default())
            .expect("a same-shape redefinition succeeds");
        let len = cm
            .class_store
            .get(cid)
            .map_or(0, |cls| cls.constant_pool.len());
        longest = longest.max(len);
        if len >= last_len {
            last_len = len;
            continue;
        }
        last_len = len;
        compactions += 1;
        // Right after a compaction: the frames the last eight steps can
        // translate read their own constants.
        let history = cm.redefinition_history(cid).expect("recorded");
        let cls = cm.class_store.get(cid).expect("loaded");
        for &(stamp, constant) in stamps.iter().rev().take(MAX_KEPT_REDEFINITIONS) {
            let merged = history.translate(stamp, 8).expect("a kept step reaches it");
            assert!(
                matches!(cls.constant_pool.get(merged), Some(E::Integer(c)) if *c == constant),
                "a frame of the version with {constant} reads #{merged}"
            );
        }
    }
    assert!(compactions >= 2, "{compactions} compactions");
    assert!(
        longest <= 9 + COMPACT_TAIL_MIN + MAX_KEPT_REDEFINITIONS,
        "the pool stays bounded: {longest} slots"
    );
}

/// Two old UTF-8 entries that differ only in their lone surrogates are
/// different constants.
#[test]
fn surrogate_bearing_strings_do_not_merge_with_their_lossy_twin() {
    let mut wide = HashMap::new();
    wide.insert(3u16, Arc::from(&[0xd800u16][..]));
    let old = ConstantPool::new_with_wide(
        vec![
            E::Tombstone,
            utf8("Holder"),
            E::ClassReference { name_index: 1 },
            utf8("\u{fffd}"),
            E::StringReference { string_index: 3 },
        ],
        wide,
    );
    let new = pool(vec![
        utf8("\u{fffd}"),
        E::StringReference { string_index: 3 },
    ]);
    let merged = merge_for_obsolete_code(&old, &[], &new, &[], &[]).expect("fits");
    let moved = merged.translation[3];
    assert_ne!(moved, 3);
    assert_eq!(
        merged.constant_pool.get_utf8_wide(moved),
        Some(&[0xd800u16][..])
    );
}
