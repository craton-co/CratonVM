use super::*;

#[test]
fn graduated_rows_come_back_whatever_the_set_says() {
    let none = HotSpotAlignments::NONE;
    assert_eq!(
        none.graduated_rows_turned_off(),
        vec!["jdk-fork-join-pool", "jdk-streams"]
    );
    let back = none.with_graduated_rows();
    assert!(back.contains(HotSpotAlignment::JdkForkJoinPool));
    assert!(back.contains(HotSpotAlignment::JdkStreams));
    assert!(back.graduated_rows_turned_off().is_empty());
    // Only the graduated rows move.
    assert!(!back.contains(HotSpotAlignment::JdkStackWalker));
    let reverted = HotSpotAlignments::defaults().edited("-jdk-streams,-jdk-stack-walker");
    let fixed = reverted.with_graduated_rows();
    assert!(fixed.contains(HotSpotAlignment::JdkStreams));
    assert!(!fixed.contains(HotSpotAlignment::JdkStackWalker));
    assert_eq!(
        HotSpotAlignments::defaults().with_graduated_rows(),
        HotSpotAlignments::defaults()
    );
}

#[test]
fn tokens_round_trip_and_bits_are_distinct() {
    let mut seen = 0u128;
    for (i, a) in HotSpotAlignment::ALL.into_iter().enumerate() {
        assert_eq!(a as usize, i, "ALL is in bit order");
        assert_eq!(HotSpotAlignment::from_token(a.token()), Some(a));
        assert_eq!(seen & a.bit(), 0);
        seen |= a.bit();
    }
}

#[test]
fn the_list_edits_left_to_right() {
    let d = HotSpotAlignments::defaults();
    assert!(d.contains(HotSpotAlignment::LoaderThrows));
    let e = d.edited("-loader-throws, +catch-types-at-link");
    assert!(!e.contains(HotSpotAlignment::LoaderThrows));
    assert!(e.contains(HotSpotAlignment::CatchTypesAtLink));
    assert_eq!(d.edited("none"), HotSpotAlignments::NONE);
    assert_eq!(d.edited("none,all"), HotSpotAlignments::all());
    assert_eq!(d.edited("1"), HotSpotAlignments::all());
    assert_eq!(d.edited("0"), HotSpotAlignments::NONE);
    let one = d.edited("none,namespaced-array-types");
    assert_eq!(one.tokens(), vec!["namespaced-array-types"]);
    assert_eq!(d.edited("no-such-row"), d);
}

#[test]
fn the_census_line_counts_marks_uncounted_and_off_rows() {
    let census = AlignmentCensus::new();
    census.note(HotSpotAlignment::RuntimeImageNames);
    census.note(HotSpotAlignment::RuntimeImageNames);
    let set = HotSpotAlignments::defaults().with(HotSpotAlignment::LoaderThrows, false);
    let line = census.census_line(CompatibilityMode::Compatible, set);
    assert!(line.starts_with("mode=compatible "), "{line}");
    assert!(line.contains(" runtime-image-names=2"), "{line}");
    assert!(line.contains(" loader-throws=0/off"), "{line}");
    assert!(line.contains(" module-messages=?"), "{line}");
    let jdk = census.census_line(CompatibilityMode::JdkOnly, HotSpotAlignments::NONE);
    assert!(jdk.starts_with("mode=jdk-only "), "{jdk}");
    assert!(!jdk.contains("/off"), "{jdk}");
}

/// nio20 wave 5 (L1): the server row is real-Real-JDK and reachable by its
/// token; default ON since nio21 wave 13 (L4).
#[test]
fn jdk_http_server_is_a_default_on_real_jdk_row() {
    let a = HotSpotAlignment::JdkHttpServer;
    assert!(a.default_on());
    assert!(a.needs_real_jdk());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults()
        .edited("-jdk-http-server")
        .contains(a));
    assert_eq!(HotSpotAlignment::from_token("jdk-http-server"), Some(a));
}

