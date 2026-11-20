use super::*;
use cratonvm_types::flags::with_thread_overrides;

#[test]
fn equality_masks_preserve_every_unaligned_lane() {
    with_thread_overrides(
        &[("CRATONVM_JIT", None), ("CRATONVM_AVX512", Some("1"))],
        || {
            for expected in [0u16, 0xffff, 0xaaaa, 0x8001, 0x00ff] {
                let mut a = vec![0i32; 18];
                let mut b = vec![0i32; 18];
                for i in 0..16 {
                    a[i + 1] = (i as i32).wrapping_mul(1_000_003).wrapping_add(i32::MIN);
                    b[i + 1] = if expected & (1 << i) != 0 {
                        a[i + 1]
                    } else {
                        a[i + 1] ^ 0x55555555
                    };
                }
                let a: &[i32; 16] = a[1..17].try_into().unwrap();
                let b: &[i32; 16] = b[1..17].try_into().unwrap();
                let got = mask(a, b);
                #[cfg(target_arch = "x86_64")]
                if std::arch::is_x86_feature_detected!("avx512f") {
                    assert_eq!(got, Some(expected));
                    continue;
                }
                assert_eq!(got, None);
            }
        },
    );
}

#[test]
fn master_veto_declines_hardware_equality() {
    with_thread_overrides(
        &[("CRATONVM_JIT", None), ("CRATONVM_AVX512", Some("0"))],
        || {
            assert_eq!(mask(&[0; 16], &[0; 16]), None);
        },
    );
}
