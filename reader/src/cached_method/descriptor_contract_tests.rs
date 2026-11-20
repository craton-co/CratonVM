use super::count_param_slots;

#[test]
fn compact_parameter_count_is_backend_neutral() {
    assert_eq!(count_param_slots("()V"), 0);
    assert_eq!(count_param_slots("(IJDF)V"), 4);
    assert_eq!(
        count_param_slots("([[I[Ljava/lang/Object;Ljava/lang/String;)V"),
        3
    );
    assert_eq!(count_param_slots("not-a-method-descriptor"), 0);
}

/// Round 11 wave 4 (lane lower): the return tag is the byte after the `)`
/// that CLOSES the parameter list. A class name may contain `)` (JVMS
/// 4.2.2), and the first-`)` scan answered with a byte of that name.
#[test]
fn the_return_tag_is_not_fooled_by_a_paren_in_a_class_name() {
    use super::{scan_return_tag, DescriptorFacts};
    for (d, want) in [
        ("(LA)B;)Z", b'Z'),
        ("(LA)B;)J", b'J'),
        ("([LA)Z;I)B", b'B'),
        ("(LA)B;)Ljava/lang/Object;", b'L'),
        ("()V", b'V'),
        ("(IJ)[I", b'['),
        ("(I)", b'V'),
        ("(I", b'V'),
        ("", b'V'),
    ] {
        assert_eq!(scan_return_tag(d), want, "{d:?}");
        assert_eq!(DescriptorFacts::of(d).ret_tag, want, "{d:?}");
    }
    // The parameter walk was already right; only the tag was not.
    assert_eq!(count_param_slots("(LA)B;)Z"), 1);
}

/// Round 11 wave 5 (lane rt): the shared cut point the VM-side parsers
/// (`descriptors_match_modulo_return`, `varhandle_cas_operand_kinds`,
/// `descriptor_return_ref`, the `varhandle_*_helper_slot` pre-filters)
/// now use instead of `find(')')`.
#[test]
fn the_param_list_end_is_the_closing_paren_not_the_first() {
    use super::descriptor_param_list_end as end;
    assert_eq!(end("(LA)B;)Z"), Some(6));
    assert_eq!(end("([LA)Z;I)B"), Some(8));
    assert_eq!(end("()LA)I;"), Some(1));
    assert_eq!(end("(IJ)V"), Some(3));
    assert_eq!(end("()V"), Some(1));
    assert_eq!(end("(I"), None);
    assert_eq!(end("(LA)B"), None, "unterminated class name");
    assert_eq!(end(""), None);
}
