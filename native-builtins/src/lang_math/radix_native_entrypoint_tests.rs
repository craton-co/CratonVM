use super::*;
use crate::test_utils::mock_ctx;
// `read_string` / `create_string` live on NativeHeapAccess; it is not in
// this module's prelude, so the trait must be imported for method
// resolution on the concrete MockNativeContext.
use cratonvm_native_api::NativeHeapAccess;

/// Drive a String-returning native and read the answer back out of the
/// mock heap.
fn as_text(
    f: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    args: &[Value],
) -> String {
    let mut ctx = mock_ctx();
    let out = f(&mut ctx, args).expect("this native never throws");
    match out {
        Some(Value::Object(Some(o))) => ctx.read_string(o).expect("a readable String"),
        other => panic!("expected a String result, got {other:?}"),
    }
}

fn int_to_string(val: i32, radix: i32) -> String {
    as_text(
        native_integer_to_string_radix,
        &[Value::Int(val), Value::Int(radix)],
    )
}

fn long_to_string(val: i64, radix: i32) -> String {
    as_text(
        native_long_to_string_radix,
        &[Value::Long(val), Value::Int(radix)],
    )
}

/// The `toString` family IGNORES an out-of-range radix and uses 10. It
/// does not throw, and it does not clamp into 2..=36 — a clamp would
/// answer "11111111" for `toString(255, 0)` where the JDK says "255".
#[test]
fn out_of_range_radix_substitutes_ten_and_never_clamps() {
    for bad in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
        assert_eq!(int_to_string(5, bad), "5", "radix {bad}");
        assert_eq!(int_to_string(-5, bad), "-5", "radix {bad}");
        assert_eq!(int_to_string(0, bad), "0", "radix {bad}");
        assert_eq!(int_to_string(-1, bad), "-1", "radix {bad}");
        assert_eq!(int_to_string(255, bad), "255", "radix {bad}");
        assert_eq!(int_to_string(i32::MIN, bad), "-2147483648", "radix {bad}");
        assert_eq!(int_to_string(i32::MAX, bad), "2147483647", "radix {bad}");
        assert_eq!(long_to_string(5, bad), "5", "radix {bad}");
        assert_eq!(long_to_string(-5, bad), "-5", "radix {bad}");
        assert_eq!(long_to_string(0, bad), "0", "radix {bad}");
        assert_eq!(long_to_string(-1, bad), "-1", "radix {bad}");
        assert_eq!(long_to_string(255, bad), "255", "radix {bad}");
        assert_eq!(
            long_to_string(i64::MIN, bad),
            "-9223372036854775808",
            "radix {bad}"
        );
        assert_eq!(
            long_to_string(i64::MAX, bad),
            "9223372036854775807",
            "radix {bad}"
        );
    }
    // A clamp would have produced these instead. Named so a future
    // `clamp(2, 36)` cannot pass by looking plausible.
    assert_ne!(int_to_string(255, 0), "11111111");
    assert_ne!(int_to_string(255, -1), "73");
}

/// BOUNDED ON PURPOSE. Radix 0 divides by zero and radix 1 never
/// terminates in an unguarded digit loop (it also grows the digit buffer
/// without limit), and radix > 36 aborts inside `char::from_digit`. Run on
/// a worker against a deadline so a regression FAILS this test — a hang
/// becomes a timeout and an abort becomes a channel disconnect — instead
/// of wedging or killing the whole suite.
///
/// The mock context is built INSIDE the worker: `MockNativeContext` holds
/// `UnsafeCell`s and a raw pointer, so it is not `Send`. Only `String`
/// crosses the channel.
#[test]
fn hostile_radices_terminate_within_a_deadline() {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let out = vec![
            int_to_string(5, 0),
            int_to_string(5, 1),
            int_to_string(-5, 1),
            int_to_string(255, 0),
            int_to_string(i32::MIN, 0),
            int_to_string(i32::MIN, 1),
            int_to_string(5, 40),
            int_to_string(5, -1),
            int_to_string(5, i32::MIN),
            long_to_string(5, 0),
            long_to_string(5, 1),
            long_to_string(i64::MIN, 1),
            long_to_string(i64::MIN, 40),
            long_to_string(-1, -1),
        ];
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("radix 0/1/negative/>36 must terminate and must not abort");
    assert_eq!(
        out,
        vec![
            "5",
            "5",
            "-5",
            "255",
            "-2147483648",
            "-2147483648",
            "5",
            "5",
            "5",
            "5",
            "5",
            "-9223372036854775808",
            "-9223372036854775808",
            "-1",
        ]
    );
    worker.join().expect("worker thread panicked");
}