/// nio21 wave 1 (L4): the `ConcurrentHashMap` / `Properties` row is
/// real-Real-JDK, takes bit 64 (dev's interpreter round i3 wave 76 row of
/// the same token, which the nio21 dev merge kept), and is reachable by its token in both
/// directions; on by default since nio21 wave 4 (L1).
#[test]
fn jdk_concurrent_hash_map_is_a_default_on_real_jdk_row() {
    let a = HotSpotAlignment::JdkConcurrentHashMap;
    assert_eq!(a as usize, 64);
    assert!(a.default_on());
    assert!(a.needs_real_jdk());
    assert!(!a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    let off = HotSpotAlignments::defaults().edited("-jdk-concurrent-hash-map");
    assert!(!off.contains(a));
    assert!(off.edited("+jdk-concurrent-hash-map").contains(a));
    assert_eq!(
        HotSpotAlignment::from_token("jdk-concurrent-hash-map"),
        Some(a)
    );
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    // Its sibling, the scalar atomics: bit 67, on by default since nio21
    // wave 3 (L1), and its token still reverts it.
    let b = HotSpotAlignment::JdkScalarAtomics;
    assert_eq!(b as usize, 67);
    assert!(b.default_on() && b.needs_real_jdk() && !b.counted());
    assert!(HotSpotAlignments::defaults().contains(b));
    assert!(!HotSpotAlignments::defaults()
        .edited("-jdk-scalar-atomics")
        .contains(b));
    assert_eq!(HotSpotAlignment::from_token("jdk-scalar-atomics"), Some(b));
}

/// nio21 wave 2 (L4): the six `HttpClient`-path families take bits 68,
/// 69, 66 (dev's row of the same token), 70, 71, 72 since the nio21 dev
/// merge
/// (past a `u64`, so the set's width is part of the test), are on by
/// default, need a real JDK, and each reverts alone by its token.
#[test]
fn r21w2_http_path_family_rows_are_on_and_revert_alone() {
    let rows = [
        (HotSpotAlignment::JdkTreeMap, 68, "jdk-tree-map"),
        (
            HotSpotAlignment::JdkLinkedCollections,
            69,
            "jdk-linked-collections",
        ),
        (
            HotSpotAlignment::JdkCompletableFuture,
            66,
            "jdk-completable-future",
        ),
        (HotSpotAlignment::JdkTimeUnit, 70, "jdk-time-unit"),
        (HotSpotAlignment::JdkAdders, 71, "jdk-adders"),
        (
            HotSpotAlignment::JdkCarrierThreadLocals,
            72,
            "jdk-carrier-thread-locals",
        ),
    ];
    let d = HotSpotAlignments::defaults();
    for (a, bit, token) in rows {
        assert_eq!(a as usize, bit, "{token}");
        assert_eq!(a.token(), token);
        assert_eq!(HotSpotAlignment::from_token(token), Some(a));
        assert!(
            a.default_on() && a.needs_real_jdk() && !a.counted(),
            "{token}"
        );
        assert!(d.contains(a), "{token}");
        let off = d.edited(&format!("-{token}"));
        assert!(!off.contains(a), "{token}");
        // Only that row moved.
        for (b, _, other) in rows {
            if b != a {
                assert!(off.contains(b), "-{token} turned {other} off");
            }
        }
        assert!(
            off.contains(HotSpotAlignment::JdkScalarAtomics)
                == d.contains(HotSpotAlignment::JdkScalarAtomics)
        );
    }
    // The census has a slot for every row.
    let census = AlignmentCensus::new();
    census.note(HotSpotAlignment::JdkCarrierThreadLocals);
    assert_eq!(census.count(HotSpotAlignment::JdkCarrierThreadLocals), 1);
    assert!(HotSpotAlignments::all().contains(HotSpotAlignment::JdkCarrierThreadLocals));
    assert!(!HotSpotAlignments::NONE.contains(HotSpotAlignment::JdkTreeMap));
}

/// nio21 wave 3 (L1): four more `HttpClient`-path rows take bits 73-76,
/// and wave 4 (L1) the `SharedSecrets` getter bit 77 (since the nio21 dev merge); all are on by
/// default since wave 4, need a real JDK, are registration-time, and each
/// toggles alone by its token.
#[test]
fn r21w3_http_path_rows_take_bits_66_to_69() {
    let rows = [
        (
            HotSpotAlignment::JdkThreadInterrupt,
            73,
            "jdk-thread-interrupt",
            true,
        ),
        (
            HotSpotAlignment::JdkUnsafeDelegations,
            74,
            "jdk-unsafe-delegations",
            true,
        ),
        (HotSpotAlignment::JdkUri, 75, "jdk-uri", true),
        (
            HotSpotAlignment::JdkByteArrayStreams,
            76,
            "jdk-byte-array-streams",
            true,
        ),
        (
            HotSpotAlignment::JdkSharedSecrets,
            77,
            "jdk-shared-secrets",
            true,
        ),
        (
            HotSpotAlignment::JdkSunMiscUnsafe,
            81,
            "jdk-sun-misc-unsafe",
            true,
        ),
        (HotSpotAlignment::JdkForeign, 82, "jdk-foreign", true),
        (
            HotSpotAlignment::JdkAtomicArrays,
            83,
            "jdk-atomic-arrays",
            true,
        ),
        (
            HotSpotAlignment::JdkJavacFileManager,
            84,
            "jdk-javac-file-manager",
            true,
        ),
    ];
    let d = HotSpotAlignments::defaults();
    for (a, bit, token, on) in rows {
        assert_eq!(a as usize, bit, "{token}");
        assert_eq!(HotSpotAlignment::ALL[bit], a, "{token}");
        assert_eq!(HotSpotAlignment::from_token(token), Some(a));
        assert_eq!(a.default_on(), on, "{token}");
        assert!(a.needs_real_jdk() && !a.counted(), "{token}");
        assert_eq!(d.contains(a), on, "{token}");
        let flipped = d.edited(&format!("{}{token}", if on { "-" } else { "+" }));
        assert_eq!(flipped.contains(a), !on, "{token}");
        for (b, _, other, _) in rows {
            if b != a {
                assert_eq!(flipped.contains(b), d.contains(b), "{token} moved {other}");
            }
        }
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    }
    // 81 since the interpreter round i3 waves 81-83 dev merge
    // (`JdkArchiveFs` is bit 79, `JdkThreadPools` bit 80); 82 with
    // `JdkSunMiscUnsafe` (bit 81, nio21 wave 13); 84 with `JdkAtomicArrays` (bit 83);
    // 85 with `JdkJavacFileManager` (bit 84, JIT round 20 wave 14, lane cjavac);
    // 86 with `JdkForeignSegments` (bit 85, JIT round 20 wave 19, lane
    // yCompatSeg). The round-20 rows moved up one bit at the dev merge.
    // 90 with the four L4 check families (bits 86-89, interpreter round
    // i6 wave 118, lane L4).
    // 93 with the three wave-49 registration-time switches (bits 90-92,
    // interpreter round i6 wave 118, lane L5).
    // 94 with `JdkHashtable` (bit 93, interpreter round i6 wave 119, lane L5).
    // 95 with `JdkLogging` (bit 94, interpreter round i6 wave 120, lane L5).
    // 96 with `JdkStringGetBytes` (bit 95, interpreter round i6 wave 122,
    // lane L1).
    // 97 with `JdkMemoryUsage` (bit 96, interpreter round i6 wave 123,
    // lane L1).
    // 98 with `JdkManagementBeans` (bit 97, interpreter round i6 wave 124,
    // lane L1).
    // 99 with `JdkTypeVariableImpl` (bit 98, interpreter round i6 wave
    // 125, lane L1).
    // 100 with `JdkPriorityQueue` (bit 99, interpreter round i6 wave 133,
    // lane L6).
    // 101 with `JdkVmInitLevel` (bit 100, JIT round j27 wave 5, lane
    // VMALIGN).
    assert_eq!(HotSpotAlignment::ALL.len(), 101);
}

/// JIT round j27 wave 5, lane VMALIGN: the `VM.initLevel` / `isBooted`
/// row is bit 100, default ON, a real-JDK row, uncounted, with no legacy
/// variable (its token reverts it alone), not graduated, and in force
/// under `--real-jdk`.
#[test]
fn jdk_vm_init_level_is_a_default_on_real_jdk_row_at_bit_100() {
    let a = HotSpotAlignment::JdkVmInitLevel;
    assert_eq!(a as usize, 100);
    assert_eq!(HotSpotAlignment::ALL[100], a);
    assert_eq!(a.token(), "jdk-vm-init-level");
    assert_eq!(HotSpotAlignment::from_token("jdk-vm-init-level"), Some(a));
    assert_eq!(a.legacy_key(), None);
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(!a.graduated());
    let d = HotSpotAlignments::defaults();
    assert!(d.contains(a));
    let off = d.edited("-jdk-vm-init-level");
    assert!(!off.contains(a));
    assert_eq!(off.with(a, true), d, "only this row moves");
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    assert!(!off.aligned(CompatibilityMode::Compatible, a));
}

/// Interpreter round i6 wave 133, lane L6: the `PriorityQueue` family row
/// is bit 99, default ON (228-flag flip), a real-JDK row, uncounted, turned on by its own
/// variable or its token, in force under `--real-jdk`.
#[test]
fn jdk_priority_queue_is_a_default_on_real_jdk_row_at_bit_99() {
    let a = HotSpotAlignment::JdkPriorityQueue;
    assert_eq!(a as usize, 99);
    assert_eq!(HotSpotAlignment::ALL[99], a);
    assert_eq!(HotSpotAlignment::from_token("jdk-priority-queue"), Some(a));
    assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_PRIORITY_QUEUE"));
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(!a.graduated());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults().edited("-jdk-priority-queue").contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
}

/// Interpreter round i6 wave 125, lane L1: the `TypeVariableImpl` family
/// row is bit 98, default ON (228-flag flip), a real-JDK row, uncounted, turned on by its
/// own variable or its token, in force under `--real-jdk`.
#[test]
fn jdk_type_variable_impl_is_a_default_on_real_jdk_row_at_bit_98() {
    let a = HotSpotAlignment::JdkTypeVariableImpl;
    assert_eq!(a as usize, 98);
    assert_eq!(HotSpotAlignment::ALL[98], a);
    assert_eq!(HotSpotAlignment::from_token("jdk-type-variable-impl"), Some(a));
    assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL"));
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults().edited("-jdk-type-variable-impl").contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
}

/// Interpreter round i6 wave 124, lane L1: the `sun.management` memory
/// beans' family row is bit 97, default ON (228-flag flip), a real-JDK row, uncounted,
/// turned on by its own variable or its token, in force under `--real-jdk`.
#[test]
fn jdk_management_beans_is_a_default_on_real_jdk_row_at_bit_97() {
    let a = HotSpotAlignment::JdkManagementBeans;
    assert_eq!(a as usize, 97);
    assert_eq!(HotSpotAlignment::ALL[97], a);
    assert_eq!(HotSpotAlignment::from_token("jdk-management-beans"), Some(a));
    assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS"));
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults().edited("-jdk-management-beans").contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
}

/// Interpreter round i6 wave 123, lane L1: the `MemoryUsage` family row
/// is bit 96, default ON (228-flag flip), a real-JDK row, uncounted, turned on by its own
/// variable or its token, in force under `--real-jdk`.
#[test]
fn jdk_memory_usage_is_a_default_on_real_jdk_row_at_bit_96() {
    let a = HotSpotAlignment::JdkMemoryUsage;
    assert_eq!(a as usize, 96);
    assert_eq!(HotSpotAlignment::ALL[96], a);
    assert_eq!(HotSpotAlignment::from_token("jdk-memory-usage"), Some(a));
    assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_MEMORY_USAGE"));
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults().edited("-jdk-memory-usage").contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
}

/// Interpreter round i6 wave 122, lane L1: the `String.getBytes` family
/// row is bit 95, default ON (228-flag flip), a real-JDK row, uncounted, turned on by its
/// own variable or its token, in force under `--real-jdk`.
#[test]
fn jdk_string_get_bytes_is_a_default_on_real_jdk_row_at_bit_95() {
    let a = HotSpotAlignment::JdkStringGetBytes;
    assert_eq!(a as usize, 95);
    assert_eq!(HotSpotAlignment::ALL[95], a);
    assert_eq!(HotSpotAlignment::from_token("jdk-string-get-bytes"), Some(a));
    assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_STRING_GET_BYTES"));
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults().edited("-jdk-string-get-bytes").contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
}

/// Interpreter round i6 wave 120, lane L5: the `java.util.logging` family
/// row is bit 94, default ON (228-flag flip), a real-JDK row, uncounted, turned on by its
/// own variable or its token, in force under `--real-jdk`.
#[test]
fn jdk_logging_is_a_default_on_real_jdk_row_at_bit_94() {
    let a = HotSpotAlignment::JdkLogging;
    assert_eq!(a as usize, 94);
    assert_eq!(HotSpotAlignment::ALL[94], a);
    assert_eq!(HotSpotAlignment::from_token("jdk-logging"), Some(a));
    assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_LOGGING"));
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults().edited("-jdk-logging").contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
}

/// Interpreter round i6 wave 119, lane L5: the `Hashtable` family row is
/// bit 93, default ON since interpreter round i7 wave 144 (A/B w142lto),
/// a real-JDK row, uncounted, turned off by its own variable (`=0`) or its
/// token, and in force under `--real-jdk` whatever the set says.
#[test]
fn jdk_hashtable_is_a_default_on_real_jdk_row_at_bit_93() {
    let a = HotSpotAlignment::JdkHashtable;
    assert_eq!(a as usize, 93);
    assert_eq!(HotSpotAlignment::ALL[93], a);
    assert_eq!(HotSpotAlignment::from_token("jdk-hashtable"), Some(a));
    assert_eq!(a.legacy_key(), Some("CRATONVM_COMPAT_JDK_HASHTABLE"));
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults().edited("-jdk-hashtable").contains(a));
    assert!(!HotSpotAlignments::from_lookup(|k| {
        (k == "CRATONVM_COMPAT_JDK_HASHTABLE").then(|| "0".to_string())
    })
    .contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
}

/// Interpreter round i6 wave 118, lane L4 (`i50-L4-proposal-a-per-vm-table-of-hotspot-checks`):
/// the four check families are rows 86-89, on by default, not real-JDK
/// rows, uncounted, in force under `--real-jdk` whatever the set says, and
/// each one's old `*_EVERY_MODE` variable is its legacy key (`=0` turns
/// that row alone off).
#[test]
fn the_four_l4_check_families_are_rows_86_to_89() {
    let rows = [
        (HotSpotAlignment::IndyLinkChecks, 86, "indy-link-checks", "CRATONVM_INDY_CHECKS_EVERY_MODE"),
        (HotSpotAlignment::SelectionChecks, 87, "selection-checks", "CRATONVM_SELECTION_CHECKS_EVERY_MODE"),
        (
            HotSpotAlignment::MhCombinatorChecks,
            88,
            "mh-combinator-checks",
            "CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE",
        ),
        (
            HotSpotAlignment::LdcHandleAccessChecks,
            89,
            "ldc-handle-access-checks",
            "CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE",
        ),
    ];
    let d = HotSpotAlignments::defaults();
    for (a, bit, token, key) in rows {
        assert_eq!(a as usize, bit, "{token}");
        assert_eq!(HotSpotAlignment::ALL[bit], a, "{token}");
        assert_eq!(HotSpotAlignment::from_token(token), Some(a));
        assert_eq!(a.legacy_key(), Some(key), "{token}");
        assert!(a.default_on() && !a.needs_real_jdk() && !a.counted(), "{token}");
        assert!(d.contains(a), "{token}");
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
        let only = move |k: &str| (k == key).then(|| "0".to_string());
        let off = HotSpotAlignments::from_lookup(only);
        assert!(!off.contains(a), "{token}");
        for (b, _, other, _) in rows {
            if b != a {
                assert!(off.contains(b), "{key}=0 moved {other}");
            }
        }
    }
}

/// JIT round 20 wave 19 (lane yCompatSeg, MEM20-3): the pointer-segment
/// row takes bit 85, is on by default, needs a real JDK, is COUNTED (a
/// call-time row), reverts alone by its token, and is in force under
/// `--real-jdk` whatever the set says.
#[test]
fn jdk_foreign_segments_is_a_default_on_counted_row_at_bit_85() {
    let a = HotSpotAlignment::JdkForeignSegments;
    assert_eq!(a as usize, 85);
    assert_eq!(HotSpotAlignment::ALL[85], a);
    assert_eq!(
        HotSpotAlignment::from_token("jdk-foreign-segments"),
        Some(a)
    );
    assert!(a.default_on() && a.needs_real_jdk() && a.counted());
    let d = HotSpotAlignments::defaults();
    assert!(d.contains(a) && d.contains(HotSpotAlignment::JdkForeign));
    let off = d.edited("-jdk-foreign-segments");
    assert!(!off.contains(a));
    assert!(
        off.contains(HotSpotAlignment::JdkForeign),
        "-jdk-foreign-segments moved jdk-foreign"
    );
    assert!(
        d.edited("-jdk-foreign").contains(a),
        "-jdk-foreign moved jdk-foreign-segments"
    );
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    assert!(d.aligned(CompatibilityMode::Compatible, a));
}

/// nio21 wave 12 (L3): the `java.lang.invoke` row takes bit 78 (75 before
/// the nio21 dev merge), is opt-in,
/// needs a real JDK, is registration-time, and toggles by its token (default ON since 2026-10-09).
#[test]
fn jdk_invoke_is_a_default_on_real_jdk_row_at_bit_78() {
    let a = HotSpotAlignment::JdkInvoke;
    assert_eq!(a as usize, 78);
    assert_eq!(HotSpotAlignment::ALL[78], a);
    assert!(a.default_on() && a.needs_real_jdk() && !a.counted());
    assert!(HotSpotAlignments::defaults().contains(a));
    assert!(!HotSpotAlignments::defaults()
        .edited("-jdk-invoke")
        .contains(a));
    assert_eq!(HotSpotAlignment::from_token("jdk-invoke"), Some(a));
    assert!(HotSpotAlignments::all().contains(a));
    assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
    assert!(HotSpotAlignments::defaults().aligned(CompatibilityMode::Compatible, a));
}

#[test]
fn jdk_only_has_every_row() {
    for a in HotSpotAlignment::ALL {
        assert!(HotSpotAlignments::NONE.aligned(CompatibilityMode::JdkOnly, a));
        assert!(!HotSpotAlignments::NONE.aligned(CompatibilityMode::Compatible, a));
    }
}

#[test]
fn a_legacy_variable_still_turns_its_row_off() {
    // A plain lookup, not `with_thread_overrides`: the flags module's own
    // tests count live overrides process-wide.
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let set = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_LOADER_THROWS", "0"));
    assert!(!set.contains(HotSpotAlignment::LoaderThrows));
    assert!(set.contains(HotSpotAlignment::LoaderConstraints));
    let set = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-loader-constraints"));
    assert!(!set.contains(HotSpotAlignment::LoaderConstraints));
    // The list is read after the legacy variables.
    let set = HotSpotAlignments::from_lookup(|key: &str| match key {
        "CRATONVM_COMPAT_LOADER_THROWS" => Some("0".to_string()),
        COMPAT_ALIGN_KEY => Some("+loader-throws".to_string()),
        _ => None,
    });
    assert!(set.contains(HotSpotAlignment::LoaderThrows));
}

/// Interpreter round i2 wave 69, lane L1: the `LockSupport` row is on by
/// default, needs a real JDK, and its own variable is its `=0` kill switch.
#[test]
fn the_lock_support_row_is_on_and_has_a_kill_switch() {
    let row = HotSpotAlignment::JdkLockSupport;
    assert!(HotSpotAlignments::defaults().contains(row));
    assert!(row.needs_real_jdk());
    assert_eq!(HotSpotAlignment::from_token("jdk-lock-support"), Some(row));
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_LOCK_SUPPORT", "0"));
    assert!(!off.contains(row));
    assert!(off.contains(HotSpotAlignment::JdkPreconditions));
    let off = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-jdk-lock-support"));
    assert!(!off.contains(row));
}

/// Interpreter round i2 wave 70, lane L1: the `StampedLock` row is on by
/// default, needs a real JDK, and its own variable is its `=0` kill switch.
#[test]
fn the_stamped_lock_row_is_on_and_has_a_kill_switch() {
    let row = HotSpotAlignment::JdkStampedLock;
    assert_eq!(row as usize, 43);
    assert!(HotSpotAlignments::defaults().contains(row));
    assert!(row.needs_real_jdk());
    assert_eq!(HotSpotAlignment::from_token("jdk-stamped-lock"), Some(row));
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_STAMPED_LOCK", "0"));
    assert!(!off.contains(row));
    assert!(off.contains(HotSpotAlignment::JdkLockSupport));
    let off = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-jdk-stamped-lock"));
    assert!(!off.contains(row));
    assert!(off.contains(HotSpotAlignment::JdkLockSupport));
}

/// Interpreter round i3 wave 71, lane L1: the `CountDownLatch` row needs a
/// real JDK, its own variable turns it on (`1`) and off (`0`) whatever the
/// default, and so does a `CRATONVM_COMPAT_ALIGN` item; neither touches
/// the neighbouring lock rows. (No assertion on the default itself.)
#[test]
fn the_count_down_latch_row_has_its_own_switch() {
    let row = HotSpotAlignment::JdkCountDownLatch;
    assert_eq!(row as usize, 59);
    assert!(row.needs_real_jdk());
    assert!(!row.counted());
    assert_eq!(
        HotSpotAlignment::from_token("jdk-count-down-latch"),
        Some(row)
    );
    assert_eq!(
        row.legacy_key(),
        Some("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH")
    );
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let on = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH", "1"));
    assert!(on.contains(row));
    let off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH", "0"));
    assert!(!off.contains(row));
    assert_eq!(
        off.contains(HotSpotAlignment::JdkStampedLock),
        HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkStampedLock)
    );
    let on = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "+jdk-count-down-latch"));
    assert!(on.contains(row));
    let off = HotSpotAlignments::from_lookup(only(COMPAT_ALIGN_KEY, "-jdk-count-down-latch"));
    assert!(!off.contains(row));
}

