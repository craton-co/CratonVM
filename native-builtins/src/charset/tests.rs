use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn make_charset(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    let cs = try_alloc_concurrent_synthetic(ctx, "java/nio/charset/Charset", 1).unwrap();
    let n = ctx.create_string(name);
    ctx.set_field(cs, CHARSET_FIELD_NAME, Value::Object(Some(n)));
    cs
}

#[test]
fn charset_name_readback() {
    let mut ctx = mock_ctx();
    let cs = make_charset(&mut ctx, "UTF-8");
    let name = read_string_field(&ctx, cs, CHARSET_FIELD_NAME);
    assert_eq!(name.as_deref(), Some("UTF-8"));
}

#[test]
fn encode_with_charset_utf8() {
    let mut ctx = mock_ctx();
    let cs = make_charset(&mut ctx, "UTF-8");
    let chars: Vec<u16> = "A\u{00A9}".encode_utf16().collect();
    let bytes = encode_with_charset(&ctx, cs, &chars);
    assert_eq!(bytes, vec![0x41, 0xC2, 0xA9]);
}

#[test]
fn decode_with_charset_utf8() {
    let mut ctx = mock_ctx();
    let cs = make_charset(&mut ctx, "UTF-8");
    let chars = decode_with_charset(&ctx, cs, &[0x41, 0xC2, 0xA9]);
    assert_eq!(String::from_utf16(&chars).unwrap(), "A\u{00A9}");
}

#[test]
fn consuming_empty_bytebuffer_does_not_decode_its_backing_array() {
    // A legitimate empty ByteBuffer may retain a non-empty backing array:
    // ByteBuffer.wrap(array, offset, 0) is exactly such a view. The
    // real-layout slot fallback is only for buffers whose state cannot be
    // read, not for a known empty remaining range.
    let mut ctx = mock_ctx();
    let bb = alloc_byte_buffer(&mut ctx, b"ABCD").unwrap();
    ctx.set_field(bb, BUF_FIELD_POS, Value::Int(2));
    ctx.set_field(bb, BUF_FIELD_LIMIT, Value::Int(2));
    assert!(read_and_consume_bytebuffer(&mut ctx, bb).is_empty());
}

/// Round 15 wave 2, lane jdkonly: on a real-layout `HeapByteBuffer` (the
/// mock resolves `mark@0 position@1 .. hb@5`, as the JDK class does) the
/// indexed fallback wrote the backing array over `mark`, so the JDK's
/// `reset()` took it for a set mark. It must stay the declared `-1`.
#[test]
fn r15_jdkonly_real_layout_byte_buffer_keeps_its_declared_mark() {
    let mut ctx = mock_ctx();
    let bb = alloc_byte_buffer(&mut ctx, b"ABCD").unwrap();
    assert_eq!(ctx.get_field_by_name(bb, "mark"), Value::Int(-1));
    assert_eq!(ctx.get_field_by_name(bb, "position"), Value::Int(0));
    assert_eq!(ctx.get_field_by_name(bb, "limit"), Value::Int(4));
    assert_eq!(ctx.get_field_by_name(bb, "address"), Value::Long(16));
    assert!(matches!(
        ctx.get_field_by_name(bb, "hb"),
        Value::Object(Some(_))
    ));
}

#[test]
fn latin1_roundtrip_via_named() {
    let s = "café";
    let b = encode_str_named("ISO-8859-1", s);
    assert_eq!(b.last().copied(), Some(0xE9));
    assert_eq!(decode_str_named("ISO-8859-1", &b), s);
}

#[test]
fn engine_supports_known_canonical_names() {
    assert!(engine_supports("UTF-8"));
    assert!(engine_supports("ISO-8859-1"));
    assert!(engine_supports("windows-1252"));
}

#[test]
fn engine_supports_rejects_unimplemented_canonical_name() {
    // `normalize_charset_name` canonicalizes "KOI8-U" successfully, but the
    // transcoding engine has no KOI8-U coder — the name-path natives must
    // surface an unsupported-charset error rather than fall back to a lossy
    // Latin-1 encode. (Shift_JIS / EUC-JP are now handled via encoding_rs;
    // see `shift_jis_decodes_via_encoding_rs`.)
    assert_eq!(normalize_charset_name("KOI8-U"), "KOI8-U");
    assert!(!engine_supports("KOI8-U"));
}

