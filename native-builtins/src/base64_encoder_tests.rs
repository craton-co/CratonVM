use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

#[test]
fn url_encoder_without_padding_preserves_flag_for_encode_to_string() {
    let mut ctx = MockNativeContext::new();
    let encoder = match native_b64_get_url_encoder(&mut ctx, &[])
        .expect("getUrlEncoder succeeds")
        .expect("getUrlEncoder returns encoder")
    {
        Value::Object(Some(value)) => value,
        other => panic!("expected encoder, got {other:?}"),
    };
    let unpadded = match native_b64_without_padding(&mut ctx, &[Value::Object(Some(encoder))])
        .expect("withoutPadding succeeds")
        .expect("withoutPadding returns encoder")
    {
        Value::Object(Some(value)) => value,
        other => panic!("expected encoder, got {other:?}"),
    };
    let input = ctx.new_array(ArrayElementType::Byte, 2);
    ctx.set_array_element(input, 0, Value::Int(0xff));
    ctx.set_array_element(input, 1, Value::Int(0xff));

    let encoded = match native_b64_encode_to_string(
        &mut ctx,
        &[Value::Object(Some(unpadded)), Value::Object(Some(input))],
    )
    .expect("encodeToString succeeds")
    .expect("encodeToString returns a string")
    {
        Value::Object(Some(value)) => value,
        other => panic!("expected String, got {other:?}"),
    };

    assert_eq!(ctx.read_string(encoded).as_deref(), Some("__8"));
}

/// A null ARGUMENT to any `Base64` encode/decode overload is a
/// `NullPointerException` on HotSpot 25 — measured for all 13
/// encoder/decoder combinations, e.g.
///
/// ```text
/// Decoder.decode((String)null) -> java.lang.NullPointerException
///     / Cannot invoke "String.getBytes(java.nio.charset.Charset)" because "src" is null
/// ```
///
/// `Base64$Decoder.decode(Ljava/lang/String;)[B` was the LAST of the four
/// to be fixed: it defaulted a null to `String::new()` and returned an
/// EMPTY `byte[]`, which is why `RJdkIntrinsics2 --only=b64` reported
/// "decode((String) null) must throw NullPointerException, got none" while
/// the other three arms were already patched. Returning `Ok(None)` is
/// equally wrong — every descriptor here returns a value, so `Ok(None)` is
/// the VOID shape.
///
/// Mutation check: revert any one arm to `Ok(None)` or to a defaulted
/// empty input and exactly that row fails.
#[test]
fn null_argument_throws_npe_on_every_value_returning_overload() {
    fn assert_npe(label: &str, result: MethodCallResult) {
        match result {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. },
            ))) => {}
            Ok(Some(v)) => {
                panic!("{label}: returned {v:?}, HotSpot throws NullPointerException")
            }
            Ok(None) => panic!(
                "{label}: returned the VOID shape `Ok(None)` for a descriptor that returns a \
                 value — the interpreter gets a value of the wrong kind"
            ),
            Err(other) => panic!("{label}: threw {other:?}, expected NullPointerException"),
        }
    }

    // Every (factory, method) pair whose descriptor takes a reference
    // argument, across all three variants.
    let encoder_factories: [(&str, NativeCallback); 3] = [
        ("basic", native_b64_get_encoder),
        ("url", native_b64_get_url_encoder),
        ("mime", native_b64_get_mime_encoder),
    ];
    let decoder_factories: [(&str, NativeCallback); 3] = [
        ("basic", native_b64_get_decoder),
        ("url", native_b64_get_url_decoder),
        ("mime", native_b64_get_mime_decoder),
    ];

    for (variant, factory) in encoder_factories {
        let mut ctx = MockNativeContext::new();
        let enc = match factory(&mut ctx, &[])
            .expect("factory ok")
            .expect("encoder")
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected encoder, got {other:?}"),
        };
        let null_arg = [Value::Object(Some(enc)), Value::Object(None)];
        assert_npe(
            &format!("{variant} Encoder.encode((byte[])null)"),
            native_b64_encode(&mut ctx, &null_arg),
        );
        assert_npe(
            &format!("{variant} Encoder.encodeToString(null)"),
            native_b64_encode_to_string(&mut ctx, &null_arg),
        );
        // …and through withoutPadding(), which returns a DIFFERENT object.
        let unpadded = match native_b64_without_padding(&mut ctx, &[Value::Object(Some(enc))])
            .expect("withoutPadding ok")
            .expect("encoder")
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected encoder, got {other:?}"),
        };
        assert_npe(
            &format!("{variant} Encoder.withoutPadding().encodeToString(null)"),
            native_b64_encode_to_string(
                &mut ctx,
                &[Value::Object(Some(unpadded)), Value::Object(None)],
            ),
        );
    }

    for (variant, factory) in decoder_factories {
        let mut ctx = MockNativeContext::new();
        let dec = match factory(&mut ctx, &[])
            .expect("factory ok")
            .expect("decoder")
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected decoder, got {other:?}"),
        };
        let null_arg = [Value::Object(Some(dec)), Value::Object(None)];
        assert_npe(
            &format!("{variant} Decoder.decode((byte[])null)"),
            native_b64_decode_bytes(&mut ctx, &null_arg),
        );
        // THE ROW THAT WAS FAILING.
        assert_npe(
            &format!("{variant} Decoder.decode((String)null)"),
            native_b64_decode_string(&mut ctx, &null_arg),
        );
    }
}

