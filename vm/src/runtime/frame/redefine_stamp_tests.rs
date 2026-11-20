use super::*;

/// A moved frame keeps its pc, locals and names, runs the new bytes,
/// hands back the old ones, drops its cached method and slot, and
/// carries the stamp and the obsolete mark -- through a continuation too.
#[test]
fn adopting_a_redefined_body_keeps_the_frame_state_and_marks_it() {
    let mut frame = Frame::new(
        ClassId::new(3),
        "T".to_string(),
        "m".to_string(),
        "(I)V".to_string(),
        None,
        vec![0x12, 4, 0x57, 0xb1], // ldc #4; pop; return
        vec![],
        4,
        2,
        &[Value::Int(41)],
    );
    frame.pc = 2;
    frame.set_method_index(Some(1));
    let built = frame.redefine_stamp();
    assert!(!frame.runs_obsolete_method());

    let old = frame.adopt_redefined_body(
        padded_bytecode(&[0x12, 9, 0x57, 0xb1]),
        Arc::from(Vec::<ExceptionTableEntry>::new()),
        built + 5,
        true,
        None,
    );
    assert_eq!(&old[..4], &[0x12, 4, 0x57, 0xb1]);
    assert_eq!(frame.code[1], 9);
    assert_eq!(frame.pc, 2);
    assert_eq!(frame.get_local(0), Value::Int(41));
    assert_eq!(frame.method_name(), "m");
    assert_eq!(frame.method_descriptor(), "(I)V");
    assert!(frame.cached_method().is_none());
    assert_eq!(frame.method_index(), None);
    assert_eq!(frame.redefine_stamp(), built + 5);
    assert!(frame.runs_obsolete_method());

    let thawed = Frame::from_frozen_frame(frame.to_frozen_frame());
    assert_eq!(thawed.redefine_stamp(), built + 5);
    assert!(
        thawed.runs_obsolete_method(),
        "the mark survives a continuation"
    );

    let mut emcp = Frame::new(
        ClassId::new(3),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        0,
        &[],
    );
    let _ = emcp.adopt_redefined_body(
        padded_bytecode(&[0xb1]),
        Arc::from(Vec::<ExceptionTableEntry>::new()),
        built + 5,
        false,
        None,
    );
    assert!(!emcp.runs_obsolete_method());
    assert_eq!(emcp.redefine_stamp(), built + 5);
    assert!(emcp.replaced_body().is_none());

    // Wave 39, lane L3: the "predates its class's redefinition" mark is
    // outside the stamp, survives later moves and a continuation, and is
    // never set by a move itself.
    assert!(!emcp.predates_its_class_redefinition());
    emcp.mark_predates_its_class_redefinition();
    assert!(emcp.predates_its_class_redefinition());
    assert!(!emcp.runs_obsolete_method());
    assert_eq!(emcp.redefine_stamp(), built + 5);
    emcp.restamp_redefined_body(built + 7, true, None);
    assert!(emcp.predates_its_class_redefinition());
    assert!(emcp.runs_obsolete_method());
    assert_eq!(emcp.redefine_stamp(), built + 7);
    let _ = emcp.adopt_redefined_body(
        padded_bytecode(&[0xb1]),
        Arc::from(Vec::<ExceptionTableEntry>::new()),
        built + 9,
        false,
        None,
    );
    assert!(emcp.predates_its_class_redefinition());
    assert!(!emcp.runs_obsolete_method());
    assert_eq!(emcp.redefine_stamp(), built + 9);
    let thawed = Frame::from_frozen_frame(emcp.to_frozen_frame());
    assert!(thawed.predates_its_class_redefinition());
    assert_eq!(thawed.redefine_stamp(), built + 9);
    assert!(!frame.predates_its_class_redefinition());
}

/// Interpreter round i1 wave 21, lane L4: a moved frame carries its
/// body's own line table and widened `ldc` stream, reports its lines
/// from them, and keeps both through a continuation; an empty record is
/// not kept at all.
#[test]
fn a_replaced_body_travels_with_the_frame() {
    use cratonvm_reader::attribute::LineNumberEntry;
    let new_frame = || {
        Frame::new(
            ClassId::new(3),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0x12, 4, 0x57, 0xb1], // ldc #4; pop; return
            vec![],
            1,
            0,
            &[],
        )
    };
    let code = padded_bytecode(&[0x12, 4, 0x57, 0xb1]);
    let stream = cratonvm_reader::QuickenedCode::with_widened_ldc(&code, &[(0, 300)]);
    assert!(stream.is_some(), "a whole body with an ldc at pc 0");
    let replaced = Arc::new(ReplacedBody {
        line_numbers: Some(Arc::from(vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            },
            LineNumberEntry {
                start_pc: 2,
                line_number: 41,
            },
        ])),
        widened_ldc: vec![(0, 300)].into_boxed_slice(),
        widened_stream: stream,
        retired_code: Box::new([]),
        code_reached_top: std::sync::atomic::AtomicBool::new(false),
        fresh_code: false,
        decoded: std::sync::OnceLock::new(),
    });
    let mut frame = new_frame();
    assert_eq!(frame.own_line_number(0), None, "not moved");
    let stamp = frame.redefine_stamp() + 1;
    let _ = frame.adopt_redefined_body(
        Arc::clone(&code),
        Arc::from(Vec::<ExceptionTableEntry>::new()),
        stamp,
        true,
        Some(Arc::clone(&replaced)),
    );
    assert_eq!(frame.own_line_number(0), Some(40));
    assert_eq!(frame.own_line_number(3), Some(41));
    assert_eq!(frame.own_line_number(-1), None);
    assert!(frame.widened_stream().is_some());

    let thawed = Frame::from_frozen_frame(frame.to_frozen_frame());
    assert_eq!(thawed.own_line_number(2), Some(41));
    assert!(
        thawed.widened_stream().is_some(),
        "the stream survives a continuation: its operand bytes are stale"
    );

    let mut plain = new_frame();
    let _ = plain.adopt_redefined_body(
        code,
        Arc::from(Vec::<ExceptionTableEntry>::new()),
        stamp,
        true,
        Some(Arc::new(ReplacedBody {
            line_numbers: None,
            widened_ldc: Box::new([]),
            widened_stream: None,
            retired_code: Box::new([]),
            code_reached_top: std::sync::atomic::AtomicBool::new(false),
            fresh_code: false,
            decoded: std::sync::OnceLock::new(),
        })),
    );
    assert!(plain.replaced_body().is_none(), "nothing to carry");
}