/// Interpreter round i3 wave 72, lane L1: the `AbstractQueuedLongSynchronizer`
/// and `CopyOnWriteArrayList` rows need a real JDK, are registration-time,
/// and each has its own variable (`1` on, `0` off, whatever the default)
/// and token; neither variable touches the other row or the latch row.
/// (No assertion on either default.)
#[test]
fn the_aqls_and_copy_on_write_rows_have_their_own_switches() {
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    for (row, bit, token, key) in [
        (
            HotSpotAlignment::JdkAqls,
            60usize,
            "jdk-aqls",
            "CRATONVM_COMPAT_JDK_AQLS",
        ),
        (
            HotSpotAlignment::JdkCopyOnWrite,
            61usize,
            "jdk-copy-on-write",
            "CRATONVM_COMPAT_JDK_COPY_ON_WRITE",
        ),
    ] {
        assert_eq!(row as usize, bit);
        assert!(row.needs_real_jdk());
        assert!(!row.counted());
        assert_eq!(HotSpotAlignment::from_token(token), Some(row));
        assert_eq!(row.legacy_key(), Some(key));
        assert!(HotSpotAlignments::from_lookup(only(key, "1")).contains(row));
        let off = HotSpotAlignments::from_lookup(only(key, "0"));
        assert!(!off.contains(row));
        assert_eq!(
            off.contains(HotSpotAlignment::JdkCountDownLatch),
            HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkCountDownLatch)
        );
        let plus = format!("+{token}");
        let minus = format!("-{token}");
        assert!(HotSpotAlignments::NONE.edited(&plus).contains(row));
        assert!(!HotSpotAlignments::all().edited(&minus).contains(row));
    }
    let aqls_off = HotSpotAlignments::from_lookup(only("CRATONVM_COMPAT_JDK_AQLS", "0"));
    assert_eq!(
        aqls_off.contains(HotSpotAlignment::JdkCopyOnWrite),
        HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkCopyOnWrite)
    );
}