#[test]
fn shift_jis_decodes_via_encoding_rs() {
    // "日本語" in Shift_JIS = 93FA 967B 8CEA. The engine must now decode it
    // (previously it fell through to a Latin-1 read that produced "ú{ê"),
    // matching HotSpot for Spring's ContentDisposition Shift_JIS filenames.
    assert!(engine_supports("Shift_JIS"));
    let bytes = [0x93u8, 0xFA, 0x96, 0x7B, 0x8C, 0xEA];
    let decoded = engine::decode_bytes("Shift_JIS", &bytes).expect("valid Shift_JIS");
    let expected: Vec<u16> = "日本語".encode_utf16().collect();
    assert_eq!(decoded, expected);
}

#[test]
fn get_bytes_charset_supported_encodes_correctly() {
    // The supported-charset path must still produce real bytes. "A©" in
    // ISO-8859-1 = [0x41, 0xE9-ish]; use a code point representable in
    // Latin-1 to confirm the requested charset (not UTF-8) is honoured.
    let mut ctx = mock_ctx();
    let cs = make_charset(&mut ctx, "ISO-8859-1");
    let this = ctx.create_string("A\u{00A9}");
    let r = native_string_get_bytes_charset(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(cs))],
    )
    .expect("supported charset must not error");
    let arr = match r {
        Some(Value::Object(Some(a))) => a,
        other => panic!("expected byte[] result, got {:?}", other),
    };
    // 'A' = 0x41, '©' (U+00A9) = 0xA9 in Latin-1 (single byte, not the
    // 2-byte 0xC2 0xA9 a UTF-8 fallback would emit).
    assert_eq!(ctx.array_length(arr), 2);
    assert_eq!(ctx.get_array_element(arr, 0).as_int(), Some(0x41));
    assert_eq!(
        ctx.get_array_element(arr, 1).as_int(),
        Some(0xA9u8 as i8 as i32)
    );
}

#[test]
fn get_bytes_charset_unsupported_throws_instead_of_latin1() {
    // FIX (charset-nb): `String.getBytes(Charset)` for an unsupported
    // charset must FAIL LOUD rather than silently substitute Latin-1 bytes.
    let mut ctx = mock_ctx();
    let cs = make_charset(&mut ctx, "KOI8-U");
    let this = ctx.create_string("hello");
    let err = native_string_get_bytes_charset(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(cs))],
    )
    .expect_err("unsupported charset must surface an error, not Latin-1 bytes");
    // It must be the UnsupportedOperationException we raise, mentioning the
    // offending charset name — not a silently-wrong byte array.
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("UnsupportedOperationException"),
        "expected UnsupportedOperationException, got: {msg}"
    );
    assert!(
        msg.contains("KOI8-U"),
        "error should name the unsupported charset, got: {msg}"
    );
}

// --- BUG-TC0622: streaming encoder cross-call high-surrogate carry ---

/// Build a synthetic `CharsetEncoder` whose charset (slot 0) is `name`.
fn make_encoder(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    let cs = make_charset(ctx, name);
    let enc =
        try_alloc_concurrent_synthetic(ctx, "java/nio/charset/CharsetEncoder", 3).unwrap();
    ctx.set_field(enc, 0, Value::Object(Some(cs)));
    enc
}

fn coder_tag(ctx: &dyn NativeContext, r: Option<Value>) -> i32 {
    match r {
        Some(Value::Object(Some(cr))) => ctx.get_field(cr, 0).as_int().unwrap_or(-1),
        other => panic!("expected CoderResult, got {:?}", other),
    }
}

fn cb_pos(ctx: &dyn NativeContext, cb: ObjectRef) -> i32 {
    ctx.get_field(cb, BUF_FIELD_POS).as_int().unwrap_or(-1)
}

fn bb_bytes(ctx: &dyn NativeContext, bb: ObjectRef) -> Vec<u8> {
    let (arr, _off, _pos, _lim) = buf_state(ctx, bb).expect("byte buffer state");
    let upto = ctx.get_field(bb, BUF_FIELD_POS).as_int().unwrap_or(0) as usize;
    (0..upto)
        .map(|i| (ctx.get_array_element(arr, i).as_int().unwrap_or(0) & 0xFF) as u8)
        .collect()
}

