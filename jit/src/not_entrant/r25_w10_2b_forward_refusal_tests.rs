use crate::{CompiledMethod, ExecutableBuffer, ENTRY_PATCH_PAD};

/// A body answering `value`, taking the VM context when `context`.
fn body(value: u8, context: bool, args: u16) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&ENTRY_PATCH_PAD);
    buf.emit(&[0xB8, value, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, value ; RET
    let mut cm = if context {
        CompiledMethod::new_with_context(buf)
    } else {
        CompiledMethod::new(buf)
    };
    cm.entry_java_args = args;
    cm
}

/// The refusal is the admissibility test's own, named: the context ABI in
/// both directions, the arity, and `None` exactly when admissible.
#[test]
fn w10_2b_the_refused_forward_names_its_conjunct() {
    let c1_ctx = body(7, true, 3);
    let c2_plain = body(9, false, 3);
    let c2_ctx = body(9, true, 3);
    let why = super::supersede_forward_refusal(&c1_ctx, &c2_plain).expect("refused");
    assert!(why.contains("the C1 body takes the VM context"), "{why}");
    let why = super::supersede_forward_refusal(&c2_plain, &c1_ctx).expect("refused");
    assert!(why.contains("the C2 body takes the VM context"), "{why}");
    assert_eq!(super::supersede_forward_refusal(&c1_ctx, &c2_ctx), None);
    assert!(super::supersede_forward_admissible(&c1_ctx, &c2_ctx));
    let other_arity = body(9, true, 2);
    assert_eq!(
        super::supersede_forward_refusal(&c1_ctx, &other_arity),
        Some("argument count")
    );
    for (old, new) in [
        (&c1_ctx, &c2_plain),
        (&c1_ctx, &c2_ctx),
        (&c1_ctx, &other_arity),
    ] {
        assert_eq!(
            super::supersede_forward_admissible(old, new),
            super::supersede_forward_refusal(old, new).is_none()
        );
    }
}
