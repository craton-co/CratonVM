use super::*;
// Named by their real module: `pub(crate)` items, reached explicitly so the
// test does not depend on how the crate root's glob re-export treats them.
use crate::descriptor_shapes::{fp_in_body, method_uses_category2, method_uses_fp};

/// A range can be freed several times over (a recycled address), and the
/// crash handler must be told about the MOST RECENT free, whose flags are
/// the ones that describe the fault. The old slot-0-upward scan answered
/// with whichever record sat lowest in the ring.
#[test]
fn the_newest_free_of_a_recycled_range_is_the_one_reported() {
    // Far above any real mapping, so no concurrent test's frees cover it.
    let base = usize::MAX - 0x20_0000;
    let len = 0x1000;
    record_code_free(base, len, 7, 0);
    record_code_free(base, len, 3, CODE_FREE_PUBLISHED | CODE_FREE_AUTHORISED);
    let (b, l, active, flags) = recent_code_free_covering(base + 8).expect("covered");
    assert_eq!((b, l), (base, len));
    assert_eq!(active, 3, "the newer record must win");
    assert_eq!(flags, CODE_FREE_PUBLISHED | CODE_FREE_AUTHORISED);
    record_code_free(base, len, 11, CODE_FREE_PUBLISHED);
    assert_eq!(
        recent_code_free_covering(base + 8).map(|(_, _, a, f)| (a, f)),
        Some((11, CODE_FREE_PUBLISHED)),
        "and again after a third free of the same range"
    );
}

/// The two constructors differ in `needs_context` and in nothing else.
#[test]
fn the_context_constructor_differs_only_in_needs_context() {
    let mut plain = ExecutableBuffer::new(64).expect("alloc");
    plain.emit(&[0xC3]);
    let mut ctx = ExecutableBuffer::new(64).expect("alloc");
    ctx.emit(&[0xC3]);
    let plain = CompiledMethod::new(plain);
    let ctx = CompiledMethod::new_with_context(ctx);
    assert!(!plain.needs_context());
    assert!(ctx.needs_context());
    assert_eq!(plain.code_len(), ctx.code_len());
    assert_eq!(ctx.code_bytes(), &[0xC3]);
    assert_ne!(plain.artifact_id, ctx.artifact_id);
    assert!(!ctx.retired.load(std::sync::atomic::Ordering::SeqCst));
}

/// `ir_value_shape_admitted` is an exact rewrite of the three-disjunct
/// form the admission chain, the verdict line and `c2_upgrade_would_engage`
/// used to spell out, over every combination of the two gates.
#[test]
fn the_value_shape_term_matches_its_spelled_out_form() {
    fn spelled_out(code: &[u8], desc: &str, long: bool, fp: bool) -> bool {
        let len = code.len();
        let fp_free = !method_uses_fp(code, len, desc);
        (!method_uses_category2(code, len, desc) && fp_free)
            || (long && fp_free)
            || (fp && fp_in_body(code, len))
    }
    let cases: &[(&[u8], &str)] = &[
        // iconst_0; ireturn
        (&[0x03, 0xac], "()I"),
        // lconst_0; lreturn
        (&[0x09, 0xad], "()J"),
        // iconst_0; ireturn — a long only in the signature
        (&[0x03, 0xac], "(J)I"),
        // fconst_0; f2i; ireturn — FP only in the body
        (&[0x0b, 0x8b, 0xac], "()I"),
        // iconst_0; ireturn — FP only in the signature
        (&[0x03, 0xac], "(F)I"),
        // dconst_0; d2i; ireturn — double in the body
        (&[0x0e, 0x8e, 0xac], "()I"),
        // dconst_0; dreturn — double in both
        (&[0x0e, 0xaf], "(D)D"),
    ];
    for &(code, desc) in cases {
        for long in [false, true] {
            for fp in [false, true] {
                assert_eq!(
                    ir_value_shape_admitted(code, code.len(), desc, long, fp),
                    spelled_out(code, desc, long, fp),
                    "{desc} {code:02x?} long={long} fp={fp}"
                );
            }
        }
    }
}
