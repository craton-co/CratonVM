//! Interpreter round i2 wave 64, lane L7: the `ldc` arm's inline answer
//! (`CRATONVM_INTERP_LDC_INLINE_HIT`, `ldc_inline_hit`) and the fused
//! compare-and-branch's quickened `getstatic` int bound
//! (`CRATONVM_INTERP_FUSED_GETSTATIC_BOUND`, `fused_int_operand` /
//! `getstatic_int_site`).
use super::site_cache::{PrimitiveConstant, PrimitiveConstantSiteCache};
use super::{fused_int_operand, ldc_inline_hit, FieldSiteCache};
use crate::classloading::resolution::ResolvedField;
use crate::classloading::ClassId;
use crate::runtime::frame::Frame;
use crate::types::Value;

/// A static `(I)V` frame over `code`, local 0 = `5`.
fn frame(class_id: ClassId, code: Vec<u8>) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/i64l7/Bound".to_string(),
        "m".to_string(),
        "(I)V".to_string(),
        None,
        code,
        vec![],
        4,
        2,
        &[Value::Int(5)],
    )
}

/// Fill a numeric-constant site; `false` when the process epochs moved
/// under the fill (another test defined a class) and it was dropped.
fn fill_ldc(
    sites: &mut PrimitiveConstantSiteCache,
    class_id: ClassId,
    index: u16,
    value: PrimitiveConstant,
) -> bool {
    let epochs = PrimitiveConstantSiteCache::epochs_now();
    sites.put(class_id, index, epochs, value);
    sites.get(class_id, index).is_some()
}

/// Fill a static-field site for a field of descriptor byte `desc` in
/// holder `holder`, slot `slot`; `false` when the fill was dropped.
fn fill_static(
    sites: &mut FieldSiteCache,
    class_id: ClassId,
    index: u16,
    holder: ClassId,
    slot: usize,
    desc: u8,
    volatile: bool,
) -> bool {
    let epochs = FieldSiteCache::epochs_now();
    sites.put(
        class_id,
        index,
        epochs,
        ResolvedField {
            declaring_class_id: holder,
            field_index: slot,
            is_static: true,
            is_volatile: volatile,
            is_final: false,
            is_reference: matches!(desc, b'L' | b'['),
            desc_byte: desc,
        },
    );
    sites.get(class_id, index).is_some()
}

/// `fused_int_operand` at `pc` with only the `getstatic` bound on (or
/// off), the field-watch answer `watch`, and `read` as the static store.
fn decode_gs(
    f: &Frame,
    pc: usize,
    on: bool,
    watch: Option<bool>,
    sites: &mut FieldSiteCache,
    read: impl FnOnce(ClassId, usize) -> Value,
) -> Option<(i32, usize)> {
    // SAFETY: `f.code` is the padded bytecode and `pc` is below its
    // unpadded length in every caller.
    unsafe {
        fused_int_operand(
            f,
            f.code.as_ptr(),
            pc,
            false,
            false,
            false,
            &mut PrimitiveConstantSiteCache::new(),
            on,
            watch,
            sites,
            read,
        )
    }
}

#[test]
fn a_warm_ldc_int_and_float_are_pushed_inline() {
    let class_id = ClassId::new(0x00ab_0641);
    let mut f = frame(class_id, vec![0x12, 5, 0x12, 6, 0xb1]);
    let mut sites = PrimitiveConstantSiteCache::new();
    if !fill_ldc(&mut sites, class_id, 5, PrimitiveConstant::Int(100_003)) {
        return; // epochs moved under the fill: nothing to observe
    }
    if ldc_inline_hit(&mut f, &mut sites, 5, false) {
        assert_eq!(f.stack.len(), 1);
        assert_eq!(f.stack.pop_int().ok(), Some(100_003));
    } else {
        // Only a table that no longer holds the site may decline.
        assert!(sites.get(class_id, 5).is_none());
    }
    if !fill_ldc(&mut sites, class_id, 6, PrimitiveConstant::Float(0.75)) {
        return;
    }
    if ldc_inline_hit(&mut f, &mut sites, 6, false) {
        assert_eq!(f.stack.len(), 1);
        assert_eq!(f.stack.pop_float().ok(), Some(0.75));
    } else {
        assert!(sites.get(class_id, 6).is_none());
    }
}

#[test]
fn a_miss_a_category_two_site_and_a_full_stack_decline() {
    let class_id = ClassId::new(0x00ab_0642);
    let mut f = frame(class_id, vec![0x12, 5, 0xb1]);
    let mut sites = PrimitiveConstantSiteCache::new();
    // Nothing held: the call answers.
    assert!(!ldc_inline_hit(&mut f, &mut sites, 5, false));
    assert_eq!(f.stack.len(), 0);
    // A long held at the index (an `ldc2_w` site) is not an `ldc` answer.
    let _ = fill_ldc(&mut sites, class_id, 7, PrimitiveConstant::Long(1));
    assert!(!ldc_inline_hit(&mut f, &mut sites, 7, false));
    assert_eq!(f.stack.len(), 0);
    // Another class's site with the same index is not this one.
    let _ = fill_ldc(
        &mut sites,
        ClassId::new(0x00ab_0643),
        5,
        PrimitiveConstant::Int(9),
    );
    assert!(!ldc_inline_hit(&mut f, &mut sites, 5, false));
    // A full stack: the checked push refuses, nothing is written, and the
    // call (which raises the overflow) runs.
    while f.stack.push_int(1).is_ok() {}
    let depth = f.stack.len();
    if fill_ldc(&mut sites, class_id, 5, PrimitiveConstant::Int(2)) {
        assert!(!ldc_inline_hit(&mut f, &mut sites, 5, false));
        assert_eq!(f.stack.len(), depth);
    }
}

