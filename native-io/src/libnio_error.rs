// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! libnio's / libnet's failure-exception rules, in one place (round nio20
//! wave 2, lane L4; proposal NIO20-L5-4).
//!
//! `native-io` used to build NIO failure exceptions in four styles
//! (`io_error(format!("<op>: {e}"))` carrying std's ` (os error N)` suffix,
//! `ioex`, `throw_named` + `errno_text`, `net_err`'s `"SocketException: .."`
//! message prefix on a plain `IOException`). JDK 25's native libraries have a
//! handful of rules, transcribed here:
//!
//! * `JNU_ThrowIOExceptionWithLastError(env, defaultDetail)`: an `IOException`
//!   whose message is the bare `strerror(errno)` text, or `defaultDetail` when
//!   there is no errno ([`io_exception_with_last_error`]).
//! * `JNU_ThrowIOExceptionWithMessageAndLastError(env, message)`: the message
//!   is `"<strerror> (<message>)"` ([`io_exception_with_message_and_last_error`]).
//! * `IOUtil.c`'s `convertReturnVal` / `convertLongReturnVal`: `EAGAIN` answers
//!   `IOStatus.UNAVAILABLE`, `EINTR` answers `IOStatus.INTERRUPTED`, anything
//!   else throws with default detail `"Read failed"` / `"Write failed"`
//!   ([`convert_return_err`]).
//! * `UnixFileDispatcherImpl.map0`: `ENOMEM` is `OutOfMemoryError("Map
//!   failed")` -- `FileChannelImpl.mapInternal` catches exactly that, runs
//!   `System.gc()`, sleeps and retries once -- and any other errno an
//!   `IOException(strerror)` ([`map_failed`]).
//! * `Net.c`'s `handleSocketError`: the exception CLASS follows the errno
//!   (`ConnectException`, `BindException`, `NoRouteToHostException`,
//!   `ProtocolException`, else `SocketException`), the message is the
//!   `strerror` text ([`handle_socket_error`]).
//! * `SocketDispatcher.c`'s `read0`: `ECONNRESET` / `EPIPE` throw
//!   `sun.net.ConnectionResetException("Connection reset")`, which
//!   `NioSocketImpl` / `SocketChannelImpl` turn into
//!   `SocketException("Connection reset")` ([`socket_dispatcher_error`]).
//!
//! The `strerror` text is std's `Display` of the OS error with its
//! ` (os error N)` suffix removed; std renders it through `strerror_r`, the
//! same table libnio's `getLastErrorString` reads.

use std::io;

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::{MethodCallFailed, RuntimeError};
use cratonvm_types::Value;

/// `IOStatus.EOF`.
pub const IOS_EOF: i32 = -1;
/// `IOStatus.UNAVAILABLE`.
pub const IOS_UNAVAILABLE: i32 = -2;
/// `IOStatus.INTERRUPTED`.
pub const IOS_INTERRUPTED: i32 = -3;

/// What libnio says for an operation on a descriptor that is not open
/// (`EBADF`). Spelled out rather than read from the host's error table so the
/// text is glibc's on every build host (Windows' CRT has no `strerror_r` that
/// std consults for a raw errno).
pub const EBADF_TEXT: &str = "Bad file descriptor";

