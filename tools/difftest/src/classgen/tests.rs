use super::*;

fn frame(locals: Vec<VType>, stack: Vec<VType>) -> Frame {
    Frame { locals, stack }
}

#[test]
fn descriptor_parsing() {
    let (p, r) = parse_method_descriptor("(IJFD[I[[Ljava/lang/String;)J").unwrap();
    assert_eq!(
        p,
        vec![
            VType::Int,
            VType::Long,
            VType::Float,
            VType::Double,
            VType::obj("[I"),
            VType::obj("[[Ljava/lang/String;")
        ]
    );
    assert_eq!(r, Some(VType::Long));
    assert_eq!(
        parse_method_descriptor("([Ljava/lang/String;)V").unwrap().1,
        None
    );
    assert!(parse_method_descriptor("I)V").is_none());
}

#[test]
fn constant_pool_dedupes_and_reserves_two_slots_for_wide_constants() {
    let mut cp = ConstantPool::new();
    let a = cp.long(5);
    let b = cp.integer(7);
    assert_eq!(b, a + 2, "a long constant consumes two indices");
    assert_eq!(cp.long(5), a);
    // -0.0 and 0.0 are distinct constants.
    assert_ne!(cp.float(0.0), cp.float(-0.0));
}

#[test]
fn a_counted_loop_assembles_and_passes_the_shape_check() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "(I)I", 2);
    a.iconst(0);
    a.store(VType::Int, 1, false);
    let head = a.new_label();
    let exit = a.new_label();
    let f = frame(vec![VType::Int, VType::Int], vec![]);
    a.bind(head, f.clone());
    a.load(VType::Int, 1, false);
    a.load(VType::Int, 0, false);
    a.if_icmp(op::IF_ICMPGE, exit);
    a.iinc(1, 1, true);
    a.goto(head);
    a.bind(exit, f);
    a.load(VType::Int, 1, true);
    a.ret();
    let body = a.finish().expect("assembles");
    assert_eq!(body.frames.len(), 2);
    assert_eq!(body.max_stack, 2);
    // `wide iinc 1 1` is c4 84 0001 0001.
    assert!(body.code.windows(6).any(|w| w == [0xc4, 0x84, 0, 1, 0, 1]));
    let class = write_class(
        cp,
        "L",
        vec![MethodDef {
            name: "t".into(),
            descriptor: "(I)I".into(),
            body,
        }],
        ClassVersion::Java8,
    );
    check_class_shape(&class).expect("shape");
    assert!(crate::matrix::scan_class_bytes("L", &class).is_some());
}

#[test]
fn type_errors_are_latched() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "(J)J", 2);
    a.load(VType::Long, 0, false);
    a.simple(op::IADD, &[VType::Int, VType::Int], &[VType::Int]);
    a.ret();
    let err = a.finish().unwrap_err();
    assert!(err.contains("expected Int"), "{err}");
}

#[test]
fn code_after_goto_needs_a_label() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "()V", 0);
    let l = a.new_label();
    a.goto(l);
    a.aconst_null();
    a.pop1();
    a.bind(l, frame(vec![], vec![]));
    a.ret();
    assert!(a.finish().unwrap_err().contains("unconditional transfer"));
}

#[test]
fn branch_state_must_match_the_target_frame() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "(I)I", 1);
    let l = a.new_label();
    a.iconst(1);
    a.load(VType::Int, 0, false);
    a.if_int(op::IFEQ, l); // leaves an int on the stack
    a.iconst(2);
    a.simple(op::IADD, &[VType::Int, VType::Int], &[VType::Int]);
    a.bind(l, frame(vec![VType::Int], vec![]));
    a.ret();
    let err = a.finish().unwrap_err();
    assert!(err.contains("stack depth"), "{err}");
}

