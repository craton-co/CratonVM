use super::{index_of_units_from, last_index_of_units_from};

fn scalar_index_of(haystack: &[u16], needle: &[u16], from: i32) -> i32 {
    let n = haystack.len() as i32;
    let m = needle.len() as i32;
    if from >= n {
        return if m == 0 { n } else { -1 };
    }
    let from = from.max(0);
    if m == 0 {
        return from;
    }
    if m > n - from {
        return -1;
    }
    (from..=n - m)
        .find(|&i| haystack[i as usize..i as usize + needle.len()] == *needle)
        .unwrap_or(-1)
}

#[test]
fn avx512_candidate_search_matches_scalar_across_blocks_and_tails() {
    let mut haystack: Vec<u16> = (0..137).map(|i| (i % 251 + 1) as u16).collect();
    // Include UTF-16 values that cannot be represented as Unicode scalar values.
    haystack[65] = 0xD800;
    haystack[66] = 0xDC00;
    for from in [-7, 0, 1, 31, 32, 63, 64, 65, 95, 128, 137, 200] {
        for needle in [vec![5], vec![0xD800, 0xDC00], vec![777], vec![]] {
            let expected = scalar_index_of(&haystack, &needle, from);
            assert_eq!(index_of_units_from(&haystack, &needle, from), expected);
            let disabled = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_AVX512", Some("0"))],
                || index_of_units_from(&haystack, &needle, from),
            );
            assert_eq!(
                disabled, expected,
                "disabled: from={from} needle={needle:?}"
            );
        }
    }
}

fn scalar_last_index_of(haystack: &[u16], needle: &[u16], from: i64) -> i32 {
    if needle.is_empty() {
        return haystack.len() as i32;
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let mut i = from.min((haystack.len() - needle.len()) as i64);
    while i >= 0 {
        let at = i as usize;
        if haystack[at..at + needle.len()] == *needle {
            return i as i32;
        }
        i -= 1;
    }
    -1
}

#[test]
fn avx512_reverse_search_matches_scalar_across_blocks_and_tails() {
    let mut haystack: Vec<u16> = (0..145).map(|i| (i % 251 + 1) as u16).collect();
    haystack[63] = 0xD800;
    haystack[64] = 0xDC00;
    haystack[110] = 5;
    for from in [-1, 0, 1, 31, 32, 63, 64, 95, 127, 144, 200] {
        for needle in [vec![5], vec![0xD800, 0xDC00], vec![777], vec![]] {
            let expected = scalar_last_index_of(&haystack, &needle, from);
            assert_eq!(last_index_of_units_from(&haystack, &needle, from), expected);
            let disabled = cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_AVX512", Some("0"))],
                || last_index_of_units_from(&haystack, &needle, from),
            );
            assert_eq!(disabled, expected, "disabled: from={from} needle={needle:?}");
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn avx512_mask_marks_every_matching_lane() {
    if !std::is_x86_feature_detected!("avx512f") || !std::is_x86_feature_detected!("avx512bw") {
        return;
    }
    let mut block = [9u16; 32];
    block[0] = 7;
    block[17] = 7;
    block[31] = 7;
    // SAFETY: this test runs only on AVX-512F/BW hosts and passes a full block.
    let mask = unsafe { super::avx512_u16_equal_mask(block.as_ptr(), 7) };
    assert_eq!(mask, (1 << 0) | (1 << 17) | (1 << 31));
}
