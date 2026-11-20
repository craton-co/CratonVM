use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Helper: look up a native and call it through the registry.
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

fn setup() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_deprecated_io_util_natives(&mut r);
    r
}

// --- T8.2.1 Date constructors ---

#[test]
fn test_date_init_ymd() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    // year=70 (1970), month=0 (Jan), day=1 => epoch 0
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let result = call_native(
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
    );
    assert!(result.is_ok());
    let ms = get_date_millis(&mut ctx, date_obj);
    assert_eq!(ms, 0, "1970-01-01 should be epoch 0");
}

#[test]
fn test_date_init_ymdhm() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    // year=70, month=0, day=1, hrs=1, min=0 => 3_600_000 ms
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let result = call_native(
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
    );
    assert!(result.is_ok());
    let ms = get_date_millis(&mut ctx, date_obj);
    assert_eq!(ms, 3_600_000);
}

#[test]
fn test_date_init_ymdhms() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    // year=70, month=0, day=1, hrs=0, min=0, sec=30 => 30_000 ms
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let result = call_native(
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
    );
    assert!(result.is_ok());
    let ms = get_date_millis(&mut ctx, date_obj);
    assert_eq!(ms, 30_000);
}

#[test]
fn test_date_init_string() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let str_obj = ctx.create_string("2000/01/01");
    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
    );
    assert!(result.is_ok());
    let ms = get_date_millis(&mut ctx, date_obj);
    // 2000-01-01 00:00:00 UTC
    assert_eq!(ms, 946684800000);
}

#[test]
fn test_date_init_string_tostring_form() {
    // Date.toString form: "EEE MMM dd HH:mm:ss zzz yyyy"
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let str_obj = ctx.create_string("Sat Jan 01 00:00:00 GMT 2000");
    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
    );
    assert!(result.is_ok());
    assert_eq!(get_date_millis(&mut ctx, date_obj), 946684800000);
}

#[test]
fn test_date_init_string_rfc1123_form() {
    // RFC-1123 form: "EEE, dd MMM yyyy HH:mm:ss zzz"
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let str_obj = ctx.create_string("Sat, 01 Jan 2000 00:00:00 GMT");
    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
    );
    assert!(result.is_ok());
    assert_eq!(get_date_millis(&mut ctx, date_obj), 946684800000);
}

#[test]
fn test_date_init_string_numeric_tz_offset() {
    // "+0100" means local is one hour ahead of UTC → 01:00 local == 00:00 UTC.
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let str_obj = ctx.create_string("Sat, 01 Jan 2000 01:00:00 +0100");
    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
    );
    assert!(result.is_ok());
    assert_eq!(get_date_millis(&mut ctx, date_obj), 946684800000);
}

#[test]
fn test_date_init_string_unparseable_throws() {
    // A genuinely unparseable string must throw IllegalArgumentException,
    // NOT silently fall back to epoch 0.
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let str_obj = ctx.create_string("not a date at all");
    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(date_obj)), Value::Object(Some(str_obj))],
    );
    assert!(
        result.is_err(),
        "unparseable string should throw, got {result:?}"
    );
}

#[test]
fn test_parse_date_string_helper() {
    // ISO numeric still works.
    assert_eq!(parse_date_string("1970-01-01"), Some(0));
    assert_eq!(parse_date_string("1970/01/01 00:00:00"), Some(0));
    // toString + RFC-1123 textual forms agree on the same instant.
    assert_eq!(parse_date_string("Thu Jan 01 00:00:00 GMT 1970"), Some(0));
    assert_eq!(parse_date_string("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
    // Unparseable / empty → None.
    assert_eq!(parse_date_string(""), None);
    assert_eq!(parse_date_string("garbage"), None);
    assert_eq!(parse_date_string("Jan 1970"), None); // no day-of-month
}

// --- T8.2.2 Date getters/setters ---

#[test]
fn test_date_get_year() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    // Set to 2000-06-15 12:30:45
    let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
    set_date_millis(&mut ctx, date_obj, ms);

    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getYear",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(result.unwrap(), Some(Value::Int(100))); // 2000 - 1900 = 100
}

#[test]
fn test_date_get_month_and_date() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
    set_date_millis(&mut ctx, date_obj, ms);

    let month = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getMonth",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(month.unwrap(), Some(Value::Int(5))); // June = 5

    let day = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getDate",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(day.unwrap(), Some(Value::Int(15)));
}

