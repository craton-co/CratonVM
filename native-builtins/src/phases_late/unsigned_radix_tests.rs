use super::*;
use crate::test_utils::mock_ctx;
// NativeHeapAccess carries `read_string`; without it in scope, method
// resolution on the concrete MockNativeContext fails.
use cratonvm_native_api::{NativeContext, NativeHeapAccess, NativeMethodRegistry};

fn registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_p71_wrapper_extras(&mut r);
    r
}

fn int_unsigned(v: i32, radix: i32) -> String {
    let r = registry();
    let f = r
        .find(
            "java/lang/Integer",
            "toUnsignedString",
            "(II)Ljava/lang/String;",
        )
        .expect("Integer.toUnsignedString(II) must be registered");
    let mut ctx = mock_ctx();
    match f(&mut ctx, &[Value::Int(v), Value::Int(radix)]).expect("never throws") {
        Some(Value::Object(Some(o))) => ctx.read_string(o).expect("a readable String"),
        other => panic!("expected a String, got {other:?}"),
    }
}

fn long_unsigned(v: i64, radix: i32) -> String {
    let r = registry();
    let f = r
        .find(
            "java/lang/Long",
            "toUnsignedString",
            "(JI)Ljava/lang/String;",
        )
        .expect("Long.toUnsignedString(JI) must be registered");
    let mut ctx = mock_ctx();
    match f(&mut ctx, &[Value::Long(v), Value::Int(radix)]).expect("never throws") {
        Some(Value::Object(Some(o))) => ctx.read_string(o).expect("a readable String"),
        other => panic!("expected a String, got {other:?}"),
    }
}

/// The exact case `clamp(2, 36)` got wrong: radix 0 clamps UP to 2 and
/// prints binary, where the JDK substitutes 10 and prints "255".
#[test]
fn out_of_range_radix_substitutes_ten_and_never_clamps() {
    for bad in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
        assert_eq!(int_unsigned(255, bad), "255", "radix {bad}");
        assert_eq!(int_unsigned(5, bad), "5", "radix {bad}");
        assert_eq!(int_unsigned(0, bad), "0", "radix {bad}");
        // Unsigned: -1 is 2^32-1, and MIN_VALUE is 2^31 — no "-" sign.
        assert_eq!(int_unsigned(-1, bad), "4294967295", "radix {bad}");
        assert_eq!(int_unsigned(i32::MIN, bad), "2147483648", "radix {bad}");
        assert_eq!(long_unsigned(255, bad), "255", "radix {bad}");
        assert_eq!(
            long_unsigned(-1, bad),
            "18446744073709551615",
            "radix {bad}"
        );
        assert_eq!(
            long_unsigned(i64::MIN, bad),
            "9223372036854775808",
            "radix {bad}"
        );
    }
    // The two answers the old clamp produced, named so they cannot come
    // back looking plausible: radix 0 -> 2, radix -1 -> 36.
    assert_ne!(int_unsigned(255, 0), "11111111");
    assert_ne!(int_unsigned(255, -1), "73");
}

/// Legal radices, including both boundaries and the unsigned wrap.
#[test]
fn legal_radices_including_both_boundaries() {
    assert_eq!(int_unsigned(255, 2), "11111111");
    assert_eq!(int_unsigned(255, 8), "377");
    assert_eq!(int_unsigned(255, 16), "ff");
    assert_eq!(int_unsigned(255, 36), "73");
    assert_eq!(int_unsigned(-1, 16), "ffffffff");
    assert_eq!(int_unsigned(-1, 36), "1z141z3");
    assert_eq!(int_unsigned(i32::MIN, 36), "zik0zk");
    assert_eq!(long_unsigned(-1, 16), "ffffffffffffffff");
    assert_eq!(long_unsigned(-1, 36), "3w5e11264sgsf");
    assert_eq!(long_unsigned(255, 2), "11111111");
    assert_eq!(long_unsigned(255, 36), "73");
}

/// BOUNDED ON PURPOSE. `p71_fmt_radix` indexes a 36-byte digit table with
/// `v % radix`, divides by `radix`, and loops while `v > 0`: radix > 36
/// panics on the index, radix 0 divides by zero, radix 1 never terminates.
/// The old `clamp` was the only thing standing between ordinary Java code
/// and all three, and it is now gone — so this drives them on a worker
/// against a deadline, where a hang is a timeout and an abort is a channel
/// disconnect rather than a wedged suite.
#[test]
fn hostile_radices_terminate_within_a_deadline() {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let out = vec![
            p71_fmt_radix(255, 0),
            p71_fmt_radix(255, 1),
            p71_fmt_radix(255, -1),
            p71_fmt_radix(255, 37),
            p71_fmt_radix(255, i32::MIN),
            p71_fmt_radix(255, i32::MAX),
            p71_fmt_radix(0, 1),
            p71_fmt_radix(u64::MAX, 1),
            int_unsigned(255, 0),
            int_unsigned(255, 1),
            long_unsigned(255, 40),
        ];
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("radix 0/1/negative/>36 must terminate and must not abort");
    assert_eq!(
        out,
        vec![
            "255",
            "255",
            "255",
            "255",
            "255",
            "255",
            "0",
            "18446744073709551615",
            "255",
            "255",
            "255",
        ]
    );
    worker.join().expect("worker thread panicked");
}

/// `parseUnsignedInt` / `parseUnsignedLong` invert the rule: an
/// out-of-range radix is a `NumberFormatException`, not a substitution.
/// Bounded because the clamp that was removed here was also what kept
/// `from_str_radix`'s 2..=36 assertion from firing.
#[test]
fn parse_unsigned_rejects_hostile_radices_within_a_deadline() {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let r = registry();
        let pi = r
            .find(
                "java/lang/Integer",
                "parseUnsignedInt",
                "(Ljava/lang/String;I)I",
            )
            .expect("Integer.parseUnsignedInt(String,int) must be registered");
        let pl = r
            .find(
                "java/lang/Long",
                "parseUnsignedLong",
                "(Ljava/lang/String;I)J",
            )
            .expect("Long.parseUnsignedLong(String,int) must be registered");
        let mut ctx = mock_ctx();
        let s = ctx.create_string("5");
        let mut threw = Vec::new();
        for radix in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
            let args = [Value::Object(Some(s)), Value::Int(radix)];
            threw.push(pi(&mut ctx, &args).is_err());
            threw.push(pl(&mut ctx, &args).is_err());
        }
        // Legal radices still parse.
        let legal = [2i32, 10, 36]
            .iter()
            .map(|&radix| {
                let args = [Value::Object(Some(s)), Value::Int(radix)];
                pi(&mut ctx, &args).is_ok() && pl(&mut ctx, &args).is_ok()
            })
            .collect::<Vec<_>>();
        let _ = tx.send((threw, legal));
    });
    let (threw, legal) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("parseUnsigned* with a bad radix must return Err, not abort");
    assert!(
        threw.iter().all(|&t| t),
        "every out-of-range radix must throw NumberFormatException, got {threw:?}"
    );
    assert_eq!(
        legal,
        vec![true, true, true],
        "legal radices must still parse"
    );
    worker.join().expect("worker thread panicked");
}
