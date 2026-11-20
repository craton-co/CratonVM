use super::bind_spliced_final_calls;
use cratonvm_types::ClassId;

fn virtual_row(pc: usize, name: &str) -> (usize, cratonvm_jit::InlineInvokeTarget) {
    (
        pc,
        cratonvm_jit::InlineInvokeTarget {
            class_name: "java/util/HashMap".to_string(),
            method_name: name.to_string(),
            descriptor: "(Ljava/lang/Object;)Ljava/util/HashMap$Node;".to_string(),
            num_jit_args: 2,
            return_type: b'L',
            invoke_kind: 0,
            declaring_class_id: 64,
            target_class_id: 0,
            resolved_class_id: 0,
            owner_class_id: 0,
            direct_entry: None,
        },
    )
}

/// JIT round 16 (lane callbind): a pinned final `invokevirtual` becomes a
/// statically bound row at its declaring class only when the binder
/// answers an entry; an unpinned row, a refused bind and a nested pc
/// (spliced, or exact-receiver) keep the virtual row untouched, and a
/// nested pc is not even asked.
#[test]
fn a_final_virtual_row_is_bound_only_when_the_binder_answers() {
    let mut targets = vec![
        virtual_row(2, "getNode"),
        virtual_row(9, "unpinned"),
        virtual_row(14, "notCompiled"),
        virtual_row(20, "exactNested"),
        virtual_row(30, "splicedNested"),
    ];
    let pins = vec![
        (2, "java/util/HashMap".to_string(), 64),
        (14, "java/util/HashMap".to_string(), 64),
        (20, "java/util/HashMap".to_string(), 64),
        (30, "java/util/HashMap".to_string(), 64),
    ];
    let nested = [
        cratonvm_jit::NestedInlineSite {
            callee_pc: 30,
            ..Default::default()
        },
        cratonvm_jit::NestedInlineSite {
            callee_pc: 20,
            guard_class_id: 64,
            ..Default::default()
        },
    ];
    let asked: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
    let bind = |holder: ClassId,
                class: &str,
                method: &str,
                _descriptor: &str|
     -> Option<(usize, bool)> {
        assert_eq!(holder.as_u32(), 64, "asked on behalf of the body's class");
        asked.borrow_mut().push(format!("{class}.{method}"));
        (method == "getNode" || method == "splicedNested").then_some((0x4000, true))
    };
    bind_spliced_final_calls(&mut targets, &pins, &nested, &bind);
    assert_eq!(
        asked.borrow().clone(),
        vec![
            "java/util/HashMap.getNode".to_string(),
            "java/util/HashMap.notCompiled".to_string()
        ]
    );
    let bound = &targets[0].1;
    assert_eq!(
        (bound.invoke_kind, bound.owner_class_id, bound.direct_entry),
        (1, 64, Some((0x4000, true)))
    );
    for (_, t) in &targets[1..] {
        assert_eq!(
            (t.invoke_kind, t.owner_class_id, t.direct_entry),
            (0, 0, None)
        );
    }
}
