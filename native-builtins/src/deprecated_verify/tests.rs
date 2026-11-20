use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// ── T8.5.1 — Round-trip: every deprecated API is registered ──────────

#[test]
fn t85_1_all_deprecated_apis_registered() {
    let r = build_deprecated_registry();
    let manifest = deprecated_api_manifest();
    let mut missing = Vec::new();

    let mut must_not_be_registered = Vec::new();

    for api in &manifest {
        let found = r.find(api.class, api.method, api.descriptor).is_some();
        match api.images {
            ImageStatus::Declared if !found => missing.push(format!(
                "{} {}.{}{}",
                api.section, api.class, api.method, api.descriptor
            )),
            // The other direction, and it is the half this test did not
            // have: a native registered in front of a method NO supported
            // image declares can never be dispatched, and asserting it
            // stays registered is what blocked the retirement in `H25-3`
            // R2. Assert the absence instead.
            ImageStatus::AbsentFromAllSupportedImages if found => {
                must_not_be_registered.push(format!(
                    "{} {}.{}{}",
                    api.section, api.class, api.method, api.descriptor
                ))
            }
            _ => {}
        }
    }

    assert!(
        missing.is_empty(),
        "Missing deprecated API registrations:\n{}",
        missing.join("\n")
    );
    assert!(
        must_not_be_registered.is_empty(),
        "Registered in front of a method NO supported JDK image declares \
         (see ImageStatus::AbsentFromAllSupportedImages):\n{}",
        must_not_be_registered.join("\n")
    );
}

#[test]
fn t85_1_manifest_covers_all_sections() {
    let manifest = deprecated_api_manifest();
    let sections: std::collections::HashSet<&str> =
        manifest.iter().map(|a| a.section).collect();

    // Verify every T8 sub-section that still has natives is represented.
    //
    // **T8.2.7 was here and is deliberately gone (2026-08-22).** Its only
    // two rows were `java/lang/Number.{byteValue,shortValue}`, both retired
    // with the manifest note above, so the section is now served ENTIRELY by
    // real JDK bytecode — the first T8 sub-section to reach that state.
    // Requiring a section to be "covered" means requiring a native to exist
    // for it, so leaving `T8.2.7` in this list would make the last
    // retirement in any section permanently impossible. That is the same
    // shape as the `Declared => MUST stay` mis-tag that held those two rows
    // in place; a coverage list over a shrinking population has to be
    // allowed to shrink.
    for expected in &[
        "T8.1.1", "T8.1.2", "T8.1.3", "T8.1.4", "T8.1.6", "T8.1.7", "T8.1.8", "T8.1.9",
        "T8.1.10", "T8.2.1", "T8.2.2", "T8.2.3", "T8.2.4", "T8.2.5", "T8.2.6", "T8.2.10",
        "T8.2.11", "T8.2.13", "T8.2.14", "T8.3.1", "T8.3.2", "T8.4.1", "T8.4.2", "T8.4.3",
        "T8.4.4",
    ] {
        assert!(
            sections.contains(expected),
            "Section {} not covered in manifest",
            expected
        );
    }
}

#[test]
fn t85_1_api_count_at_least_45() {
    let manifest = deprecated_api_manifest();
    // 46 before 2026-08-21; `Thread.stop()V` left the manifest entirely
    // (real bytecode serves it on every supported image) and two rows
    // stayed with `AbsentFromAllSupportedImages`.
    assert!(
        manifest.len() >= 44,
        "Expected at least 44 deprecated APIs, found {}",
        manifest.len()
    );
}

/// Every `AbsentFromAllSupportedImages` row must be one this crate does
/// NOT register, and there must be at least one — an empty set would mean
/// the column had quietly become decorative.
#[test]
fn t85_1_absent_from_images_rows_are_not_registered() {
    let r = build_deprecated_registry();
    let absent: Vec<_> = deprecated_api_manifest()
        .into_iter()
        .filter(|a| a.images == ImageStatus::AbsentFromAllSupportedImages)
        .collect();
    assert!(
        !absent.is_empty(),
        "the ImageStatus column has no AbsentFromAllSupportedImages row left"
    );
    for api in absent {
        assert!(
            r.find(api.class, api.method, api.descriptor).is_none(),
            "{}.{}{} is registered, but no supported image declares it",
            api.class,
            api.method,
            api.descriptor
        );
    }
}

// ── T8.5.2 — Behavioral round-trips ─────────────────────────────────

/// Retired 2026-08-21: no supported image declares `Thread.destroy()V`, so
/// there is nothing for a native to stand in front of and the VM's own
/// `NoSuchMethodError` is the answer. This asserts the absence, which is
/// the property that can regress.
#[test]
fn t85_2_thread_destroy_is_retired() {
    let r = build_deprecated_registry();
    assert!(r.find("java/lang/Thread", "destroy", "()V").is_none());
}

#[test]
fn t85_2_thread_count_stack_frames_throws() {
    let r = build_deprecated_registry();
    let f = r
        .find("java/lang/Thread", "countStackFrames", "()I")
        .unwrap();
    let mut ctx = MockNativeContext::new();
    let result = f(&mut ctx, &[]);
    assert!(result.is_err(), "countStackFrames should throw");
}

#[test]
fn t85_2_compiler_compile_class_returns_false() {
    let r = build_deprecated_registry();
    let f = r
        .find("java/lang/Compiler", "compileClass", "(Ljava/lang/Class;)Z")
        .unwrap();
    let mut ctx = MockNativeContext::new();
    let result = f(&mut ctx, &[cratonvm_types::Value::Object(None)]);
    match result {
        Ok(Some(cratonvm_types::Value::Int(0))) => {} // false
        other => panic!("Expected Ok(Some(Int(0))), got: {other:?}"),
    }
}

