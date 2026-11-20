//! Round-trip coverage for the bulk-intrinsic migration of
//! `native_files_read_all_bytes` / `native_files_write_bytes`
//! (2026-05-29 perf fix). The mock uses the default per-element
//! `write_byte_array_from` / `read_byte_array_into` impls, so these
//! tests verify the call-site wiring (offsets, length, byte fidelity)
//! rather than the memcpy override itself.
use super::*;
use crate::test_support::{confine_test_lock, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Build a synthetic `Path`-like object whose slot-0 field is a string
/// holding `path_str` — exactly what `read_path_str` falls back to.
fn make_path(ctx: &mut MockNativeContext, path_str: &str) -> ObjectRef {
    let obj = ctx.alloc_object(1);
    let s = ctx.create_string(path_str);
    ctx.set_field(obj, PATH_FIELD_STR, Value::Object(Some(s)));
    obj
}

/// Unique temp path for a test; cleaned up by the caller.
fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    p.push(format!("cratonvm_files_bulk_{tag}_{nanos}.bin"));
    p
}

#[test]
fn write_bytes_then_read_all_bytes_round_trips() {
    // Confinement is global; pin it off for the duration so an
    // absolute temp path validates. Pair with restore.
    let _g = confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_confine_to_cwd(false);

    let path = temp_path("roundtrip");
    let path_str = path.to_string_lossy().into_owned();

    // Include high bytes (>= 0x80) to confirm the i8/u8 sign handling
    // survives the bulk read path.
    let data: Vec<u8> = vec![0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff, b'A', b'Z'];

    let mut ctx = MockNativeContext::new();
    let p = make_path(&mut ctx, &path_str);

    // Source byte[] for the write.
    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
    for (i, b) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }

    let w = native_files_write_bytes(
        &mut ctx,
        &[Value::Object(Some(p)), Value::Object(Some(arr))],
    )
    .expect("write ok");
    // Returns the Path arg unchanged.
    assert_eq!(w, Some(Value::Object(Some(p))));
    assert_eq!(std::fs::read(&path).unwrap(), data, "file bytes on disk");

    // Read it back through the native and compare the byte[].
    let r = native_files_read_all_bytes(&mut ctx, &[Value::Object(Some(p))]).expect("read ok");
    let read_arr = match r {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected byte[] object, got {other:?}"),
    };
    assert_eq!(ctx.array_length(read_arr), data.len());
    let mut got = Vec::with_capacity(data.len());
    for i in 0..data.len() {
        if let Value::Int(v) = ctx.get_array_element(read_arr, i) {
            got.push(v as u8);
        }
    }
    assert_eq!(got, data, "round-tripped bytes match");

    let _ = std::fs::remove_file(&path);
    set_path_confine_to_cwd(prev);
}

#[test]
fn read_all_bytes_empty_file_yields_zero_length_array() {
    let _g = confine_test_lock().lock();
    let prev = is_path_confine_to_cwd();
    set_path_confine_to_cwd(false);

    let path = temp_path("empty");
    std::fs::write(&path, b"").unwrap();
    let path_str = path.to_string_lossy().into_owned();

    let mut ctx = MockNativeContext::new();
    let p = make_path(&mut ctx, &path_str);
    let r = native_files_read_all_bytes(&mut ctx, &[Value::Object(Some(p))]).expect("read ok");
    match r {
        Some(Value::Object(Some(o))) => assert_eq!(ctx.array_length(o), 0),
        other => panic!("expected empty byte[], got {other:?}"),
    }

    let _ = std::fs::remove_file(&path);
    set_path_confine_to_cwd(prev);
}

#[test]
fn write_bytes_null_byte_path_is_rejected_before_io() {
    // Validation must still fire: a NUL in the path is a security
    // check that runs regardless of confinement.
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    let p = make_path(&mut ctx, "bad\0path");
    let arr = ctx.new_array(ArrayElementType::Byte, 1);
    ctx.set_array_element(arr, 0, Value::Int(1));
    let res = native_files_write_bytes(
        &mut ctx,
        &[Value::Object(Some(p)), Value::Object(Some(arr))],
    );
    assert!(res.is_err(), "null-byte path must be rejected");
}