/// Drop std's ` (os error N)` suffix from an OS error's `Display`.
pub fn strip_os_error_suffix(text: String) -> String {
    match text.rfind(" (os error ") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

/// `strerror(errno)` as libnio prints it.
pub fn strerror(errno: i32) -> String {
    let text = strip_os_error_suffix(io::Error::from_raw_os_error(errno).to_string());
    #[cfg(windows)]
    {
        // `WSAECONNRESET`: HotSpot's Windows `write0` failure reads "Connection reset by peer" (measured,
        // `Nio20Conf nioerr-reset-write`), not the localised FormatMessage text.
        if errno == 10054 {
            return "Connection reset by peer".to_string();
        }
        // jni_util's `getLastErrorString` drops FormatMessage's trailing period (`Nio20Conf trunc`).
        return text
            .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
            .to_string();
    }
    #[cfg(not(windows))]
    text
}

/// The message `JNU_ThrowIOExceptionWithLastError(env, default_detail)` builds
/// for `err`: the `strerror` text when `err` carries an errno, else
/// `default_detail`.
pub fn last_error_message(err: &io::Error, default_detail: &str) -> String {
    match err.raw_os_error() {
        Some(errno) if errno != 0 => strerror(errno),
        _ => default_detail.to_string(),
    }
}

/// `JNU_ThrowIOExceptionWithLastError(env, default_detail)` for `err`.
pub fn io_exception_with_last_error(err: &io::Error, default_detail: &str) -> MethodCallFailed {
    RuntimeError::IOException {
        message: last_error_message(err, default_detail),
    }
    .into()
}

/// `JNU_ThrowIOExceptionWithLastError(env, default_detail)` for a raw errno.
pub fn io_exception_errno(errno: i32, default_detail: &str) -> MethodCallFailed {
    let message = if errno != 0 {
        strerror(errno)
    } else {
        default_detail.to_string()
    };
    RuntimeError::IOException { message }.into()
}

/// The message `JNU_ThrowIOExceptionWithMessageAndLastError(env, message)`
/// builds: `"<strerror> (<message>)"`, or `message` alone without an errno.
pub fn message_and_last_error(err: &io::Error, message: &str) -> String {
    match err.raw_os_error() {
        Some(errno) if errno != 0 => format!("{} ({message})", strerror(errno)),
        _ => message.to_string(),
    }
}

/// `JNU_ThrowIOExceptionWithMessageAndLastError(env, message)` for `err`.
pub fn io_exception_with_message_and_last_error(
    err: &io::Error,
    message: &str,
) -> MethodCallFailed {
    RuntimeError::IOException {
        message: message_and_last_error(err, message),
    }
    .into()
}

/// The `IOException` libnio throws for an operation on a descriptor that is
/// not open: the call reaches the kernel with `fd == -1` and fails `EBADF`.
pub fn bad_descriptor() -> MethodCallFailed {
    RuntimeError::IOException {
        message: EBADF_TEXT.to_string(),
    }
    .into()
}

/// `convertReturnVal`'s failure arm: `Ok(IOS_UNAVAILABLE)` for `EAGAIN`,
/// `Ok(IOS_INTERRUPTED)` for `EINTR`, else the `IOException` with default
/// detail `"Read failed"` / `"Write failed"`.
pub fn convert_return_err(err: &io::Error, reading: bool) -> Result<i32, MethodCallFailed> {
    match err.kind() {
        io::ErrorKind::WouldBlock => Ok(IOS_UNAVAILABLE),
        io::ErrorKind::Interrupted => Ok(IOS_INTERRUPTED),
        _ => Err(io_exception_with_last_error(
            err,
            if reading {
                "Read failed"
            } else {
                "Write failed"
            },
        )),
    }
}

/// Is `err` the platform's "out of memory" for a failed mapping (`ENOMEM`;
/// `ERROR_NOT_ENOUGH_MEMORY` on Windows, which libnio's Windows `map0` tests)?
pub fn is_map_enomem(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::ENOMEM)
    }
    #[cfg(windows)]
    {
        const ERROR_NOT_ENOUGH_MEMORY: i32 = 8;
        err.raw_os_error() == Some(ERROR_NOT_ENOUGH_MEMORY)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = err;
        false
    }
}

/// `map0`'s failure: `OutOfMemoryError("Map failed")` for `ENOMEM` (so the
/// JDK's GC-and-retry in `FileChannelImpl.mapInternal` runs), else
/// `IOException(strerror)` with default detail `"Map failed"`.
pub fn map_failed(err: &io::Error) -> MethodCallFailed {
    if is_map_enomem(err) {
        return RuntimeError::OutOfMemoryError {
            message: "Map failed".to_string(),
        }
        .into();
    }
    io_exception_with_last_error(err, "Map failed")
}

/// A Java exception `class` built through its `(String)V` constructor (or
/// `()V` for `None`), libnio's `JNU_ThrowByName`. A constructor that threw is
/// propagated; a class that cannot be built at all (synthetic mode) falls back
/// to an `IOException` carrying the same message.
pub fn throw_by_name(
    ctx: &mut dyn NativeContext,
    class: &str,
    message: Option<&str>,
) -> MethodCallFailed {
    let built = match message {
        Some(m) => {
            let jmsg = ctx.create_string_uninterned(m);
            ctx.construct_throwable(class, "(Ljava/lang/String;)V", &[Value::Object(Some(jmsg))])
        }
        None => ctx.construct_throwable(class, "()V", &[]),
    };
    match built {
        Ok(Some(exc)) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown) => thrown,
        Ok(None) => RuntimeError::IOException {
            message: message.unwrap_or("").to_string(),
        }
        .into(),
    }
}

/// `JNU_ThrowByNameWithLastError(env, "java/net/SocketException",
/// default_detail)`: a `SocketException` whose message is the `strerror` text,
/// or `default_detail` when `err` carries no errno. This is what `Net.c`'s
/// `setIntOption0` / `getIntOption0` throw (default detail
/// `"sun.nio.ch.Net.setIntOption"` / `"sun.nio.ch.Net.getIntOption"`) -- the
/// class is always `SocketException`, not `handleSocketError`'s table (round
/// nio20 wave 3, lane L5).
pub fn socket_exception_with_last_error(
    ctx: &mut dyn NativeContext,
    err: &io::Error,
    default_detail: &str,
) -> MethodCallFailed {
    let message = last_error_message(err, default_detail);
    throw_by_name(ctx, "java/net/SocketException", Some(&message))
}

