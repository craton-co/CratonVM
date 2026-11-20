use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Each getter returns ONE stream object per pipe, as the JDK's
/// `ProcessImpl` does: closing it closes the pipe for every holder.
#[test]
fn each_getter_returns_one_stream_per_pipe() {
    let mut ctx = MockNativeContext::new();
    let process = ctx.alloc_object_with_class(PROC_FIELD_COUNT, SYNTHETIC_PROCESS_CLASS);
    for slot in [PROC_FIELD_STDIN_FD, PROC_FIELD_STDOUT_FD, PROC_FIELD_STDERR_FD] {
        ctx.set_field(process, slot, Value::Int(-1));
    }
    let args = [Value::Object(Some(process))];
    let a = native_process_get_input_stream(&mut ctx, &args).expect("stream");
    let b = native_process_get_input_stream(&mut ctx, &args).expect("stream");
    assert!(matches!(a, Some(Value::Object(Some(_)))), "{a:?}");
    assert_eq!(a, b, "getInputStream() == getInputStream()");
    let e = native_process_get_error_stream(&mut ctx, &args).expect("stream");
    let o = native_process_get_output_stream(&mut ctx, &args).expect("stream");
    assert_ne!(a, e, "stdout and stderr are different pipes");
    assert_ne!(a, o);
    assert_eq!(e, native_process_get_error_stream(&mut ctx, &args).expect("stream"));
    assert_eq!(o, native_process_get_output_stream(&mut ctx, &args).expect("stream"));
}

/// The row fallback hands out an id only while the table still holds the
/// entry spawned under it.
#[test]
fn the_row_fallback_refuses_a_closed_or_reused_id() {
    let ctx = MockNativeContext::new();
    let path = std::env::temp_dir().join(format!(
        "cratonvm_r23w4_pipe_{}.txt",
        std::process::id()
    ));
    std::fs::write(&path, b"x").unwrap();
    let fd = ctx.fd_table().open_read(&path.to_string_lossy()).unwrap();
    let gen = ctx.fd_table().generation(fd).unwrap();
    let row = PipeFds {
        stdin_fd: -1,
        stdout_fd: fd as i32,
        stderr_fd: -1,
        gens: [0, gen, 0],
    };
    assert_eq!(row.live_fd(&ctx, row.stdout_fd, 1), fd as i32);
    assert_eq!(row.live_fd(&ctx, row.stdin_fd, 0), -1, "no pipe");
    let stale = PipeFds {
        gens: [0, gen.wrapping_add(1), 0],
        ..row
    };
    assert_eq!(stale.live_fd(&ctx, stale.stdout_fd, 1), -1, "another owner's generation");
    ctx.fd_table().close(fd).unwrap();
    assert_eq!(row.live_fd(&ctx, row.stdout_fd, 1), -1, "closed");
    let _ = std::fs::remove_file(&path);
}
