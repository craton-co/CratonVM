//! JIT round 21 wave 25, lane rawwrite (item 3 of
//! `r21-review-w24-findings-FIXED-20261020.md`): no production chained
//! `classes.class_manager.write().<loader>(..)` is left in the files whose
//! sites moved to [`super::ClassRealm::class_manager_write_settling`].

/// `src` without its top-level test-only items (`#[cfg(test)]` or
/// `#[cfg(all(test, ..))]`: a `mod x;`, or an item running to the next
/// column-0 `}`), with every whitespace removed.
fn production_squeezed(src: &str) -> String {
    let src = src.replace("\r\n", "\n");
    let mut out = String::new();
    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        let test_only =
            line.starts_with("#[cfg(") && line.contains("(test") && !line.contains("not(test");
        if test_only {
            // Skip further attributes, then the item.
            while lines.peek().is_some_and(|l| l.starts_with("#[")) {
                lines.next();
            }
            let Some(item) = lines.next() else { break };
            if item.trim_end().ends_with(';') {
                continue;
            }
            for inner in lines.by_ref() {
                if inner.starts_with('}') {
                    break;
                }
            }
            continue;
        }
        out.extend(line.chars().filter(|c| !c.is_whitespace()));
    }
    out
}

#[test]
fn no_production_raw_class_manager_write_reaches_a_loader() {
    let files: [(&str, &str); 9] = [
        ("native/jni.rs", concat!(include_str!("../../../native/jni/mod.rs"), "\n", include_str!("../../../native/jni/j_boolean.rs"), "\n", include_str!("../../../native/jni/raw_locals_counted.rs"), "\n", include_str!("../../../native/jni/truncate_local_frames.rs"), "\n", include_str!("../../../native/jni/jni_empty_array_elements.rs"), "\n", include_str!("../../../native/jni/lambda_proxy_jni_construct.rs"))),
        ("runtime/exceptions.rs", include_str!("../../../runtime/exceptions.rs")),
        ("runtime/interpreter.rs", concat!(include_str!("../../../runtime/interpreter/mod.rs"), "\n", include_str!("../../../runtime/interpreter/cached_invoke_target.rs"), "\n", include_str!("../../../runtime/interpreter/execute.rs"), "\n", include_str!("../../../runtime/interpreter/resume_continuation.rs"), "\n", include_str!("../../../runtime/interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../../runtime/interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../../runtime/interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../../runtime/interpreter/double_arith_apply.rs"))),
        (
            "runtime/interpreter/constants.rs",
            include_str!("../../../runtime/interpreter/constants.rs"),
        ),
        ("runtime/interpreter/invoke.rs", include_str!("../../../runtime/interpreter/invoke.rs")),
        ("runtime/interpreter/lambda.rs", include_str!("../../../runtime/interpreter/lambda.rs")),
        (
            "runtime/interpreter/typecheck.rs",
            include_str!("../../../runtime/interpreter/typecheck.rs"),
        ),
        (
            "vm/vm_exec.rs",
            concat!(
                include_str!("../../vm_exec/mod.rs"),
                "\n",
                include_str!("../../vm_exec/method.rs"),
                "\n",
                include_str!("../../vm_exec/report_pad_access.rs"),
                "\n",
                include_str!("../../vm_exec/diagnose_sync_on_value_based_class.rs"),
                "\n",
                include_str!("../../vm_exec/thread_interrupted_field_slot_of.rs"),
                "\n",
                include_str!("../../vm_exec/clear_breakpoints_of_redefined_class.rs"),
                "\n",
                include_str!("../../vm_exec/load_class.rs"),
                "\n",
                include_str!("../../vm_exec/native_new_initializes_class_first.rs"),
                "\n",
                include_str!("../../vm_exec/new_object.rs"),
                "\n",
                include_str!("../../vm_exec/gen_referent_read_keep_alive.rs"),
                "\n",
                include_str!("../../vm_exec/append_native_standin_frames.rs"),
                "\n",
                include_str!("../../vm_exec/unloaded_native_libraries.rs"),
                "\n",
                include_str!("../../vm_exec/extract_annotations_from_attributes.rs"),
                "\n",
                include_str!("../../vm_exec/invoke_static_settling_with_first.rs"),
                "\n",
                include_str!("../../vm_exec/invoke_special_shared.rs"),
                "\n",
                include_str!("../../vm_exec/proxy_resolve_declaring_method.rs"),
                "\n",
                include_str!("../../vm_exec/invoke_on_class_shared_inner.rs"),
                "\n",
                include_str!("../../vm_exec/jni_table_native_call.rs")
            ),
        ),
        (
            "debug/ensure_startup_array_classes.rs",
            include_str!("../../../debug/ensure_startup_array_classes.rs"),
        ),
    ];
    for (name, src) in files {
        let prod = production_squeezed(src);
        assert!(prod.contains(".class_manager_write_settling()"), "{name}: a settling site");
        for loader in [
            "load_class(",
            "try_ensure_synthetic_class(",
            "ensure_generated_class(",
            "load_array_class_for_loader(",
        ] {
            let raw = format!("classes.class_manager.write().{loader}");
            assert!(!prod.contains(&raw), "{name}: a raw `{raw}` in production code");
        }
    }
}
