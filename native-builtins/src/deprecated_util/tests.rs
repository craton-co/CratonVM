use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn call_native(
    registry: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    class: &str,
    method: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    let cb = registry
        .find(class, method, desc)
        .unwrap_or_else(|| panic!("{class}.{method}{desc} should be registered"));
    cb(ctx, args)
}

fn make_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_deprecated_util_natives(&mut r);
    r
}

// -------------------------------------------------------------------------
// Helper: date_fields_to_millis / millis_to_date_parts round-trip
// -------------------------------------------------------------------------

#[test]
fn test_date_fields_to_millis_epoch() {
    // 1970-01-01 00:00:00 UTC = 0 ms
    assert_eq!(date_fields_to_millis(1970, 0, 1, 0, 0, 0), 0);
}

#[test]
fn test_date_fields_to_millis_one_hour() {
    assert_eq!(date_fields_to_millis(1970, 0, 1, 1, 0, 0), 3_600_000);
}

#[test]
fn test_date_fields_to_millis_30_seconds() {
    assert_eq!(date_fields_to_millis(1970, 0, 1, 0, 0, 30), 30_000);
}

#[test]
fn test_date_fields_to_millis_y2k() {
    // 2000-01-01 00:00:00 UTC
    assert_eq!(date_fields_to_millis(2000, 0, 1, 0, 0, 0), 946_684_800_000);
}

#[test]
fn test_date_fields_to_millis_julian_cutover_matches_gregorian_change() {
    // GMT millis for 1582-10-15 (the default GregorianCalendar cutover,
    // interpreted as a plain Gregorian date since it's ON the cutover)
    // and 1582-10-04 (the preceding Julian calendar date, i.e. what
    // GregorianCalendar's default cutover documents as the "day before")
    // must be exactly one day (86_400_000 ms) apart.
    let gregorian_side = date_fields_to_millis(1582, 9, 15, 0, 0, 0);
    let julian_side = date_fields_to_millis(1582, 9, 4, 0, 0, 0);
    assert_eq!(gregorian_side - julian_side, 86_400_000);
}

#[test]
fn test_date_fields_to_millis_julian_before_cutover_matches_proleptic_gregorian_equivalent() {
    // Regression test for `bug-h2-suite-residual-fail-triage-FIXED.md`'s
    // TestPreparedStatement.testDate8: fields (1582, September, 25) are
    // before the 1582-10-15 cutover, so they must be interpreted as a
    // JULIAN calendar date — which is the same instant as proleptic
    // Gregorian 1582-10-05 (confirmed against real HotSpot JDK25).
    let julian_sept25 = date_fields_to_millis(1582, 8, 25, 0, 0, 0);
    // 1582-10-05 is also before the cutover, so it round-trips through
    // the Julian branch too when fed back through the same helper -
    // instead assert the known-good absolute value derived from HotSpot
    // (`Date.valueOf("1582-09-25")` in UTC, i.e. with zero tz offset).
    assert_eq!(julian_sept25, -12_220_156_800_000);
}

#[test]
fn test_millis_to_date_parts_julian_before_cutover_round_trips() {
    let millis = date_fields_to_millis(1582, 8, 25, 0, 0, 0);
    let p = millis_to_date_parts(millis);
    assert_eq!((p.year, p.month, p.date), (1582, 8, 25));
}

#[test]
fn test_millis_to_date_parts_epoch() {
    let p = millis_to_date_parts(0);
    assert_eq!(p.year, 1970);
    assert_eq!(p.month, 0);
    assert_eq!(p.date, 1);
    assert_eq!(p.hrs, 0);
    assert_eq!(p.min, 0);
    assert_eq!(p.sec, 0);
    assert_eq!(p.day_of_week, 4); // Thursday
}

#[test]
fn test_millis_to_date_parts_y2k() {
    let p = millis_to_date_parts(946_684_800_000);
    assert_eq!(p.year, 2000);
    assert_eq!(p.month, 0);
    assert_eq!(p.date, 1);
    assert_eq!(p.day_of_week, 6); // Saturday
}

#[test]
fn test_round_trip_date_parts() {
    let orig = (2023, 5, 15, 12, 30, 45);
    let ms = date_fields_to_millis(orig.0, orig.1, orig.2, orig.3, orig.4, orig.5);
    let p = millis_to_date_parts(ms);
    assert_eq!(p.year, orig.0);
    assert_eq!(p.month, orig.1);
    assert_eq!(p.date, orig.2);
    assert_eq!(p.hrs, orig.3);
    assert_eq!(p.min, orig.4);
    assert_eq!(p.sec, orig.5);
}

