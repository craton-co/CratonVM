use super::*;

const U: &str = "jdk/internal/misc/Unsafe";

#[test]
fn r20_qu_extra_rows_own_bits_nineteen_to_twenty_five() {
    if !unsafe_qu_extra_rows_enabled() {
        return;
    }
    for (i, (m, d)) in UNSAFE_QU_EXTRA_GATE_BITS.iter().enumerate() {
        let a = unsafe_inline_access_site(U, m, d).unwrap_or_else(|| panic!("{m}{d}"));
        let bit = 1usize << (UNSAFE_QU_EXTRA_FIRST_BIT + i);
        assert_eq!(a.plain_bit(), bit, "{m}");
        assert!(a.admitted(bit) && !a.admitted(0xFFFF), "{m}");
        assert!(a.heap_only() && !a.volatile && !a.ordered_store, "{m}");
        assert_eq!(a.is_put, i % 2 == 1, "{m}");
        assert_eq!(a.num_params(), 2 + (i % 2), "{m}");
    }
    let b = unsafe_inline_access_site(U, "getBoolean", "(Ljava/lang/Object;J)Z").unwrap();
    assert_eq!((b.width, b.value_kind, b.signed), (1, b'Z', true));
    let f = unsafe_inline_access_site(U, "getFloat", "(Ljava/lang/Object;J)F").unwrap();
    assert!(f.is_fp() && f.width == 4);
    let d = unsafe_inline_access_site(U, "putDouble", "(Ljava/lang/Object;JD)V").unwrap();
    assert!(d.is_fp() && d.width == 8 && d.is_put);
    // The extra bits sit past the CAE rows and the reference CAS row.
    assert_eq!(
        UNSAFE_QU_EXTRA_FIRST_BIT,
        UnsafeInlineRmw::CAE_FIRST_BIT + 3
    );
    assert_eq!(UNSAFE_REF_CAS_GATE_BIT, UnsafeInlineRmw::CAE_FIRST_BIT + 2);
}

#[test]
fn r20_qu_ordered_rows_are_the_volatile_rows_with_a_plain_release_store() {
    if !unsafe_qu_extra_rows_enabled() || !unsafe_volatile_inline_enabled() {
        return;
    }
    for (m, d, vm, width, put) in [
        (
            "getIntAcquire",
            "(Ljava/lang/Object;J)I",
            "getIntVolatile",
            4u8,
            false,
        ),
        (
            "getIntOpaque",
            "(Ljava/lang/Object;J)I",
            "getIntVolatile",
            4,
            false,
        ),
        (
            "getLongAcquire",
            "(Ljava/lang/Object;J)J",
            "getLongVolatile",
            8,
            false,
        ),
        (
            "getLongOpaque",
            "(Ljava/lang/Object;J)J",
            "getLongVolatile",
            8,
            false,
        ),
        (
            "putIntRelease",
            "(Ljava/lang/Object;JI)V",
            "putIntVolatile",
            4,
            true,
        ),
        (
            "putIntOpaque",
            "(Ljava/lang/Object;JI)V",
            "putIntVolatile",
            4,
            true,
        ),
        (
            "putLongRelease",
            "(Ljava/lang/Object;JJ)V",
            "putLongVolatile",
            8,
            true,
        ),
        (
            "putLongOpaque",
            "(Ljava/lang/Object;JJ)V",
            "putLongVolatile",
            8,
            true,
        ),
    ] {
        let a = unsafe_inline_access_site(U, m, d).unwrap_or_else(|| panic!("{m}"));
        let v = unsafe_inline_access_site(U, vm, d).unwrap();
        assert_eq!((a.width, a.is_put, a.volatile), (width, put, true), "{m}");
        assert_eq!(a.ordered_store, put, "{m}");
        assert!(!v.ordered_store, "{vm}: a volatile store stays an XCHG");
        assert_eq!(a.plain_bit(), v.plain_bit(), "{m} asks {vm}'s bit");        }    }