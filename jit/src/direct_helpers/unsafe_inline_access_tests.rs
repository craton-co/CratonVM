use super::*;

#[test]
fn plain_rows_name_their_own_gate_bit() {
    // Rows 14 and 15 (nio21 wave 5 L3) are the CAS gates the RMW rows
    // ask; no accessor names them (`nio21_w5_rmw_rows_ask_the_cas_bits`).
    for (bit, (name, desc)) in UnsafeInlineAccess::GATE_BITS.iter().enumerate().take(14) {
        let a = unsafe_inline_access_site("jdk/internal/misc/Unsafe", name, desc)
            .unwrap_or_else(|| panic!("{name}{desc} must be recognised"));
        assert!(!a.unaligned);
        assert_eq!(a.plain_bit(), 1usize << bit, "{name}{desc}");
        assert!(a.admitted(1usize << bit));
        assert!(!a.admitted(!(1usize << bit) & 0x3FF));
    }
}

#[test]
fn unaligned_forms_need_every_accessor_of_their_direction() {
    let g = unsafe_inline_access_site(
        "jdk/internal/misc/Unsafe",
        "getIntUnaligned",
        "(Ljava/lang/Object;JZ)I",
    )
    .unwrap();
    assert!(g.unaligned && g.big_endian_operand && !g.is_put && g.width == 4);
    assert_eq!(g.num_params(), 3);
    assert!(g.admitted(0b11111));
    assert!(!g.admitted(0b11011));
    let p = unsafe_inline_access_site(
        "jdk/internal/misc/Unsafe",
        "putLongUnaligned",
        "(Ljava/lang/Object;JJZ)V",
    )
    .unwrap();
    assert!(p.unaligned && p.big_endian_operand && p.is_put && p.width == 8);
    assert_eq!(p.num_params(), 4);
    assert!(p.admitted(0b11111 << 5));
    assert!(!p.admitted(0b11111));
}

/// `ir_lower`'s inline prefix scales the array length by
/// `element_type & 3` and accepts the tag bytes 0x11..=0x2D as "primitive
/// array"; both are layout facts, pinned here against the layout's own
/// table.
#[test]
fn element_tag_low_bits_are_log2_of_the_element_size() {
    use cratonvm_types::{element_byte_size, ArrayElementType as E, ObjectKind};
    for et in [
        E::Boolean,
        E::Char,
        E::Float,
        E::Double,
        E::Byte,
        E::Short,
        E::Int,
        E::Long,
    ] {
        let tag = et as u8;
        assert_eq!(1usize << (tag & 3), element_byte_size(et), "{et:?}");
        let byte = (ObjectKind::Array as u8) | (tag << 2);
        assert!((0x11..=0x2D).contains(&byte), "{et:?} tag byte {byte:#x}");
    }
    let reference = (ObjectKind::Array as u8) | ((E::Reference as u8) << 2);
    assert!(reference < 0x11);
}

#[test]
fn adjacent_shapes_are_not_recognised() {
    // JIT round 20 wave 12, lane qUnsafe (L2-2): these four are rows now,
    // unless `CRATONVM_JIT_UNSAFE_QU_EXTRA_ROWS=0`.
    let extra = unsafe_qu_extra_rows_enabled();
    let vol = extra && unsafe_volatile_inline_enabled();
    for (m, d, want) in [
        ("getBoolean", "(Ljava/lang/Object;J)Z", extra),
        ("getFloat", "(Ljava/lang/Object;J)F", extra),
        ("getIntAcquire", "(Ljava/lang/Object;J)I", vol),
        ("putLongRelease", "(Ljava/lang/Object;JJ)V", vol),
    ] {
        assert_eq!(
            unsafe_inline_access_site("jdk/internal/misc/Unsafe", m, d).is_some(),
            want,
            "{m}{d}"
        );
    }
    for (c, m, d) in [
        (
            "jdk/internal/misc/Unsafe",
            "getBooleanVolatile",
            "(Ljava/lang/Object;J)Z",
        ),
        (
            "jdk/internal/misc/Unsafe",
            "getFloatVolatile",
            "(Ljava/lang/Object;J)F",
        ),
        (
            "jdk/internal/misc/Unsafe",
            "putDoubleRelease",
            "(Ljava/lang/Object;JD)V",
        ),
        (
            "jdk/internal/misc/Unsafe",
            "getShortVolatile",
            "(Ljava/lang/Object;J)S",
        ),
        (
            "jdk/internal/misc/Unsafe",
            "getReferenceVolatile",
            "(Ljava/lang/Object;J)Ljava/lang/Object;",
        ),
        (
            "jdk/internal/misc/Unsafe",
            "compareAndSetInt",
            "(Ljava/lang/Object;JII)Z",
        ),
        ("jdk/internal/misc/Unsafe", "getInt", "(J)I"),
        (
            "jdk/internal/misc/Unsafe",
            "getReference",
            "(Ljava/lang/Object;J)Ljava/lang/Object;",
        ),
        ("sun/misc/Unsafe", "getInt", "(Ljava/lang/Object;J)I"),
        (
            "sun/misc/Unsafe",
            "getIntVolatile",
            "(Ljava/lang/Object;J)I",
        ),
        (
            "jdk/internal/misc/ScopedMemoryAccess",
            "getIntUnaligned",
            "(Ljava/lang/Object;JZ)I",
        ),
    ] {
        assert!(unsafe_inline_access_site(c, m, d).is_none(), "{c}.{m}{d}");
    }
    assert_eq!(DirectHelperTable::EMPTY.unsafe_inline_admitted, 0);
}

/// nio21 wave 4 L4: the four volatile rows are recognised as aligned-only
/// volatile accesses of their width and direction, own bits 10..14, and
/// are not admitted by a mask of the plain accessors' bits.
#[test]
fn nio21_w4_volatile_rows_own_bits_ten_to_fourteen() {
    let rows = [
        (
            "getIntVolatile",
            "(Ljava/lang/Object;J)I",
            4u8,
            false,
            10usize,
        ),
        ("getLongVolatile", "(Ljava/lang/Object;J)J", 8, false, 11),
        ("putIntVolatile", "(Ljava/lang/Object;JI)V", 4, true, 12),
        ("putLongVolatile", "(Ljava/lang/Object;JJ)V", 8, true, 13),
    ];
    for (m, d, width, is_put, bit) in rows {
        let a = unsafe_inline_access_site("jdk/internal/misc/Unsafe", m, d)
            .unwrap_or_else(|| panic!("{m}{d} must be recognised"));
        assert!(a.volatile && !a.unaligned && !a.big_endian_operand, "{m}");
        assert_eq!((a.width, a.is_put), (width, is_put), "{m}");
        assert_eq!(a.num_params(), 2 + is_put as usize, "{m}");
        assert_eq!(UnsafeInlineAccess::GATE_BITS[bit], (m, d));
        assert!(a.admitted(1 << bit), "{m}");
        assert!(
            !a.admitted(0x3FF),
            "{m}: the plain rows' bits must not admit it"
        );
    }
    // The plain rows are not volatile.
    let p = unsafe_inline_access_site(
        "jdk/internal/misc/Unsafe",
        "putInt",
        "(Ljava/lang/Object;JI)V",
    )
    .unwrap();
    assert!(!p.volatile);
}
