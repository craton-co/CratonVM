//! L1W1-1 of `docs/feature-designs/jit-ir-optimizations.md`
//! (`CRATONVM_JIT_SP_SPLICE_NEWARRAY`): single-pass splice arm for a
//! non-negative constant-length primitive `newarray`.

use super::*;

fn empty_site() -> crate::InlineSite {
    crate::InlineSite {
        callee_max_locals: 2,
        callee_num_args: 2,
        callee_is_static: false,
        return_type: b'V',
        elided_invoke_pcs: vec![1],
        needs_heap: true,
        class_name: "pkg/Node".to_string(),
        class_id: 42,
        method_name: "<init>".to_string(),
        descriptor: "(I)V".to_string(),
        ..Default::default()
    }
}

#[test]
fn l1w1_1_const_nonneg_int_screens_producers_and_signs() {
    let mut s = empty_site();
    s.ldc_info = vec![(0, 512), (2, -5), (4, 1024)];
    s.ldc_fp_pcs = vec![4];
    let bt = vec![false; 16];
    // iconst_0 (0x03) -> 0
    assert_eq!(inline_const_nonneg_int(&[0x03, 0xbc], 0, 1, &bt, &s), Some(0));
    // iconst_m1 (0x02) -> None
    assert_eq!(inline_const_nonneg_int(&[0x02, 0xbc], 0, 1, &bt, &s), None);
    // bipush 48 -> 48; bipush -1 -> None
    assert_eq!(inline_const_nonneg_int(&[0x10, 48, 0xbc], 0, 2, &bt, &s), Some(48));
    assert_eq!(inline_const_nonneg_int(&[0x10, 0xff, 0xbc], 0, 2, &bt, &s), None);
    // sipush 224 -> 224; sipush -1 -> None
    assert_eq!(
        inline_const_nonneg_int(&[0x11, 0x00, 224, 0xbc], 0, 3, &bt, &s),
        Some(224)
    );
    assert_eq!(
        inline_const_nonneg_int(&[0x11, 0xff, 0xff, 0xbc], 0, 3, &bt, &s),
        Some(-1).filter(|&v| v >= 0)
    );
    // integer ldc -> 512; negative ldc -> None; float ldc -> None
    assert_eq!(inline_const_nonneg_int(&[0x12, 1, 0xbc], 0, 2, &bt, &s), Some(512));
    assert_eq!(
        inline_const_nonneg_int(&[0x00, 0x00, 0x12, 2, 0xbc], 2, 4, &bt, &s),
        None
    );
    assert_eq!(
        inline_const_nonneg_int(&[0x00, 0x00, 0x00, 0x00, 0x12, 3, 0xbc], 4, 6, &bt, &s),
        None
    );
    // branch target on `use_pc` -> None
    let mut bt_target = bt;
    bt_target[3] = true;
    assert_eq!(
        inline_const_nonneg_int(&[0x11, 0x00, 224, 0xbc], 0, 3, &bt_target, &s),
        None
    );
}
