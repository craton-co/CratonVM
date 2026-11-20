/// The text of the top-level function starting at `signature`.
fn body_of<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src.find(signature).expect("function present");
    let len = src[start..].find("\n}\n").expect("function end");
    &src[start..start + len]
}

/// gci i5/jd: `JavacFileManager.inferBinaryName`'s `--compatible` stand-in
/// makes a Java upcall (`Location.getName()`) before it reads the file
/// object, and a collection another thread requested can run inside it.
/// The file object must be read back from its pin after that upcall, and
/// the JDK-body fallback must get the pinned references, never `args`.
#[test]
fn infer_binary_name_reads_its_references_back_after_the_upcall() {
    let src = include_str!("../test_frameworks.rs");
    let outer = body_of(src, "pub fn native_javac_file_manager_infer_binary_name(");
    assert!(outer.contains("ctx.pin_native_root(file)"));
    assert!(outer.contains("ctx.unpin_native_roots(pin_base);"));
    assert!(!outer.contains("args[1..]"));
    let body = body_of(src, "fn javac_infer_binary_name_pinned(");
    let upcall = body
        .find("native_javac_file_manager_check_not_module_oriented_location(")
        .expect("the module-oriented check");
    let file_read = body
        .find("let file_now = ctx.read_native_pin(file.0, file.1);")
        .expect("file read back");
    assert!(file_read > upcall, "the file object is read before the upcall");
    assert!(!body.contains("args[1..]"));
    let fallback = body.find("\"inferBinaryName\"").expect("JDK-body fallback");
    let reread = body[..fallback]
        .rfind("let this_now = ctx.read_native_pin(this.0, this.1);")
        .expect("receiver read back");
    assert!(reread > file_read, "the fallback receiver is read back after the branches");
}
