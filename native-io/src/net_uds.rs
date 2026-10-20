// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `sun.nio.ch.UnixDomainSockets` natives, for the real JDK's channel bytecode
//! under `--jdk-only`.
//!
//! `SelectorProvider.openServerSocketChannel(StandardProtocolFamily.UNIX)` --
//! Netty's `NioServerDomainSocketChannel` / `NioDomainSocketChannel`, reached by
//! reflection on the default provider -- runs the real
//! `ServerSocketChannelImpl` / `SocketChannelImpl`, whose `UnixDomainSockets`
//! helper ends in six `ACC_NATIVE` methods that were never registered
//! (`UnsatisfiedLinkError: 'boolean sun.nio.ch.UnixDomainSockets.init()'`).
//!
//! They are served from the same virtual fd table the TCP natives use, so every
//! other native the channel reaches (`Net.listen`, `Net.poll`, the option
//! getters, `SocketDispatcher` reads and writes, `close`) works on a Unix-domain
//! id without change: an AF_UNIX listener is adopted as a `TcpListener` and an
//! accepted or connected AF_UNIX stream as a `TcpStream`, exactly as `uds.rs`
//! documents. Address decoding is the one thing that is not protocol-agnostic,
//! so nothing here asks `std` for a `SocketAddr`.
//!
//! Like the JDK, `bind` leaves the socket file behind on close.
//!
//! A child module of `net` so it shares that file's private registry.

use super::*;

#[cfg(any(unix, windows))]
mod imp {
    use super::super::nio_real::errno_exception;
    use super::*;

    /// The path bytes a `byte[]` argument holds.
    fn path_arg(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        idx: usize,
    ) -> Result<String, MethodCallFailed> {
        let arr = obj_arg(args, idx)?;
        let n = ctx.array_length(arr);
        let mut buf = vec![0u8; n];
        ctx.read_byte_array_into(arr, 0, &mut buf);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn io_errno(e: &std::io::Error) -> i32 {
        #[cfg(unix)]
        let eio = libc::EIO;
        #[cfg(windows)]
        let eio = 5; // ERROR_ACCESS_DENIED stands in for "some I/O error"; only reached for a synthesised error
        e.raw_os_error().unwrap_or(eio)
    }

    /// A new `byte[]` holding `bytes`.
    fn byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        if !bytes.is_empty() {
            ctx.write_byte_array_from(arr, 0, bytes);
        }
        arr
    }

