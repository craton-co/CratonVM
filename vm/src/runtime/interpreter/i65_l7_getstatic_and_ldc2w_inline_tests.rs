//! Interpreter round i2 wave 65, lane L7: the `getstatic` arm's inline
//! answer (`CRATONVM_INTERP_GETSTATIC_INLINE_HIT`, `getstatic_inline_hit`)
//! and the `ldc2_w` arm's (`CRATONVM_INTERP_LDC2W_INLINE_HIT`,
//! `ldc2w_inline_hit`). Each hit is compared slot for slot (bits, depth,
//! the `KIND_DOUBLE` mark, the bit-exact pops) with the push the call
//! makes: `push_static_field_value` / `ValueStack::push_long` /
//! `push_double`.
use super::field_access::push_static_field_value;
use super::site_cache::{PrimitiveConstant, PrimitiveConstantSiteCache};
use super::{getstatic_inline_hit, ldc2w_inline_hit, FieldSiteCache};
use crate::classloading::resolution::ResolvedField;
use crate::classloading::ClassId;
use crate::runtime::frame::Frame;
use crate::types::Value;

/// A static `(I)V` frame (`getstatic #0x0102; return`), `max_stack` 4.
fn frame(class_id: ClassId) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/i65l7/Inline".to_string(),
        "m".to_string(),
        "(I)V".to_string(),
        None,
        vec![0xb2, 0x01, 0x02, 0xb1],
        vec![],
        4,
        2,
        &[Value::Int(5)],
    )
}

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

fn fill_static(
    sites: &mut FieldSiteCache,
    class_id: ClassId,
    index: u16,
    holder: ClassId,
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
            field_index: 3,
            is_static: true,
            is_volatile: volatile,
            is_final: false,
            is_reference: matches!(desc, b'L' | b'['),
            desc_byte: desc,
        },
    );
    sites.get(class_id, index).is_some()
}

/// The top slot of `got` is the slot `want`'s top holds: same depth,
/// same raw bits, same `KIND_DOUBLE` mark.
fn same_top(got: &Frame, want: &Frame) {
    assert_eq!(got.stack.len(), want.stack.len());
    assert_eq!(
        got.stack.peek_compact().to_bits(),
        want.stack.peek_compact().to_bits()
    );
    assert_eq!(
        got.stack.peek_kind_is_double(),
        want.stack.peek_kind_is_double()
    );
}

#[test]
fn a_warm_long_and_double_are_pushed_bit_exact() {
    let class_id = ClassId::new(0x00ab_0651);
    let mut sites = PrimitiveConstantSiteCache::new();
    // Cast: bit patterns reinterpreted as i64 (a hash multiplier and a
    // long whose top bits collide with the NaN-box tag space).
    let longs = [
        0x9E37_79B9_7F4A_7C15_u64 as i64,
        0xFFFC_0000_DEAD_BEEF_u64 as i64,
        -1,
    ];
    for (k, v) in longs.into_iter().enumerate() {
        let index = 5 + k as u16; // Cast: test index
        if !fill_ldc(&mut sites, class_id, index, PrimitiveConstant::Long(v)) {
            continue;
        }
        let mut f = frame(class_id);
        let mut g = frame(class_id);
        if ldc2w_inline_hit(&mut f, &mut sites, index) {
            g.stack.push_long(v).unwrap();
            same_top(&f, &g);
            assert_eq!(f.stack.pop_long().ok(), Some(v));
        } else {
            assert!(sites.get(class_id, index).is_none());
        }
    }
    // A NaN whose payload collides with the tag space keeps its bits.
    let doubles = [f64::from_bits(0x7FF8_0000_DEAD_BEEF), 0.5, -0.0];
    for (k, v) in doubles.into_iter().enumerate() {
        let index = 9 + k as u16; // Cast: test index
        if !fill_ldc(&mut sites, class_id, index, PrimitiveConstant::Double(v)) {
            continue;
        }
        let mut f = frame(class_id);
        let mut g = frame(class_id);
        if ldc2w_inline_hit(&mut f, &mut sites, index) {
            g.stack.push_double(v).unwrap();
            same_top(&f, &g);
            assert!(f.stack.peek_kind_is_double());
            assert_eq!(
                f.stack.pop_double().map(f64::to_bits).ok(),
                Some(v.to_bits())
            );
        } else {
            assert!(sites.get(class_id, index).is_none());
        }
    }
}

#[test]
fn ldc2w_declines_a_miss_a_category_one_site_and_a_full_stack() {
    let class_id = ClassId::new(0x00ab_0652);
    let mut sites = PrimitiveConstantSiteCache::new();
    let mut f = frame(class_id);
    assert!(!ldc2w_inline_hit(&mut f, &mut sites, 5));
    assert_eq!(f.stack.len(), 0);
    // An int or float held at the index (an `ldc` site) is not an
    // `ldc2_w` answer.
    let _ = fill_ldc(&mut sites, class_id, 6, PrimitiveConstant::Int(1));
    assert!(!ldc2w_inline_hit(&mut f, &mut sites, 6));
    let _ = fill_ldc(&mut sites, class_id, 7, PrimitiveConstant::Float(1.0));
    assert!(!ldc2w_inline_hit(&mut f, &mut sites, 7));
    assert_eq!(f.stack.len(), 0);
    // Another class's site with the same index is not this one.
    let _ = fill_ldc(
        &mut sites,
        ClassId::new(0x00ab_0653),
        5,
        PrimitiveConstant::Long(9),
    );
    assert!(!ldc2w_inline_hit(&mut f, &mut sites, 5));
    // A full stack: nothing written, the call raises the overflow.
    while f.stack.push_int(1).is_ok() {}
    let depth = f.stack.len();
    if fill_ldc(&mut sites, class_id, 5, PrimitiveConstant::Long(2)) {
        assert!(!ldc2w_inline_hit(&mut f, &mut sites, 5));
        assert_eq!(f.stack.len(), depth);
    }
}