/// The null-argument fix must not swallow the legitimate EMPTY input:
/// HotSpot returns a zero-length array for `decode("")` and
/// `decode(new byte[0])`, and that is exactly the answer the broken
/// `decode((String) null)` used to give — so this is the negative control
/// that keeps the fix from being "throw on anything falsy".
#[test]
fn empty_input_still_decodes_to_an_empty_array() {
    let mut ctx = MockNativeContext::new();
    let dec = match native_b64_get_decoder(&mut ctx, &[])
        .expect("getDecoder ok")
        .expect("decoder")
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected decoder, got {other:?}"),
    };
    let empty_str = ctx.create_string("");
    let decoded = match native_b64_decode_string(
        &mut ctx,
        &[Value::Object(Some(dec)), Value::Object(Some(empty_str))],
    )
    .expect("decode(\"\") must NOT throw")
    .expect("decode returns an array")
    {
        Value::Object(Some(o)) => o,
        other => panic!("expected byte[], got {other:?}"),
    };
    assert_eq!(
        ctx.array_length(decoded),
        0,
        "decode(\"\") is empty, not a throw"
    );
}

// ---------------------------------------------------------------------
// Below: BEHAVIOURAL cover for the fabricated receiver. Every assertion
// reads a value the native WROTE, or an answer the native COMPUTED from a
// receiver's fields — never "the triple is registered". A sibling file's 64
// registration-only tests all stayed green through a real layout defect.
// ---------------------------------------------------------------------

fn obj(result: MethodCallResult, what: &str) -> ObjectRef {
    match result.unwrap_or_else(|e| panic!("{what} failed: {e:?}")) {
        Some(Value::Object(Some(o))) => o,
        other => panic!("{what}: expected an object, got {other:?}"),
    }
}

fn encode_to_string(ctx: &mut MockNativeContext, enc: ObjectRef, src: &[u8]) -> String {
    let arr = ctx.new_array(ArrayElementType::Byte, src.len());
    for (i, &b) in src.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i32));
    }
    let s = obj(
        native_b64_encode_to_string(ctx, &[Value::Object(Some(enc)), Value::Object(Some(arr))]),
        "encodeToString",
    );
    ctx.read_string(s).expect("encodeToString returns a String")
}

