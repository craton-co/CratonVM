use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeHeapAccess;

/// `Inflater.c` hands zlib's own `strm->msg` to `DataFormatException`;
/// flate2's `Display` would prefix "deflate decompression error: ".
#[test]
fn i115_a_data_error_carries_zlibs_bare_message() {
    let addr = inflater_init(false, false);
    let mut out = vec![0u8; 64];
    let step = infl_do_decompress(addr, &[1, 2, 3, 4, 5, 6, 7, 8], &mut out);
    assert_eq!(step.data_error.as_deref(), Some("incorrect header check"));
    let step = {
        let addr = inflater_init(true, false);
        infl_do_decompress(addr, &[0x07, 0, 0, 0], &mut out)
    };
    assert_eq!(step.data_error.as_deref(), Some("invalid block type"));
}

/// `checkSetDictionaryResult`: zlib refuses a dictionary on a fresh
/// zlib-wrapped inflater (`Z_STREAM_ERROR`) and on a deflater whose header
/// is out; a raw inflater and a fresh deflater take it. A refusal has no
/// message, and the native throws.
#[test]
fn i115_a_refused_dictionary_is_reported() {
    assert_eq!(infl_apply_dictionary(inflater_init(false, false), b"abc"), Some(None));
    assert_eq!(infl_apply_dictionary(inflater_init(true, false), b"abc"), None);

    let mut ctx = mock_ctx();
    let daddr = match defl_init(&mut ctx, &[Value::Int(6), Value::Int(0), Value::Int(0)])
        .unwrap()
        .unwrap()
    {
        Value::Long(a) => a,
        other => panic!("expected Long handle, got {other:?}"),
    };
    assert_eq!(defl_apply_dictionary(daddr, b"abc"), None, "before the header");
    let payload = b"some payload some payload";
    let in_arr = ctx.new_array(ArrayElementType::Byte, payload.len());
    for (i, b) in payload.iter().enumerate() {
        ctx.set_array_element(in_arr, i, Value::Int(*b as i8 as i32));
    }
    let out_arr = ctx.new_array(ArrayElementType::Byte, 256);
    defl_deflate_bytes_bytes(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Long(daddr),
            Value::Object(Some(in_arr)),
            Value::Int(0),
            Value::Int(payload.len() as i32),
            Value::Object(Some(out_arr)),
            Value::Int(0),
            Value::Int(256),
            Value::Int(2), // SYNC_FLUSH
            Value::Int(0),
        ],
    )
    .unwrap();
    assert_eq!(defl_apply_dictionary(daddr, b"abc"), Some(None), "past the header");

    let iaddr = inflater_init(false, false);
    let dict = ctx.new_array(ArrayElementType::Byte, 3);
    assert!(infl_set_dictionary(
        &mut ctx,
        &[Value::Long(iaddr), Value::Object(Some(dict)), Value::Int(0), Value::Int(3)],
    )
    .is_err());
}
