use super::decimal_digits_latin1_into;

/// Run the writer over a `#`-filled buffer, returning its answer and the
/// buffer afterwards.
fn run(value: i64, index: i64, len: usize) -> (Option<i64>, Vec<u8>) {
    let mut buf = vec![b'#'; len];
    let r = decimal_digits_latin1_into(value, index, &mut buf);
    (r, buf)
}

/// The bytes `DecimalDigits.uncheckedGetCharsLatin1` writes are the
/// decimal rendering (`Integer.toString` / `Long.toString`), ending at
/// `index`, and nothing else is touched; the answer is the new `charPos`.
#[test]
fn writes_exactly_the_decimal_rendering_ending_at_index() {
    let mut values: Vec<i64> = vec![
        0,
        1,
        -1,
        9,
        10,
        -10,
        99,
        100,
        -100,
        12_345,
        -98_765,
        i64::from(i32::MIN),
        i64::from(i32::MAX),
        i64::from(i32::MIN) + 1,
        i64::MIN,
        i64::MAX,
        i64::MIN + 1,
    ];
    let mut p: i64 = 1;
    for _ in 0..18 {
        values.extend([p - 1, p, p + 1, -(p - 1), -p, -(p + 1)]);
        p *= 10;
    }
    for v in values {
        let text = v.to_string();
        let n = text.len() as i64;
        let (r, buf) = run(v, 25, 30);
        assert_eq!(r, Some(25 - n), "charPos for {v}");
        let start = (25 - n) as usize;
        assert_eq!(&buf[start..25], text.as_bytes(), "digits of {v}");
        assert!(buf[..start].iter().all(|&b| b == b'#'), "before {v}");
        assert!(buf[25..].iter().all(|&b| b == b'#'), "after {v}");
    }
}

/// A dense sweep against Rust's own rendering, the int overload's range
/// edges included.
#[test]
fn agrees_with_to_string_over_a_dense_sweep() {
    for v in (-200_000i64..200_000).chain([i64::from(i32::MIN), i64::from(i32::MAX)]) {
        let text = v.to_string();
        let (r, buf) = run(v, text.len() as i64, text.len());
        assert_eq!(r, Some(0), "{v}");
        assert_eq!(buf, text.as_bytes(), "{v}");
    }
}

/// Every range the JDK body would write outside the array is declined
/// with nothing written: the caller then runs the real bytecode.
#[test]
fn an_out_of_range_write_is_declined_untouched() {
    for (value, index, len) in [
        (12_345i64, 4i64, 10usize), // digits would start at -1
        (-1, 1, 10),                // "-1" needs two bytes before index 1
        (7, 11, 10),                // index past the end
        (7, -1, 10),                // negative index
        (i64::MIN, 19, 19),         // 20 bytes into 19
        (0, 0, 0),                  // one digit into nothing
    ] {
        let (r, buf) = run(value, index, len);
        assert_eq!(r, None, "{value} at {index} in {len}");
        assert!(buf.iter().all(|&b| b == b'#'), "{value}: buffer written");
    }
    // The exact fit is served.
    assert_eq!(run(i64::MIN, 20, 20).0, Some(0));
    assert_eq!(run(-7, 2, 2), (Some(0), b"-7".to_vec()));
}
