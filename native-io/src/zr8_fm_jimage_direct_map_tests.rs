use super::jimage_direct_mapping;

/// The mapping is the file's bytes, made once per path; an empty or
/// missing file is refused (the caller then falls back to the copy).
#[test]
fn zr8_fm_the_direct_mapping_is_the_files_bytes_once_per_path() {
    let dir =
        std::env::temp_dir().join(format!("cratonvm_zr8fm_jimage_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("modules");
    let bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let key = path.to_string_lossy().into_owned();
    let (addr, len) = jimage_direct_mapping(&key).expect("maps");
    assert_eq!(len, bytes.len());
    // SAFETY: the mapping is leaked for the process, `len` bytes long.
    let seen = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    assert_eq!(seen, &bytes[..]);
    let again = jimage_direct_mapping(&key);
    assert_eq!(again, Some((addr, len)), "one mapping per path");
    let empty = dir.join("empty");
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(jimage_direct_mapping(&empty.to_string_lossy()), None);
    let absent = dir.join("absent");
    assert_eq!(jimage_direct_mapping(&absent.to_string_lossy()), None);
}

/// zr z11/rv9: a `DirectByteBuffer(long, long)` constructor that fails
/// (an image older than JDK 19, or a constructor that throws) falls back
/// to the `byte[]` copy instead of throwing out of `getNativeMap`. The arm
/// with a working constructor is the positive control: no copy, no wrap.
#[test]
fn zr11_rv9_a_failed_direct_buffer_constructor_takes_the_copy() {
    use crate::test_support::MockNativeContext;
    use cratonvm_native_api::{NativeHeapAccess, NativeInvokeAccess};
    use cratonvm_types::error::MethodCallFailed;
    use cratonvm_types::Value;
    if !super::io_flags().jimage_direct_map {
        return; // the kill arm is set for this process: nothing to test
    }
    let dir =
        std::env::temp_dir().join(format!("cratonvm_zr11rv9_jimage_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("modules");
    let bytes: Vec<u8> = (0..5_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let path = path.to_string_lossy().into_owned();
    let wrap = ("wrap", "([B)Ljava/nio/ByteBuffer;");

    // Positive control: the constructor works, the direct buffer is it.
    let mut ctx = MockNativeContext::new();
    let arg = ctx.create_string_uninterned(&path);
    let got = super::native_jimage_get_native_map(&mut ctx, &[Value::Object(Some(arg))])
        .expect("direct buffer");
    assert!(matches!(got, Some(Value::Object(Some(_)))));
    assert!(
        !ctx.recorded_calls().iter().any(|c| c.method_name == wrap.0),
        "a served direct map copies nothing"
    );

    // The constructor throws: the copy is wrapped and returned.
    let mut ctx = MockNativeContext::new();
    let arg = ctx.create_string_uninterned(&path);
    let thrown = match ctx.new_object("java/lang/NoSuchMethodError") {
        Ok(Some(Value::Object(Some(t)))) => t,
        other => panic!("{other:?}"),
    };
    ctx.script_invoke("<init>", "(JJ)V", Err(MethodCallFailed::ExceptionThrown(thrown)));
    let wrapped = ctx.new_object("java/nio/HeapByteBuffer").unwrap();
    ctx.script_invoke(wrap.0, wrap.1, Ok(wrapped));
    let got = super::native_jimage_get_native_map(&mut ctx, &[Value::Object(Some(arg))])
        .expect("the copy, not the constructor's exception");
    assert_eq!(got, wrapped, "the wrapped copy is the answer");
    let call = ctx
        .recorded_calls()
        .iter()
        .find(|c| c.method_name == wrap.0)
        .expect("ByteBuffer.wrap over the copy")
        .clone();
    let Some(Value::Object(Some(arr))) = call.args.first().copied() else {
        panic!("wrap arg {:?}", call.args);
    };
    assert_eq!(ctx.array_length(arr), bytes.len());
    assert_eq!(ctx.get_array_element(arr, 4999), Value::Int((4999 % 251) as u8 as i8 as i32));
}

/// JIT round 21, lane jimage: with round 20's off-heap rows retired, the
/// copy fall-back is the only thing `jimage_map_cache` holds. A second
/// call in the same VM wraps the SAME rooted array (no second copy, no
/// second root), and VM teardown drops the row.
#[test]
fn jr21_jimage_the_copy_fallback_is_cached_once_per_vm() {
    use crate::test_support::MockNativeContext;
    use cratonvm_native_api::{NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess};
    use cratonvm_types::error::MethodCallFailed;
    use cratonvm_types::Value;
    let dir = std::env::temp_dir().join(format!("cratonvm_jr21jimage_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("modules");
    let bytes: Vec<u8> = (0..3_000u32).map(|i| (i % 241) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    let path = path.to_string_lossy().into_owned();
    let wrap = ("wrap", "([B)Ljava/nio/ByteBuffer;");

    let mut ctx = MockNativeContext::new();
    let mut wrapped_arrays = Vec::new();
    for _ in 0..2 {
        let arg = ctx.create_string_uninterned(&path);
        // Force the fall-back whatever the direct-map switch says: the
        // direct buffer's constructor throws (it is not called at all
        // under `CRATONVM_JIMAGE_DIRECT_MAP=0`; the script then stays
        // unused, which changes nothing below).
        let thrown = match ctx.new_object("java/lang/NoSuchMethodError") {
            Ok(Some(Value::Object(Some(t)))) => t,
            other => panic!("{other:?}"),
        };
        ctx.script_invoke(
            "<init>",
            "(JJ)V",
            Err(MethodCallFailed::ExceptionThrown(thrown)),
        );
        let buf = ctx.new_object("java/nio/HeapByteBuffer").unwrap();
        ctx.script_invoke(wrap.0, wrap.1, Ok(buf));
        let got = super::native_jimage_get_native_map(&mut ctx, &[Value::Object(Some(arg))])
            .expect("the copy");
        assert_eq!(got, buf);
        let call = ctx
            .recorded_calls()
            .iter()
            .rev()
            .find(|c| c.method_name == wrap.0)
            .expect("ByteBuffer.wrap over the copy")
            .clone();
        let Some(Value::Object(Some(arr))) = call.args.first().copied() else {
            panic!("wrap arg {:?}", call.args);
        };
        assert_eq!(ctx.array_length(arr), bytes.len());
        wrapped_arrays.push(arr);
    }
    assert_eq!(wrapped_arrays[0], wrapped_arrays[1], "one copy per VM");
    assert_eq!(ctx.global_root_count(), 1, "one root per VM");
    let key = (ctx.vm_identity(), path.clone());
    assert!(super::jimage_map_cache().lock().contains_key(&key));
    super::forget_vm_io_side_tables(ctx.vm_identity());
    assert!(!super::jimage_map_cache().lock().contains_key(&key));
    let _ = std::fs::remove_dir_all(&dir);
}