/// Interpreter round i3 wave 74, lane L1: the `CopyOnWriteArraySet` row is
/// bit 62, needs a real JDK, is registration-time, and has its own
/// variable (`1` on, `0` off, whatever the default) and token; neither
/// touches the `CopyOnWriteArrayList` row, nor that row's variable this
/// one. (No assertion on either default.)
#[test]
fn the_copy_on_write_set_row_has_its_own_switch() {
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let row = HotSpotAlignment::JdkCopyOnWriteArraySet;
    let key = "CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET";
    assert_eq!(row as usize, 62);
    assert!(row.needs_real_jdk());
    assert!(!row.counted());
    assert_eq!(
        HotSpotAlignment::from_token("jdk-copy-on-write-set"),
        Some(row)
    );
    assert_eq!(row.legacy_key(), Some(key));
    assert!(HotSpotAlignments::from_lookup(only(key, "1")).contains(row));
    let off = HotSpotAlignments::from_lookup(only(key, "0"));
    assert!(!off.contains(row));
    assert_eq!(
        off.contains(HotSpotAlignment::JdkCopyOnWrite),
        HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkCopyOnWrite)
    );
    let list_key = "CRATONVM_COMPAT_JDK_COPY_ON_WRITE";
    let list_on = HotSpotAlignments::from_lookup(only(list_key, "1"));
    assert_eq!(
        list_on.contains(row),
        HotSpotAlignments::defaults().contains(row)
    );
    assert!(HotSpotAlignments::NONE
        .edited("+jdk-copy-on-write-set")
        .contains(row));
    assert!(!HotSpotAlignments::all()
        .edited("-jdk-copy-on-write-set")
        .contains(row));
    // `jdk-copy-on-write` is a prefix of this token, not an alias of it.
    assert!(!HotSpotAlignments::NONE
        .edited("+jdk-copy-on-write")
        .contains(row));
}

