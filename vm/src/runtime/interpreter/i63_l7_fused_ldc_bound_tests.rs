//! Interpreter round i2 wave 63, lane L7: `fused_int_operand` admits a
//! warm `ldc` / `ldc_w` int as the fused compare-and-branch's second
//! operand (`CRATONVM_INTERP_FUSED_LDC_BOUND`), answered from the
//! thread's numeric-constant table only.
use super::fused_int_operand;
use super::site_cache::{PrimitiveConstant, PrimitiveConstantSiteCache};
use crate::classloading::ClassId;
use crate::runtime::frame::Frame;
use crate::types::Value;

/// A static `(I)V` frame over `code`, local 0 = `5`.
fn frame(class_id: ClassId, code: Vec<u8>) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/i63l7/LdcBound".to_string(),
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

/// Fill `index` of `class_id` with `value`; `false` when the process
/// epochs moved (another test defined a class) and the fill was dropped.
fn fill(
    sites: &mut PrimitiveConstantSiteCache,
    class_id: ClassId,
    index: u16,
    value: PrimitiveConstant,
) -> bool {
    let epochs = PrimitiveConstantSiteCache::epochs_now();
    sites.put(class_id, index, epochs, value);
    sites.get(class_id, index).is_some()
}

/// `fused_int_operand` at `pc` of `frame`, `arraylength` off, census off.
fn decode(
    frame: &Frame,
    pc: usize,
    ldc_bound: bool,
    sites: &mut PrimitiveConstantSiteCache,
) -> Option<(i32, usize)> {
    // SAFETY: `frame.code` is the padded bytecode and `pc` is below its
    // unpadded length in every caller.
    unsafe {
        fused_int_operand(
            frame,
            frame.code.as_ptr(),
            pc,
            false,
            false,
            ldc_bound,
            sites,
            false,
            Some(false),
            &mut crate::runtime::interpreter::FieldSiteCache::new(),
            |_, _| Value::Int(0),
        )
    }
}

/// The decode at `pc` with the switch on, checked against the table's own
/// answer: a fusion may decline only where a direct probe misses too
/// (the epochs are process-wide and other tests move them).
fn decode_on(
    frame: &Frame,
    pc: usize,
    sites: &mut PrimitiveConstantSiteCache,
    index: u16,
) -> Option<(i32, usize)> {
    let got = decode(frame, pc, true, sites);
    if got.is_none() {
        assert!(
            !matches!(
                sites.get(frame.class_id, index),
                Some(PrimitiveConstant::Int(_))
            ),
            "the fusion declined a site the table holds"
        );
    }
    got
}

#[test]
fn a_warm_ldc_int_is_the_second_operand() {
    let class_id = ClassId::new(0x00ab_0631);
    // iload_0; ldc #5; if_icmpge +5; return; return
    let f = frame(class_id, vec![0x1a, 0x12, 5, 0xa2, 0, 5, 0xb1, 0xb1]);
    let mut sites = PrimitiveConstantSiteCache::new();
    if !fill(&mut sites, class_id, 5, PrimitiveConstant::Int(1_000_000)) {
        return; // epochs moved under the fill: nothing to observe
    }
    if let Some(got) = decode_on(&f, 1, &mut sites, 5) {
        assert_eq!(got, (1_000_000, 3));
    }
}

#[test]
fn a_warm_ldc_w_int_is_the_second_operand() {
    let class_id = ClassId::new(0x00ab_0632);
    // iload_0; ldc_w #0x0105; if_icmplt +5; return; return
    let f = frame(
        class_id,
        vec![0x1a, 0x13, 0x01, 0x05, 0xa1, 0, 5, 0xb1, 0xb1],
    );
    let mut sites = PrimitiveConstantSiteCache::new();
    if !fill(
        &mut sites,
        class_id,
        0x0105,
        PrimitiveConstant::Int(-40_000),
    ) {
        return;
    }
    if let Some(got) = decode_on(&f, 1, &mut sites, 0x0105) {
        assert_eq!(got, (-40_000, 4));
    }
}

#[test]
fn the_switch_off_a_miss_and_a_float_all_decline() {
    let class_id = ClassId::new(0x00ab_0633);
    // iload_0; ldc #5; if_icmpge; ldc #6; if_icmpge; return
    let f = frame(
        class_id,
        vec![0x1a, 0x12, 5, 0xa2, 0, 8, 0x12, 6, 0xa2, 0, 3, 0xb1],
    );
    let mut sites = PrimitiveConstantSiteCache::new();
    let _ = fill(&mut sites, class_id, 5, PrimitiveConstant::Int(70_000));
    // Switch off: the held int is not consulted.
    assert_eq!(decode(&f, 1, false, &mut sites), None);
    // A float site declines (the unfused `ldc` pushes it).
    let _ = fill(&mut sites, class_id, 6, PrimitiveConstant::Float(1.5));
    assert_eq!(decode(&f, 6, true, &mut sites), None);
    // An index the table never held declines (the unfused `ldc` fills it).
    let g = frame(class_id, vec![0x1a, 0x12, 9, 0xa2, 0, 5, 0xb1, 0xb1]);
    assert_eq!(decode(&g, 1, true, &mut sites), None);
    // Another class's site with the same index is not this one.
    let h = frame(
        ClassId::new(0x00ab_0634),
        vec![0x1a, 0x12, 5, 0xa2, 0, 5, 0xb1, 0xb1],
    );
    assert_eq!(decode(&h, 1, true, &mut sites), None);
}

#[test]
fn a_held_ldc_not_followed_by_a_compare_is_not_an_operand() {
    let class_id = ClassId::new(0x00ab_0636);
    let mut sites = PrimitiveConstantSiteCache::new();
    let _ = fill(&mut sites, class_id, 5, PrimitiveConstant::Int(100_003));
    let _ = fill(
        &mut sites,
        class_id,
        0x0105,
        PrimitiveConstant::Int(100_004),
    );
    // iload_0; ldc #5; iadd; ireturn -- `s += 100_003`
    let f = frame(class_id, vec![0x1a, 0x12, 5, 0x60, 0xac]);
    assert_eq!(decode(&f, 1, true, &mut sites), None);
    // iload_0; ldc_w #0x0105; iadd; ireturn
    let g = frame(class_id, vec![0x1a, 0x13, 0x01, 0x05, 0x60, 0xac]);
    assert_eq!(decode(&g, 1, true, &mut sites), None);
    // An `ldc_w` cut off by the end of the code: its "next opcode" would
    // lie past the padded buffer, so it is not read.
    let h = frame(class_id, vec![0x1a, 0x13]);
    assert_eq!(decode(&h, 1, true, &mut sites), None);
}

#[test]
fn the_older_operands_decode_as_before_with_the_switch_on() {
    let class_id = ClassId::new(0x00ab_0635);
    // iload_0; sipush 300; if_icmpge; bipush -3; iconst_2; iload_0
    let f = frame(
        class_id,
        vec![
            0x1a, 0x11, 0x01, 0x2c, 0xa2, 0, 3, 0x10, 0xfd, 0x05, 0x1a, 0xb1,
        ],
    );
    let mut sites = PrimitiveConstantSiteCache::new();
    assert_eq!(decode(&f, 1, true, &mut sites), Some((300, 4)));
    assert_eq!(decode(&f, 7, true, &mut sites), Some((-3, 9)));
    assert_eq!(decode(&f, 9, true, &mut sites), Some((2, 10)));
    assert_eq!(decode(&f, 10, true, &mut sites), Some((5, 11)));
}