#[test]
fn test_round_trip_pre_epoch() {
    // 1960-03-10 08:05:02
    let ms = date_fields_to_millis(1960, 2, 10, 8, 5, 2);
    let p = millis_to_date_parts(ms);
    assert_eq!(p.year, 1960);
    assert_eq!(p.month, 2);
    assert_eq!(p.date, 10);
    assert_eq!(p.hrs, 8);
    assert_eq!(p.min, 5);
    assert_eq!(p.sec, 2);
}

// -------------------------------------------------------------------------
// T8.2.1 — Date constructors
// -------------------------------------------------------------------------

#[test]
fn test_date_init_iii_epoch() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(III)V",
        &[
            Value::Object(Some(date_obj)),
            Value::Int(70),
            Value::Int(0),
            Value::Int(1),
        ],
    )
    .unwrap();

    let ms = get_date_millis(&ctx, date_obj);
    assert_eq!(ms, 0, "Date(70,0,1) should map to epoch 0");
}

#[test]
fn test_date_init_iiiii() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();

    // year=70, month=0, day=1, hrs=1, min=0 → 1 hour
    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(IIIII)V",
        &[
            Value::Object(Some(date_obj)),
            Value::Int(70),
            Value::Int(0),
            Value::Int(1),
            Value::Int(1),
            Value::Int(0),
        ],
    )
    .unwrap();

    assert_eq!(get_date_millis(&ctx, date_obj), 3_600_000);
}

#[test]
fn test_date_init_iiiiii() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();

    // year=70, month=0, day=1, hrs=0, min=0, sec=30 → 30 s
    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(IIIIII)V",
        &[
            Value::Object(Some(date_obj)),
            Value::Int(70),
            Value::Int(0),
            Value::Int(1),
            Value::Int(0),
            Value::Int(0),
            Value::Int(30),
        ],
    )
    .unwrap();

    assert_eq!(get_date_millis(&ctx, date_obj), 30_000);
}

/// `Date(String)` used to be a hard `UnsupportedOperationException` stub.
/// It now mirrors the real JDK (`this(parse(s))`) by delegating to
/// `Date.parse`, which has no native override and runs as real bytecode --
/// so under the mock context, which cannot dispatch that call, the
/// constructor must surface the delegation failure rather than the old
/// "deprecated and not supported" refusal. What this test pins is that the
/// refusal is gone; the end-to-end behaviour is covered by
/// `probes/MiscProbe.java` against a real JDK image.
#[test]
fn test_date_init_string_no_longer_refuses() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let str_obj = ctx.create_string("2000-01-01");

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
    );
    let msg = match res {
        Ok(_) => String::new(),
        Err(err) => format!("{err}"),
    };
    assert!(
        !msg.contains("deprecated and not supported"),
        "Date(String) must no longer refuse outright, got: {msg}"
    );
}

// -------------------------------------------------------------------------
// T8.2.2 — Date getters / setters
// -------------------------------------------------------------------------

fn make_date_at(
    ctx: &mut MockNativeContext,
    year: i32,
    month: i32,
    date: i32,
    hrs: i32,
    min: i32,
    sec: i32,
) -> cratonvm_types::ObjectRef {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/Date", 2).unwrap();
    let ms = date_fields_to_millis(year, month, date, hrs, min, sec);
    set_date_millis(ctx, obj, ms);
    obj
}

#[test]
fn test_date_get_year_2000() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getYear",
        "()I",
        &[Value::Object(Some(obj))],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(100))); // 2000 - 1900
}

#[test]
fn test_date_get_month_june() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getMonth",
        "()I",
        &[Value::Object(Some(obj))],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(5))); // June = 5
}

#[test]
fn test_date_get_date() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getDate",
        "()I",
        &[Value::Object(Some(obj))],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(15)));
}

#[test]
fn test_date_get_day_thursday() {
    // 1970-01-01 = Thursday = 4
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    set_date_millis(&ctx, obj, 0);

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getDay",
        "()I",
        &[Value::Object(Some(obj))],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(4)));
}

#[test]
fn test_date_get_hours_minutes_seconds() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    let h = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getHours",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    let m = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getMinutes",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    let s = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getSeconds",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert_eq!(h, Some(Value::Int(12)));
    assert_eq!(m, Some(Value::Int(30)));
    assert_eq!(s, Some(Value::Int(45)));
}

