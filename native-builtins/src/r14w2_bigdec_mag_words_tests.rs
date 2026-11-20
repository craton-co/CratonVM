use super::{decimal_to_mag_words, mag_words_to_decimal};

fn ref_to_words(s: &str) -> Vec<u32> {
    let mut words: Vec<u32> = Vec::new();
    for ch in s.trim_start_matches('-').chars() {
        let Some(d) = ch.to_digit(10) else { continue };
        let mut carry = u64::from(d);
        for w in words.iter_mut().rev() {
            let p = u64::from(*w) * 10 + carry;
            *w = p as u32;
            carry = p >> 32;
        }
        while carry != 0 {
            words.insert(0, carry as u32);
            carry >>= 32;
        }
    }
    words
}

fn ref_to_decimal(mag: &[u32]) -> String {
    let mut words = mag.to_vec();
    let mut digits: Vec<u8> = Vec::new();
    while words.iter().any(|&w| w != 0) {
        let mut rem = 0u64;
        for w in words.iter_mut() {
            let cur = (rem << 32) | u64::from(*w);
            *w = (cur / 10) as u32;
            rem = cur % 10;
        }
        digits.push(rem as u8);
    }
    if digits.is_empty() {
        return "0".to_string();
    }
    digits.iter().rev().map(|&d| char::from(d + b'0')).collect()
}

#[test]
fn fast_roads_match_the_digit_loops() {
    let mut st = 0x7e57_ab1e_u64;
    for len in 1..700usize {
        let s: String = (0..len)
            .map(|i| {
                st = st
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let d = ((st >> 33) % 10) as u8;
                char::from(b'0' + if i == 0 && len > 1 { d.max(1) } else { d })
            })
            .collect();
        for t in [
            s.clone(),
            format!("-{s}"),
            format!("000{s}"),
            format!("{s}.25"),
        ] {
            let words = decimal_to_mag_words(&t);
            assert_eq!(words, ref_to_words(&t), "{t}");
            assert_eq!(mag_words_to_decimal(&words), ref_to_decimal(&words), "{t}");
        }
        // A big-endian array with leading zero words reads the same.
        let mut padded = vec![0u32, 0, 0];
        padded.extend(ref_to_words(&s));
        assert_eq!(
            mag_words_to_decimal(&padded),
            ref_to_decimal(&padded),
            "{s}"
        );
    }
}
