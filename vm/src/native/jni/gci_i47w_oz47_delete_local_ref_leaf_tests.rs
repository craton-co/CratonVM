//! gci i47w lane oz47: `DeleteLocalRef` opens its leaf window without the
//! relocation check (it resolves nothing); every other leaf keeps it.

fn body<'a>(src: &'a str, head: &str) -> &'a str {
    let at = src.find(head).expect(head);
    let rest = &src[at..];
    &rest[..rest.find("\n    \u{7d}\n").expect("its end")]
}

#[test]
fn gci_i47w_oz47_delete_local_ref_skips_only_the_relocation_check() {
    let src = concat!(include_str!("../jni/mod.rs"), "\n", include_str!("../jni/j_boolean.rs"), "\n", include_str!("../jni/raw_locals_counted.rs"), "\n", include_str!("../jni/truncate_local_frames.rs"), "\n", include_str!("../jni/jni_empty_array_elements.rs"), "\n", include_str!("../jni/lambda_proxy_jni_construct.rs")).replace("\r\n", "\n");
    let at = src.find("extern \"C\" fn jni_delete_local_ref(").expect("DeleteLocalRef");
    let del = &src[at..at + src[at..].find("\n\u{7d}\n").expect("its end")];
    assert!(del.contains("ForeignJniEntry::enter_leaf_table_only(lref)"));
    let leaf = body(&src, "    pub(crate) fn enter_leaf(handle: JObject) -> Self {");
    assert!(leaf.contains("Self::enter_leaf_window(handle, true)"), "the other leaves keep the check");
    let table = body(&src, "    pub(crate) fn enter_leaf_table_only(handle: JObject) -> Self {");
    assert!(table.contains("Self::enter_leaf_window(handle, false)"));
    let window = body(
        &src,
        "    pub(in crate::native::jni) fn enter_leaf_window(handle: JObject, needs_current_table: bool) -> Option<Self> {",
    );
    let open = window
        .find("try_open_leaf_window(word)")
        .expect("the window");
    let check = window
        .find("needs_current_table\n                    && NativeSync::read(")
        .expect("the gated check");
    assert!(open < check, "the pause exclusion is still taken first");
    assert!(window.contains("decode_indirect_local(handle).is_none()"), "own locals only, as before");
}

