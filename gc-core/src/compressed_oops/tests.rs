use super::*;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;

// -- mode determination -------------------------------------------------

#[test]
fn zero_based_small_heap() {
    let c = CompressedOops::new(0, 2 * GB);
    assert_eq!(c.mode(), CompressedOopsMode::ZeroBased);
    assert_eq!(c.shift(), 0);
    assert!(c.is_enabled());
}

#[test]
fn zero_based_large_heap() {
    let c = CompressedOops::new(0, 16 * GB);
    assert_eq!(c.mode(), CompressedOopsMode::ZeroBased);
    assert_eq!(c.shift(), 3);
    assert!(c.is_enabled());
}

#[test]
fn heap_based_mode() {
    let base = 0x7000_0000_0000u64;
    let c = CompressedOops::new(base, 8 * GB);
    assert_eq!(c.mode(), CompressedOopsMode::HeapBased);
    assert_eq!(c.base(), base);
    assert_eq!(c.shift(), 3);
}

#[test]
fn disabled_for_large_heap() {
    let c = CompressedOops::new(0, 64 * GB);
    assert_eq!(c.mode(), CompressedOopsMode::Uncompressed);
    assert!(!c.is_enabled());
}

#[test]
fn explicitly_disabled() {
    let c = CompressedOops::disabled();
    assert!(!c.is_enabled());
    assert_eq!(c.mode(), CompressedOopsMode::Uncompressed);
}

// -- shift determination ------------------------------------------------

#[test]
fn shift_zero_for_small_heap() {
    assert_eq!(CompressedOops::determine_shift(GB), 0);
    assert_eq!(CompressedOops::determine_shift(4 * GB), 0);
}

#[test]
fn shift_three_for_medium_heap() {
    assert_eq!(CompressedOops::determine_shift(4 * GB + 1), 3);
    assert_eq!(CompressedOops::determine_shift(32 * GB), 3);
}

// -- encode / decode roundtrip ------------------------------------------

#[test]
fn roundtrip_zero_shift() {
    let c = CompressedOops::new(0, 2 * GB);
    let addr = 0x1000u64;
    let encoded = c.encode(addr);
    assert_eq!(c.decode(encoded), addr);
}

#[test]
fn roundtrip_shift_3() {
    let c = CompressedOops::new(0, 16 * GB);
    // Address must be 8-byte aligned (shift=3).
    let addr = 0x1000_0000u64;
    let encoded = c.encode(addr);
    assert_eq!(c.decode(encoded), addr);
}

#[test]
fn roundtrip_with_base() {
    let base = 0x1_0000_0000u64; // 4 GB
    let c = CompressedOops::new(base, 8 * GB);
    let addr = base + 0x800_0000u64; // base + 128 MB, 8-byte aligned
    let encoded = c.encode(addr);
    assert_eq!(c.decode(encoded), addr);
}

#[test]
fn null_encode_decode() {
    let c = CompressedOops::new(0, 4 * GB);
    let encoded = c.encode(0);
    assert_eq!(encoded, CompressedOop::NULL);
    assert!(encoded.is_null());
    assert_eq!(c.decode(CompressedOop::NULL), 0);
}

// -- NarrowKlass --------------------------------------------------------

#[test]
fn klass_roundtrip() {
    let base = 0x8_0000_0000u64;
    let c = CompressedOops::new(base, 8 * GB);
    let klass_addr = base + 0x100_0000u64; // base + 16 MB, 8-aligned
    let nk = c.encode_klass(klass_addr);
    assert_eq!(c.decode_klass(nk), klass_addr);
}

#[test]
fn klass_null() {
    let c = CompressedOops::new(0, 4 * GB);
    let nk = c.encode_klass(0);
    assert!(nk.is_null());
    assert_eq!(c.decode_klass(NarrowKlass::NULL), 0);
}

// -- is_encodable -------------------------------------------------------

#[test]
fn encodable_valid_address() {
    let c = CompressedOops::new(0, 4 * GB);
    assert!(c.is_encodable(0));
    assert!(c.is_encodable(0x1000));
    assert!(c.is_encodable(4 * GB - 8));
}

