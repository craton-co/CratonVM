use super::bind_spliced_static_calls;
use cratonvm_types::ClassId;

/// i11-L0: a call inside a spliced body is direct-bound on behalf of the
/// class whose constant pool named it (the body's declaring class, carried
/// on the target), never the root compile's class. Before wave 12 the
/// binders resolved the name through the ROOT's loader and asked JVMS
/// §5.4.4 with the root as the accessor, which is what put
/// `RArraysMismatch -> jdk.internal.util.ArraysSupport` in the export
/// census. Virtual / interface kinds and nested pcs are not asked at all.
#[test]
fn a_spliced_call_is_bound_on_behalf_of_the_callees_class() {
    const CALLEE_CLASS: u32 = 4_711;
    let target = |pc: usize, kind: u8, name: &str| {
        (
            pc,
            cratonvm_jit::InlineInvokeTarget {
                class_name: "jdk/internal/util/ArraysSupport".to_string(),
                method_name: name.to_string(),
                descriptor: "()I".to_string(),
                num_jit_args: if kind == 3 { 0 } else { 1 },
                return_type: b'I',
                invoke_kind: kind,
                declaring_class_id: CALLEE_CLASS,
                target_class_id: 0,
                resolved_class_id: 0,
                owner_class_id: 0,
                direct_entry: None,
            },
        )
    };
    let mut targets = vec![
        target(1, 3, "st"),
        target(4, 1, "sp"),
        target(7, 0, "vi"),
        target(12, 2, "itf"),
        target(17, 3, "nested"),
    ];
    let nested = [cratonvm_jit::NestedInlineSite {
        callee_pc: 17,
        ..Default::default()
    }];
    let asked: std::cell::RefCell<Vec<(u32, String)>> = std::cell::RefCell::new(Vec::new());
    let bind = |holder: ClassId,
                class: &str,
                method: &str,
                _descriptor: &str|
     -> Option<(usize, bool)> {
        asked
            .borrow_mut()
            .push((holder.as_u32(), format!("{class}.{method}")));
        Some((0x1000, false))
    };
    bind_spliced_static_calls(&mut targets, &nested, &bind);
    let asked = asked.borrow().clone();
    assert_eq!(
        asked,
        vec![
            (
                CALLEE_CLASS,
                "jdk/internal/util/ArraysSupport.st".to_string()
            ),
            (
                CALLEE_CLASS,
                "jdk/internal/util/ArraysSupport.sp".to_string()
            ),
        ],
        "only the statically bound, un-nested calls, each on behalf of the callee's class"
    );
    let entries: Vec<Option<(usize, bool)>> =
        targets.iter().map(|(_, t)| t.direct_entry).collect();
    assert_eq!(
        entries,
        vec![
            Some((0x1000, false)),
            Some((0x1000, false)),
            None,
            None,
            None
        ]
    );
}