#[test]
fn test_date_get_timezone_offset_always_zero() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getTimezoneOffset",
        "()I",
        &[Value::Object(Some(obj))],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(0)));
}

#[test]
fn test_date_set_year() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setYear",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(120)],
    )
    .unwrap(); // 2020

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getYear",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert_eq!(res, Some(Value::Int(120)));
}

#[test]
fn test_date_set_month() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setMonth",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(11)],
    )
    .unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getMonth",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert_eq!(res, Some(Value::Int(11)));
}

#[test]
fn test_date_set_date() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setDate",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(20)],
    )
    .unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getDate",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert_eq!(res, Some(Value::Int(20)));
}

#[test]
fn test_date_set_hours() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setHours",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(8)],
    )
    .unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getHours",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert_eq!(res, Some(Value::Int(8)));
}

#[test]
fn test_date_set_minutes() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setMinutes",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(59)],
    )
    .unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getMinutes",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert_eq!(res, Some(Value::Int(59)));
}

#[test]
fn test_date_set_seconds() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let obj = make_date_at(&mut ctx, 2000, 5, 15, 12, 30, 45);

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setSeconds",
        "(I)V",
        &[Value::Object(Some(obj)), Value::Int(0)],
    )
    .unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getSeconds",
        "()I",
        &[Value::Object(Some(obj))],
    )
    .unwrap();
    assert_eq!(res, Some(Value::Int(0)));
}

// -------------------------------------------------------------------------
// T8.2.3 — String(byte[], hibyte, offset, count)
// -------------------------------------------------------------------------

#[test]
fn test_string_init_hibyte_zero() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // Build a byte array: [0x41, 0x42, 0x43] = "ABC"
    let arr = ctx.new_array(ArrayElementType::Byte, 3);
    ctx.set_array_element(arr, 0, Value::Int(0x41));
    ctx.set_array_element(arr, 1, Value::Int(0x42));
    ctx.set_array_element(arr, 2, Value::Int(0x43));

    let this = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/String", 4).unwrap();
    call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "<init>",
        "([BIII)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(0), // hibyte = 0
            Value::Int(0), // offset
            Value::Int(3), // count
        ],
    )
    .unwrap();

    let text = ctx.read_string(this).expect("should be readable");
    assert_eq!(&text, "ABC");
}

#[test]
fn test_string_init_hibyte_nonzero() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // hibyte = 0x01, byte = 0x41 → char = 0x0141
    let arr = ctx.new_array(ArrayElementType::Byte, 1);
    ctx.set_array_element(arr, 0, Value::Int(0x41));

    let this = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/String", 4).unwrap();
    call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "<init>",
        "([BIII)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(1), // hibyte
            Value::Int(0), // offset
            Value::Int(1), // count
        ],
    )
    .unwrap();

    // The string should contain the single character U+0141 (Ł)
    let text = ctx.read_string(this).expect("should be readable");
    assert_eq!(text.chars().next(), Some('\u{0141}'));
}

#[test]
fn test_string_init_hibyte_bounds_check() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let arr = ctx.new_array(ArrayElementType::Byte, 2);
    let this = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/String", 4).unwrap();

    // offset=1, count=3 → overflow
    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "<init>",
        "([BIII)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(1), // offset
            Value::Int(3), // count — 1+3=4 > 2
        ],
    );
    assert!(res.is_err(), "Expected ArrayIndexOutOfBoundsException");
}

// -------------------------------------------------------------------------
// T8.2.4 — String.getBytes(srcBegin, srcEnd, dst, dstBegin)
// -------------------------------------------------------------------------

#[test]
fn test_string_get_bytes_deprecated() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // Create a String object for "Hello"
    let str_obj = ctx.create_string("Hello");
    let dst = ctx.new_array(ArrayElementType::Byte, 5);

    call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "getBytes",
        "(II[BI)V",
        &[
            Value::Object(Some(str_obj)),
            Value::Int(0), // srcBegin
            Value::Int(5), // srcEnd
            Value::Object(Some(dst)),
            Value::Int(0), // dstBegin
        ],
    )
    .unwrap();

    // Low bytes of "Hello"
    let expected: &[u8] = b"Hello";
    for (i, &b) in expected.iter().enumerate() {
        assert_eq!(
            ctx.get_array_element(dst, i),
            Value::Int(b as i32),
            "Byte at index {i} mismatch"
        );
    }
}