#[test]
fn encodable_out_of_range() {
    let base = 0x1_0000_0000u64;
    let c = CompressedOops::new(base, 8 * GB);
    // Address below base is not encodable.
    assert!(!c.is_encodable(base - 1));
}

/// The base itself encodes to narrow 0, which is null — so it must be
/// refused, not accepted. Accepting it made `encode(base)` a silent
/// reference-to-null conversion that `decode` could not undo, and it
/// disagreed with `cratonvm_types::narrow_oop::is_encodable`, which has
/// always spelled this `addr <= base`.
#[test]
fn the_heap_base_itself_is_not_encodable_because_it_would_read_back_as_null() {
    let base = 0x1_0000_0000u64;
    let c = CompressedOops::new(base, 8 * GB);
    assert!(!c.is_encodable(base));
    // `encode(base)` is deliberately NOT exercised: the release fail-safe
    // returns `CompressedOop::NULL` for it, but the debug_assert above that
    // fail-safe fires first in a test build, and asserting on a panic would
    // be a test of the assertion rather than of the predicate.
    // The first encodable address is one object-alignment step above it.
    assert!(c.is_encodable(base + CompressedOops::OBJECT_ALIGNMENT));
}

#[test]
fn not_encodable_when_disabled() {
    let c = CompressedOops::disabled();
    assert!(!c.is_encodable(0x1000));
}

// -- CompressedOopArray -------------------------------------------------

#[test]
fn array_basic_ops() {
    let mut arr = CompressedOopArray::new(10);
    assert_eq!(arr.len(), 10);
    assert!(!arr.is_empty());

    // Default is null.
    assert!(arr.get(0).is_null());

    arr.set(3, CompressedOop::from_raw(42));
    assert_eq!(arr.get(3).raw(), 42);
}

#[test]
fn array_savings() {
    let arr = CompressedOopArray::new(1000);
    // Each entry saves 4 bytes.
    assert_eq!(arr.savings_bytes(), 4000);
}

#[test]
fn array_empty() {
    let arr = CompressedOopArray::new(0);
    assert!(arr.is_empty());
    assert_eq!(arr.savings_bytes(), 0);
}

// -- constants ----------------------------------------------------------

#[test]
fn max_heap_constant() {
    assert_eq!(CompressedOops::MAX_HEAP_FOR_COMPRESSED, 32 * GB);
}

#[test]
fn max_heap_zero_shift_constant() {
    assert_eq!(CompressedOops::MAX_HEAP_ZERO_SHIFT, 4 * GB);
}

#[test]
fn object_alignment_constant() {
    assert_eq!(CompressedOops::OBJECT_ALIGNMENT, 8);
}

// -- stats / savings estimation -----------------------------------------

#[test]
fn stats_snapshot() {
    let c = CompressedOops::new(0, 8 * GB);
    let s = c.stats(8 * GB);
    assert_eq!(s.mode, CompressedOopsMode::ZeroBased);
    assert_eq!(s.shift, 3);
    assert_eq!(s.heap_base, 0);
    assert_eq!(s.heap_size, 8 * GB);
    assert!(s.estimated_savings_percent > 0.0);
}

/// The estimate is a *ceiling*, not the HotSpot 20-30 % point estimate that
/// used to live here — CratonVM's header (16 bytes, 32 before 2026-08-06)
/// makes a per-heap-size figure meaningless. Pins that it no longer varies
/// with heap size.
#[test]
fn savings_is_a_size_independent_ceiling() {
    assert_eq!(
        CompressedOops::estimate_savings_percent(2 * GB),
        CompressedOops::MAX_SAVINGS_PERCENT
    );
    assert_eq!(
        CompressedOops::estimate_savings_percent(16 * GB),
        CompressedOops::MAX_SAVINGS_PERCENT
    );
    assert_eq!(CompressedOops::MAX_SAVINGS_PERCENT, 50.0);
}

#[test]
fn savings_large_heap() {
    assert_eq!(CompressedOops::estimate_savings_percent(64 * GB), 0.0);
}