/// Give an encoder the field values real `Base64.getMimeEncoder(20, {'\n'})`
/// bytecode would have built, since that factory is deliberately
/// unregistered and this file therefore never allocates such an object
/// itself.
fn retune(ctx: &mut MockNativeContext, enc: ObjectRef, linemax: i32, sep: &[u8]) {
    let nl = ctx.new_array(ArrayElementType::Byte, sep.len());
    for (i, &b) in sep.iter().enumerate() {
        ctx.set_array_element(nl, i, Value::Int(b as i32));
    }
    ctx.set_field(enc, B64_ENCODER_FIELD_LINEMAX, Value::Int(linemax));
    ctx.set_field(enc, B64_ENCODER_FIELD_NEWLINE, Value::Object(Some(nl)));
}

/// (1) The fabricated `Encoder` must carry all FOUR JDK fields.
///
/// `newline` was left unwritten, and real `Encoder.encodedOutLength` reads
/// `newline.length` for EVERY encode call whose `linemax > 0` — not only
/// ones long enough to wrap. Measured on HotSpot by building the same shape
/// through the private constructor (`scratchpad/e14/B64Fab.java`):
///
/// ```text
/// Encoder(false, null, 76, true).encodeToString(30 bytes) ->
///   NullPointerException / Cannot read the array length because "this.newline" is null
/// Encoder(false, {13,10}, 76, true).encodeToString(60 bytes) -> 82  (== getMimeEncoder())
/// ```
///
/// The mock's unwritten slots read back as `Value::Int(0)`, so the
/// `Value::Object(None)` assertions below fail if the explicit null write is
/// dropped — they measure the WRITE, not the default.
#[test]
fn fabricated_encoders_carry_the_jdk_newline_and_linemax_fields() {
    let mut ctx = MockNativeContext::new();

    let mime = obj(native_b64_get_mime_encoder(&mut ctx, &[]), "getMimeEncoder");
    assert_eq!(
        ctx.get_field(mime, B64_ENCODER_FIELD_LINEMAX),
        Value::Int(76)
    );
    let nl = match ctx.get_field(mime, B64_ENCODER_FIELD_NEWLINE) {
        Value::Object(Some(a)) => a,
        other => panic!(
            "getMimeEncoder()'s `newline` is {other:?}; real encode0 and encodedOutLength \
             both dereference it, and HotSpot NPEs on a null one"
        ),
    };
    assert_eq!(ctx.array_length(nl), 2, "CRLF is two bytes");
    assert_eq!(ctx.get_array_element(nl, 0), Value::Int(13));
    assert_eq!(ctx.get_array_element(nl, 1), Value::Int(10));

    // (2) -1, NOT 0, for the non-wrapping encoders. `> 0` reads them alike;
    // `Base64$EncOutputStream`'s `linepos == linemax` does not, and with 0 it
    // fires on the very first byte: `getEncoder().wrap(os)` measured as
    // NullPointerException / Cannot read the array length because "b" is null.
    for (name, factory) in [
        ("getEncoder", native_b64_get_encoder as NativeCallback),
        ("getUrlEncoder", native_b64_get_url_encoder),
    ] {
        let enc = obj(factory(&mut ctx, &[]), name);
        assert_eq!(
            ctx.get_field(enc, B64_ENCODER_FIELD_LINEMAX),
            Value::Int(-1),
            "{name}: real JDK writes -1; 0 makes the unregistered wrap(OutputStream) \
             throw NullPointerException on its first write"
        );
        assert_eq!(
            ctx.get_field(enc, B64_ENCODER_FIELD_NEWLINE),
            Value::Object(None),
            "{name}: `newline` must be written null explicitly, not left as the \
             allocator found it"
        );
    }
}