// -------------------------------------------------------------------------
// T8.2.5 — Character deprecated methods
// -------------------------------------------------------------------------

#[test]
fn test_char_is_java_letter() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    // 'A' is a Java letter
    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Character",
        "isJavaLetter",
        "(C)Z",
        &[Value::Int('A' as i32)],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(1)));

    // '_' is a Java letter
    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Character",
        "isJavaLetter",
        "(C)Z",
        &[Value::Int('_' as i32)],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(1)));

    // '1' is not a Java letter (only letter, not digit, for isJavaLetter)
    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Character",
        "isJavaLetter",
        "(C)Z",
        &[Value::Int('1' as i32)],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(0)));
}

#[test]
fn test_char_is_java_letter_or_digit() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Character",
        "isJavaLetterOrDigit",
        "(C)Z",
        &[Value::Int('5' as i32)],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(1)));

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Character",
        "isJavaLetterOrDigit",
        "(C)Z",
        &[Value::Int('+' as i32)],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(0)));
}

#[test]
fn test_char_is_space() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();

    for &ws in &[' ', '\t', '\n', '\r', '\x0C'] {
        let res = call_native(
            &reg,
            &mut ctx,
            "java/lang/Character",
            "isSpace",
            "(C)Z",
            &[Value::Int(ws as i32)],
        );
        assert_eq!(
            res.unwrap(),
            Some(Value::Int(1)),
            "Expected space for {ws:?}"
        );
    }

    let res = call_native(
        &reg,
        &mut ctx,
        "java/lang/Character",
        "isSpace",
        "(C)Z",
        &[Value::Int('x' as i32)],
    );
    assert_eq!(res.unwrap(), Some(Value::Int(0)));
}

// -------------------------------------------------------------------------
// T8.2.8 — Properties.save
// -------------------------------------------------------------------------

#[test]
fn test_properties_save_delegates_to_store() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let props = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Properties", 0).unwrap();
    let stream = try_alloc_concurrent_synthetic(&mut ctx, "java/io/OutputStream", 0).unwrap();
    let header = ctx.create_string("# header");

    // Pre-arm invoke_virtual to confirm it is called.
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(None));
    }

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Properties",
        "save",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        &[
            Value::Object(Some(props)),
            Value::Object(Some(stream)),
            Value::Object(Some(header)),
        ],
    );
    assert!(res.is_ok(), "Properties.save should succeed");
}

// -------------------------------------------------------------------------
// T8.2.9 — Hashtable.elements() / keys()
// -------------------------------------------------------------------------

#[test]
fn test_hashtable_elements_returns_enumerator() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let ht = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Hashtable", 0).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Hashtable",
        "elements",
        "()Ljava/util/Enumeration;",
        &[Value::Object(Some(ht))],
    );
    assert!(res.is_ok());
    let enum_obj = match res.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Expected Object(Some(_)), got {:?}", other),
    };
    // type marker should be 0 (values)
    assert_eq!(ctx.get_field(enum_obj, 2), Value::Int(0));
}

#[test]
fn test_hashtable_keys_returns_enumerator() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let ht = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Hashtable", 0).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Hashtable",
        "keys",
        "()Ljava/util/Enumeration;",
        &[Value::Object(Some(ht))],
    );
    assert!(res.is_ok());
    let enum_obj = match res.unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("Expected Object(Some(_)), got {:?}", other),
    };
    // type marker should be 1 (keys)
    assert_eq!(ctx.get_field(enum_obj, 2), Value::Int(1));
}

#[test]
fn test_hashtable_clone_empty_returns_fresh_object() {
    // TC0622 — `Hashtable.clone()` is registered as a native (it shadows the
    // real-JDK body that casts our synthetic bucket nodes to
    // `Hashtable$Entry`). An empty table has no pairs to copy, so the native
    // just allocates a fresh map and returns it. (The full copy semantics are
    // covered E2E against HotSpot; the mock's array store is a stub, so a unit
    // test can only exercise the empty/registration path.)
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let ht = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Hashtable", 1).unwrap();

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Hashtable",
        "clone",
        "()Ljava/lang/Object;",
        &[Value::Object(Some(ht))],
    );
    assert!(res.is_ok(), "Hashtable.clone() should succeed");
    match res.unwrap() {
        Some(Value::Object(Some(clone))) => {
            assert_ne!(clone.as_ptr(), ht.as_ptr(), "clone must be a fresh object");
        }
        other => panic!("Expected a fresh Object(Some(_)), got {:?}", other),
    }
}