#[test]
fn test_date_get_hours_min_sec() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
    set_date_millis(&mut ctx, date_obj, ms);

    let hours = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getHours",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(hours.unwrap(), Some(Value::Int(12)));

    let mins = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getMinutes",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(mins.unwrap(), Some(Value::Int(30)));

    let secs = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getSeconds",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(secs.unwrap(), Some(Value::Int(45)));
}

#[test]
fn test_date_get_day_of_week() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    // 1970-01-01 = Thursday = 4
    set_date_millis(&mut ctx, date_obj, 0);
    let dow = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getDay",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(dow.unwrap(), Some(Value::Int(4))); // Thursday
}

#[test]
fn test_date_set_year() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
    set_date_millis(&mut ctx, date_obj, ms);

    // Set year to 120 (= 2020)
    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setYear",
        "(I)V",
        &[Value::Object(Some(date_obj)), Value::Int(120)],
    )
    .unwrap();

    let year = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getYear",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(year.unwrap(), Some(Value::Int(120)));
}

#[test]
fn test_date_set_month() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    let ms = to_epoch_millis(2000, 5, 15, 12, 30, 45);
    set_date_millis(&mut ctx, date_obj, ms);

    call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "setMonth",
        "(I)V",
        &[Value::Object(Some(date_obj)), Value::Int(11)],
    )
    .unwrap();

    let month = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "getMonth",
        "()I",
        &[Value::Object(Some(date_obj))],
    );
    assert_eq!(month.unwrap(), Some(Value::Int(11)));
}

// `test_date_to_locale_string` was REMOVED 2026-08-21 (WORKER 4) with the
// registration it pinned. It asserted
//
//     assert_eq!(text, "01/01/1970 00:00:00");
//
// which is not what `Date.toLocaleString()` returns on any JDK — HotSpot
// 25.0.4 answers `Jan 1, 1970, 12:00:00 AM` — so the test's function was to
// hold a divergence in place. `[a test that freezes VM output locks in the
// divergence]`. See the retirement note beside the old registration.

#[test]
fn test_date_to_gmt_string() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let date_obj = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Date", 2).unwrap();
    set_date_millis(&mut ctx, date_obj, 0);

    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Date",
        "toGMTString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(date_obj))],
    );
    let val = result.unwrap().unwrap();
    if let Value::Object(Some(s)) = val {
        let text = ctx.read_string(s).unwrap();
        assert_eq!(text, "1 Jan 1970 00:00:00 GMT");
    } else {
        panic!("Expected string object");
    }
}

// --- T8.2.3 String hibyte constructor ---

#[test]
fn test_string_hibyte_constructor() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let this = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/String", 4).unwrap();
    // Create byte array [65, 66, 67] = "ABC" with hibyte=0
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 3);
    ctx.set_array_element(arr, 0, Value::Int(65));
    ctx.set_array_element(arr, 1, Value::Int(66));
    ctx.set_array_element(arr, 2, Value::Int(67));

    let result = call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "<init>",
        "([BIII)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(0),
            Value::Int(3),
        ],
    );
    assert!(result.is_ok());
}

/// gc-common w19-g: a non-zero hibyte reaches `this` as the exact code
/// units `(hibyte << 8) | (b & 0xFF)`, written through
/// `init_string_from_units` (the mock's default stores them as a `char[]`
/// in field 0), and the pin taken around it is released.
#[test]
fn w19g_string_hibyte_constructor_writes_the_exact_units() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let this = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/String", 4).unwrap();
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 2);
    ctx.set_array_element(arr, 0, Value::Int(0x41));
    ctx.set_array_element(arr, 1, Value::Int(-1)); // 0xFF
    let pins_before = ctx.native_pin_count_for_test();
    let result = call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "<init>",
        "([BIII)V",
        &[
            Value::Object(Some(this)),
            Value::Object(Some(arr)),
            Value::Int(1),
            Value::Int(0),
            Value::Int(2),
        ],
    );
    assert!(result.is_ok(), "got {result:?}");
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);
    let Value::Object(Some(chars)) = ctx.get_field(this, 0) else {
        panic!("field 0 must hold the backing array");
    };
    assert_eq!(ctx.array_length(chars), 2);
    assert!(matches!(
        ctx.get_array_element(chars, 0),
        Value::Int(0x0141)
    ));
    assert!(matches!(
        ctx.get_array_element(chars, 1),
        Value::Int(0x01FF)
    ));
}