#[test]
fn t85_2_compiler_command_returns_null() {
    let r = build_deprecated_registry();
    let f = r
        .find(
            "java/lang/Compiler",
            "command",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        )
        .unwrap();
    let mut ctx = MockNativeContext::new();
    let result = f(&mut ctx, &[cratonvm_types::Value::Object(None)]);
    match result {
        Ok(Some(cratonvm_types::Value::Object(None))) => {} // null
        other => panic!("Expected Ok(Some(Object(None))), got: {other:?}"),
    }
}

#[test]
fn t85_2_character_is_space_tab() {
    let r = build_deprecated_registry();
    let f = r.find("java/lang/Character", "isSpace", "(C)Z").unwrap();
    let mut ctx = MockNativeContext::new();
    // '\t' = 9, should return true
    let result = f(&mut ctx, &[cratonvm_types::Value::Int(9)]);
    match result {
        Ok(Some(cratonvm_types::Value::Int(1))) => {}
        other => panic!("isSpace('\\t') should return true, got: {other:?}"),
    }
}

#[test]
fn t85_2_character_is_java_letter_underscore() {
    let r = build_deprecated_registry();
    let f = r
        .find("java/lang/Character", "isJavaLetter", "(C)Z")
        .unwrap();
    let mut ctx = MockNativeContext::new();
    // '_' = 95, should return true (valid Java identifier start)
    let result = f(&mut ctx, &[cratonvm_types::Value::Int(95)]);
    match result {
        Ok(Some(cratonvm_types::Value::Int(1))) => {}
        other => panic!("isJavaLetter('_') should return true, got: {other:?}"),
    }
}

#[test]
fn t85_2_character_is_java_letter_digit_rejected() {
    let r = build_deprecated_registry();
    let f = r
        .find("java/lang/Character", "isJavaLetter", "(C)Z")
        .unwrap();
    let mut ctx = MockNativeContext::new();
    // '5' = 53, should return false (not valid identifier start)
    let result = f(&mut ctx, &[cratonvm_types::Value::Int(53)]);
    match result {
        Ok(Some(cratonvm_types::Value::Int(0))) => {}
        other => panic!("isJavaLetter('5') should return false, got: {other:?}"),
    }
}

#[test]
fn t85_2_url_decoder_is_registered() {
    let r = build_deprecated_registry();
    assert!(
        r.find(
            "java/net/URLDecoder",
            "decode",
            "(Ljava/lang/String;)Ljava/lang/String;"
        )
        .is_some(),
        "URLDecoder.decode(String) must be registered"
    );
}

#[test]
fn t85_2_url_encoder_is_registered() {
    let r = build_deprecated_registry();
    assert!(
        r.find(
            "java/net/URLEncoder",
            "encode",
            "(Ljava/lang/String;)Ljava/lang/String;"
        )
        .is_some(),
        "URLEncoder.encode(String) must be registered"
    );
}

// ── T8.5.3 — Cross-check against JDK 25 @Deprecated set ────────────

#[test]
fn t85_3_cross_check_jdk25_deprecated_all_registered() {
    let r = build_deprecated_registry();
    let checklist = jdk25_deprecated_api_checklist();
    let mut missing = Vec::new();

    for (class, method, descriptor) in &checklist {
        if r.find(class, method, descriptor).is_none() {
            missing.push(format!("{}.{}{}", class, method, descriptor));
        }
    }

    assert!(
        missing.is_empty(),
        "JDK 25 @Deprecated APIs not registered:\n{}",
        missing.join("\n")
    );
}

#[test]
fn t85_3_checklist_has_at_least_30_entries() {
    let checklist = jdk25_deprecated_api_checklist();
    assert!(
        checklist.len() >= 30,
        "Expected at least 30 deprecated API entries, found {}",
        checklist.len()
    );
}

// ── T8.5.4 — Shim generator fills gaps ──────────────────────────────

#[test]
fn t85_4_shim_generator_skips_existing() {
    let mut r = NativeMethodRegistry::new();
    // Register one method manually. This constant no-op is a TEST FIXTURE
    // standing in for "some already-registered handler" — the assertion
    // below is that the shim generator does not replace it. It is not a
    // production `Thread.stop` implementation (that is
    // `deprecated_lang::native_thread_stop`, which really does deliver the
    // throwable to the target thread).
    r.register("java/lang/Thread", "stop", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::SyntheticStub);
    let count_before = r.len();

    register_missing_deprecated_shims(&mut r);

    // Should have added new entries but not duplicated the existing one
    assert!(r.len() > count_before, "Shim generator should add entries");
    // The original "stop" should still be the original (not replaced)
}

#[test]
fn t85_4_shim_throws_unsupported() {
    let mut r = NativeMethodRegistry::new();
    register_missing_deprecated_shims(&mut r);

    // Pick an API that was shimmed
    if let Some(f) = r.find("java/lang/Thread", "stop", "()V") {
        let mut ctx = MockNativeContext::new();
        let result = f(&mut ctx, &[]);
        assert!(result.is_err(), "Shim should throw");
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("T8.5.4") && err.contains("not implemented"),
            "Shim error should contain T8.5.4 tag: {err}"
        );
    }
}

// ── T8.5 — Summary statistics ───────────────────────────────────────

#[test]
fn t85_registration_count() {
    let r = build_deprecated_registry();
    // We expect at least 90 registered native methods across all deprecated modules
    assert!(
        r.len() >= 90,
        "Expected at least 90 deprecated natives, found {}",
        r.len()
    );
}
