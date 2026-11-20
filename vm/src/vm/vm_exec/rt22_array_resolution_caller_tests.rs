/// Source pin: `array_resolution_referencing_class` reads the compiled-aware
/// walk (`frame_class_ids_tail` / `frame_class_ids_with_compiled`), never
/// `thread.frames` alone, and still skips the same plumbing classes.
#[test]
fn rt22_array_resolution_referencing_class_walks_compiled_frames() {
    let src =
        include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs").replace("\r\n", "\n");
    let start = src
        .find(concat!("fn array_resolution_referencing", "_class(&self)"))
        .expect("the function");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n    }\n").expect("its end")];
    assert!(body.contains("stackwalker::frame_class_ids_tail(&self.thread.frames"));
    assert!(body.contains("stackwalker::frame_class_ids_with_compiled(&self.thread.frames)"));
    assert!(!body.contains("self.thread.frames.iter()"));
    for plumbing in [
        "java/lang/Class",
        "java/lang/reflect/Array",
        "java/lang/invoke/MethodType",
    ] {
        assert!(body.contains(plumbing), "{plumbing} must stay plumbing");
    }
}