/// Interpreter round i3 wave 75, lane L1: the `PriorityBlockingQueue` row
/// is bit 63 -- the top bit of the old `u64` set, so `all()` / `NONE` and
/// the edits must reach it without a wrapped shift -- needs a real JDK, is
/// registration-time, and has its own variable (`1` on, `0` off, whatever
/// the default) and token; it moves no other row. (No assertion on its
/// default.) Wave 76 widened the set to a `u128`; the row keeps bit 63.
#[test]
fn the_priority_blocking_queue_row_is_the_top_bit_and_has_its_own_switch() {
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let row = HotSpotAlignment::JdkPriorityBlockingQueue;
    let key = "CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE";
    assert_eq!(row as usize, 63);
    assert!(HotSpotAlignment::ALL.into_iter().count() >= 64);
    assert_eq!(row.bit(), 1u128 << 63);
    assert!(row.needs_real_jdk());
    assert!(!row.counted());
    assert_eq!(
        HotSpotAlignment::from_token("jdk-priority-blocking-queue"),
        Some(row)
    );
    assert_eq!(row.legacy_key(), Some(key));
    assert!(HotSpotAlignments::all().contains(row));
    assert!(!HotSpotAlignments::NONE.contains(row));
    // Every row's bit is set in `all()`, and no other bit.
    let every = HotSpotAlignment::ALL
        .into_iter()
        .fold(0u128, |b, a| b | a.bit());
    assert_eq!(HotSpotAlignments::all(), HotSpotAlignments(every));
    assert!(HotSpotAlignments::from_lookup(only(key, "1")).contains(row));
    let on = HotSpotAlignments::from_lookup(only(key, "1"));
    let off = HotSpotAlignments::from_lookup(only(key, "0"));
    assert!(!off.contains(row));
    // Only this row moves.
    assert_eq!(on.with(row, false), off);
    let default_on = HotSpotAlignments::defaults().contains(row);
    assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
    assert!(HotSpotAlignments::NONE
        .edited("+jdk-priority-blocking-queue")
        .contains(row));
    assert!(!HotSpotAlignments::all()
        .edited("-jdk-priority-blocking-queue")
        .contains(row));
    assert_eq!(
        HotSpotAlignments::NONE
            .edited("+jdk-priority-blocking-queue")
            .tokens(),
        vec!["jdk-priority-blocking-queue"]
    );
    // The census line has a slot for it (one counter per row).
    let census = AlignmentCensus::new();
    assert_eq!(census.count(row), 0);
    let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
    assert!(
        line.contains(" jdk-priority-blocking-queue=?/off"),
        "{line}"
    );
}

