use super::*;

/// Every reason variant, for exhaustive category/format coverage. Adding a
/// variant without adding it here makes `every_category_is_distinct` fail
/// its count assertion, which is the intended tripwire.
fn all_reasons() -> Vec<BailoutReason> {
    vec![
        BailoutReason::GraphTooLarge {
            nodes: 30_000,
            limit: DEFAULT_MAX_NODES,
        },
        BailoutReason::FrameTooLarge {
            bytes: 70_000,
            limit: DEFAULT_MAX_FRAME_BYTES,
        },
        BailoutReason::UnsupportedOpcode { opcode: 0xc5 },
        BailoutReason::UnsupportedShape("multianewarray"),
        BailoutReason::RegisterPressure,
        BailoutReason::DisplacementOutOfRange { disp: 1 << 40 },
        BailoutReason::RelocationOutOfRange { delta: -(1 << 40) },
        BailoutReason::CodeBufferExhausted {
            needed: 9000,
            capacity: 4096,
        },
        BailoutReason::UnallocatedValue { node: 17 },
        BailoutReason::IrVerification("n3 input[0] is out of range".to_string()),
        BailoutReason::DeoptMetadata(
            "safepoint at bci 12: local[0] is MaterializationRequired".to_string(),
        ),
        BailoutReason::Internal("schedule block count mismatch"),
    ]
}

#[test]
fn every_category_is_distinct_and_registered() {
    let reasons = all_reasons();
    assert_eq!(
        reasons.len(),
        CATEGORIES.len(),
        "a BailoutReason variant was added without a category"
    );
    let mut seen: Vec<&'static str> = Vec::new();
    for reason in &reasons {
        let cat = reason.category();
        assert!(
            category_index(cat).is_some(),
            "category {cat} is not in CATEGORIES"
        );
        assert!(!seen.contains(&cat), "duplicate category {cat}");
        seen.push(cat);
    }
    // The order of `all_reasons` mirrors CATEGORIES, so the mapping is
    // positional as well as unique.
    assert_eq!(seen, CATEGORIES.to_vec());
}

/// `record_bailout` indexes `COUNTERS` by the variant's own index rather
/// than by string-searching the name, so the index and the name must be
/// the same fact.
#[test]
fn the_counter_index_is_the_category_name_position() {
    for reason in all_reasons() {
        assert_eq!(
            Some(reason.category_index()),
            category_index(reason.category()),
            "{reason:?}"
        );
    }
}

#[test]
fn display_mentions_category_and_operands() {
    let b = Bailout::new(BailoutReason::GraphTooLarge {
        nodes: 30_000,
        limit: 20_000,
    });
    let s = b.to_string();
    assert!(s.contains("graph_too_large"), "{s}");
    assert!(s.contains("30000"), "{s}");
    assert!(s.contains("20000"), "{s}");
    assert!(b.context.is_none());
}

#[test]
fn display_includes_context_when_present() {
    let b = Bailout::with_context(
        BailoutReason::UnsupportedOpcode { opcode: 0xba },
        "java/lang/String.hashCode()I at pc 12",
    );
    let s = b.to_string();
    assert!(s.contains("unsupported_opcode"), "{s}");
    assert!(s.contains("0xba"), "{s}");
    assert!(s.contains("hashCode"), "{s}");
}

#[test]
fn every_reason_formats_non_empty() {
    for reason in all_reasons() {
        let rendered = Bailout::new(reason.clone()).to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains(reason.category()),
            "{rendered} lacks its category"
        );
    }
}

/// Counters are process-wide and tests share a process, so this asserts on
/// the *delta* rather than an absolute value.
#[test]
fn record_bailout_increments_its_category_only() {
    let before = bailout_counts();
    let b = Bailout::new(BailoutReason::RegisterPressure);
    record_bailout(&b);
    record_bailout(&b);
    let after = bailout_counts();
    assert_eq!(before.len(), after.len());
    for ((name_b, count_b), (name_a, count_a)) in before.iter().zip(after.iter()) {
        assert_eq!(name_b, name_a);
        if *name_a == "register_pressure" {
            assert!(
                count_a - count_b >= 2,
                "register_pressure did not advance: {count_b} → {count_a}"
            );
        }
    }
    assert!(bailout_count("register_pressure").is_some());
    assert!(bailout_count("no_such_category").is_none());
}

