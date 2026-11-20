//! Interpreter round i2 wave 66, lane L7: the `putstatic` arm's inline
//! store (`CRATONVM_INTERP_PUTSTATIC_INLINE_HIT`, `putstatic_inline_hit`).
//! Each hit is compared with what the call's hit stores: the value
//! `pop_static_field_value` pops for the descriptor from an identical
//! stack, bit for bit, at the site's holder and slot; each decline leaves
//! the stack untouched and writes nothing.
use super::field_access::pop_static_field_value;
use super::{putstatic_inline_hit, FieldSiteCache};
use crate::classloading::resolution::ResolvedField;
use crate::classloading::ClassId;
use crate::runtime::frame::Frame;
use crate::types::Value;

/// A static `()V` frame (`putstatic #0x0102; return`), `max_stack` 4.
fn frame(class_id: ClassId) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/i66l7/Inline".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb3, 0x01, 0x02, 0xb1],
        vec![],
        4,
        1,
        &[],
    )
}

fn fill_static(
    sites: &mut FieldSiteCache,
    class_id: ClassId,
    holder: ClassId,
    desc: u8,
    volatile: bool,
    is_final: bool,
) -> bool {
    let epochs = FieldSiteCache::epochs_now();
    sites.put(
        class_id,
        0x0102,
        epochs,
        ResolvedField {
            declaring_class_id: holder,
            field_index: 3,
            is_static: true,
            is_volatile: volatile,
            is_final,
            is_reference: matches!(desc, b'L' | b'['),
            desc_byte: desc,
        },
    );
    sites.get(class_id, 0x0102).is_some()
}

/// A value with its float payload spelled as bits, so a NaN compares.
fn shape(v: Value) -> String {
    match v {
        Value::Double(x) => format!("D{:016x}", x.to_bits()),
        Value::Float(x) => format!("F{:08x}", x.to_bits()),
        other => format!("{other:?}"),
    }
}

/// Push `value` as the producer of `desc` pushes it.
fn push_for(f: &mut Frame, desc: u8, value: Value) {
    match (desc, value) {
        (b'J', Value::Long(x)) => f.stack.push_long(x).unwrap(),
        (b'D', Value::Double(x)) => f.stack.push_double(x).unwrap(),
        (b'F', Value::Float(x)) => f.stack.push_float(x).unwrap(),
        (_, Value::Int(x)) => f.stack.push_int(x).unwrap(),
        (_, other) => f.stack.push(other).unwrap(),
    }
}

#[test]
fn a_quickened_static_is_stored_as_the_call_stores_it() {
    let class_id = ClassId::new(0x00ab_0661);
    let holder = ClassId::new(0x00ab_0662);
    // Cast: bit patterns reinterpreted as i64 (a long whose top bits
    // collide with the NaN-box tag space, and -1).
    let collision = 0xFFFC_0000_DEAD_BEEF_u64 as i64;
    let cases: [(u8, Value); 13] = [
        (b'I', Value::Int(7)),
        (b'I', Value::Int(i32::MIN)),
        (b'S', Value::Int(0x1_8001)),
        (b'B', Value::Int(0x180)),
        (b'C', Value::Int(-1)),
        (b'Z', Value::Int(2)),
        (b'F', Value::Float(f32::from_bits(0x7FC0_BEEF))),
        (b'J', Value::Long(collision)),
        (b'J', Value::Long(-1)),
        (b'D', Value::Double(f64::from_bits(0x7FF8_0000_DEAD_BEEF))),
        (b'D', Value::Double(-0.0)),
        (b'L', Value::Object(None)),
        (b'[', Value::Object(None)),
    ];
    for (desc, value) in cases {
        for volatile in [false, true] {
            let mut sites = FieldSiteCache::new();
            if !fill_static(&mut sites, class_id, holder, desc, volatile, false) {
                continue;
            }
            let mut f = frame(class_id);
            f.stack.push_int(42).unwrap();
            push_for(&mut f, desc, value);
            let mut g = frame(class_id);
            g.stack.push_int(42).unwrap();
            push_for(&mut g, desc, value);
            let want = pop_static_field_value(&mut g.stack, Some(desc)).unwrap();
            let mut stored = None;
            let hit = putstatic_inline_hit(&mut f, &mut sites, 0x0102, |c, slot, v| {
                stored = Some((c, slot, v));
            });
            if hit {
                let (c, slot, v) = stored.expect("a hit stores");
                assert_eq!(
                    (c, slot),
                    (holder, 3),
                    "the site's holder and slot are written"
                );
                assert_eq!(
                    shape(v),
                    shape(want),
                    "descriptor {} {value:?}",
                    desc as char
                );
                assert_eq!(f.stack.len(), 1, "exactly one slot popped");
                assert_eq!(f.stack.pop_int().ok(), Some(42));
            } else {
                assert!(stored.is_none());
                assert!(sites.get(class_id, 0x0102).is_none());
            }
        }
    }
}

