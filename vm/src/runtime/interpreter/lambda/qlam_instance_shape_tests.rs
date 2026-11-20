use super::{lambda_instance_shape, ClassId};
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

fn site(
    kind: MethodHandleKind,
    sam: &str,
    impl_desc: &str,
    inst: &str,
    captures: &[char],
) -> LambdaCallSite {
    LambdaCallSite {
        functional_interface: "java/util/function/Function".into(),
        functional_interface_id: None,
        sam_method_name: "apply".into(),
        sam_descriptor: sam.into(),
        impl_handle: MethodHandle {
            kind,
            class_name: "test/Box".into(),
            member_name: "m".into(),
            descriptor: impl_desc.into(),
        },
        instantiated_descriptor: inst.into(),
        capture_types: captures.to_vec(),
        proxy_class_id: ClassId::new(0x8000_0D01),
        serializable_flag: false,
    }
}

/// Unbound `Box::name` as a `Function<Box,String>`: the receiver is SAM
/// argument 0, whose bridge cast is replayed; no captures.
#[test]
fn an_unbound_reference_is_receiver_first_with_its_cast() {
    let s = site(
        MethodHandleKind::InvokeVirtual,
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "()Ljava/lang/String;",
        "(Ltest/Box;)Ljava/lang/String;",
        &[],
    );
    let shape = lambda_instance_shape(&s, false).expect("an instance shape");
    assert!(!shape.bound);
    assert_eq!(shape.num_captures, 0);
    assert_eq!(shape.total_args, 1);
    assert!(shape.capture_descs.is_empty());
    assert_eq!(shape.checkcasts.len(), 1);
    assert_eq!(shape.checkcasts[0].0, 0);
    assert_eq!(&*shape.checkcasts[0].1, "Ltest/Box;");
}

/// Bound `box::add` as a `Function<String,String>` through an interface
/// handle: the receiver is capture 0 (a reference load), the SAM argument
/// is the impl's parameter 0.
#[test]
fn a_bound_reference_takes_its_receiver_from_capture_zero() {
    let s = site(
        MethodHandleKind::InvokeInterface,
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/String;)Ljava/lang/String;",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &['L'],
    );
    let shape = lambda_instance_shape(&s, false).expect("an instance shape");
    assert!(shape.bound);
    assert_eq!(shape.capture_descs, vec![b'L']);
    assert_eq!(shape.total_args, 2);
    assert_eq!(
        shape.checkcasts.len(),
        1,
        "the String cast of SAM argument 0"
    );
}

/// Never served here: a boxing return (`String::length` as a `Function`),
/// an unboxing parameter, a `void` SAM over a valued impl, a static or a
/// special handle, an arity the receiver-first layout does not explain.
#[test]
fn non_identity_and_non_virtual_shapes_are_refused() {
    let boxing = site(
        MethodHandleKind::InvokeVirtual,
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "()I",
        "(Ljava/lang/String;)Ljava/lang/Integer;",
        &[],
    );
    assert!(lambda_instance_shape(&boxing, false).is_none());
    // With the boxing arm: `String::length` as a `Function` boxes its int.
    let shape = lambda_instance_shape(&boxing, true).expect("a boxing shape");
    assert_eq!(shape.box_ret, Some('I'));
    assert!(shape.unbox.is_empty());
    let unboxing = site(
        MethodHandleKind::InvokeVirtual,
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "(I)Ljava/lang/String;",
        "(Ltest/Box;Ljava/lang/Integer;)Ljava/lang/String;",
        &[],
    );
    assert!(lambda_instance_shape(&unboxing, false).is_none());
    // With the boxing arm: SAM argument 1 is exactly an `Integer`, unboxed.
    let shape = lambda_instance_shape(&unboxing, true).expect("an unboxing shape");
    assert_eq!(shape.unbox, vec![(1, 'I')]);
    assert_eq!(
        shape.checkcasts.len(),
        1,
        "only the receiver's cast is replayed"
    );
    assert_eq!(shape.checkcasts[0].0, 0);
    let void_over_value = site(
        MethodHandleKind::InvokeVirtual,
        "(Ljava/lang/Object;)V",
        "(Ljava/lang/Object;)Z",
        "(Ljava/lang/Object;)V",
        &['L'],
    );
    for adapt in [false, true] {
        assert!(lambda_instance_shape(&void_over_value, adapt).is_none());
    }
    for kind in [
        MethodHandleKind::InvokeStatic,
        MethodHandleKind::InvokeSpecial,
    ] {
        let s = site(
            kind,
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "()Ljava/lang/String;",
            "(Ltest/Box;)Ljava/lang/String;",
            &[],
        );
        assert!(lambda_instance_shape(&s, false).is_none());
    }
    let arity = site(
        MethodHandleKind::InvokeVirtual,
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/String;)Ljava/lang/String;",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[],
    );
    assert!(lambda_instance_shape(&arity, false).is_none());
}

/// The boxing arm's two token rules: only LambdaMetafactory's EXACT box /
/// unbox, and only 32-bit returns.
#[test]
fn the_box_and_unbox_token_rules_are_exact() {
    use super::{lambda_box_return_prim, lambda_unbox_param_prim};
    assert_eq!(lambda_box_return_prim("Ljava/lang/Object;", "I"), Some('I'));
    assert_eq!(
        lambda_box_return_prim("Ljava/lang/Integer;", "I"),
        Some('I')
    );
    assert_eq!(lambda_box_return_prim("Ljava/lang/Object;", "Z"), Some('Z'));
    assert_eq!(lambda_box_return_prim("Ljava/lang/Object;", "F"), Some('F'));
    assert_eq!(
        lambda_box_return_prim("Ljava/lang/Object;", "J"),
        None,
        "64-bit"
    );
    assert_eq!(
        lambda_box_return_prim("Ljava/lang/Object;", "D"),
        None,
        "64-bit"
    );
    assert_eq!(lambda_box_return_prim("Ljava/lang/Number;", "I"), None);
    assert_eq!(lambda_box_return_prim("Ljava/lang/Long;", "I"), None);
    assert_eq!(lambda_box_return_prim("V", "I"), None);
    assert_eq!(
        lambda_box_return_prim("Ljava/lang/Object;", "Ljava/lang/String;"),
        None
    );
    let int = Some("Ljava/lang/Integer;");
    assert_eq!(
        lambda_unbox_param_prim("Ljava/lang/Object;", "I", int),
        Some('I')
    );
    assert_eq!(
        lambda_unbox_param_prim("Ljava/lang/Integer;", "I", None),
        Some('I')
    );
    assert_eq!(
        lambda_unbox_param_prim("Ljava/lang/Object;", "J", int),
        None,
        "no widening after the unbox"
    );
    assert_eq!(
        lambda_unbox_param_prim("Ljava/lang/Object;", "I", Some("Ljava/lang/Number;")),
        None
    );
    assert_eq!(
        lambda_unbox_param_prim("Ljava/lang/Object;", "I", None),
        None
    );
    assert_eq!(
        lambda_unbox_param_prim("I", "I", None),
        None,
        "not a reference"
    );
    assert_eq!(
        lambda_unbox_param_prim("Ljava/lang/Object;", "D", Some("Ljava/lang/Double;")),
        Some('D')
    );
}

/// A primitive SAM argument never stands for the receiver.
#[test]
fn a_primitive_receiver_slot_is_refused() {
    let s = site(MethodHandleKind::InvokeVirtual, "(I)I", "()I", "(I)I", &[]);
    assert!(lambda_instance_shape(&s, false).is_none());
}