    fn uds_init(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(1)))
    }

    /// `socket0() -> int`: a fresh fd id. The OS socket is made by `bind0` /
    /// `connect0`, as for TCP.
    fn uds_socket0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        let fd = register_handle(NetSocketHandle::Unbound);
        dbgnet!("uds socket0 -> fd={fd:#x}");
        Ok(Some(Value::Int(fd)))
    }

    /// `bind0(FileDescriptor fd, byte[] path) -> void`. Bound, NOT listening:
    /// `ServerSocketChannelImpl.bind` calls `Net.listen` next with the caller's
    /// backlog.
    fn uds_bind0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let fd_obj = obj_arg(args, 0)?;
        let path = path_arg(ctx, args, 1)?;
        let fd = net_fd_from_descriptor(ctx, fd_obj)
            .ok_or_else(|| ioex("bind0: FileDescriptor has no fd id"))?;
        dbgnet!("uds bind0 fd={fd:#x} path={path}");
        let listener = match crate::uds::bind_unlistened(&path) {
            Ok(l) => l,
            Err(e) => return Err(errno_exception(ctx, io_errno(&e))),
        };
        if let Some(nonblocking) = net_pending_nonblocking().read().get(&fd).copied() {
            listener
                .set_nonblocking(nonblocking)
                .map_err(|e| nio_net_err(ctx, "bind0", e))?;
        }
        net_sockets().write().insert(
            fd,
            NetSocketHandle::Listener(Arc::new(Mutex::new(listener))),
        );
        net_apply_recorded_options(fd);
        Ok(None)
    }

    /// `connect0(FileDescriptor fd, byte[] path) -> int`. An AF_UNIX connect
    /// completes (or is refused) synchronously against the local kernel, so
    /// this never answers "in progress".
    fn uds_connect0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let fd_obj = obj_arg(args, 0)?;
        let path = path_arg(ctx, args, 1)?;
        let fd = net_fd_from_descriptor(ctx, fd_obj)
            .ok_or_else(|| ioex("connect0: FileDescriptor has no fd id"))?;
        dbgnet!("uds connect0 fd={fd:#x} path={path}");
        // Interpreter round i3 wave 80, lane L6 (page
        // `i59-L6-blocking-regions-not-yet-classified`): `connect0` is
        // `ACC_NATIVE` in JDK 25 `UnixDomainSockets`: RUNNABLE in native.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let res = crate::uds::connect(&path);
        ctx.end_blocking_region();
        let stream = match res {
            Ok(s) => s,
            Err(e) => return Err(errno_exception(ctx, io_errno(&e))),
        };
        if let Some(nonblocking) = net_pending_nonblocking().read().get(&fd).copied() {
            stream
                .set_nonblocking(nonblocking)
                .map_err(|e| nio_net_err(ctx, "connect0", e))?;
        }
        net_sockets()
            .write()
            .insert(fd, NetSocketHandle::Stream(Arc::new(stream)));
        net_apply_recorded_options(fd);
        Ok(Some(Value::Int(1)))
    }

    /// Accept once, retrying `EINTR`.
    fn accept_once(listener: &TcpListener) -> std::io::Result<(TcpStream, String)> {
        loop {
            match crate::uds::accept_from(listener) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// The close- and interrupt-aware blocking accept (`net_accept_close_aware_intr`
    /// for an AF_UNIX listener).
    fn accept_close_aware(
        listener: &TcpListener,
        fd: i32,
        intr: Option<&dyn Fn() -> bool>,
    ) -> std::io::Result<(TcpStream, String)> {
        listener.set_nonblocking(true)?;
        loop {
            match accept_once(listener) {
                Ok(pair) => return Ok(pair),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if !net_listener_still_registered(fd) {
                        return Err(net_accept_closed_err());
                    }
                    if intr.is_some_and(|p| p()) {
                        return Err(nio_interrupted_err());
                    }
                    if cratonvm_native_api::net_wait::event_waits_enabled() {
                        cratonvm_native_api::net_wait::wait_readable_raw(
                            cratonvm_native_api::net_wait::raw_sock(listener),
                            NET_ACCEPT_CLOSE_POLL.as_millis() as i32,
                        );
                    } else {
                        std::thread::sleep(NET_ACCEPT_CLOSE_POLL);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `accept0(FileDescriptor ssfd, FileDescriptor newfd, Object[] array) -> int`:
    /// 1 accepted (`newfd` filled, `array[0]` the peer's path bytes, empty for an
    /// unnamed client), -2 nothing pending (non-blocking), -3 interrupted.
    fn uds_accept0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let fd_obj = obj_arg(args, 0)?;
        let fd = net_fd_from_descriptor(ctx, fd_obj)
            .ok_or_else(|| ioex("accept: FileDescriptor has no fd id"))?;
        let nonblocking = net_fd_is_nonblocking(fd);
        dbgnet!("uds accept0 fd={fd:#x} nonblocking={nonblocking}");
        let listener_handle = {
            let map = net_sockets().read();
            match map.get(&fd) {
                Some(NetSocketHandle::Listener(l)) => Arc::clone(l),
                Some(_) => return Err(ioex("accept: fd is not a listener")),
                None => return Err(ioex("accept: unknown fd")),
            }
        };
        let mut largs = args.to_vec();
        let interruptible = !nonblocking && nio_blocker_armed(ctx);
        let mut interrupted = false;
        let accepted = {
            let listener = listener_handle.lock();
            if nonblocking {
                listener
                    .set_nonblocking(true)
                    .map_err(|e| nio_net_err(ctx, "accept", e))?;
                match accept_once(&listener) {
                    Ok(pair) => Some(pair),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        if !net_listener_still_registered(fd) {
                            return Err(nio_net_err(ctx, "accept", net_accept_closed_err()));
                        }
                        None
                    }
                    Err(e) => return Err(nio_net_err(ctx, "accept", e)),
                }
            } else {
                // Interpreter round i3 wave 80, lane L6: `accept0` is
                // `ACC_NATIVE` in JDK 25 `UnixDomainSockets`: RUNNABLE in
                // native, as the TCP accept regions already say.
                ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
                let res = if interruptible {
                    let probe = || ctx.is_interrupted(false);
                    accept_close_aware(&listener, fd, Some(&probe))
                } else {
                    accept_close_aware(&listener, fd, None)
                };
                ctx.end_blocking_region_refs(&mut largs);
                match res {
                    Err(e) if is_nio_interrupted(&e) => {
                        interrupted = true;
                        None
                    }
                    other => Some(other.map_err(|e| nio_net_err(ctx, "accept", e))?),
                }
            }
        };
        if interrupted {
            nio_deliver_interrupt(ctx);
            return Ok(Some(Value::Int(-3)));
        }
        let Some((stream, peer)) = accepted else {
            return Ok(Some(Value::Int(-2)));
        };
        let _ = stream.set_nonblocking(false);
        let new_fd = register_handle(NetSocketHandle::Stream(Arc::new(stream)));
        dbgnet!("uds accept0 fd={fd:#x} -> newfd={new_fd:#x} peer={peer}");
        if let Some(Value::Object(Some(newfd_obj))) = largs.get(1).copied() {
            ctx.set_field_by_name(newfd_obj, "fd", Value::Int(new_fd));
        }
        if let Some(Value::Object(Some(arr))) = largs.get(2).copied() {
            if ctx.array_length(arr) >= 1 {
                let arr_pin = ctx.pin_native_root(arr);
                let bytes = byte_array(ctx, peer.as_bytes());
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, 0, Value::Object(Some(bytes)));
                ctx.unpin_native_roots(arr_pin);
            }
        }
        Ok(Some(Value::Int(1)))
    }

    /// `localAddress0(FileDescriptor fd) -> byte[]`: the bound path, empty when
    /// the socket has none.
    fn uds_local_address0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let fd_obj = obj_arg(args, 0)?;
        let fd = net_fd_from_descriptor(ctx, fd_obj)
            .ok_or_else(|| ioex("localAddress0: FileDescriptor has no fd id"))?;
        let path = {
            let map = net_sockets().read();
            match map.get(&fd) {
                Some(NetSocketHandle::Listener(l)) => crate::uds::listener_local_path(&l.lock()),
                Some(NetSocketHandle::Stream(s)) => crate::uds::stream_local_path(s),
                _ => Ok(String::new()),
            }
        };
        let path = match path {
            Ok(p) => p,
            Err(e) => return Err(errno_exception(ctx, io_errno(&e))),
        };
        let arr = byte_array(ctx, path.as_bytes());
        Ok(Some(Value::Object(Some(arr))))
    }

    pub(super) fn register(r: &mut NativeMethodRegistry) {
        use cratonvm_native_api::NativeKind;
        let c = "sun/nio/ch/UnixDomainSockets";
        let b = NativeKind::Bridge;
        r.register_with_kind(c, "init", "()Z", uds_init, b);
        r.register_with_kind(c, "socket0", "()I", uds_socket0, b);
        r.register_with_kind(c, "bind0", "(Ljava/io/FileDescriptor;[B)V", uds_bind0, b);
        r.register_with_kind(
            c,
            "connect0",
            "(Ljava/io/FileDescriptor;[B)I",
            uds_connect0,
            b,
        );
        r.register_with_kind(
            c,
            "accept0",
            "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;[Ljava/lang/Object;)I",
            uds_accept0,
            b,
        );
        r.register_with_kind(
            c,
            "localAddress0",
            "(Ljava/io/FileDescriptor;)[B",
            uds_local_address0,
            b,
        );
    }
}

/// Register the `UnixDomainSockets` natives. Linux/Unix only; elsewhere the
/// class keeps failing as before.
pub(super) fn register(r: &mut NativeMethodRegistry) {
    // Windows 10 1803+ has AF_UNIX, and the JDK's Windows selector wakes itself through a Unix-domain pair
    // (`PipeImpl(sp, true, false)`), so `UnixDomainSockets` must initialise there; the primitives are
    // `uds.rs`'s Winsock half.
    #[cfg(any(unix, windows))]
    imp::register(r);
    #[cfg(not(any(unix, windows)))]
    let _ = r;
}