/// Both boundary radices, and the digits either side of them.
#[test]
fn boundary_radices_two_and_thirty_six() {
    assert_eq!(int_to_string(255, 2), "11111111");
    assert_eq!(int_to_string(255, 36), "73");
    assert_eq!(int_to_string(5, 2), "101");
    assert_eq!(int_to_string(35, 36), "z");
    assert_eq!(long_to_string(255, 2), "11111111");
    assert_eq!(long_to_string(255, 36), "73");
    assert_eq!(long_to_string(35, 36), "z");
    // 37 is NOT a legal radix, so it falls back to 10 rather than
    // extending the alphabet.
    assert_eq!(int_to_string(36, 37), "36");
}

/// Negatives are sign-magnitude, never a two's-complement bit pattern.
#[test]
fn negatives_are_sign_magnitude() {
    assert_eq!(int_to_string(-1, 16), "-1");
    assert_eq!(int_to_string(-1, 2), "-1");
    assert_eq!(int_to_string(-255, 16), "-ff");
    assert_eq!(int_to_string(-5, 2), "-101");
    assert_eq!(long_to_string(-1, 16), "-1");
    assert_eq!(long_to_string(-255, 16), "-ff");
    assert_ne!(int_to_string(-1, 16), "ffffffff");
    assert_ne!(long_to_string(-1, 16), "ffffffffffffffff");
}

/// `MIN_VALUE` has no positive counterpart, so the magnitude has to be
/// taken at a wider width. Checked at EVERY legal radix, plus the two
/// literals the JDK prints at radix 36.
#[test]
fn min_value_at_every_legal_radix() {
    assert_eq!(int_to_string(i32::MIN, 36), "-zik0zk");
    assert_eq!(long_to_string(i64::MIN, 36), "-1y2p0ij32e8e8");
    assert_eq!(int_to_string(i32::MIN, 16), "-80000000");
    assert_eq!(long_to_string(i64::MIN, 16), "-8000000000000000");
    for r in 2..=36i32 {
        let s = int_to_string(i32::MIN, r);
        let mag = s.strip_prefix('-').expect("MIN_VALUE renders negative");
        assert_eq!(
            u32::from_str_radix(mag, r as u32),
            Ok(2_147_483_648u32),
            "int radix {r}"
        );
        let s = long_to_string(i64::MIN, r);
        let mag = s.strip_prefix('-').expect("MIN_VALUE renders negative");
        assert_eq!(
            u64::from_str_radix(mag, r as u32),
            Ok(9_223_372_036_854_775_808u64),
            "long radix {r}"
        );
    }
}

// -----------------------------------------------------------------------
// Character.digit / Character.forDigit — a THIRD radix contract
// -----------------------------------------------------------------------

fn digit(ch: char, radix: i32) -> i32 {
    let mut ctx = mock_ctx();
    let out = native_character_digit(&mut ctx, &[Value::Int(ch as i32), Value::Int(radix)])
        .expect("Character.digit never throws");
    match out {
        Some(Value::Int(v)) => v,
        other => panic!("expected Int, got {other:?}"),
    }
}

