use super::*;

fn entry(class: &str, method: &str, descriptor: &str, bci: i32, id: u32) -> StackTraceEntry {
    StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: Some(Arc::from(descriptor)),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(ClassId::new(id)),
        method_index: None,
    }
}

fn row(name: &'static str, descriptor: &'static str) -> crate::vm::realms::jit_realm::ReflectLinkAdapter {
    crate::vm::realms::jit_realm::ReflectLinkAdapter {
        declaring_class_id: 7,
        method_name: name,
        descriptor,
        adapter_class_id: 9,
        adapter_method: Arc::from("invoke"),
        holder_kind: 3,
    }
}

#[test]
fn r22_runtime_a_linked_member_is_found_by_its_names_and_class() {
    let store = ClassStore::new();
    let rows = [row("hiddenWalk", "(I)Ljava/lang/String;")];
    let target = entry("p/T", "hiddenWalk", "(I)Ljava/lang/String;", 3, 7);
    assert!(linked_row_for(&store, &target, &rows).is_some());
    let other = entry("p/T", "other", "(I)Ljava/lang/String;", 3, 7);
    assert!(linked_row_for(&store, &other, &rows).is_none());
    let wrong_desc = entry("p/T", "hiddenWalk", "()V", 3, 7);
    assert!(linked_row_for(&store, &wrong_desc, &rows).is_none());
    // A class the store cannot place is no subclass of the row's.
    let unrelated = entry("p/U", "hiddenWalk", "(I)Ljava/lang/String;", 3, 8);
    assert!(linked_row_for(&store, &unrelated, &rows).is_none());
    assert!(linked_row_for(&store, &target, &[]).is_none());
}

/// Fail-closed: with no class the store holds, a linked shape (a member
/// directly under an accessor's `invokeImpl` with no bci) names nothing.
#[test]
fn r22_runtime_a_linked_shape_the_store_cannot_place_names_nothing() {
    let store = ClassStore::new();
    let rows = [row("hiddenWalk", "(I)Ljava/lang/String;")];
    let frames = [
        entry("p/T", "hiddenWalk", "(I)Ljava/lang/String;", 3, 7),
        entry(
            "jdk/internal/reflect/DirectMethodHandleAccessor",
            "invokeImpl",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
            -1,
            11,
        ),
    ];
    assert!(hidden_invoke_standin_frames_with(&store, &frames, &rows).is_empty());
    assert!(
        linked_reflective_adapter_entry(&store, &frames[0], &frames[1], None, &rows).is_none()
    );
    assert!(linked_reflective_adapter_entry(
        &store,
        &frames[0],
        &frames[1],
        Some(("invoke", "(Ljava/lang/Object;)Ljava/lang/Object;")),
        &rows
    )
    .is_none());
}