#[test]
fn a_quickened_int_static_is_the_second_operand() {
    let class_id = ClassId::new(0x00ab_0644);
    let holder = ClassId::new(0x00ab_0645);
    // iload_0; getstatic #0x0102; if_icmpge +5; return; return
    let f = frame(
        class_id,
        vec![0x1a, 0xb2, 0x01, 0x02, 0xa2, 0, 5, 0xb1, 0xb1],
    );
    let mut sites = FieldSiteCache::new();
    if !fill_static(&mut sites, class_id, 0x0102, holder, 3, b'I', false) {
        return;
    }
    let read = |c: ClassId, slot: usize| {
        assert_eq!(
            (c, slot),
            (holder, 3),
            "the site's holder and slot are read"
        );
        Value::Int(1_000_000)
    };
    match decode_gs(&f, 1, true, Some(false), &mut sites, read) {
        Some(got) => assert_eq!(got, (1_000_000, 4)),
        None => assert!(sites.get(class_id, 0x0102).is_none()),
    }
    // A zero-initialized slot reads as the int 0, as the unfused
    // `getstatic` pushes it; a volatile static is answered too.
    if fill_static(&mut sites, class_id, 0x0102, holder, 3, b'I', true) {
        if let Some(got) = decode_gs(&f, 1, true, Some(false), &mut sites, |_, _| {
            Value::Object(None)
        }) {
            assert_eq!(got, (0, 4));
        }
    }
    // The sub-int descriptors are ints on the operand stack.
    for desc in *b"SBCZ" {
        if fill_static(&mut sites, class_id, 0x0102, holder, 3, desc, false) {
            if let Some(got) =
                decode_gs(&f, 1, true, Some(false), &mut sites, |_, _| Value::Int(1))
            {
                assert_eq!(got, (1, 4), "descriptor {}", desc as char);
            }
        }
    }
}

#[test]
fn the_getstatic_bound_declines_off_watched_missed_or_not_int() {
    let class_id = ClassId::new(0x00ab_0646);
    let holder = ClassId::new(0x00ab_0647);
    // iload_0; getstatic #0x0102; if_icmplt +5; return; return
    let f = frame(
        class_id,
        vec![0x1a, 0xb2, 0x01, 0x02, 0xa1, 0, 5, 0xb1, 0xb1],
    );
    let mut sites = FieldSiteCache::new();
    let unread = |_: ClassId, _: usize| -> Value { panic!("the field must not be read") };
    // A miss: the unfused `getstatic` resolves and fills.
    assert_eq!(
        decode_gs(&f, 1, true, Some(false), &mut sites, unread),
        None
    );
    let _ = fill_static(&mut sites, class_id, 0x0102, holder, 0, b'I', false);
    // The switch off.
    assert_eq!(
        decode_gs(&f, 1, false, Some(false), &mut sites, unread),
        None
    );
    // A field watch armed: declined before the probe, so the unfused
    // `getstatic` reports the access.
    assert_eq!(decode_gs(&f, 1, true, Some(true), &mut sites, unread), None);
    // A long, a double, a float and a reference static decline.
    for desc in *b"JDFL[" {
        let _ = fill_static(&mut sites, class_id, 0x0102, holder, 0, desc, false);
        assert_eq!(
            decode_gs(&f, 1, true, Some(false), &mut sites, unread),
            None,
            "descriptor {}",
            desc as char
        );
    }
    // An int site holding a non-int shape declines (the unfused path
    // answers it).
    if fill_static(&mut sites, class_id, 0x0102, holder, 0, b'I', false) {
        assert_eq!(
            decode_gs(&f, 1, true, Some(false), &mut sites, |_, _| Value::Long(7)),
            None
        );
    }
}

#[test]
fn a_getstatic_not_followed_by_a_compare_is_not_an_operand() {
    let class_id = ClassId::new(0x00ab_0648);
    let holder = ClassId::new(0x00ab_0649);
    let mut sites = FieldSiteCache::new();
    let _ = fill_static(&mut sites, class_id, 0x0102, holder, 0, b'I', false);
    let unread = |_: ClassId, _: usize| -> Value { panic!("the field must not be read") };
    // iload_0; getstatic #0x0102; iadd; ireturn -- `s + count`
    let f = frame(class_id, vec![0x1a, 0xb2, 0x01, 0x02, 0x60, 0xac]);
    assert_eq!(
        decode_gs(&f, 1, true, Some(false), &mut sites, unread),
        None
    );
    // A `getstatic` cut off by the end of the code: its "next opcode"
    // would lie past the padded buffer, so it is not read.
    let g = frame(class_id, vec![0x1a, 0xb2]);
    assert_eq!(
        decode_gs(&g, 1, true, Some(false), &mut sites, unread),
        None
    );
}