/// Interpreter round i3 wave 76, lane L1: the `ConcurrentHashMap` row is
/// bit 64 -- the first past the old `u64` -- needs a real JDK, is not
/// counted, and has its own variable (`1` on, `0` off, whatever the
/// default) and token; it moves no other row, and `all()` / `NONE` / the
/// list edits reach it. (No assertion on its default.)
#[test]
fn the_concurrent_hash_map_row_is_bit_64_and_has_its_own_switch() {
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let row = HotSpotAlignment::JdkConcurrentHashMap;
    let key = "CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP";
    assert_eq!(row as usize, 64);
    assert_eq!(row.bit(), 1u128 << 64);
    assert!(row.needs_real_jdk());
    assert!(!row.counted());
    assert_eq!(
        HotSpotAlignment::from_token("jdk-concurrent-hash-map"),
        Some(row)
    );
    assert_eq!(row.legacy_key(), Some(key));
    assert!(HotSpotAlignments::all().contains(row));
    assert!(!HotSpotAlignments::NONE.contains(row));
    let on = HotSpotAlignments::from_lookup(only(key, "1"));
    let off = HotSpotAlignments::from_lookup(only(key, "0"));
    assert!(on.contains(row));
    assert!(!off.contains(row));
    assert_eq!(on.with(row, false), off);
    let default_on = HotSpotAlignments::defaults().contains(row);
    assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
    // The bit-63 neighbour does not move with it.
    assert_eq!(
        on.contains(HotSpotAlignment::JdkPriorityBlockingQueue),
        HotSpotAlignments::defaults().contains(HotSpotAlignment::JdkPriorityBlockingQueue)
    );
    assert_eq!(
        HotSpotAlignments::NONE
            .edited("+jdk-concurrent-hash-map")
            .tokens(),
        vec!["jdk-concurrent-hash-map"]
    );
    assert!(!HotSpotAlignments::all()
        .edited("-jdk-concurrent-hash-map")
        .contains(row));
    let census = AlignmentCensus::new();
    census.note(row);
    assert_eq!(census.count(row), 1);
    let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
    // Not the last item since wave 80 (`jdk-completable-future` is bit 66).
    assert!(line.contains(" jdk-concurrent-hash-map=?/off "), "{line}");
}

