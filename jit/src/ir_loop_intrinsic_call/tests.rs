use super::*;

const ROWS: &[(&str, &str, &str)] = &[
    ("java/util/Arrays", "fill", "([BB)V"),
    ("java/util/Arrays", "fill", "([ZZ)V"),
    ("java/util/Arrays", "fill", "([CC)V"),
    ("java/util/Arrays", "fill", "([SS)V"),
    ("java/util/Arrays", "fill", "([II)V"),
    ("java/util/Arrays", "fill", "([JJ)V"),
    ("java/util/Arrays", "fill", "([BIIB)V"),
    ("java/util/Arrays", "fill", "([ZIIZ)V"),
    ("java/util/Arrays", "fill", "([CIIC)V"),
    ("java/util/Arrays", "fill", "([SIIS)V"),
    ("java/util/Arrays", "fill", "([IIII)V"),
    ("java/util/Arrays", "fill", "([JIIJ)V"),
    ("java/util/Arrays", "sort", "([I)V"),
    ("java/util/Arrays", "sort", "([J)V"),
    ("java/util/Arrays", "sort", "([C)V"),
    ("java/util/Arrays", "sort", "([S)V"),
    ("java/util/Arrays", "sort", "([B)V"),
    ("java/util/zip/CRC32C", "update", "(I)V"),
    ("java/util/zip/CRC32C", "update", "([BII)V"),
];

#[test]
fn j27_w8rev_loop_intrinsic_rows_need_the_switch() {
    cratonvm_types::flags::with_thread_overrides(&[(SWITCH, None)], || {
        for &(c, m, d) in ROWS {
            assert!(!admits(c, m, d), "default must refuse {c}.{m}{d}");
        }
    });
    cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
        for &(c, m, d) in ROWS {
            assert!(admits(c, m, d), "switch on must admit {c}.{m}{d}");
        }
        // Not loop rows: FP fills (no single-pass row), ranged fills,
        // FP sorts, `Arrays.equals` (its own admission), `CRC32`.
        for (c, m, d) in [
            ("java/util/Arrays", "fill", "([FF)V"),
            ("java/util/Arrays", "fill", "([DD)V"),
            ("java/util/Arrays", "sort", "([D)V"),
            ("java/util/Arrays", "sort", "([III)V"),
            ("java/util/Arrays", "equals", "([I[I)Z"),
            ("java/util/zip/CRC32", "update", "([BII)V"),
            ("java/util/zip/CRC32C", "update", "([B)V"),
        ] {
            assert!(!admits(c, m, d), "{c}.{m}{d} is not a loop row");
        }
    });
}

/// Every admitted row must be a real single-pass intrinsic site (that is
/// the refusal it removes); a row that drifted off the resolver would be
/// a dead admission. CRC32C rows exist only on SSE4.2 hosts.
#[test]
fn j27_w8rev_every_loop_row_is_a_single_pass_intrinsic() {
    for &(c, m, d) in ROWS {
        if c == "java/util/zip/CRC32C" && !crate::x64::has_sse42() {
            continue;
        }
        assert!(
            crate::try_resolve_intrinsic(c, m, d).is_some(),
            "{c}.{m}{d} is no longer a single-pass intrinsic row"
        );
    }
}
