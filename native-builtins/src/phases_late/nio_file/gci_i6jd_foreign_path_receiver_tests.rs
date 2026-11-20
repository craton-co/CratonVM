/// gci i6/jd: a `java/nio/file/Path` row whose RECEIVER is a foreign Path
/// (`JrtPath`, `ZipPath`) must run the receiver's own body virtually;
/// `invokespecial` on the interface's abstract method answered
/// `Object.toString()` and javac read a jrt `module-info.class` as SOURCE.
#[test]
fn a_foreign_path_receiver_yields_to_its_own_body() {
    let src = concat!(include_str!("../nio_file/mod.rs"), "\n", include_str!("../nio_file/register_phase57_natives.rs"), "\n", include_str!("../nio_file/register_phase57_nio_file.rs"), "\n", include_str!("../nio_file/register_phase57_nio_file_a.rs"), "\n", include_str!("../nio_file/register_phase57_nio_file_b.rs"), "\n", include_str!("../nio_file/register_phase57_nio_file_c.rs"), "\n", include_str!("../nio_file/p57_line_separator.rs"), "\n", include_str!("../nio_file/p57_resolve_paths.rs"), "\n", include_str!("../nio_file/fs_list_dir.rs"), "\n", include_str!("../nio_file/win_filetime_from_millis.rs"), "\n", include_str!("../nio_file/register_p70_file_attributes.rs"));
    let start = src
        .find("pub fn yield_to_foreign_path(")
        .expect("yield_to_foreign_path");
    let len = src[start..].find("\n}\n").expect("function end");
    let body = &src[start..start + len];
    let virtual_call = body
        .find("ctx.invoke_virtual_bytecode_only(")
        .expect("virtual yield on the receiver");
    let special_call = body
        .rfind("ctx.invoke_special_bytecode_only(owner, method, descriptor, args)")
        .expect("special yield kept for the rest");
    assert!(virtual_call < special_call);
    assert!(body.contains("if owner == \"java/nio/file/Path\""));
    assert!(body.contains("receiver_chain_declares(ctx, recv, method, descriptor)"));
}
