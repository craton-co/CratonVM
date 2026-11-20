use super::*;

fn info(class_name: &'static str, method_name: &'static str, kind: u8) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name,
        method_name,
        descriptor: "()V",
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}

#[test]
fn only_the_three_signature_polymorphic_shapes_are_linked() {
    let mh = "java/lang/invoke/MethodHandle";
    assert_eq!(
        mh_link_shape(&info(mh, "invokeExact", 0)),
        Some(MhLinkShape::Linker)
    );
    assert_eq!(
        mh_link_shape(&info(mh, "invokeBasic", 0)),
        Some(MhLinkShape::Basic)
    );
    // Interpreter round i5 wave 109, lane L2: `linkToSpecial` has the
    // shape too; the redirect links it only under
    // `CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL`.
    for name in [
        "linkToStatic",
        "linkToVirtual",
        "linkToInterface",
        "linkToSpecial",
    ] {
        assert_eq!(mh_link_shape(&info(mh, name, 3)), Some(MhLinkShape::LinkTo));
    }
    // `invoke` (asType), `linkToNative`, other kinds and other classes
    // keep their doors.
    assert_eq!(mh_link_shape(&info(mh, "invoke", 0)), None);
    assert_eq!(mh_link_shape(&info(mh, "linkToSpecial", 0)), None);
    assert_eq!(mh_link_shape(&info(mh, "linkToNative", 3)), None);
    assert_eq!(mh_link_shape(&info(mh, "invokeBasic", 3)), None);
    assert_eq!(
        mh_link_shape(&info("java/lang/invoke/VarHandle", "invokeExact", 0)),
        None
    );
}

#[test]
fn basic_signatures_erase_to_the_lambda_form_types() {
    assert_eq!(
        mh_link_basic_sig("(ZBSCIJFD[ILjava/lang/String;[[Ljava/lang/Object;)Z"),
        Some((b"IIIIIJFDLLL".to_vec(), b'I'))
    );
    assert_eq!(mh_link_basic_sig("()V"), Some((Vec::new(), b'V')));
    assert_eq!(mh_link_basic_sig("(V)V"), None);
    assert_eq!(mh_link_basic_sig("(Ljava/lang/Object)V"), None);
    assert_eq!(mh_link_basic_sig("(I)Vx"), None);
}

#[test]
fn the_words_must_line_up_with_the_member_in_basic_types() {
    // invokeExact (Object,Object,Object)Object -> linker (L,L,L,L,L)L.
    let site = "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
    let linker = "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
    assert!(mh_link_basic_agree(site, linker, 1, 0, 0, 1));
    assert!(!mh_link_basic_agree(site, linker, 1, 0, 0, 0));
    // invokeBasic (I)I on an entry (Object,int)int.
    assert!(mh_link_basic_agree(
        "(I)I",
        "(Ljava/lang/Object;I)I",
        1,
        0,
        0,
        0
    ));
    // A boxed word where the entry takes an int is the door's (it unboxes).
    assert!(!mh_link_basic_agree(
        "(Ljava/lang/Object;)I",
        "(Ljava/lang/Object;I)I",
        1,
        0,
        0,
        0
    ));
    // linkToStatic (II MemberName)I -> add(II)I.
    assert!(mh_link_basic_agree(
        "(IILjava/lang/invoke/MemberName;)I",
        "(II)I",
        0,
        0,
        1,
        0
    ));
    // linkToVirtual (receiver, I, MemberName)I -> scale(I)I.
    assert!(mh_link_basic_agree(
        "(Ljava/lang/Object;ILjava/lang/invoke/MemberName;)I",
        "(I)I",
        0,
        1,
        1,
        0
    ));
    // Interpreter round i5 wave 109, lane L2: linkToSpecial (receiver,
    // MemberName)I -> String.length()I, and a constructor form's
    // (receiver, I, MemberName)V -> <init>(I)V, lay out as linkToVirtual.
    assert!(mh_link_basic_agree(
        "(Ljava/lang/Object;Ljava/lang/invoke/MemberName;)I",
        "()I",
        0,
        1,
        1,
        0
    ));
    assert!(mh_link_basic_agree(
        "(Ljava/lang/Object;ILjava/lang/invoke/MemberName;)V",
        "(I)V",
        0,
        1,
        1,
        0
    ));
    // A primitive where the receiver / MemberName word stands is refused.
    assert!(!mh_link_basic_agree("(IIJ)I", "(I)I", 0, 1, 1, 0));
    // Returns must agree.
    assert!(!mh_link_basic_agree(
        "(IILjava/lang/invoke/MemberName;)J",
        "(II)I",
        0,
        0,
        1,
        0
    ));
}
