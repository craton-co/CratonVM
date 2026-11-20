use super::*;
use crate::nio_native::{
    descriptor_fd_id, descriptor_fields_for_id, fd_fields_per_platform_enabled,
    standard_stream_handle,
};
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn stream_with_descriptor(ctx: &mut MockNativeContext) -> (ObjectRef, ObjectRef) {
    let fd_obj = ctx.alloc_object(2);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(-1));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
    let stream = ctx.alloc_object(2);
    ctx.set_field_by_name(stream, "fd", Value::Object(Some(fd_obj)));
    // The real-JDK layout: slot 0 IS the `fd` reference. The mock keeps
    // named fields apart from slots, and a slot left at its `Int(0)`
    // default reads as the legacy synthetic layout's raw id 0 (the
    // wave-2 failure of `an_unconstructed_descriptor_is_not_a_standard_stream`
    // was that fallback, not the descriptor reader).
    ctx.set_field(stream, 0, Value::Object(Some(fd_obj)));
    (stream, fd_obj)
}

/// HotSpot's shapes: Unix `fd = n, handle = -1`; Windows `fd = -1,
/// handle = n`; the off arm keeps the old both-fields shape.
#[test]
fn fields_follow_the_platform() {
    if cfg!(windows) {
        assert_eq!(descriptor_fields_for_id(9, true), (-1, 9));
    } else {
        assert_eq!(descriptor_fields_for_id(9, true), (9, -1));
    }
    assert_eq!(descriptor_fields_for_id(9, false), (9, 9));
    assert_eq!(standard_stream_handle(0, false), 0);
    assert_eq!(
        standard_stream_handle(1, true),
        cratonvm_native_api::os_encoding::standard_stream_os_handle(1)
    );
    if !cfg!(windows) {
        assert_eq!(standard_stream_handle(1, true), -1);
    }
}

/// JIT r23 w2 L6: a Windows standard stream's descriptor carries the real
/// `HANDLE` (HotSpot's `getHandle`) and still reads as its fd; any other
/// handle beside `fd` 1 is a table id, as before.
#[test]
fn a_standard_stream_handle_reads_as_its_fd() {
    let mut ctx = MockNativeContext::new();
    let (_stream, fd_obj) = stream_with_descriptor(&mut ctx);
    let os = cratonvm_native_api::os_encoding::standard_stream_os_handle(1);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(1));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(os));
    // Unix: -1, the standard-stream arm; Windows: the real handle (or no
    // stream at all under a test harness, 0 / -1, which the guard skips).
    if os != 0 {
        assert_eq!(descriptor_fd_id(&ctx, fd_obj), Some(1));
    }
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(9_501));
    assert_eq!(descriptor_fd_id(&ctx, fd_obj), Some(9_501));
}

/// `fis_set_fd` / `fos_set_fd` write the platform's spelling and the
/// readers find the id again, in both arms of the switch.
#[test]
fn set_then_get_round_trips_for_both_streams() {
    let mut ctx = MockNativeContext::new();
    let (fis, fis_fd) = stream_with_descriptor(&mut ctx);
    fis_set_fd(&mut ctx, fis, 9_201);
    let (fd, handle) = descriptor_fields_for_id(9_201, fd_fields_per_platform_enabled());
    assert_eq!(ctx.get_field_by_name(fis_fd, "fd"), Value::Int(fd));
    assert_eq!(ctx.get_field_by_name(fis_fd, "handle"), Value::Long(handle));
    assert_eq!(fis_get_fd(&ctx, fis), Some(9_201));
    assert_eq!(fd_of_descriptor(&ctx, fis_fd), Some(9_201));
    assert!(!fis_is_closed(&ctx, fis));

    let (fos, fos_fd) = stream_with_descriptor(&mut ctx);
    fos_set_fd(&mut ctx, fos, 9_202);
    assert_eq!(fos_get_fd(&ctx, fos), Some(9_202));
    assert_eq!(descriptor_fd_id(&ctx, fos_fd), Some(9_202));
    assert!(!fos_is_closed(&ctx, fos));
}

/// Each platform shape, written by hand, is read by every migrated reader
/// (a Windows shape on a Unix build and vice versa: a descriptor handed in
/// from either spelling must not be lost).
#[test]
fn both_shapes_are_read_on_any_platform() {
    for (fd, handle) in [(9_301, -1i64), (-1, 9_301i64), (9_301, 9_301i64)] {
        let mut ctx = MockNativeContext::new();
        let (fos, fd_obj) = stream_with_descriptor(&mut ctx);
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(handle));
        assert_eq!(fos_get_fd(&ctx, fos), Some(9_301), "fos ({fd}, {handle})");
        assert_eq!(fis_get_fd(&ctx, fos), Some(9_301), "fis ({fd}, {handle})");
        assert_eq!(
            fd_of_descriptor(&ctx, fd_obj),
            Some(9_301),
            "isRegularFile0 ({fd}, {handle})"
        );
    }
}

/// The one-reader rule: a descriptor minted without `<init>` (`fd` and
/// `handle` both 0) is not stdin for `fos_get_fd` / `fd_of_descriptor`, while
/// a Unix standard-stream shape (`fd = 1, handle = -1`) still is fd 1.
///
/// Not a HotSpot answer: Java code cannot build `fd = 0, handle = 0`
/// (`new FileDescriptor()` is `-1 / -1`), only `Unsafe.allocateInstance` or
/// a VM allocation that skipped `<init>` can, and on Unix HotSpot would use
/// OS fd 0 for it. Here `fd` is a table id, and the guard (nio20 wave 2,
/// L4) keeps a VM-minted, unpopulated descriptor off stdin.
#[test]
fn an_unconstructed_descriptor_is_not_a_standard_stream() {
    let mut ctx = MockNativeContext::new();
    let (fos, fd_obj) = stream_with_descriptor(&mut ctx);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(0));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(0));
    assert_eq!(fd_of_descriptor(&ctx, fd_obj), None);
    assert_eq!(fos_get_fd(&ctx, fos), None);

    ctx.set_field_by_name(fd_obj, "fd", Value::Int(1));
    ctx.set_field_by_name(fd_obj, "handle", Value::Long(-1));
    assert_eq!(fos_get_fd(&ctx, fos), Some(1));
    assert_eq!(fd_of_descriptor(&ctx, fd_obj), Some(1));
}

/// `FileDescriptor.close0` releases the id from either spelling and leaves
/// both fields -1.
#[test]
fn close0_reads_either_spelling() {
    for (fd, handle) in [(9_401, -1i64), (-1, 9_401i64)] {
        let mut ctx = MockNativeContext::new();
        let (_stream, fd_obj) = stream_with_descriptor(&mut ctx);
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(handle));
        let _ = native_fd_close0(&mut ctx, &[Value::Object(Some(fd_obj))]);
        assert_eq!(ctx.get_field_by_name(fd_obj, "fd"), Value::Int(-1));
        assert_eq!(ctx.get_field_by_name(fd_obj, "handle"), Value::Long(-1));
    }
}