/// (2) A custom `linemax`/`newline` must reach the encoder.
///
/// `Base64.getMimeEncoder(int, byte[])` is unregistered, so it runs real
/// bytecode and hands back an `Encoder` with `linemax=20, newline=[10]`;
/// `encodeToString` on it is then intercepted here. HotSpot answers **83**
/// for the 60-byte fixture and this file used to answer **82** — one short,
/// because it re-derived 76/CRLF from a variant tag. 82 vs 83 is the
/// dangerous kind of wrong: both round-trip through a MIME decoder and both
/// pass a length-is-about-right assertion. So assert the TEXT.
#[test]
fn a_custom_linemax_and_separator_reach_the_encoder() {
    let mut ctx = MockNativeContext::new();
    let payload: Vec<u8> = (0..60u16).map(|i| i as u8).collect();

    let canonical = obj(native_b64_get_mime_encoder(&mut ctx, &[]), "getMimeEncoder");
    assert_eq!(
        encode_to_string(&mut ctx, canonical, &payload).len(),
        82,
        "getMimeEncoder(): 80 chars + one CRLF"
    );

    let custom = obj(native_b64_get_mime_encoder(&mut ctx, &[]), "getMimeEncoder");
    retune(&mut ctx, custom, 20, b"\n");
    let encoded = encode_to_string(&mut ctx, custom, &payload);
    assert_eq!(
        encoded,
        "AAECAwQFBgcICQoLDA0O\nDxAREhMUFRYXGBkaGxwd\nHh8gISIjJCUmJygpKiss\nLS4vMDEyMzQ1Njc4OTo7",
        "four 20-char lines separated by LF"
    );
    assert_eq!(
        encoded.len(),
        83,
        "HotSpot: 83. The hardcoded 76/CRLF gave 82"
    );
}

/// (2, cont.) `withoutPadding()` COPIES the fields; it does not rebuild the
/// encoder from a variant. Rebuilding silently reset a custom 20/LF encoder
/// to 76/CRLF. Measured on HotSpot:
/// `getMimeEncoder(20,{'\n'}).withoutPadding()` keeps `linemax=20`,
/// `newline=[10]`, and encodes 61 bytes to 86 (88 with padding).
#[test]
fn without_padding_preserves_a_custom_linemax_and_shares_the_separator() {
    let mut ctx = MockNativeContext::new();
    let padded = obj(native_b64_get_mime_encoder(&mut ctx, &[]), "getMimeEncoder");
    retune(&mut ctx, padded, 20, b"\n");
    let sep = ctx.get_field(padded, B64_ENCODER_FIELD_NEWLINE);

    let unpadded = obj(
        native_b64_without_padding(&mut ctx, &[Value::Object(Some(padded))]),
        "withoutPadding",
    );
    assert_ne!(
        unpadded, padded,
        "a PADDED encoder returns a fresh object (HotSpot: \
         enc.withoutPadding() == enc.withoutPadding() is false)"
    );
    assert_eq!(
        ctx.get_field(unpadded, B64_ENCODER_FIELD_LINEMAX),
        Value::Int(20),
        "the copy must keep the custom linemax, not fall back to 76"
    );
    assert_eq!(
        ctx.get_field(unpadded, B64_ENCODER_FIELD_NEWLINE),
        sep,
        "real JDK passes the SAME newline array to the copy"
    );

    let payload = vec![0u8; 61];
    assert_eq!(encode_to_string(&mut ctx, padded, &payload).len(), 88);
    assert_eq!(encode_to_string(&mut ctx, unpadded, &payload).len(), 86);

    // …and an ALREADY-unpadded encoder returns `this` (HotSpot: identity true).
    let again = obj(
        native_b64_without_padding(&mut ctx, &[Value::Object(Some(unpadded))]),
        "withoutPadding",
    );
    assert_eq!(again, unpadded);
}

