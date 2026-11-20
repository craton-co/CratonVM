use super::*;

fn pairs(declaring: Option<&'static str>) -> std::collections::HashMap<usize, u32> {
    // invokevirtual #5 ; ldc #7 ; if_acmpne +7 ; iconst_1 ; ireturn ; iconst_0 ; ireturn
    let code = [
        0xb6, 0x00, 0x05, 0x12, 0x07, 0xa6, 0x00, 0x07, 0x04, 0xac, 0x03, 0xac,
    ];
    let ldc = |i: u16| {
        (i == 7).then_some(JitLdcConstant::ClassMirror {
            holder_class_id: 1,
            cp_idx: 7,
        })
    };
    let new_site = |_: u16| {
        Some(JitNewSite::Resolved {
            class_id: 42,
            num_fields: 0,
            needs_post_alloc_init: false,
            has_finalizer: false,
        })
    };
    let invoke = |_: u16| {
        Some((
            "P".to_string(),
            "getClass".to_string(),
            "()Ljava/lang/Class;".to_string(),
        ))
    };
    let declaring_of = move |_: u16| declaring.map(str::to_string);
    let class_name = |_: u16| Some("java/lang/String".to_string());
    let fold = |_: u32| true;
    ir_getclass_exact_compares(
        &code,
        code.len(),
        &[(3, 7)],
        &ldc,
        &new_site,
        &invoke,
        &declaring_of,
        &class_name,
        &fold,
    )
}

#[test]
fn folds_only_object_get_class() {
    assert_eq!(pairs(Some("java/lang/Object")).get(&0), Some(&42));
    assert!(
        pairs(Some("P")).is_empty(),
        "a private P.getClass keeps its call"
    );
    assert!(pairs(None).is_empty(), "an unresolved site keeps its call");
}