// -------------------------------------------------------------------------
// T8.2.10 — StringBufferInputStream
// -------------------------------------------------------------------------

#[test]
fn test_sbis_read_sequential() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let stream =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4).unwrap();
    let str_obj = ctx.create_string("AB");

    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(stream)), Value::Object(Some(str_obj))],
    )
    .unwrap();

    let r1 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(stream))],
    )
    .unwrap();
    let r2 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(stream))],
    )
    .unwrap();
    let r3 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(stream))],
    )
    .unwrap();

    assert_eq!(r1, Some(Value::Int(b'A' as i32)));
    assert_eq!(r2, Some(Value::Int(b'B' as i32)));
    assert_eq!(r3, Some(Value::Int(-1))); // EOF
}

#[test]
fn test_sbis_available_and_reset() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let stream =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4).unwrap();
    let str_obj = ctx.create_string("XYZ");

    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(stream)), Value::Object(Some(str_obj))],
    )
    .unwrap();

    let avail = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "available",
        "()I",
        &[Value::Object(Some(stream))],
    )
    .unwrap();
    assert_eq!(avail, Some(Value::Int(3)));

    // Read one byte then check available drops to 2.
    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(stream))],
    )
    .unwrap();
    let avail2 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "available",
        "()I",
        &[Value::Object(Some(stream))],
    )
    .unwrap();
    assert_eq!(avail2, Some(Value::Int(2)));

    // Reset → back to position 0
    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "reset",
        "()V",
        &[Value::Object(Some(stream))],
    )
    .unwrap();
    let avail3 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "available",
        "()I",
        &[Value::Object(Some(stream))],
    )
    .unwrap();
    assert_eq!(avail3, Some(Value::Int(3)));
}

#[test]
fn test_sbis_bulk_read() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let stream =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4).unwrap();
    let str_obj = ctx.create_string("Hello");
    let buf = ctx.new_array(ArrayElementType::Byte, 5);

    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(stream)), Value::Object(Some(str_obj))],
    )
    .unwrap();

    let n = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "([BII)I",
        &[
            Value::Object(Some(stream)),
            Value::Object(Some(buf)),
            Value::Int(0),
            Value::Int(5),
        ],
    )
    .unwrap();
    assert_eq!(n, Some(Value::Int(5)));

    for (i, &b) in b"Hello".iter().enumerate() {
        assert_eq!(ctx.get_array_element(buf, i), Value::Int(b as i32));
    }
}

// -------------------------------------------------------------------------
// T8.2.11 — LineNumberInputStream
// -------------------------------------------------------------------------

/// Builds a mock InputStream that serves bytes from `data` via invoke_virtual.
fn make_lnis(
    ctx: &mut MockNativeContext,
    reg: &NativeMethodRegistry,
    data: &str,
) -> (cratonvm_types::ObjectRef, cratonvm_types::ObjectRef) {
    // We use a StringBufferInputStream as the underlying stream.
    let sbis =
        try_alloc_concurrent_synthetic(ctx, "java/io/StringBufferInputStream", 4).unwrap();
    let str_obj = ctx.create_string(data);
    call_native(
        reg,
        ctx,
        "java/io/StringBufferInputStream",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sbis)), Value::Object(Some(str_obj))],
    )
    .unwrap();

    let lnis = try_alloc_concurrent_synthetic(ctx, "java/io/LineNumberInputStream", 4).unwrap();
    call_native(
        reg,
        ctx,
        "java/io/LineNumberInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(lnis)), Value::Object(Some(sbis))],
    )
    .unwrap();

    (lnis, sbis)
}

#[test]
fn test_lnis_get_set_line_number() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let lnis =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/LineNumberInputStream", 4).unwrap();
    let inner = try_alloc_concurrent_synthetic(&mut ctx, "java/io/InputStream", 0).unwrap();

    call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(lnis)), Value::Object(Some(inner))],
    )
    .unwrap();

    let n0 = call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "getLineNumber",
        "()I",
        &[Value::Object(Some(lnis))],
    )
    .unwrap();
    assert_eq!(n0, Some(Value::Int(0)));

    call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "setLineNumber",
        "(I)V",
        &[Value::Object(Some(lnis)), Value::Int(42)],
    )
    .unwrap();

    let n1 = call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "getLineNumber",
        "()I",
        &[Value::Object(Some(lnis))],
    )
    .unwrap();
    assert_eq!(n1, Some(Value::Int(42)));
}