#[test]
fn the_putstatic_inline_hit_declines_without_popping() {
    let class_id = ClassId::new(0x00ab_0663);
    let holder = ClassId::new(0x00ab_0664);
    let unwritten = |_: ClassId, _: usize, _: Value| panic!("nothing may be stored");
    let mut sites = FieldSiteCache::new();
    let mut f = frame(class_id);
    f.stack.push_int(9).unwrap();
    // A miss: the call resolves, checks and fills.
    assert!(!putstatic_inline_hit(&mut f, &mut sites, 0x0102, unwritten));
    assert_eq!(f.stack.len(), 1);
    // A final static: the call decides (`check_final_field_put`).
    if fill_static(&mut sites, class_id, holder, b'I', false, true) {
        assert!(!putstatic_inline_hit(&mut f, &mut sites, 0x0102, unwritten));
        assert_eq!(f.stack.len(), 1);
    }
    // A `D` field over a tag no double producer leaves: the decode's
    // `Err` popped nothing (i66-L7), and the call reports it.
    if fill_static(&mut sites, class_id, holder, b'D', false, false) {
        f.stack.push_float(2.5).unwrap();
        assert!(!putstatic_inline_hit(&mut f, &mut sites, 0x0102, unwritten));
        assert_eq!(f.stack.len(), 2);
        assert_eq!(f.stack.pop_float().ok(), Some(2.5));
        assert_eq!(f.stack.pop_int().ok(), Some(9));
    }
    // An empty stack, every descriptor: the call reports the underflow.
    for desc in *b"IJDFLZ" {
        if fill_static(&mut sites, class_id, holder, desc, false, false) {
            assert!(!putstatic_inline_hit(&mut f, &mut sites, 0x0102, unwritten));
            assert_eq!(f.stack.len(), 0, "descriptor {}", desc as char);
        }
    }
    // Another class's site with the same index is not this one.
    let mut other = FieldSiteCache::new();
    if fill_static(
        &mut other,
        ClassId::new(0x00ab_0665),
        holder,
        b'I',
        false,
        false,
    ) {
        f.stack.push_int(1).unwrap();
        assert!(!putstatic_inline_hit(&mut f, &mut other, 0x0102, unwritten));
        assert_eq!(f.stack.len(), 1);
    }
}

/// The fused `getstatic; if<cond>` (`CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO`)
/// branches exactly where the `ifeq` .. `ifle` arms do on the int.
#[test]
fn if_zero_taken_is_the_six_zero_compares() {
    for v in [i32::MIN, -1, 0, 1, i32::MAX] {
        assert_eq!(super::if_zero_taken(0x99, v), v == 0);
        assert_eq!(super::if_zero_taken(0x9a, v), v != 0);
        assert_eq!(super::if_zero_taken(0x9b, v), v < 0);
        assert_eq!(super::if_zero_taken(0x9c, v), v >= 0);
        assert_eq!(super::if_zero_taken(0x9d, v), v > 0);
        assert_eq!(super::if_zero_taken(0x9e, v), v <= 0);
        // Each is `if_icmp<cond>` against a zero second operand.
        for op in 0x99..=0x9e_u8 {
            assert_eq!(
                super::if_zero_taken(op, v),
                super::if_icmp_taken(op + 6, v, 0)
            );
        }
    }
}