/// The deopt-metadata reason is a *distinct* bucket from `ir_verification`,
/// not an alias for it: `deopt.rs` used to spell it as an `IrVerification`
/// with a `phase=deopt-metadata` context, and a metrics consumer could not
/// separate a bad graph from a badly-described good one.
#[test]
fn deopt_metadata_is_its_own_category_and_counter() {
    let b = Bailout::with_context(
        BailoutReason::DeoptMetadata("2 deopt-metadata violation(s): …".to_string()),
        "phase=install",
    );
    assert_eq!(b.category(), "deopt_metadata");
    assert_ne!(
        b.category(),
        BailoutReason::IrVerification(String::new()).category()
    );

    let s = b.to_string();
    assert!(s.contains("deopt_metadata"), "{s}");
    assert!(s.contains("deopt metadata verification failed"), "{s}");
    assert!(s.contains("2 deopt-metadata violation(s)"), "{s}");
    assert!(s.contains("phase=install"), "{s}");

    // It has a counter of its own. Asserted as a delta, not an absolute:
    // the table is process-wide and shared with every other test.
    let before = bailout_count("deopt_metadata").expect("registered category");
    record_bailout(&b);
    assert!(bailout_count("deopt_metadata").unwrap() - before >= 1);
}

/// The positional `category()` → [`CATEGORIES`] mapping is what
/// `every_category_is_distinct_and_registered` proves in aggregate; this
/// pins the two indices that moved when `deopt_metadata` was inserted
/// *before* `internal` rather than appended.
#[test]
fn inserting_deopt_metadata_kept_internal_last() {
    assert_eq!(CATEGORIES[9], "ir_verification");
    assert_eq!(CATEGORIES[10], "deopt_metadata");
    assert_eq!(CATEGORIES[11], "internal");
    assert_eq!(CATEGORIES.len(), COUNTERS.len());
    assert_eq!(*CATEGORIES.last().unwrap(), "internal");
}

#[test]
fn bailout_counts_reports_every_category() {
    let names: Vec<&str> = bailout_counts().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, CATEGORIES.to_vec());
}

#[test]
fn bail_compile_returns_err_without_context() {
    fn f() -> CompileResult<u32> {
        bail_compile!(BailoutReason::RegisterPressure);
    }
    let err = f().unwrap_err();
    assert_eq!(err.reason, BailoutReason::RegisterPressure);
    assert_eq!(err.context, None);
}

#[test]
fn bail_compile_formats_context() {
    fn f(pc: usize) -> CompileResult<u32> {
        bail_compile!(
            BailoutReason::UnsupportedOpcode { opcode: 0xc4 },
            "at pc {pc}"
        );
    }
    let err = f(42).unwrap_err();
    assert_eq!(err.category(), "unsupported_opcode");
    assert_eq!(err.context.as_deref(), Some("at pc 42"));
}

#[test]
fn bail_compile_allows_early_return_on_success_path() {
    fn f(n: usize) -> CompileResult<usize> {
        if n > DEFAULT_MAX_NODES {
            bail_compile!(
                BailoutReason::GraphTooLarge {
                    nodes: n,
                    limit: DEFAULT_MAX_NODES,
                },
                "graph of {n}"
            );
        }
        Ok(n)
    }
    assert_eq!(f(10).unwrap(), 10);
    assert_eq!(
        f(DEFAULT_MAX_NODES + 1).unwrap_err().category(),
        "graph_too_large"
    );
}

/// `DEFAULT_MAX_NODES` is an alias, not a copy — this pins that.
#[test]
fn default_limits_match_the_compiler_they_describe() {
    assert_eq!(DEFAULT_MAX_NODES, crate::ir::IR_MAX_GRAPH_NODES);
    // Every frame slot offset must fit `ir_lower`'s i16 oop-map encoding.
    assert_eq!(DEFAULT_MAX_FRAME_BYTES, 32_768);
    assert!(DEFAULT_MAX_FRAME_BYTES - 1 <= i16::MAX as usize);
}

#[test]
fn bailout_is_a_std_error() {
    fn as_error(b: Bailout) -> Box<dyn std::error::Error> {
        Box::new(b)
    }
    let e = as_error(Bailout::new(BailoutReason::Internal("x")));
    assert!(e.to_string().contains("internal"));
}