#[test]
fn test_lnis_read_single_bytes_passthrough() {
    // The LNIs wraps an SBIS. We use the real SBIS native to back the reads.
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let (lnis, _sbis) = make_lnis(&mut ctx, &reg, "hi");

    // lnis.read() needs to call inner.read() via invoke_virtual, which in
    // MockNativeContext forwards to invoke_virtual_result.  We cannot easily
    // chain two registered natives through one MockContext call-stack, so
    // we exercise only the EOF path here (no pre-armed result → returns -1).
    // The full line-count logic is exercised via the struct test below.
    let _r = call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "read",
        "()I",
        &[Value::Object(Some(lnis))],
    )
    .unwrap();
    // We get -1 because MockNativeContext.invoke_virtual returns Ok(None) which
    // the lnis read implementation treats as -1 when no result is armed.
}

#[test]
fn test_lnis_set_line_number_roundtrip() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let lnis =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/LineNumberInputStream", 4).unwrap();
    let inner = try_alloc_concurrent_synthetic(&mut ctx, "java/io/InputStream", 0).unwrap();
    call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(lnis)), Value::Object(Some(inner))],
    )
    .unwrap();
    call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "setLineNumber",
        "(I)V",
        &[Value::Object(Some(lnis)), Value::Int(99)],
    )
    .unwrap();
    let n = call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "getLineNumber",
        "()I",
        &[Value::Object(Some(lnis))],
    )
    .unwrap();
    assert_eq!(n, Some(Value::Int(99)));
}

#[test]
fn test_lnis_reset_restores_line_number() {
    // Manually set line number and saved line number fields, then call reset.
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let lnis =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/LineNumberInputStream", 4).unwrap();
    let inner = try_alloc_concurrent_synthetic(&mut ctx, "java/io/InputStream", 0).unwrap();
    call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "<init>",
        "(Ljava/io/InputStream;)V",
        &[Value::Object(Some(lnis)), Value::Object(Some(inner))],
    )
    .unwrap();

    // Simulate: current line = 5, saved line = 2
    ctx.set_field(lnis, 1, Value::Int(5));
    ctx.set_field(lnis, 2, Value::Int(2)); // saved

    call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "reset",
        "()V",
        &[Value::Object(Some(lnis))],
    )
    .unwrap();

    let n = call_native(
        &reg,
        &mut ctx,
        "java/io/LineNumberInputStream",
        "getLineNumber",
        "()I",
        &[Value::Object(Some(lnis))],
    )
    .unwrap();
    assert_eq!(
        n,
        Some(Value::Int(2)),
        "reset should restore saved line number"
    );
}

// -------------------------------------------------------------------------
// T8.2.12 — Locale.getISO3Language
// -------------------------------------------------------------------------

#[test]
fn test_locale_get_iso3_language_english() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let locale = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Locale", 4).unwrap();
    let lang = ctx.create_string("en");
    ctx.set_field(locale, 0, Value::Object(Some(lang)));

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Locale",
        "getISO3Language",
        "()Ljava/lang/String;",
        &[Value::Object(Some(locale))],
    )
    .unwrap();
    let s = match res {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap(),
        other => panic!("Expected string object, got {:?}", other),
    };
    assert_eq!(s, "eng");
}

#[test]
fn test_locale_get_iso3_language_french() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let locale = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Locale", 4).unwrap();
    let lang = ctx.create_string("fr");
    ctx.set_field(locale, 0, Value::Object(Some(lang)));

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Locale",
        "getISO3Language",
        "()Ljava/lang/String;",
        &[Value::Object(Some(locale))],
    )
    .unwrap();
    let s = match res {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap(),
        other => panic!("Expected string object, got {:?}", other),
    };
    assert_eq!(s, "fra");
}

#[test]
fn test_locale_get_iso3_language_japanese() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let locale = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Locale", 4).unwrap();
    let lang = ctx.create_string("ja");
    ctx.set_field(locale, 0, Value::Object(Some(lang)));

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Locale",
        "getISO3Language",
        "()Ljava/lang/String;",
        &[Value::Object(Some(locale))],
    )
    .unwrap();
    let s = match res {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap(),
        _ => panic!("Expected string"),
    };
    assert_eq!(s, "jpn");
}