/// (1, cont.) An encoder with `linemax > 0` and a null `newline` is the
/// exact shape this file used to fabricate, and HotSpot throws
/// `NullPointerException` for it on every encode call. Reproduce the throw
/// rather than quietly defaulting to CRLF — a default would give a plausible
/// answer for an object the JDK considers malformed.
#[test]
fn a_wrapping_encoder_with_a_null_newline_throws_npe() {
    let mut ctx = MockNativeContext::new();
    let enc = obj(native_b64_get_mime_encoder(&mut ctx, &[]), "getMimeEncoder");
    ctx.set_field(enc, B64_ENCODER_FIELD_NEWLINE, Value::Object(None));
    let arr = ctx.new_array(ArrayElementType::Byte, 30);
    match native_b64_encode_to_string(
        &mut ctx,
        &[Value::Object(Some(enc)), Value::Object(Some(arr))],
    ) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. },
        ))) => {}
        other => panic!(
            "linemax=76 with a null newline must throw NullPointerException \
             (HotSpot: Cannot read the array length because \"this.newline\" is null), got {other:?}"
        ),
    }
    // A NON-wrapping encoder with a null newline is perfectly legal — that
    // is what `getEncoder()` is — so the throw must not generalise.
    let basic = obj(native_b64_get_encoder(&mut ctx, &[]), "getEncoder");
    assert_eq!(encode_to_string(&mut ctx, basic, &[0u8; 30]).len(), 40);
}

/// (4) The `Decoder` synthetic must be the real JDK's TWO-boolean layout.
///
/// It was one Int slot holding a variant tag, and slot 0 of the real layout
/// is `isURL` — so `getMimeDecoder()` wrote 2 into `isURL` and left `isMIME`
/// false. Measured on HotSpot by building `Decoder(isURL=true, isMIME=false)`
/// through the private constructor and feeding it what a MIME decoder must
/// accept (`scratchpad/e14/B64Unreg.java`): 76-column wrapped text became
/// `IllegalArgumentException / Illegal base64 character d`, `"-_-_"` decoded
/// to 3 bytes instead of 0, `"QQ\n=="` threw instead of decoding to 1 byte.
///
/// Both halves are asserted: the field VALUES (what real bytecode reads) and
/// the decode ANSWERS (what this file's own natives compute from them).
#[test]
fn decoder_uses_the_two_boolean_jdk_layout() {
    let mut ctx = MockNativeContext::new();
    // (factory, isURL, isMIME) — read off HotSpot with reflection.
    let rows: [(&str, NativeCallback, i32, i32); 3] = [
        ("getDecoder", native_b64_get_decoder, 0, 0),
        ("getUrlDecoder", native_b64_get_url_decoder, 1, 0),
        ("getMimeDecoder", native_b64_get_mime_decoder, 0, 1),
    ];
    for (name, factory, is_url, is_mime) in rows {
        let dec = obj(factory(&mut ctx, &[]), name);
        assert_eq!(
            ctx.get_field(dec, B64_DECODER_FIELD_IS_URL),
            Value::Int(is_url),
            "{name}: field 0 is `isURL`, and real bytecode reads it as a boolean"
        );
        assert_eq!(
            ctx.get_field(dec, B64_DECODER_FIELD_IS_MIME),
            Value::Int(is_mime),
            "{name}: field 1 is `isMIME`"
        );
    }

    // The three variants must still answer differently — this is what the
    // old layout broke, by making the MIME decoder read back as a URL one.
    // `None` means the decoder threw, which is a legitimate answer here.
    //
    // `-_-_` is the input with THREE different correct answers across the
    // variants, so it alone separates all three field pairs above.
    assert_eq!(
        decode_len(
            &mut ctx,
            native_b64_get_url_decoder,
            "getUrlDecoder",
            "-_-_"
        ),
        Some(3),
        "the URL alphabet decodes -_-_ to 3 bytes"
    );
    assert_eq!(
        decode_len(
            &mut ctx,
            native_b64_get_mime_decoder,
            "getMimeDecoder",
            "-_-_"
        ),
        Some(0),
        "MIME IGNORES them -> empty. Under the old layout this decoder read \
         back as URL and answered 3."
    );
    assert_eq!(
        decode_len(&mut ctx, native_b64_get_decoder, "getDecoder", "-_-_"),
        None,
        "BASIC rejects them"
    );
    // A CRLF-wrapped payload: MIME accepts, BASIC rejects.
    assert_eq!(
        decode_len(
            &mut ctx,
            native_b64_get_mime_decoder,
            "getMimeDecoder",
            "QQ\r\n=="
        ),
        Some(1)
    );
    assert_eq!(
        decode_len(&mut ctx, native_b64_get_decoder, "getDecoder", "QQ\r\n=="),
        None
    );
}

