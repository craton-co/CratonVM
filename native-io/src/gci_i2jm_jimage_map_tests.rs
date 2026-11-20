use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Does `vm` hold a heap-copy row for `path`? Since JIT round 21 (lane
/// jimage) the heap copy is the only row `jimage_map_cache` holds.
fn heap_row(vm: usize, path: &str) -> bool {
    jimage_map_cache()
        .lock()
        .contains_key(&(vm, path.to_string()))
}

/// The heap-copy arm: `CRATONVM_JIMAGE_DIRECT_MAP` off (`direct = false`).
fn heap_copy(ctx: &mut MockNativeContext, path: String) -> MethodCallResult {
    jimage_native_map_for(ctx, path, false)
}

/// A file standing in for `lib/modules`, unique to this test and this
/// process. Never deleted: its process map is never unmapped, and Windows
/// refuses to delete a mapped file.
fn image_file(tag: &str, bytes: &[u8]) -> String {
    let path = std::env::temp_dir().join(format!(
        "cratonvm-gci-i2jm-{tag}-{}.jimage",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("write the stand-in image");
    path.to_string_lossy().into_owned()
}

/// Not a run of one byte value, so an offset error cannot read back equal.
fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) + 7) as u8).collect()
}

/// `(address, capacity)` of the last `DirectByteBuffer.<init>(JJ)V` the
/// native ran (the mock records `new_object_initialized` as `<init>`,
/// receiver first).
fn direct_ctor(ctx: &MockNativeContext) -> Option<(i64, i64)> {
    ctx.recorded_calls()
        .iter()
        .rev()
        .find(|c| {
            c.declared_class.as_deref() == Some("java/nio/DirectByteBuffer")
                && c.method_name == "<init>"
                && c.descriptor == "(JJ)V"
        })
        .and_then(|c| match (c.args.get(1), c.args.get(2)) {
            (Some(Value::Long(address)), Some(Value::Long(capacity))) => {
                Some((*address, *capacity))
            }
            _ => None,
        })
}

fn wrap_calls(ctx: &MockNativeContext) -> usize {
    ctx.recorded_calls()
        .iter()
        .filter(|c| {
            c.declared_class.as_deref() == Some("java/nio/ByteBuffer")
                && c.method_name == "wrap"
        })
        .count()
}

/// The bytes at `address`, read as `Unsafe.get*(null, address)` reads them.
fn mapped_bytes(address: i64, len: usize) -> Vec<u8> {
    // SAFETY: `address` is the base of a live read-only map of at least
    // `len` bytes (the process map, never unmapped).
    unsafe { std::slice::from_raw_parts(address as usize as *const u8, len) }.to_vec()
}

#[test]
fn the_default_arm_answers_a_direct_buffer_over_the_image_bytes() {
    let bytes = sample(4099);
    let path = image_file("direct", &bytes);
    let mut ctx = MockNativeContext::new();
    let answer = jimage_native_map_for(&mut ctx, path.clone(), true).expect("getNativeMap");
    assert!(
        matches!(answer, Some(Value::Object(Some(_)))),
        "a buffer object, got {answer:?}"
    );
    let (address, capacity) = direct_ctor(&ctx).expect("DirectByteBuffer(long, long)");
    assert_eq!(capacity, bytes.len() as i64);
    // What `copy_from_native_memory` reads raw: positive, no arena tag.
    assert!(address > 0 && address & (1i64 << 62) == 0, "{address:#x}");
    assert_eq!(mapped_bytes(address, bytes.len()), bytes);
    assert_eq!(wrap_calls(&ctx), 0, "no heap copy");
    let vm = ctx.vm_identity();
    assert!(!heap_row(vm, &path), "no heap row for this VM");
    crate::forget_vm_io_side_tables(vm);
}