fn for_digit(d: i32, radix: i32) -> i32 {
    let mut ctx = mock_ctx();
    let out = native_character_for_digit(&mut ctx, &[Value::Int(d), Value::Int(radix)])
        .expect("Character.forDigit never throws");
    match out {
        Some(Value::Int(v)) => v,
        other => panic!("expected Int, got {other:?}"),
    }
}

/// Out of range answers -1 / NUL — it neither throws nor aborts. Bounded
/// for the same reason as above: the old bodies handed the radix straight
/// to `char::to_digit` / `char::from_digit`, which PANIC above 36, and a
/// negative radix arrived there as a huge `u32`.
#[test]
fn character_digit_and_for_digit_reject_hostile_radices_within_a_deadline() {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut out = Vec::new();
        for r in [-1, 0, 1, 37, 40, i32::MIN, i32::MAX] {
            out.push(digit('7', r));
            out.push(digit('z', r));
            out.push(for_digit(0, r));
            out.push(for_digit(5, r));            }
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("Character.digit/forDigit must not abort on a bad radix");
    assert!(
        out.iter().enumerate().all(|(i, &v)| {
            // digit -> -1, forDigit -> 0 (the NUL char)
            if i % 4 < 2 {
                v == -1
            } else {
                v == 0
            }
        }),
        "every out-of-range radix must answer -1 / NUL, got {out:?}"
    );
    worker.join().expect("worker thread panicked");
}

/// The in-range behaviour the guard must not have broken.
#[test]
fn character_digit_and_for_digit_in_range() {
    assert_eq!(digit('7', 10), 7);
    assert_eq!(digit('7', 16), 7);
    assert_eq!(digit('7', 36), 7);
    assert_eq!(digit('z', 36), 35);
    // 'z' is not a digit below base 36, and '7' is not one in base 2.
    assert_eq!(digit('z', 16), -1);
    assert_eq!(digit('z', 10), -1);
    assert_eq!(digit('7', 2), -1);
    assert_eq!(digit('?', 36), -1);

    assert_eq!(for_digit(0, 2), '0' as i32);
    assert_eq!(for_digit(0, 10), '0' as i32);
    assert_eq!(for_digit(5, 10), '5' as i32);
    assert_eq!(for_digit(35, 36), 'z' as i32);
    // A digit outside 0..radix is NUL even when the radix is legal.
    assert_eq!(for_digit(5, 2), 0);
    assert_eq!(for_digit(35, 16), 0);
    assert_eq!(for_digit(36, 36), 0);
    assert_eq!(for_digit(-1, 36), 0);
}

// -----------------------------------------------------------------------
// parse*(String, int) — the radix contract INVERTS here
// -----------------------------------------------------------------------

/// `parseInt` and friends THROW `NumberFormatException` on an
/// out-of-range radix; they do NOT substitute 10. Measured message text
/// included, because the two rules are one `if` apart and the message is
/// the only thing that distinguishes "bad radix" from "bad digits".
#[test]
fn parse_radix_throws_rather_than_substituting_ten() {
    assert_eq!(java_parse_radix_or_nfe(2), Ok(2));
    assert_eq!(java_parse_radix_or_nfe(10), Ok(10));
    assert_eq!(java_parse_radix_or_nfe(36), Ok(36));
    for low in [1, 0, -1, i32::MIN] {
        assert_eq!(
            java_parse_radix_or_nfe(low),
            Err(format!("radix {low} less than Character.MIN_RADIX"))
        );
    }
    for high in [37, 40, i32::MAX] {
        assert_eq!(
            java_parse_radix_or_nfe(high),
            Err(format!("radix {high} greater than Character.MAX_RADIX"))
        );
    }
}