// --- T8.2.4 String.getBytes deprecated ---

#[test]
fn test_string_get_bytes_deprecated() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let str_obj = ctx.create_string("Hello");
    let dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 10);

    let result = call_native(
        &reg,
        &mut ctx,
        "java/lang/String",
        "getBytes",
        "(II[BI)V",
        &[
            Value::Object(Some(str_obj)),
            Value::Int(0),
            Value::Int(5),
            Value::Object(Some(dst)),
            Value::Int(0),
        ],
    );
    assert!(result.is_ok());
    // Check first byte = 'H' = 72
    assert_eq!(ctx.get_array_element(dst, 0), Value::Int(72));
    // 'e' = 101
    assert_eq!(ctx.get_array_element(dst, 1), Value::Int(101));
}

// --- T8.2.5 Character deprecated ---
//
// `test_character_is_java_letter`, `test_character_is_java_letter_or_digit`
// and `test_character_is_space` were deleted with the registrations they
// covered (see the tombstone in `register_deprecated_io_util_natives`).
// They asserted against `setup()`, which registers ONLY this file, so they
// exercised the copy that loses the registry slot at VM start-up and never
// runs — three green tests over a dead body.  The live copies are covered
// by `deprecated_util.rs`'s own tests (`isJavaLetter` at 2863/2874/2885,
// `isJavaLetterOrDigit` at 2901/2911, `isSpace` at 2928/2943).
//
// Neither test set can see the real defect: both bodies answer 949
// (`isJavaLetter`) and 1,010 (`isJavaLetterOrDigit`) of the 65,536 `char`
// values differently from HotSpot 25, and every character these tests probe
// (`A`, `$`, `3`, the five ASCII spaces) is in the agreeing majority.

// --- T8.2.7 Number.byteValue / shortValue ---

// `test_number_byte_value` and `test_number_short_value` were REMOVED
// 2026-08-21 (WORKER 4) with the two registrations they pinned. Both drove
// a `MockNativeContext` whose `invoke_virtual` was scripted to return the
// value the test then asserted the narrowing of — so they tested the cast,
// which is a Rust `as`, and could not have failed while the registration
// existed. `regression-suite/probes/W4Deprecated.java` asks the same
// question of a real VM against a real oracle, over 116 cases including
// the `BigDecimal`/`BigInteger` subclasses the retired comment was about.
// See `register_number_defaults` for the retirement note.

// --- T8.2.8 Properties.save ---

#[test]
fn test_properties_save_delegates() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let props = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Properties", 4).unwrap();
    let out = try_alloc_concurrent_synthetic(&mut ctx, "java/io/OutputStream", 0).unwrap();
    let comment = ctx.create_string("test");

    // Pre-arm invoke_virtual to return Ok(None) for store()
    unsafe {
        *ctx.invoke_virtual_result.get() = Some(Ok(None));
    }

    let result = call_native(
        &reg,
        &mut ctx,
        "java/util/Properties",
        "save",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        &[
            Value::Object(Some(props)),
            Value::Object(Some(out)),
            Value::Object(Some(comment)),
        ],
    );
    assert!(result.is_ok());
}

// --- T8.2.9 Hashtable enumerations ---

#[test]
fn test_hashtable_elements_and_keys() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let ht = try_alloc_concurrent_synthetic(&mut ctx, "java/util/Hashtable", 4).unwrap();

    let elements = call_native(
        &reg,
        &mut ctx,
        "java/util/Hashtable",
        "elements",
        "()Ljava/util/Enumeration;",
        &[Value::Object(Some(ht))],
    );
    assert!(elements.is_ok());
    match elements.unwrap() {
        Some(Value::Object(Some(_))) => {}
        other => panic!("Expected Object(Some(_)), got {:?}", other),
    }

    let keys = call_native(
        &reg,
        &mut ctx,
        "java/util/Hashtable",
        "keys",
        "()Ljava/util/Enumeration;",
        &[Value::Object(Some(ht))],
    );
    assert!(keys.is_ok());
    match keys.unwrap() {
        Some(Value::Object(Some(_))) => {}
        other => panic!("Expected Object(Some(_)), got {:?}", other),
    }
}

// --- T8.2.10 StringBufferInputStream ---

