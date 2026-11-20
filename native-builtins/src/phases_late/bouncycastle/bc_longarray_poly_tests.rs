use super::{bc_poly_inverse, bc_poly_mul_raw, bc_poly_reduce};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn sect131r1_longarray_inverse_matches_bc() {
    let m = 131;
    let ks = [2usize, 3, 8];
    let x = vec![0x0f9c_1813_4363_8399, 0x81ba_f91f_df98_33c4, 0];
    let expected = vec![
        0x4bae_8672_4f5c_7253,
        0xcdf6_616b_d803_be96,
        0x0000_0000_0000_0003,
    ];

    let inv = bc_poly_inverse(&x, m, &ks).expect("sect131r1 x must be invertible");
    assert_eq!(expected, inv);
    assert_eq!(vec![1], bc_poly_reduce(bc_poly_mul_raw(&x, &inv), m, &ks));

    let x_trimmed = vec![0x0f9c_1813_4363_8399, 0x81ba_f91f_df98_33c4];
    let inv = bc_poly_inverse(&x_trimmed, m, &ks).expect("trimmed x must be invertible");
    assert_eq!(expected, inv);
    assert_eq!(
        vec![1],
        bc_poly_reduce(bc_poly_mul_raw(&x_trimmed, &inv), m, &ks)
    );
}