#[test]
fn a_quickened_static_is_pushed_as_the_call_pushes_it() {
    let class_id = ClassId::new(0x00ab_0654);
    let holder = ClassId::new(0x00ab_0655);
    // Cast: bit patterns reinterpreted as i64.
    let collision = 0xFFFC_0000_DEAD_BEEF_u64 as i64;
    let nan_bits = 0x7FF8_0000_DEAD_BEEF_u64;
    let cases: [(u8, Value); 16] = [
        (b'I', Value::Int(7)),
        (b'I', Value::Object(None)),
        (b'S', Value::Int(-2)),
        (b'B', Value::Int(-128)),
        (b'C', Value::Int(65)),
        (b'Z', Value::Int(1)),
        (b'F', Value::Float(1.5)),
        (b'J', Value::Long(collision)),
        (b'J', Value::Int(-3)),
        (b'J', Value::Object(None)),
        (b'D', Value::Double(f64::from_bits(nan_bits))),
        // Cast: test bit pattern
        (b'D', Value::Long(nan_bits as i64)),
        (b'D', Value::Int(2)),
        (b'D', Value::Object(None)),
        (b'L', Value::Int(0)),
        (b'[', Value::Object(None)),
    ];
    for (desc, value) in cases {
        for volatile in [false, true] {
            let mut sites = FieldSiteCache::new();
            if !fill_static(&mut sites, class_id, 0x0102, holder, desc, volatile) {
                continue;
            }
            let mut f = frame(class_id);
            let read = |c: ClassId, slot: usize| {
                assert_eq!(
                    (c, slot),
                    (holder, 3),
                    "the site's holder and slot are read"
                );
                value
            };
            if getstatic_inline_hit(&mut f, &mut sites, 0x0102, false, read) {
                let mut g = frame(class_id);
                push_static_field_value(
                    &mut g.stack,
                    value,
                    matches!(desc, b'L' | b'['),
                    Some(desc),
                )
                .unwrap();
                assert_eq!(f.stack.len(), 1, "descriptor {} {value:?}", desc as char);
                same_top(&f, &g);
                if desc == b'J' {
                    assert_eq!(f.stack.pop_long().ok(), g.stack.pop_long().ok());
                }
            } else {
                assert!(sites.get(class_id, 0x0102).is_none());
            }
        }
    }
}

#[test]
fn the_getstatic_inline_hit_declines_without_writing() {
    let class_id = ClassId::new(0x00ab_0656);
    let holder = ClassId::new(0x00ab_0657);
    let unread = |_: ClassId, _: usize| -> Value { panic!("the field must not be read") };
    let mut sites = FieldSiteCache::new();
    let mut f = frame(class_id);
    // A miss: the call resolves and fills.
    assert!(!getstatic_inline_hit(
        &mut f, &mut sites, 0x0102, false, unread
    ));
    assert_eq!(f.stack.len(), 0);
    // A reference static while the provenance trace is armed: declined
    // before the read, so the call records the push.
    for desc in *b"L[" {
        if fill_static(&mut sites, class_id, 0x0102, holder, desc, false) {
            assert!(!getstatic_inline_hit(
                &mut f, &mut sites, 0x0102, true, unread
            ));
            assert_eq!(f.stack.len(), 0);
        }
    }
    // ... but a primitive static is answered with the trace armed.
    if fill_static(&mut sites, class_id, 0x0102, holder, b'I', false) {
        if getstatic_inline_hit(&mut f, &mut sites, 0x0102, true, |_, _| Value::Int(4)) {
            assert_eq!(f.stack.pop_int().ok(), Some(4));
        }
    }
    // A value shape the call would coerce otherwise declines.
    let odd: [(u8, Value); 4] = [
        (b'I', Value::Long(7)),
        // (`F` with `Object(None)` is not odd: the call pushes `Int(0)`,
        // the zero-initialised float, and so does the inline hit.)
        (b'F', Value::Double(1.0)),
        (b'J', Value::Float(1.0)),
        (b'D', Value::Float(1.0)),
    ];
    for (desc, value) in odd {
        if fill_static(&mut sites, class_id, 0x0102, holder, desc, false) {
            assert!(!getstatic_inline_hit(
                &mut f,
                &mut sites,
                0x0102,
                false,
                |_, _| value
            ));
            assert_eq!(f.stack.len(), 0, "descriptor {}", desc as char);
        }
    }
    // A full stack: nothing written, for every push shape.
    while f.stack.push_int(1).is_ok() {}
    let depth = f.stack.len();
    let full: [(u8, Value); 4] = [
        (b'I', Value::Int(1)),
        (b'J', Value::Long(1)),
        (b'D', Value::Double(1.0)),
        (b'L', Value::Object(None)),
    ];
    for (desc, value) in full {
        if fill_static(&mut sites, class_id, 0x0102, holder, desc, false) {
            assert!(!getstatic_inline_hit(
                &mut f,
                &mut sites,
                0x0102,
                false,
                |_, _| value
            ));
            assert_eq!(f.stack.len(), depth, "descriptor {}", desc as char);
        }
    }
}