#[test]
fn test_string_buffer_input_stream() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let sbis =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4).unwrap();
    let str_obj = ctx.create_string("AB");

    // init
    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sbis)), Value::Object(Some(str_obj))],
    )
    .unwrap();

    // available should be 2
    let avail = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "available",
        "()I",
        &[Value::Object(Some(sbis))],
    );
    assert_eq!(avail.unwrap(), Some(Value::Int(2)));

    // read() first byte
    let b1 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(sbis))],
    );
    assert_eq!(b1.unwrap(), Some(Value::Int(65))); // 'A'

    // read() second byte
    let b2 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(sbis))],
    );
    assert_eq!(b2.unwrap(), Some(Value::Int(66))); // 'B'

    // read() EOF
    let b3 = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(sbis))],
    );
    assert_eq!(b3.unwrap(), Some(Value::Int(-1)));

    // reset and read again
    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "reset",
        "()V",
        &[Value::Object(Some(sbis))],
    )
    .unwrap();

    let b1_again = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "()I",
        &[Value::Object(Some(sbis))],
    );
    assert_eq!(b1_again.unwrap(), Some(Value::Int(65)));
}

#[test]
fn test_string_buffer_input_stream_read_array() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let sbis =
        try_alloc_concurrent_synthetic(&mut ctx, "java/io/StringBufferInputStream", 4).unwrap();
    let str_obj = ctx.create_string("Hello");

    call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(sbis)), Value::Object(Some(str_obj))],
    )
    .unwrap();

    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 10);
    let n = call_native(
        &reg,
        &mut ctx,
        "java/io/StringBufferInputStream",
        "read",
        "([BII)I",
        &[
            Value::Object(Some(sbis)),
            Value::Object(Some(buf)),
            Value::Int(0),
            Value::Int(3),
        ],
    );
    assert_eq!(n.unwrap(), Some(Value::Int(3)));
    assert_eq!(ctx.get_array_element(buf, 0), Value::Int(72)); // 'H'
    assert_eq!(ctx.get_array_element(buf, 1), Value::Int(101)); // 'e'
    assert_eq!(ctx.get_array_element(buf, 2), Value::Int(108)); // 'l'
}

// --- T8.2.13 / T8.2.14 URL encode/decode ---

#[test]
fn test_url_decode() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let input = ctx.create_string("Hello+World%21");

    let result = call_native(
        &reg,
        &mut ctx,
        "java/net/URLDecoder",
        "decode",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(input))],
    );
    let val = result.unwrap().unwrap();
    if let Value::Object(Some(s)) = val {
        let text = ctx.read_string(s).unwrap();
        assert_eq!(text, "Hello World!");
    } else {
        panic!("Expected string object");
    }
}

#[test]
fn test_url_encode() {
    let reg = setup();
    let mut ctx = MockNativeContext::new();
    let input = ctx.create_string("Hello World!");

    let result = call_native(
        &reg,
        &mut ctx,
        "java/net/URLEncoder",
        "encode",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(input))],
    );
    let val = result.unwrap().unwrap();
    if let Value::Object(Some(s)) = val {
        let text = ctx.read_string(s).unwrap();
        assert_eq!(text, "Hello+World%21");
    } else {
        panic!("Expected string object");
    }
}

// --- Registration count ---

#[test]
fn test_registration_count() {
    let reg = setup();
    // Count all registrations: should be well over 40
    assert!(
        reg.len() >= 40,
        "Expected at least 40 registered deprecated natives, got {}",
        reg.len()
    );
}

// --- Date math edge cases ---

#[test]
fn test_epoch_millis_roundtrip() {
    // Verify roundtrip for a known date: 2024-03-15 10:30:00
    let ms = to_epoch_millis(2024, 2, 15, 10, 30, 0);
    let (year, month, day, hour, min, sec, _) = from_epoch_millis(ms);
    assert_eq!(year, 2024);
    assert_eq!(month, 2); // March = 2
    assert_eq!(day, 15);
    assert_eq!(hour, 10);
    assert_eq!(min, 30);
    assert_eq!(sec, 0);
}

#[test]
fn test_leap_year() {
    assert!(is_leap_year(2000));
    assert!(is_leap_year(2024));
    assert!(!is_leap_year(1900));
    assert!(!is_leap_year(2023));
}

#[test]
fn test_url_encode_decode_roundtrip() {
    let original = "key=value&foo=bar baz";
    let encoded = url_encode(original);
    let decoded = url_decode(&encoded).expect("well-formed escape sequence");
    assert_eq!(decoded, original);
}
