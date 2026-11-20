use super::*;

const DESCS: &[&str] = &[
    "()V",
    "()I",
    "(Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;II)I",
    "(ZBSCIJFD[ILjava/lang/String;[[Ljava/lang/Object;)Z",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(V)V",
    "(Ljava/lang/Object)V",
    "(I)Vx",
    "I)V",
    "(I",
    "",
    "(Q)V",
    "(J)D",
];

#[test]
fn the_stack_parse_answers_as_the_vec_parse() {
    for desc in DESCS {
        let mut buf = [0u8; JR21_REFLECT2_SIG_CAP];
        let lean = jr21_reflect2_basic_sig(desc, &mut buf)
            .expect("no test descriptor is wider than the buffer")
            .map(|(n, r)| (buf[..n].to_vec(), r));
        assert_eq!(lean, mh_link_basic_sig(desc), "{desc}");
    }
}

#[test]
fn a_descriptor_wider_than_the_buffer_takes_the_vec_parse() {
    let wide = format!("({})V", "I".repeat(JR21_REFLECT2_SIG_CAP + 1));
    let mut buf = [0u8; JR21_REFLECT2_SIG_CAP];
    assert!(jr21_reflect2_basic_sig(&wide, &mut buf).is_err());
    // The agreement still answers, through the allocating parse.
    assert!(jr21_reflect2_basic_agree(&wide, &wide, 0, 0, 0, 0));
}

#[test]
fn the_lean_agreement_answers_as_the_allocating_one() {
    let shapes: &[(usize, usize, usize, usize)] = &[
        (0, 0, 0, 0),
        (1, 0, 0, 1),
        (1, 0, 0, 0),
        (0, 1, 1, 0),
        (0, 0, 1, 0),
    ];
    for site in DESCS {
        for member in DESCS {
            for &(lead, front, back, trail) in shapes {
                let reference = match (mh_link_basic_sig(site), mh_link_basic_sig(member)) {
                    (Some((sp, sr)), Some((mp, mr))) => jr21_reflect2_basic_agree_slices(
                        &sp, sr, &mp, mr, lead, front, back, trail,
                    ),
                    _ => false,
                };
                assert_eq!(
                    jr21_reflect2_basic_agree(site, member, lead, front, back, trail),
                    reference,
                    "{site} vs {member} ({lead},{front},{back},{trail})"
                );
            }
        }
    }
    // The `Method.invoke` linker shape: a compiled
    // `invokeExact(Object,Object,Object)Object` site against the linker
    // `invokeExact_MT(handle, Object, Object, Object, appendix)Object`.
    assert!(jr21_reflect2_basic_agree(
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        1,
        0,
        0,
        1,
    ));
    // An `int` the member takes where the site passes a reference is the door's.
    assert!(!jr21_reflect2_basic_agree(
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/Object;IILjava/lang/Object;)Ljava/lang/Object;",
        1,
        0,
        0,
        1,
    ));
}
