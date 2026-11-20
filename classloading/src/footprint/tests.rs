use super::*;

#[test]
fn zs1fp_the_estimate_covers_a_real_table() {
    assert_eq!(hash_table_bytes(0, 24), 0);
    // hashbrown gives 3 usable slots in 4 buckets.
    assert_eq!(hash_table_bytes(3, 8), 4 * 9 + 16);
    let mut m: rustc_hash::FxHashMap<u32, u64> = Default::default();
    m.reserve(10_000);
    let entry = std::mem::size_of::<(u32, u64)>();
    let est = hash_table_bytes(m.capacity(), entry);
    assert!(est >= m.capacity() * entry, "{est}");
    assert!(est <= 2 * m.capacity() * (entry + 1) + 64, "{est}");
}