#[test]
fn stack_shuffles_follow_the_jvms_forms() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "(IJ)J", 3);
    // dup_x2 form 2: J I -> I J I
    a.load(VType::Long, 1, false);
    a.load(VType::Int, 0, false);
    a.dup_x2();
    assert_eq!(a.stack(), &[VType::Int, VType::Long, VType::Int]);
    a.simple(op::I2L, &[VType::Int], &[VType::Long]);
    a.simple(op::LSUB, &[VType::Long, VType::Long], &[VType::Long]);
    // dup2_x2 form 3: J I I -> I I J I I
    a.load(VType::Int, 0, false);
    a.load(VType::Int, 0, false);
    a.dup2_x2();
    assert_eq!(
        a.stack(),
        &[
            VType::Int,
            VType::Int,
            VType::Int,
            VType::Long,
            VType::Int,
            VType::Int
        ]
    );
    a.pop2();
    a.simple(op::L2I, &[VType::Long], &[VType::Int]);
    a.swap();
    a.simple(op::IXOR, &[VType::Int, VType::Int], &[VType::Int]);
    a.simple(op::IXOR, &[VType::Int, VType::Int], &[VType::Int]);
    a.simple(op::IADD, &[VType::Int, VType::Int], &[VType::Int]);
    a.simple(op::I2L, &[VType::Int], &[VType::Long]);
    a.ret();
    a.finish().expect("assembles");
}

#[test]
fn switches_pad_to_four_and_resolve_32_bit_offsets() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "(I)I", 1);
    a.load(VType::Int, 0, false); // pc 0 (iload_0), switch at pc 1
    let d = a.new_label();
    let c0 = a.new_label();
    a.tableswitch(i32::MAX - 1, d, &[c0, c0]);
    let f = frame(vec![VType::Int], vec![]);
    a.bind(c0, f.clone());
    a.iconst(1);
    a.ret();
    a.bind(d, f);
    a.lookupswitch_default_only_helper();
    let body = a.finish().expect("assembles");
    // opcode at 1, padding to 4 ⇒ default offset field at 4.
    assert_eq!(body.code[1], op::TABLESWITCH);
    assert_eq!(&body.code[2..4], &[0, 0]);
}

impl Asm<'_> {
    fn lookupswitch_default_only_helper(&mut self) {
        let out = self.new_label();
        self.load(VType::Int, 0, false);
        self.lookupswitch(out, &[(i32::MIN, out), (i32::MAX, out)]);
        self.bind(out, frame(vec![VType::Int], vec![]));
        self.iconst(0);
        self.ret();
    }
}

#[test]
fn handler_frames_are_checked_against_every_state_in_range() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "(I)I", 2);
    a.iconst(0);
    a.store(VType::Int, 1, false);
    let h = a.new_label();
    let join = a.new_label();
    let t = a.try_begin(h, "java/lang/ArithmeticException");
    a.load(VType::Int, 0, false);
    a.load(VType::Int, 0, false);
    a.simple(op::IDIV, &[VType::Int, VType::Int], &[VType::Int]);
    a.store(VType::Int, 1, false);
    a.try_end(t);
    a.goto(join);
    a.bind(
        h,
        frame(
            vec![VType::Int, VType::Int],
            vec![VType::obj("java/lang/ArithmeticException")],
        ),
    );
    a.pop1();
    a.bind(join, frame(vec![VType::Int, VType::Int], vec![]));
    a.load(VType::Int, 1, false);
    a.ret();
    let body = a.finish().expect("assembles");
    assert_eq!(body.handlers.len(), 1);
}

#[test]
fn java5_classes_carry_no_stack_map_table() {
    let mut cp = ConstantPool::new();
    let mut a = Asm::new_static(&mut cp, "(I)I", 1);
    let l = a.new_label();
    a.load(VType::Int, 0, false);
    a.if_int(op::IFEQ, l);
    a.iconst(1);
    a.ret();
    a.bind(l, frame(vec![VType::Int], vec![]));
    a.iconst(2);
    a.ret();
    let body = a.finish().unwrap();
    let class = write_class(
        cp,
        "V5",
        vec![MethodDef {
            name: "t".into(),
            descriptor: "(I)I".into(),
            body,
        }],
        ClassVersion::Java5,
    );
    check_class_shape(&class).expect("shape");
    assert!(!class.windows(13).any(|w| w == b"StackMapTable"));
}