/// Decoded length through a factory's decoder, or `None` when it threw.
fn decode_len(
    ctx: &mut MockNativeContext,
    factory: NativeCallback,
    name: &str,
    text: &str,
) -> Option<usize> {
    let dec = obj(factory(ctx, &[]), name);
    let s = ctx.create_string(text);
    match native_b64_decode_string(ctx, &[Value::Object(Some(dec)), Value::Object(Some(s))]) {
        Ok(Some(Value::Object(Some(a)))) => Some(ctx.array_length(a)),
        Ok(other) => panic!("{name}.decode({text:?}): expected a byte[], got {other:?}"),
        Err(_) => None,
    }
}

/// (3) The six no-arg factories are SINGLETONS on HotSpot — `==`, not
/// `equals` (`scratchpad/e14/B64Ident.java`, all six rows `true`) — and the
/// singletons are the JDK's own `Encoder.RFC4648` / `RFC4648_URLSAFE` /
/// `RFC2045` statics and the three matching ones on `Decoder`.
///
/// So the fix is to RETURN THE JDK'S FIELD, not to memoize a fabricated
/// object in a native `static`. A cache would reproduce `getEncoder() ==
/// getEncoder()` but not this, also measured:
///
/// ```text
/// Base64.getMimeEncoder(0, sep) == Base64.getEncoder() -> true
/// ```
///
/// because `getMimeEncoder(int, byte[])` is unregistered, runs real
/// bytecode, and returns the real `RFC4648` for any `lineLength <= 0`.
///
/// This test DECLARES the static field on the mock rather than relying on a
/// name-to-slot fallback, so it measures the lookup and not the mock.
#[test]
fn the_factories_return_the_jdk_singleton_when_there_is_one() {
    use cratonvm_native_api::FieldMetadata;
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("java/util/Base64$Encoder")
        .expect("class");
    let marker = obj(
        native_b64_get_mime_encoder(&mut ctx, &[]),
        "stand-in object",
    );
    ctx.set_declared_fields(
        cid,
        vec![FieldMetadata {
            name: "RFC4648".to_string(),
            descriptor: "Ljava/util/Base64$Encoder;".to_string(),
            access_flags: 0x0008, // ACC_STATIC
            slot_index: 0,
            declaring_class_id: cid,
            is_static: true,
        }],
    );
    ctx.set_static_field(cid, 0, Value::Object(Some(marker)));

    let first = obj(native_b64_get_encoder(&mut ctx, &[]), "getEncoder");
    let second = obj(native_b64_get_encoder(&mut ctx, &[]), "getEncoder");
    assert_eq!(
        first, marker,
        "getEncoder() must hand back java.util.Base64$Encoder.RFC4648 itself"
    );
    assert_eq!(first, second, "HotSpot: getEncoder() == getEncoder()");
}

/// …and the fallback is what keeps the row above from being a regression in
/// synthetic-JDK mode, where there is no `RFC4648` field to take. With no
/// declared static the factory must fabricate, exactly as before, and the
/// fabricated object must still be usable.
#[test]
fn the_factories_fabricate_when_there_is_no_jdk_singleton() {
    let mut ctx = MockNativeContext::new();
    let first = obj(native_b64_get_encoder(&mut ctx, &[]), "getEncoder");
    let second = obj(native_b64_get_encoder(&mut ctx, &[]), "getEncoder");
    assert_ne!(
        first, second,
        "with no real class library there is nothing to be a singleton OF; \
         fabricating a fresh encoder is the documented fallback"
    );
    assert_eq!(encode_to_string(&mut ctx, first, b"\xff\xff\xff").len(), 4);
}