/// Is `err` "protocol not available" (`ENOPROTOOPT`; `WSAENOPROTOOPT` on
/// Windows)?
pub fn is_no_protocol_option(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::ENOPROTOOPT)
    }
    #[cfg(windows)]
    {
        const WSAENOPROTOOPT: i32 = 10042;
        err.raw_os_error() == Some(WSAENOPROTOOPT)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = err;
        false
    }
}

/// `jdk.net`'s `{Linux,Windows}SocketOptions.c` `handleError(env, rv, errmsg)`
/// for a failed extended-option `getsockopt` / `setsockopt`: `ENOPROTOOPT` is
/// `UnsupportedOperationException("unsupported socket option")`, anything
/// else `SocketException(strerror)` with default detail `errmsg` (`"set option
/// TCP_KEEPIDLE failed"`, ...). Round nio20 wave 3, lane L5.
pub fn extended_socket_option_error(
    ctx: &mut dyn NativeContext,
    err: &io::Error,
    errmsg: &str,
) -> MethodCallFailed {
    if is_no_protocol_option(err) {
        return RuntimeError::UnsupportedOperationException {
            message: "unsupported socket option".to_string(),
        }
        .into();
    }
    socket_exception_with_last_error(ctx, err, errmsg)
}

/// The exception class libnio's `handleSocketError` picks for `errno`.
#[cfg(unix)]
pub fn socket_error_class(errno: i32) -> &'static str {
    match errno {
        libc::EPROTO => "java/net/ProtocolException",
        libc::ECONNREFUSED | libc::ETIMEDOUT | libc::ENOTCONN => "java/net/ConnectException",
        libc::EHOSTUNREACH => "java/net/NoRouteToHostException",
        libc::EADDRINUSE | libc::EADDRNOTAVAIL | libc::EACCES => "java/net/BindException",
        _ => "java/net/SocketException",
    }
}

/// The exception class `handleSocketError` would pick for an error that
/// carries no usable errno, judged by its kind (Windows, or an error
/// synthesised inside the VM).
pub fn socket_error_class_of_kind(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::ConnectionRefused
        | io::ErrorKind::NotConnected
        | io::ErrorKind::TimedOut => "java/net/ConnectException",
        io::ErrorKind::HostUnreachable => "java/net/NoRouteToHostException",
        io::ErrorKind::AddrInUse
        | io::ErrorKind::AddrNotAvailable
        | io::ErrorKind::PermissionDenied => "java/net/BindException",
        _ => "java/net/SocketException",
    }
}

/// libnio's `handleSocketError(env, errno)` for a raw errno: the class from
/// [`socket_error_class`], the message the `strerror` text.
#[cfg(unix)]
pub fn handle_socket_error(ctx: &mut dyn NativeContext, errno: i32) -> MethodCallFailed {
    let text = strerror(errno);
    throw_by_name(ctx, socket_error_class(errno), Some(&text))
}

/// [`handle_socket_error`] for an `io::Error`. An error with an errno follows
/// libnio's table (Unix); one without (synthesised in the VM: a socket closed
/// under a parked accept, ...) is classed by kind and keeps its own text.
pub fn handle_socket_io_error(ctx: &mut dyn NativeContext, err: &io::Error) -> MethodCallFailed {
    #[cfg(unix)]
    if let Some(errno) = err.raw_os_error() {
        if errno != 0 {
            return handle_socket_error(ctx, errno);
        }
    }
    let class = socket_error_class_of_kind(err.kind());
    let text = match err.raw_os_error() {
        Some(errno) if errno != 0 => strerror(errno),
        _ => err.to_string(),
    };
    throw_by_name(ctx, class, Some(&text))
}

/// `SocketDispatcher.read0` / `write0`'s failure: reading, `ECONNRESET` /
/// `EPIPE` is `sun.net.ConnectionResetException("Connection reset")`, which
/// `NioSocketImpl` and `SocketChannelImpl` rethrow as
/// `SocketException("Connection reset")`; anything else is `convertReturnVal`'s
/// `IOException(strerror)`.
///
/// Callers handle `EAGAIN` / `EINTR` (the `IOStatus` answers) before this.
pub fn socket_dispatcher_error(
    ctx: &mut dyn NativeContext,
    err: &io::Error,
    reading: bool,
) -> MethodCallFailed {
    // Only `read0` / `readv0` special-case the reset; `write0` is plain
    // `convertReturnVal`, so a write to a reset peer is
    // `IOException("Connection reset by peer")` / `IOException("Broken pipe")`
    // (which `NioSocketImpl` rewraps as a `SocketException` of that text).
    let reset = reading
        && matches!(
            err.kind(),
            io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
        );
    if reset {
        return throw_by_name(
            ctx,
            "sun/net/ConnectionResetException",
            Some("Connection reset"),
        );
    }
    io_exception_with_last_error(
        err,
        if reading {
            "Read failed"
        } else {
            "Write failed"
        },
    )
}

#[cfg(test)]
mod tests {
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
}
