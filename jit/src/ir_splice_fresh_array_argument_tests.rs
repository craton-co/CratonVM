use super::*;

fn site(descriptor: &str) -> InlineSite {
    InlineSite {
        descriptor: descriptor.to_string(),
        ..InlineSite::default()
    }
}

#[test]
fn fresh_array_reference_argument_keeps_the_inline_call_boundary() {
    // iload_0; newarray long; iload_1; invokestatic; ireturn
    let caller = [0x1a, 0xbc, 0x0b, 0x1b, 0xb8, 0x00, 0x01, 0xac];
    assert!(ir_splice_site_takes_fresh_array_argument(
        &site("([JZ)Z"),
        &caller,
        4,
    ));
}

#[test]
fn no_reference_argument_or_prior_allocation_keeps_the_inline_site() {
    let caller = [0x2a, 0x1b, 0xb8, 0x00, 0x01, 0xac];
    assert!(!ir_splice_site_takes_fresh_array_argument(
        &site("(Ljava/lang/Object;Z)Z"),
        &caller,
        2,
    ));
    assert!(!ir_splice_site_takes_fresh_array_argument(
        &site("([JZ)Z"),
        &caller,
        2,
    ));
    // new #1; dup; aconst_null; aconst_null; invokespecial Node.<init>(LNode;LNode;)V; areturn
    let new_ctor = [0xbb, 0x00, 0x01, 0x59, 0x01, 0x01, 0xb7, 0x00, 0x02, 0xb0];
    let ctor_site = InlineSite {
        method_name: "<init>".to_string(),
        descriptor: "(LNode;LNode;)V".to_string(),
        ..InlineSite::default()
    };
    assert!(!ir_splice_site_takes_fresh_array_argument(
        &ctor_site,
        &new_ctor,
        6,
    ));
    assert!(!ir_splice_site_takes_fresh_array_argument(
        &site("(LNode;LNode;)V"),
        &new_ctor,
        6,
    ));
}