/// `Integer.getInteger` / `Long.getLong` decode the property with
/// `Integer.decode` / `Long.decode` (JIT round 20, lane huntIntr): radix
/// prefixes count, whitespace does not. Rows measured on HotSpot 25.
#[test]
fn get_integer_property_uses_the_decode_grammar() {
    let i = |s: &str| java_decode_signed(s, i32::MIN as i64, i32::MAX as i64);
    assert_eq!(i("5"), Some(5));
    assert_eq!(i(" 5"), None);
    assert_eq!(i("5 "), None);
    assert_eq!(i("0x10"), Some(16));
    assert_eq!(i("0X1F"), Some(31));
    assert_eq!(i("#ff"), Some(255));
    assert_eq!(i("-#1"), Some(-1));
    assert_eq!(i("010"), Some(8));
    assert_eq!(i("08"), None);
    assert_eq!(i("0"), Some(0));
    assert_eq!(i("-0"), Some(0));
    assert_eq!(i("-0x10"), Some(-16));
    assert_eq!(i("+5"), Some(5));
    assert_eq!(i("0x-5"), None);
    assert_eq!(i("-"), None);
    assert_eq!(i(""), None);
    assert_eq!(i("2147483648"), None);
    assert_eq!(i("-0x80000000"), Some(i32::MIN as i64));
    assert_eq!(i("\u{661}"), Some(1));
    assert_eq!(
        java_decode_signed("9223372036854775807", i64::MIN, i64::MAX),
        Some(i64::MAX)
    );
}

/// `Double.parseDouble` / `Float.parseFloat` decide "empty String" after
/// JAVA's `trim()` (chars <= U+0020), not Rust's Unicode `trim()`: U+0001
/// is blank to Java, U+00A0 and U+2003 are not (JIT round 20, lane
/// huntIntr; `R20HuntIntrNumStr` `parse.35/37/40`).
#[test]
fn fp_parse_empty_string_uses_java_trim() {
    let msg = |r: Result<f64, cratonvm_types::error::RuntimeError>| match r {
        Err(cratonvm_types::error::RuntimeError::NumberFormatException { message }) => message,
        _ => String::from("not an NFE"),
    };
    assert_eq!(msg(parse_double_string("\u{1}")), "empty String");
    assert_eq!(
        msg(parse_double_string("\u{a0}")),
        "For input string: \"\u{a0}\""
    );
    assert_eq!(
        msg(parse_double_string("\u{2003}")),
        "For input string: \"\u{2003}\""
    );
    assert!(matches!(
        parse_float_string("\u{1}"),
        Err(cratonvm_types::error::RuntimeError::NumberFormatException { ref message })
            if message == "empty String"
    ));
}

/// Bounded: `<int>::from_str_radix` PANICS outside 2..=36, and the radix
/// used to reach it via an unchecked `as u32`, so `Integer.parseInt("5",
/// 0)` from ordinary Java bytecode aborted the VM. Each call must now come
/// back as an ordinary `Err`.
#[test]
fn parse_natives_reject_hostile_radices_within_a_deadline() {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut ctx = mock_ctx();
        let s = ctx.create_string("5");
        let mut threw = Vec::new();
        for r in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
            let args = [Value::Object(Some(s)), Value::Int(r)];
            threw.push(native_integer_parse_int_radix(&mut ctx, &args).is_err());
            threw.push(native_long_parse_long_radix(&mut ctx, &args).is_err());
            threw.push(native_byte_parse_byte_radix(&mut ctx, &args).is_err());
            threw.push(native_short_parse_short_radix(&mut ctx, &args).is_err());
        }
        // …and the legal radices still parse.
        let ok = [2i32, 10, 36].iter().all(|&r| {
            let args = [Value::Object(Some(s)), Value::Int(r)];
            // "5" is not a base-2 digit string, so only 10 and 36 parse.
            native_integer_parse_int_radix(&mut ctx, &args).is_ok() == (r != 2)
        });
        let _ = tx.send((threw, ok));
    });
    let (threw, ok) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("parse* with a bad radix must return Err, not abort");
    assert!(
        threw.iter().all(|&t| t),
        "every out-of-range radix must throw NumberFormatException, got {threw:?}"
    );
    assert!(
        ok,
        "legal radices must still parse (and base 2 must reject \"5\")"
    );
    worker.join().expect("worker thread panicked");
}
