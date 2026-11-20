use super::*;
use cratonvm_types::error::VmError;

fn io_message(f: MethodCallFailed) -> String {
    match f {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
            message,
        })) => message,
        other => panic!("expected an IOException, got {other:?}"),
    }
}

#[test]
fn strerror_has_no_os_error_suffix() {
    let t = strerror(2);
    assert!(!t.contains("os error"), "{t}");
    assert!(!t.is_empty());
}

#[test]
fn last_error_falls_back_to_default_detail_without_errno() {
    let e = io::Error::other("synthetic");
    assert_eq!(last_error_message(&e, "Map failed"), "Map failed");
    let e = io::Error::from_raw_os_error(2);
    assert_eq!(last_error_message(&e, "Map failed"), strerror(2));
}

#[test]
fn message_and_last_error_is_strerror_then_parenthesised_message() {
    let e = io::Error::from_raw_os_error(2);
    assert_eq!(
        message_and_last_error(&e, "msync with parameter MS_SYNC failed"),
        format!("{} (msync with parameter MS_SYNC failed)", strerror(2))
    );
    let e = io::Error::other("x");
    assert_eq!(message_and_last_error(&e, "m"), "m");
}

#[test]
fn convert_return_err_maps_eagain_and_eintr_to_iostatus() {
    let e = io::Error::from(io::ErrorKind::WouldBlock);
    assert_eq!(convert_return_err(&e, true).ok(), Some(IOS_UNAVAILABLE));
    let e = io::Error::from(io::ErrorKind::Interrupted);
    assert_eq!(convert_return_err(&e, false).ok(), Some(IOS_INTERRUPTED));
    let e = io::Error::other("x");
    let msg = io_message(convert_return_err(&e, true).unwrap_err());
    assert_eq!(msg, "Read failed");
    let e = io::Error::other("x");
    let msg = io_message(convert_return_err(&e, false).unwrap_err());
    assert_eq!(msg, "Write failed");
}

#[cfg(unix)]
#[test]
fn map_failed_enomem_is_out_of_memory_error_map_failed() {
    let e = io::Error::from_raw_os_error(libc::ENOMEM);
    match map_failed(&e) {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message,
        })) => assert_eq!(message, "Map failed"),
        other => panic!("expected OOME, got {other:?}"),
    }
    let e = io::Error::from_raw_os_error(libc::EACCES);
    assert_eq!(io_message(map_failed(&e)), strerror(libc::EACCES));
    let e = io::Error::other("no errno");
    assert_eq!(io_message(map_failed(&e)), "Map failed");
}

#[cfg(unix)]
#[test]
fn socket_error_class_follows_libnio_handle_socket_error() {
    assert_eq!(
        socket_error_class(libc::ECONNREFUSED),
        "java/net/ConnectException"
    );
    assert_eq!(
        socket_error_class(libc::ETIMEDOUT),
        "java/net/ConnectException"
    );
    assert_eq!(
        socket_error_class(libc::ENOTCONN),
        "java/net/ConnectException"
    );
    assert_eq!(
        socket_error_class(libc::EHOSTUNREACH),
        "java/net/NoRouteToHostException"
    );
    assert_eq!(
        socket_error_class(libc::EADDRINUSE),
        "java/net/BindException"
    );
    assert_eq!(socket_error_class(libc::EACCES), "java/net/BindException");
    assert_eq!(
        socket_error_class(libc::EPROTO),
        "java/net/ProtocolException"
    );
    assert_eq!(
        socket_error_class(libc::ECONNRESET),
        "java/net/SocketException"
    );
}

#[test]
fn bad_descriptor_text_is_ebadf() {
    assert_eq!(io_message(bad_descriptor()), "Bad file descriptor");
}