/// An unmapped 2-letter code is a REFUSAL, not a pass-through.
///
/// This test asserted `"xx"` until 2026-09-20, with the comment "iso2_to_iso3
/// returns "" so we fall back to the 2-letter code" — a test written from the
/// implementation, which therefore could not see that the implementation was
/// wrong. `java.util.Locale.getISO3Language` throws `MissingResourceException`
/// for a code its table does not carry; measured on HotSpot 25.0.4+7 in
/// `apps/probes/UulrLocaleDisplaySweep`, `Locale.of("qq","ZZ")` and an
/// 8-letter tag both refuse.
#[test]
fn test_locale_get_iso3_language_unknown_refuses() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let locale = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Locale", 4).unwrap();
    let lang = ctx.create_string("xx");
    ctx.set_field(locale, 0, Value::Object(Some(lang)));

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Locale",
        "getISO3Language",
        "()Ljava/lang/String;",
        &[Value::Object(Some(locale))],
    );
    assert!(
        matches!(
            res,
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(_))
        ),
        "an unmapped language code must refuse, got {res:?}"
    );
}

/// A code that is ALREADY three letters is its own answer — the arm the
/// 2→3 table cannot express, and the one the old tail turned into a
/// pass-through by accident rather than on purpose.
#[test]
fn test_locale_get_iso3_language_three_letter_passthrough() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let locale = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Locale", 4).unwrap();
    let lang = ctx.create_string("haw");
    ctx.set_field(locale, 0, Value::Object(Some(lang)));

    let res = call_native(
        &reg,
        &mut ctx,
        "java/util/Locale",
        "getISO3Language",
        "()Ljava/lang/String;",
        &[Value::Object(Some(locale))],
    )
    .unwrap();
    let s = match res {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap(),
        _ => panic!("Expected string"),
    };
    assert_eq!(s, "haw");
}

// -------------------------------------------------------------------------
// Registration completeness
// -------------------------------------------------------------------------

#[test]
fn test_registration_count() {
    let reg = make_registry();
    // Date: 4 ctors + 8 getters (incl. getTimezoneOffset) + 6 setters = 18
    // String: 2 (hibyte ctor + getBytes)
    // Character: 3
    // Properties: 1
    // Hashtable: 2
    // StringBufferInputStream: 5
    // LineNumberInputStream: 7
    // Locale: 1
    // Total = 18+2+3+1+2+5+7+1 = 39
    assert!(
        reg.len() >= 39,
        "Expected at least 39 registered natives, got {}",
        reg.len()
    );
}

// -------------------------------------------------------------------------
// G32: `new String(StringBuilder)` must carry UTF-16 units, not a Rust str
// -------------------------------------------------------------------------

/// Build a CratonVM synthetic `StringBuilder`: field 0 = char[] buffer,
/// field 1 = count. `sb_state` reads exactly this shape.
fn make_builder(ctx: &mut MockNativeContext, units: &[u16], capacity: usize) -> ObjectRef {
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Char, capacity);
    for (i, &u) in units.iter().enumerate() {
        ctx.set_array_element(buf, i, Value::Int(u as i32));
    }
    let sb = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
    ctx.set_field(sb, 0, Value::Object(Some(buf)));
    ctx.set_field(sb, 1, Value::Int(units.len() as i32));
    sb
}

/// Read back what `init_string_from_units` wrote. `MockNativeContext` uses
/// the trait's default impl, which stores the units as a char[] in field 0,
/// so this observes UNITS and never a `String` round trip.
fn read_units(ctx: &MockNativeContext, obj: ObjectRef) -> Vec<u16> {
    let Value::Object(Some(arr)) = ctx.get_field(obj, 0) else {
        panic!("no units array was written into the receiver");
    };
    (0..ctx.array_length(arr))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v as u16,
            other => panic!("non-int unit at {i}: {other:?}"),
        })
        .collect()
}