/// Interpreter round i3 wave 80, lane L1: the `CompletableFuture` row is
/// bit 66, needs a real JDK, is not counted, and has its own variable (`1`
/// on, `0` off, whatever the default) and token; it moves no other row.
/// (No assertion on its default.)
#[test]
fn the_completable_future_row_is_bit_66_and_has_its_own_switch() {
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let row = HotSpotAlignment::JdkCompletableFuture;
    let key = "CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE";
    assert_eq!(row as usize, 66);
    assert_eq!(row.bit(), 1u128 << 66);
    assert!(row.needs_real_jdk());
    assert!(!row.counted());
    assert_eq!(
        HotSpotAlignment::from_token("jdk-completable-future"),
        Some(row)
    );
    assert_eq!(row.legacy_key(), Some(key));
    assert!(HotSpotAlignments::all().contains(row));
    assert!(!HotSpotAlignments::NONE.contains(row));
    let on = HotSpotAlignments::from_lookup(only(key, "1"));
    let off = HotSpotAlignments::from_lookup(only(key, "0"));
    assert!(on.contains(row));
    assert!(!off.contains(row));
    assert_eq!(on.with(row, false), off);
    let default_on = HotSpotAlignments::defaults().contains(row);
    assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
    assert_eq!(
        HotSpotAlignments::NONE
            .edited("+jdk-completable-future")
            .tokens(),
        vec!["jdk-completable-future"]
    );
    assert!(!HotSpotAlignments::all()
        .edited("-jdk-completable-future")
        .contains(row));
    let census = AlignmentCensus::new();
    let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
    // Not the last item since the nio21 dev merge (the nio21 rows follow).
    assert!(line.contains(" jdk-completable-future=?/off "), "{line}");
}

