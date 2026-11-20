use super::*;

#[test]
fn r21_unrolled_fold_equals_the_scalar_fold() {
    let mut bytes = vec![0u8; 4 * 41];
    let mut x: u32 = 0x9E37_79B9;
    for b in bytes.iter_mut() {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *b = (x >> 24) as u8;
    }
    for (step, basic_type) in [
        (1usize, VH_T_BOOLEAN),
        (1, VH_T_BYTE),
        (2, VH_T_CHAR),
        (2, VH_T_SHORT),
        (4, VH_T_INT),
    ] {
        for elems in 0..=41usize {
            for seed in [0i32, 1, -1, i32::MIN, 0x1234_5678] {
                let slice = &bytes[..elems * step];
                assert_eq!(
                    vectorized_hash_fold_unrolled(slice, step, basic_type, seed),
                    vectorized_hash_fold(slice, step, basic_type, seed),
                    "step {step} type {basic_type} elems {elems} seed {seed}"
                );
            }
        }
    }
    // The refusal is the scalar fold's too.
    assert_eq!(vectorized_hash_fold_unrolled(&[1, 2], 2, VH_T_INT, 1), None);
}