#[test]
fn encoder_buffers_trailing_high_surrogate_when_more_input_coming() {
    // 'A' followed by a lone high surrogate, endOfInput=false. The encoder
    // must emit 'A', leave the high surrogate buffered (input position stops
    // before it), and report UNDERFLOW — NOT substitute U+FFFD.
    let mut ctx = mock_ctx();
    let enc = make_encoder(&mut ctx, "UTF-8");
    let cb = alloc_char_buffer(&mut ctx, &[0x0041, 0xD800]).unwrap();
    let bb = alloc_byte_buffer(&mut ctx, &[0u8; 8]).unwrap();
    // alloc_byte_buffer sets limit/pos to the data length (8) with pos 0;
    // that's the writable window we need.
    let r = native_encoder_encode(
        &mut ctx,
        &[
            Value::Object(Some(enc)),
            Value::Object(Some(cb)),
            Value::Object(Some(bb)),
            Value::Int(0), // endOfInput = false
        ],
    )
    .expect("encode must not error");
    assert_eq!(coder_tag(&ctx, r), CR_UNDERFLOW);
    assert_eq!(bb_bytes(&ctx, bb), vec![0x41], "only 'A' should be written");
    assert_eq!(cb_pos(&ctx, cb), 1, "high surrogate must stay unconsumed");
}

#[test]
fn encoder_completes_pair_in_single_buffer() {
    // A full surrogate pair (U+10000) in one buffer must encode to its true
    // 4-byte UTF-8 form and consume both units — the carry logic must not
    // mistakenly hold back a well-paired surrogate.
    let mut ctx = mock_ctx();
    let enc = make_encoder(&mut ctx, "UTF-8");
    let cb = alloc_char_buffer(&mut ctx, &[0xD800, 0xDC00]).unwrap(); // U+10000
    let bb = alloc_byte_buffer(&mut ctx, &[0u8; 8]).unwrap();
    let r = native_encoder_encode(
        &mut ctx,
        &[
            Value::Object(Some(enc)),
            Value::Object(Some(cb)),
            Value::Object(Some(bb)),
            Value::Int(0),
        ],
    )
    .expect("encode must not error");
    assert_eq!(coder_tag(&ctx, r), CR_UNDERFLOW);
    assert_eq!(bb_bytes(&ctx, bb), vec![0xF0, 0x90, 0x80, 0x80]);
    assert_eq!(cb_pos(&ctx, cb), 2);
}

#[test]
fn encoder_overflow_stops_on_whole_atom_boundary() {
    // 'A' + U+10000 (a surrogate pair = 4 UTF-8 bytes) into a 3-byte window.
    // 'A' fits (1 byte); the pair (4 bytes) does not. The encoder must stop
    // on the atom boundary — write ONLY 'A', report OVERFLOW, and consume
    // exactly 1 unit — never a truncated multi-byte sequence (the prior
    // proportional path wrote part of the pair and mis-consumed input,
    // corrupting Tomcat's supplementary-char response body).
    let mut ctx = mock_ctx();
    let enc = make_encoder(&mut ctx, "UTF-8");
    let cb = alloc_char_buffer(&mut ctx, &[0x0041, 0xD800, 0xDC00]).unwrap();
    let bb = alloc_byte_buffer(&mut ctx, &[0u8; 3]).unwrap();
    let r = native_encoder_encode(
        &mut ctx,
        &[
            Value::Object(Some(enc)),
            Value::Object(Some(cb)),
            Value::Object(Some(bb)),
            Value::Int(0),
        ],
    )
    .expect("encode must not error");
    assert_eq!(coder_tag(&ctx, r), CR_OVERFLOW);
    assert_eq!(
        bb_bytes(&ctx, bb),
        vec![0x41],
        "only the whole 'A' atom fits"
    );
    assert_eq!(
        cb_pos(&ctx, cb),
        1,
        "only 'A' consumed; the pair is retried"
    );
}

#[test]
fn encoder_trailing_high_surrogate_is_malformed_at_end_of_input() {
    // The same trailing high surrogate, but endOfInput=true: no more input
    // can complete it, so it is genuinely MALFORMED (matches the JDK).
    let mut ctx = mock_ctx();
    let enc = make_encoder(&mut ctx, "UTF-8");
    let cb = alloc_char_buffer(&mut ctx, &[0x0041, 0xD800]).unwrap();
    let bb = alloc_byte_buffer(&mut ctx, &[0u8; 8]).unwrap();
    let r = native_encoder_encode(
        &mut ctx,
        &[
            Value::Object(Some(enc)),
            Value::Object(Some(cb)),
            Value::Object(Some(bb)),
            Value::Int(1), // endOfInput = true
        ],
    )
    .expect("encode must not error");
    assert_eq!(coder_tag(&ctx, r), CR_MALFORMED);
    assert_eq!(
        cb_pos(&ctx, cb),
        1,
        "position advances to the bad surrogate"
    );
}
