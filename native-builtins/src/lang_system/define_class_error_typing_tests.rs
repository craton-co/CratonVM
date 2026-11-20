    use super::*;
    use cratonvm_types::error::{ClassFileError, VmError};

    /// Reproduce EXACTLY what `NativeContextImpl::define_class_full` puts in its
    /// `Err(String)`: `class_manager::define_class_with_options`' `VmError`,
    /// rendered by `.map_err(|e| format!("{e:?}"))` (`vm/src/vm/vm_exec.rs`).
    ///
    /// This is deliberately not a hand-written literal. The recovery in
    /// `typed_define_class_error` is a parse of that rendering, so the coupling
    /// is real and these tests are the thing that notices if either side moves —
    /// in particular if that `map_err` is ever "tidied" to `{e}` (`Display`),
    /// which would silently return every arm below to `ClassFormatError`.
    fn as_backend_string(err: VmError) -> String {
        format!("{err:?}")
    }

    fn linkage_of(failed: &MethodCallFailed) -> &LinkageError {
        match failed {
            MethodCallFailed::InternalError(VmError::Linkage(l)) => l,
            other => panic!("expected a LinkageError, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_class_version_survives_the_string_boundary() {
        // HotSpot 25's wording verbatim, from the W7-31 falsifier. `<Unknown>`
        // is what HotSpot prints when the caller passed a null name, and the
        // version check runs before `this_class` is read — so the empty
        // `class_name` here is faithful, not lost.
        let hotspot = "Preview features are not enabled for <Unknown> (class file version \
                       69.65535). Try running with '--enable-preview'";
        let backend = as_backend_string(VmError::Linkage(
            LinkageError::UnsupportedClassVersionError {
                class_name: String::new(),
                message: hotspot.to_string(),
            },
        ));
        // The shape this test exists to defend against.
        assert!(
            backend.contains("Linkage(UnsupportedClassVersionError {"),
            "the backend rendering changed shape: {backend}"
        );

        let failed = define_class_linkage_error("", "defineClass1", backend);
        match linkage_of(&failed) {
            LinkageError::UnsupportedClassVersionError { message, .. } => {
                assert_eq!(message, hotspot, "the message must be HotSpot's, verbatim");
            }
            other => panic!("must stay an UnsupportedClassVersionError, got {other:?}"),
        }
    }

    /// gc-common w6-f: a `-XX:MaxMetaspaceSize` refusal reaches Java as
    /// HotSpot's `OutOfMemoryError: Metaspace`, not as a `ClassFormatError`.
    #[test]
    fn w6f_metaspace_refusal_is_an_out_of_memory_error() {
        let backend = as_backend_string(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Metaspace".to_string(),
        }));
        match define_class_linkage_error("p/Gen", "defineClass1", backend) {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message,
            })) => assert_eq!(message, "Metaspace"),
            other => panic!("expected OutOfMemoryError: Metaspace, got {other:?}"),
        }
    }

    /// gc-common w11-e: the backend's `DuplicateClassDefinition` (its `Debug`
    /// text is what `define_class_full` hands back) is a duplicate define, as
    /// the older "already defined" wording is; anything else is not.
    #[test]
    fn w11e_both_duplicate_define_spellings_are_recognised() {
        let modern = format!(
            "{:?}",
            VmError::Linkage(LinkageError::DuplicateClassDefinition {
                class_name: "p/C".to_string(),
                loader: "Application".to_string(),
            })
        );
        assert!(is_duplicate_define_message("p/C", &modern));
        assert!(is_duplicate_define_message(
            "p/C",
            "IncompatibleClassChangeError: p/C already defined by application loader"
        ));
        assert!(!is_duplicate_define_message(
            "p/C",
            "ClassFormatError: truncated"
        ));
        assert!(!is_duplicate_define_message("", &modern));
    }

    /// Every `contains("already defined")` in `native-builtins/src` also
    /// accepts `DuplicateClassDefinition` (gc-common w11-e: two sites matched
    /// only the old wording for a month and their recovery arms were dead).
    #[test]
    fn w11e_every_already_defined_match_also_accepts_the_backend_variant() {
        let src_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut stack = vec![std::path::PathBuf::from(src_dir)];
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    if !line.contains(".contains(\"already defined") {
                        continue;
                    }
                    let window = lines[i.saturating_sub(3)..(i + 4).min(lines.len())].join(
                        "
",
                    );
                    if !window.contains("DuplicateClassDefinition") {
                        offenders.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "matches only the old wording: {offenders:?}"
        );
    }

    #[test]
    fn no_recovered_message_carries_a_rust_debug_rendering() {
        for err in [
            VmError::Linkage(LinkageError::UnsupportedClassVersionError {
                class_name: "P".to_string(),
                message: "bad version".to_string(),
            }),
            VmError::Linkage(LinkageError::ClassFormatError {
                class_name: "P".to_string(),
                message: "truncated constant pool".to_string(),
            }),
            VmError::Linkage(LinkageError::IncompatibleClassChangeError {
                message: "already defined by application loader".to_string(),
            }),
            VmError::Linkage(LinkageError::VerifyError {
                class_name: "P".to_string(),
                method_name: "m".to_string(),
                message: "bad stack map".to_string(),
            }),
            VmError::Runtime(RuntimeError::SecurityException {
                message: "Prohibited package name: java.evil".to_string(),
            }),
            VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: "P".to_string(),
                message: "circular class hierarchy detected: P".to_string(),
            }),
        ] {
            let rendered = format!("{err:?}");
            let failed = define_class_linkage_error("P", "defineClass1", rendered.clone());
            // `Display`, not `Debug`: this is the text that becomes the Java
            // exception's message. `Debug` of the *outcome* is a Rust value dump
            // by definition and asserting on it would measure nothing.
            let text = format!("{failed}");
            for artifact in [
                "class_name:",
                "message:",
                "Linkage(",
                "Runtime(",
                "ClassFile(",
            ] {
                assert!(
                    !text.contains(artifact),
                    "a Debug artifact {artifact:?} reached the Java-visible error \
                     for {rendered}: {text}"
                );
            }
        }
    }

    /// JVMS §5.3.5: the loading guard's circular hierarchy leaves the
    /// `defineClass` natives as a `ClassCircularityError`, not the
    /// `ClassFormatError` every unmapped backend shape falls back to.
    #[test]
    fn a_circular_hierarchy_stays_a_class_circularity_error() {
        let backend = as_backend_string(VmError::Linkage(LinkageError::ClassCircularityError {
            class_name: "p/A".to_string(),
        }));
        match linkage_of(&define_class_linkage_error("p/A", "defineClass1", backend)) {
            LinkageError::ClassCircularityError { class_name } => assert_eq!(class_name, "p/A"),
            other => panic!("JVMS §5.3.5 wants a ClassCircularityError, got {other:?}"),
        }
    }

    #[test]
    fn a_prohibited_package_stays_a_security_exception() {
        let backend = as_backend_string(VmError::Runtime(RuntimeError::SecurityException {
            message: "Prohibited package name: java.evil".to_string(),
        }));
        match define_class_linkage_error("java/evil/X", "defineClass1", backend) {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::SecurityException { message },
            )) => {
                assert_eq!(message, "Prohibited package name: java.evil");
            }
            other => panic!("JVMS §5.3.5 wants a SecurityException, got {other:?}"),
        }
    }

    /// A `VmError::ClassFile` returned from a native is UNCATCHABLE —
    /// `classify_fastpath_invoke_error` sends it to `FastPathInvokeError::Fatal`.
    /// Every `ClassFile` arm must therefore leave as a `Linkage` variant.
    #[test]
    fn class_file_errors_are_re_homed_onto_catchable_linkage_variants() {
        for err in [
            VmError::ClassFile(ClassFileError::ClassNotFound {
                class_name: "Missing".to_string(),
            }),
            VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: "P".to_string(),
                message: "circular class hierarchy detected: P".to_string(),
            }),
            VmError::ClassFile(ClassFileError::UnsupportedVersion {
                class_name: "P".to_string(),
                major: 99,
                minor: 0,
            }),
        ] {
            let failed = define_class_linkage_error("P", "defineClass1", format!("{err:?}"));
            assert!(
                matches!(
                    &failed,
                    MethodCallFailed::InternalError(VmError::Linkage(_))
                ),
                "a ClassFile error must be re-homed onto a Linkage variant, got {failed:?}"
            );
        }
        let failed = define_class_linkage_error(
            "P",
            "defineClass1",
            format!(
                "{:?}",
                VmError::ClassFile(ClassFileError::ClassNotFound {
                    class_name: "Missing".to_string(),
                })
            ),
        );
        match linkage_of(&failed) {
            LinkageError::NoClassDefFoundError { class_name } => assert_eq!(class_name, "Missing"),
            other => panic!("an unresolvable supertype is a NoClassDefFoundError, got {other:?}"),
        }
    }

    /// `define_class_full`'s own plain-string failures are not `Debug`
    /// renderings and must pass through as they always did.
    #[test]
    fn a_plain_backend_string_keeps_the_old_class_format_error() {
        let failed = define_class_linkage_error(
            "Foo",
            "defineClass1",
            "define_class_full failed for Foo".to_string(),
        );
        match linkage_of(&failed) {
            LinkageError::ClassFormatError {
                class_name,
                message,
            } => {
                assert_eq!(class_name, "Foo");
                assert_eq!(message, "defineClass1: define_class_full failed for Foo");
            }
            other => panic!("expected the unchanged ClassFormatError fallback, got {other:?}"),
        }
    }

    #[test]
    fn a_quoted_message_round_trips_through_the_debug_escaping() {
        let quoted = "class \"P\" has a \\ in it";
        let backend = as_backend_string(VmError::Linkage(LinkageError::ClassFormatError {
            class_name: "P".to_string(),
            message: quoted.to_string(),
        }));
        match linkage_of(&define_class_linkage_error("P", "defineClass1", backend)) {
            LinkageError::ClassFormatError { message, .. } => assert_eq!(message, quoted),
            other => panic!("expected ClassFormatError, got {other:?}"),
        }
    }

    #[test]
    fn the_backend_name_wins_over_the_callers_when_the_caller_has_none() {
        let backend = as_backend_string(VmError::Linkage(LinkageError::NoClassDefFoundError {
            class_name: "Super".to_string(),
        }));
        match linkage_of(&define_class_linkage_error("", "defineClass1", backend)) {
            LinkageError::NoClassDefFoundError { class_name } => assert_eq!(class_name, "Super"),
            other => panic!("expected NoClassDefFoundError, got {other:?}"),
        }
    }

    #[test]
    fn split_debug_error_rejects_a_human_written_string() {
        assert!(split_debug_error("initialize after define failed for Foo: boom").is_none());
        assert!(split_debug_error("").is_none());
        assert!(split_debug_error("no parens here").is_none());
    }

    #[test]
    fn debug_string_field_only_matches_at_a_field_boundary() {
        let body = r#"class_name: "A", message: "the class_name: \"B\" is wrong""#;
        assert_eq!(debug_string_field(body, "class_name").as_deref(), Some("A"));
        assert_eq!(
            debug_string_field(body, "message").as_deref(),
            Some(r#"the class_name: "B" is wrong"#)
        );
        assert!(debug_string_field(body, "loader").is_none());
    }
