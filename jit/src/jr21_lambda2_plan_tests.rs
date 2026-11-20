use super::*;

const OWN: (&str, &str, &str) = ("p/Host", "k", "(I)I");

fn answer(
    captures: usize,
    sam: (&str, &str),
    desc: &str,
) -> compile_request::IndyLambdaImplAnswer {
    compile_request::IndyLambdaImplAnswer {
        proxy_class_id: 0x8000_0021,
        num_captures: captures,
        sam_name: sam.0.to_string(),
        sam_descriptor: sam.1.to_string(),
        class: "p/Host".to_string(),
        name: "lambda$k$0".to_string(),
        descriptor: desc.to_string(),
        class_id: 0x7121,
    }
}

fn names(cp_idx: u16) -> Option<(String, String, String)> {
    match cp_idx {
        2 => Some((
            "java/util/function/IntSupplier".to_string(),
            "getAsInt".to_string(),
            "()I".to_string(),
        )),
        3 => Some((
            "java/util/function/IntSupplier".to_string(),
            "toString".to_string(),
            "()Ljava/lang/String;".to_string(),
        )),
        _ => None,
    }
}

fn plan(code: &[u8], invoke_ops: &[(usize, u16, u8)], indy_pc: usize) -> IrLambdaImplPlan {
    let lambdas = HashMap::from([(
        indy_pc,
        (1u16, 1usize, answer(1, ("getAsInt", "()I"), "(I)I")),
    )]);
    let shapes = HashMap::from([(indy_pc, (1usize, b'L'))]);
    let names: &dyn Fn(u16) -> Option<(String, String, String)> = &names;
    ir_lambda_impl_plan(
        Some(names),
        &lambdas,
        &shapes,
        code,
        code.len(),
        &[],
        invoke_ops,
        true,
        OWN,
        &|_| false,
        None,
    )
}

//  0: iload_0  1: invokedynamic #1 (I)L  6: astore_1  7: aload_1
//  8: invokeinterface #2 getAsInt()I, 1  13: ireturn
const CAPTURE: [u8; 14] = [
    0x1a, 0xba, 0x00, 0x01, 0x00, 0x00, 0x4c, 0x2b, 0xb9, 0x00, 0x02, 0x01, 0x00, 0xac,
];

#[test]
fn a_sam_call_on_a_stored_lambda_is_planned_as_its_impl() {
    let p = plan(&CAPTURE, &[(8, 2, 0xb9)], 1);
    assert_eq!(p.site_count(), 1);
    assert_eq!(
        p.names(1),
        Some((
            "p/Host".to_string(),
            "lambda$k$0".to_string(),
            "(I)I".to_string()
        ))
    );
    assert_eq!(p.class_id(1), Some(0x7121));
    assert_eq!(p.rewritten_invoke_ops(&[(8, 2, 0xb9)]), vec![(8, 1, 0xb8)]);
    assert_eq!(
        p.builder_sites(),
        HashMap::from([(8usize, (1usize, 1usize, 1usize))])
    );
}

#[test]
fn a_sam_call_directly_on_the_invokedynamic_is_planned_too() {
    //  0: iload_0  1: invokedynamic #1 (I)L  6: invokeinterface #2, 1  11: ireturn
    let code = [
        0x1a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb9, 0x00, 0x02, 0x01, 0x00, 0xac,
    ];
    let p = plan(&code, &[(6, 2, 0xb9)], 1);
    assert_eq!(
        p.builder_sites(),
        HashMap::from([(6usize, (1usize, 1usize, 1usize))])
    );
}

#[test]
fn unproven_or_foreign_calls_keep_the_interface_call() {
    // Another method of the interface (not the SAM).
    assert!(plan(&CAPTURE, &[(8, 3, 0xb9)], 1).is_empty());
    // The planned lambda is not the one the slot holds.
    assert!(plan(&CAPTURE, &[(8, 2, 0xb9)], 99).is_empty());
    // A branch target between the store and the load.
    //  0: iload_0  1: invokedynamic  6: astore_1  7: goto +3  10: aload_1
    // 11: invokeinterface #2, 1  16: ireturn
    let joined = [
        0x1a, 0xba, 0x00, 0x01, 0x00, 0x00, 0x4c, 0xa7, 0x00, 0x03, 0x2b, 0xb9, 0x00, 0x02,
        0x01, 0x00, 0xac,
    ];
    assert!(plan(&joined, &[(11, 2, 0xb9)], 1).is_empty());
    // The slot is written again between the store and the load.
    //  0: iload_0  1: invokedynamic  6: astore_1  7: aconst_null  8: astore_1
    //  9: aload_1  10: invokeinterface #2, 1  15: ireturn
    let rewritten = [
        0x1a, 0xba, 0x00, 0x01, 0x00, 0x00, 0x4c, 0x01, 0x4c, 0x2b, 0xb9, 0x00, 0x02, 0x01,
        0x00, 0xac,
    ];
    assert!(plan(&rewritten, &[(10, 2, 0xb9)], 1).is_empty());
    // The impl is the compiling method itself.
    let lambdas = HashMap::from([(
        1usize,
        (1u16, 1usize, answer(1, ("getAsInt", "()I"), "(I)I")),
    )]);
    let shapes = HashMap::from([(1usize, (1usize, b'L'))]);
    let names: &dyn Fn(u16) -> Option<(String, String, String)> = &names;
    let own = ("p/Host", "lambda$k$0", "(I)I");
    assert!(ir_lambda_impl_plan(
        Some(names),
        &lambdas,
        &shapes,
        &CAPTURE,
        CAPTURE.len(),
        &[],
        &[(8, 2, 0xb9)],
        true,
        own,
        &|_| false,
        None,
    )
    .is_empty());
    // An unresolved (first-call) site, and the static-call gate off.
    for (static_calls, unresolved) in [(true, true), (false, false)] {
        assert!(ir_lambda_impl_plan(
            Some(names),
            &lambdas,
            &shapes,
            &CAPTURE,
            CAPTURE.len(),
            &[],
            &[(8, 2, 0xb9)],
            static_calls,
            OWN,
            &|_| unresolved,
            None,
        )
        .is_empty());
    }
}