/// Interpreter round i3 wave 82, lane L1: the thread-pool row is bit 80 (it was 67; the nio21 rows took 67..79 at the waves 81-83 dev merge),
/// needs a real JDK, is not counted, and has its own variable (`1` on, `0`
/// off, whatever the default) and token; it moves no other row. (No
/// assertion on its default.)
#[test]
fn the_thread_pools_row_is_bit_80_and_has_its_own_switch() {
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let row = HotSpotAlignment::JdkThreadPools;
    let key = "CRATONVM_COMPAT_JDK_THREAD_POOLS";
    assert_eq!(row as usize, 80);
    assert_eq!(row.bit(), 1u128 << 80);
    assert!(row.needs_real_jdk());
    assert!(!row.counted());
    assert_eq!(HotSpotAlignment::from_token("jdk-thread-pools"), Some(row));
    assert_eq!(row.legacy_key(), Some(key));
    assert!(HotSpotAlignments::all().contains(row));
    assert!(!HotSpotAlignments::NONE.contains(row));
    let on = HotSpotAlignments::from_lookup(only(key, "1"));
    let off = HotSpotAlignments::from_lookup(only(key, "0"));
    assert!(on.contains(row));
    assert!(!off.contains(row));
    assert_eq!(on.with(row, false), off);
    let default_on = HotSpotAlignments::defaults().contains(row);
    assert_eq!(off.with(row, default_on), HotSpotAlignments::defaults());
    assert_eq!(
        HotSpotAlignments::NONE.edited("+jdk-thread-pools").tokens(),
        vec!["jdk-thread-pools"]
    );
    assert!(!HotSpotAlignments::all()
        .edited("-jdk-thread-pools")
        .contains(row));
    let census = AlignmentCensus::new();
    let line = census.census_line(CompatibilityMode::Compatible, HotSpotAlignments::NONE);
    assert!(line.contains(" jdk-thread-pools=?/off"), "{line}");
}

/// Interpreter round i3 wave 76, lane L1: the widening to a `u128` is a
/// pure refactor. Each row's bit is its discriminant, `defaults()` is
/// exactly the rows whose `default_on` says so, `all()` / `NONE` and the
/// list words are the full and the empty set, each row's `+token` /
/// `-token` and legacy `1` / `0` move that row alone, and the census has
/// one counter and one exit-line item per row.
#[test]
fn the_widened_set_answers_every_row_as_the_u64_did() {
    assert!(HotSpotAlignment::ALL.into_iter().count() <= ALIGNMENT_SET_CAPACITY);
    assert_eq!(ALIGNMENT_SET_CAPACITY, 128);
    let only =
        |k: &'static str, v: &'static str| move |key: &str| (key == k).then(|| v.to_string());
    let d = HotSpotAlignments::defaults();
    for (i, a) in HotSpotAlignment::ALL.into_iter().enumerate() {
        assert_eq!(a.bit(), 1u128 << i);
        assert_eq!(d.contains(a), a.default_on(), "{}", a.token());
        assert!(HotSpotAlignments::all().contains(a));
        assert!(!HotSpotAlignments::NONE.contains(a));
        let plus = format!("+{}", a.token());
        let minus = format!("-{}", a.token());
        assert_eq!(d.edited(&plus), d.with(a, true), "{}", a.token());
        assert_eq!(d.edited(&minus), d.with(a, false), "{}", a.token());
        assert_eq!(
            HotSpotAlignments::NONE.edited(&plus).tokens(),
            vec![a.token()]
        );
        if let Some(key) = a.legacy_key() {
            assert_eq!(
                HotSpotAlignments::from_lookup(only(key, "1")),
                d.with(a, true)
            );
            assert_eq!(
                HotSpotAlignments::from_lookup(only(key, "0")),
                d.with(a, false)
            );
        }
    }
    assert_eq!(d.edited("all"), HotSpotAlignments::all());
    assert_eq!(d.edited("none"), HotSpotAlignments::NONE);
    assert_eq!(
        HotSpotAlignments::all().tokens().len(),
        HotSpotAlignment::ALL.len()
    );
    let census = AlignmentCensus::new();
    for a in HotSpotAlignment::ALL {
        census.note(a);
        assert_eq!(census.count(a), 1);
    }
    let line = census.census_line(CompatibilityMode::JdkOnly, HotSpotAlignments::NONE);
    assert_eq!(
        line.split(' ').count(),
        1 + HotSpotAlignment::ALL.len(),
        "{line}"
    );
}