#[test]
fn the_map_is_the_processes_and_outlives_a_vm_teardown() {
    let bytes = sample(8192 + 13);
    let path = image_file("shared", &bytes);
    let mut a = MockNativeContext::new();
    let mut b = MockNativeContext::new();
    jimage_native_map_for(&mut a, path.clone(), true).expect("A");
    let (in_a, _) = direct_ctor(&a).expect("A's buffer");
    jimage_native_map_for(&mut b, path.clone(), true).expect("B");
    let (in_b, _) = direct_ctor(&b).expect("B's buffer");
    assert_eq!(in_a, in_b, "one map per image path, not one per VM");
    // A's teardown leaves the map: B (or a daemon thread of A) reads it.
    crate::forget_vm_io_side_tables(a.vm_identity());
    assert_eq!(jimage_direct_mapping(&path).map(|(at, _)| at as i64), Some(in_a));
    jimage_native_map_for(&mut b, path.clone(), true).expect("B again");
    assert_eq!(direct_ctor(&b).map(|(at, _)| at), Some(in_b));
    assert_eq!(mapped_bytes(in_b, bytes.len()), bytes);
    crate::forget_vm_io_side_tables(b.vm_identity());
}

#[test]
fn the_kill_arm_keeps_the_heap_copy() {
    let bytes = sample(777);
    let path = image_file("heap", &bytes);
    let mut ctx = MockNativeContext::new();
    heap_copy(&mut ctx, path.clone()).expect("getNativeMap");
    assert!(direct_ctor(&ctx).is_none(), "no direct buffer");
    let wrap = ctx
        .recorded_calls()
        .iter()
        .rev()
        .find(|c| c.method_name == "wrap")
        .cloned()
        .expect("ByteBuffer.wrap(byte[])");
    let Some(Value::Object(Some(arr))) = wrap.args.first().copied() else {
        panic!("wrap's argument: {:?}", wrap.args);
    };
    assert_eq!(ctx.array_length(arr), bytes.len());
    let copied: Vec<u8> = (0..bytes.len())
        .map(|i| ctx.get_array_element(arr, i).as_int().unwrap_or(-1) as u8)
        .collect();
    assert_eq!(copied, bytes);
    let vm = ctx.vm_identity();
    assert!(heap_row(vm, &path));
    crate::forget_vm_io_side_tables(vm);
    assert!(!heap_row(vm, &path));
}

/// One VM never answers both kinds: a heap copy it already holds answers
/// again with the switch on.
#[test]
fn a_vm_holding_a_heap_copy_keeps_answering_it() {
    let bytes = sample(300);
    let path = image_file("sticky", &bytes);
    let mut ctx = MockNativeContext::new();
    heap_copy(&mut ctx, path.clone()).expect("heap copy");
    jimage_native_map_for(&mut ctx, path, true).expect("again, switch on");
    assert_eq!(wrap_calls(&ctx), 2);
    assert!(direct_ctor(&ctx).is_none());
    crate::forget_vm_io_side_tables(ctx.vm_identity());
}

/// The JDK's fall-back contract: an image that cannot be read is `null`
/// (then `BasicImageReader` opens a `FileChannel`), in both arms; an empty
/// one is not mapped and takes the heap copy, as before.
#[test]
fn an_unreadable_image_is_null_and_an_empty_one_is_not_mapped() {
    let missing = std::env::temp_dir()
        .join(format!("cratonvm-gci-i2jm-missing-{}.jimage", std::process::id()))
        .to_string_lossy()
        .into_owned();
    for direct in [true, false] {
        let mut ctx = MockNativeContext::new();
        let answer =
            jimage_native_map_for(&mut ctx, missing.clone(), direct).expect("getNativeMap");
        assert_eq!(answer, Some(Value::Object(None)), "direct={direct}");
        crate::forget_vm_io_side_tables(ctx.vm_identity());
    }
    let empty = image_file("empty", &[]);
    let mut ctx = MockNativeContext::new();
    jimage_native_map_for(&mut ctx, empty.clone(), true).expect("getNativeMap");
    assert!(direct_ctor(&ctx).is_none());
    assert_eq!(wrap_calls(&ctx), 1);
    assert_eq!(jimage_direct_mapping(&empty), None, "an empty image is not mapped");
    crate::forget_vm_io_side_tables(ctx.vm_identity());
}