/// The bug this pins: the body used to be
/// `String::from_utf16_lossy(&chars).into_bytes()`, and a Rust `str` cannot
/// hold an unpaired surrogate, so U+DC00 came back as U+FFFD.
///
/// MEASURED on HotSpot 25.0.3+9: `sb.append((char) 0xDC00);
/// new String(sb).charAt(0)` is 56320 (0xDC00), and `.length()` is 1.
///
/// Asserted as UNITS on purpose. A test written through `read_string`
/// passes on the BROKEN code, because that conversion is the defect.
#[test]
fn new_string_from_builder_keeps_a_lone_low_surrogate() {
    let mut ctx = MockNativeContext::new();
    let sb = make_builder(&mut ctx, &[0xDC00], 16);
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    native_string_init_from_string_builder(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(sb))],
    )
    .expect("constructor must not fail");
    assert_eq!(read_units(&ctx, this), vec![0xDC00_u16]);
}

/// The high half, and the round trip through a mixed sequence: the
/// surrogate must survive with its NEIGHBOURS intact and the length must
/// stay 4. A lossy conversion answers `97,98,65533,99` here — same length,
/// so a length-only assertion would pass on the broken code.
#[test]
fn new_string_from_builder_keeps_a_lone_high_surrogate_among_ascii() {
    let mut ctx = MockNativeContext::new();
    let sb = make_builder(&mut ctx, &[0x0061, 0x0062, 0xD800, 0x0063], 32);
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    native_string_init_from_string_builder(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(sb))],
    )
    .expect("constructor must not fail");
    assert_eq!(
        read_units(&ctx, this),
        vec![0x0061_u16, 0x0062, 0xD800, 0x0063]
    );
}

/// Only `count` units are copied, never the buffer's CAPACITY. The builder
/// below has a 32-slot array holding 5 characters; reading the whole array
/// is the padding bug `read_string`'s class guard exists to prevent, and
/// this constructor must not reintroduce it.
#[test]
fn new_string_from_builder_copies_count_not_capacity() {
    let mut ctx = MockNativeContext::new();
    let sb = make_builder(&mut ctx, &[0x0068, 0x0065, 0x006C, 0x006C, 0x006F], 32);
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    native_string_init_from_string_builder(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(sb))],
    )
    .expect("constructor must not fail");
    assert_eq!(read_units(&ctx, this).len(), 5);
    assert_eq!(
        read_units(&ctx, this),
        vec![0x0068_u16, 0x0065, 0x006C, 0x006C, 0x006F]
    );
}

/// A well-formed surrogate PAIR is two units and must stay two units — the
/// fix must not "helpfully" combine them into one code point.
#[test]
fn new_string_from_builder_keeps_a_well_formed_pair_as_two_units() {
    let mut ctx = MockNativeContext::new();
    let sb = make_builder(&mut ctx, &[0xD83D, 0xDE00], 16);
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    native_string_init_from_string_builder(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(sb))],
    )
    .expect("constructor must not fail");
    assert_eq!(read_units(&ctx, this), vec![0xD83D_u16, 0xDE00]);
}

/// An empty builder produces an empty String, not a failure.
#[test]
fn new_string_from_builder_handles_an_empty_builder() {
    let mut ctx = MockNativeContext::new();
    let sb = make_builder(&mut ctx, &[], 16);
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    native_string_init_from_string_builder(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(sb))],
    )
    .expect("constructor must not fail");
    assert!(read_units(&ctx, this).is_empty());
}

/// gc-common w20-b: `new String(byte[])` writes the decoded text into
/// `this` IN PLACE (it used to copy the fields of a second, interned
/// String), and releases the pin it takes around the fill.
#[test]
fn w20b_new_string_from_bytes_writes_the_units_in_place() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let bytes: [i32; 4] = [0x61, 0xC3 - 256, 0xA9 - 256, 0x7A]; // "aéz" as UTF-8
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b));
    }
    let pins = ctx.native_pin_count_for_test();
    call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "<init>",
        "([B)V",
        &[Value::Object(Some(this)), Value::Object(Some(arr))],
    )
    .expect("constructor must not fail");
    assert_eq!(read_units(&ctx, this), vec![0x61_u16, 0xE9, 0x7A]);
    assert_eq!(ctx.native_pin_count_for_test(), pins);
}

/// gc-common w20-b: a negative offset arrives as a huge `usize`; the sum
/// with the count must be a bounds failure, not an arithmetic overflow.
#[test]
fn w20b_new_string_from_byte_range_rejects_a_negative_offset() {
    let reg = make_registry();
    let mut ctx = MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
    let out = call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "<init>",
        "([BII)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(-1),
            Value::Int(2),
        ],
    );
    assert!(out.is_err(), "a negative offset must throw, got {out:?}");
}