/// The documented footprint table is the load-bearing part of the savings
/// story, so compute it rather than trusting the prose. `round8` models the
/// 8-byte object alignment that erases the saving on one-reference objects.
#[test]
fn documented_object_footprint_arithmetic_holds() {
    // gc-common w3-e: was a hard-coded 32, the pre-2026-08-06 header.
    const HEADER: usize = cratonvm_types::HEADER_SIZE;
    assert_eq!(HEADER, 16);
    assert_eq!(cratonvm_types::ARRAY_DATA_OFFSET, HEADER);
    fn round8(n: usize) -> usize {
        (n + 7) & !7
    }
    // (wide body bytes before rounding, narrow body bytes) -> (wide, narrow)
    fn total(wide_body: usize, narrow_body: usize) -> (usize, usize) {
        (HEADER + round8(wide_body), HEADER + round8(narrow_body))
    }

    // String {byte[] value, byte coder, int hash}: 8+4+1=13 vs 4+4+1=9.
    // Both round to 16 -- compressed oops save NOTHING here.
    assert_eq!(total(13, 9), (32, 32));

    // ArrayList {Object[] elementData, int size, int modCount}: same shape.
    assert_eq!(total(16, 12), (32, 32));

    // Binary-tree node {left, right}: 16 -> 8.
    assert_eq!(total(16, 8), (32, 24));

    // HashMap.Node {int hash, K key, V value, Node next}: 4+pad4+8+8+8 = 32
    // vs 4+4+4+4 = 16.
    assert_eq!(total(32, 16), (48, 32));

    // Reference arrays are the real win and are not eroded by rounding.
    assert_eq!((HEADER + 8 * 16, HEADER + 4 * 16), (144, 80));
    // ...and approach the 50 % ceiling as length grows.
    let big = 1_000_000usize;
    let wide = HEADER + 8 * big;
    let narrow = HEADER + 4 * big;
    let pct = 100.0 * (wide - narrow) as f64 / wide as f64;
    assert!(pct > 49.9 && pct < CompressedOops::MAX_SAVINGS_PERCENT);
}

// -- multiple addresses -------------------------------------------------

#[test]
fn multiple_encode_decode() {
    let c = CompressedOops::new(0, 16 * GB);
    let addrs: Vec<u64> = (1..=100).map(|i| i * 8).collect(); // all 8-aligned
    for &a in &addrs {
        assert_eq!(c.decode(c.encode(a)), a);
    }
}

// -- edge: address at max boundary --------------------------------------

#[test]
fn address_at_max_boundary_zero_shift() {
    let c = CompressedOops::new(0, 4 * GB);
    // Largest encodable: u32::MAX since shift=0
    let addr = u32::MAX as u64;
    let encoded = c.encode(addr);
    assert_eq!(c.decode(encoded), addr);
}

#[test]
fn address_at_max_boundary_shift_3() {
    let c = CompressedOops::new(0, 32 * GB);
    // Largest encodable: u32::MAX << 3
    let addr = (u32::MAX as u64) << 3;
    assert!(c.is_encodable(addr));
    let encoded = c.encode(addr);
    assert_eq!(c.decode(encoded), addr);
}

// -- mode determination helper ------------------------------------------

#[test]
fn determine_mode_uncompressed() {
    assert_eq!(
        CompressedOops::determine_mode(0, 64 * GB),
        CompressedOopsMode::Uncompressed
    );
}

#[test]
fn determine_mode_zero_based() {
    assert_eq!(
        CompressedOops::determine_mode(0, 4 * GB),
        CompressedOopsMode::ZeroBased
    );
}

#[test]
fn determine_mode_heap_based() {
    assert_eq!(
        CompressedOops::determine_mode(0x1000, 4 * GB),
        CompressedOopsMode::HeapBased
    );
}

// -- CompressedOop / NarrowKlass structs ---------------------------------

#[test]
fn compressed_oop_from_raw() {
    let oop = CompressedOop::from_raw(0xDEAD);
    assert_eq!(oop.raw(), 0xDEAD);
    assert!(!oop.is_null());
}

#[test]
fn narrow_klass_from_raw() {
    let nk = NarrowKlass::from_raw(0xBEEF);
    assert_eq!(nk.raw(), 0xBEEF);
    assert!(!nk.is_null());
}
