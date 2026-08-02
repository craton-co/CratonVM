// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cross-crate handler set for the **plain** `java.net.ServerSocket` surface —
//! `new ServerSocket()` / `new ServerSocket(port)`, as opposed to the adapter
//! `ServerSocketChannel.socket()` hands back.
//!
//! ## Why this bridge exists
//!
//! Two sibling crates register natives on `java/net/ServerSocket`, and
//! registration is last-writer-wins per `(class, method, descriptor)`:
//!
//! * `cratonvm-native-builtins` (`net_phase_e::register_re2_server_socket`) owns
//!   the constructors, `accept()` and all the socket options, backed by the
//!   `s2` listener registry and its own per-object side table. It holds *all*
//!   the plain-`ServerSocket` state.
//! * `cratonvm-native-io` (`socket_channel::register_socket_channel_real`)
//!   registers `bind` / `getLocalPort` / `getLocalSocketAddress` / `isBound` /
//!   `isClosed` / `close` **later**, so its `ss_wrapper_*` handlers win for
//!   *every* `ServerSocket`. They are written for the channel adapter (which
//!   carries a back-ref to its `ServerSocketChannel`); a plain `ServerSocket`
//!   has no back-ref.
//!
//! The two crates do not depend on each other — they share only this crate — so
//! the winning native cannot call the owning one. Every `ss_wrapper_*` handler
//! therefore delegates its no-back-ref (plain) case through the callbacks
//! installed here, instead of guessing at the answer from a side table it can
//! only partially see.
//!
//! Handling a subset was the older shape and it did not hold: `bind`/`close`
//! delegated while `getLocalPort`/`isBound`/`isClosed`/`getLocalSocketAddress`
//! answered out of [`crate::server_socket_ports`], which records only a bound
//! socket's address. That made `isClosed()` permanently `false`, `isBound()`
//! flip back to `false` after `close()`, and `getLocalPort()` answer `0` (not
//! `-1`) on an unbound socket — all divergences from HotSpot, whose `ServerSocket`
//! keeps `bound` set and keeps reporting the port after close.
//!
//! Historically the split cost more than cosmetics: before the delegation
//! existed at all, `bind` was a silent no-op, so `new ServerSocket().bind(addr)`
//! never bound a listener and `getLocalPort()` stayed `0` — okhttp's
//! `MockWebServer` then advertised `http://localhost:0` and every Spring
//! HTTP-client test failed to connect.

use crate::registry::NativeCallback;
use std::sync::OnceLock;

/// The plain-`ServerSocket` handlers `cratonvm-native-builtins` installs and
/// `cratonvm-native-io`'s winning natives delegate to. One callback per
/// shadowed `(method, descriptor)` group; each has the ordinary
/// `NativeCallback` signature and receives the original `args` unchanged
/// (`args[0]` is the `ServerSocket` receiver).
#[derive(Clone, Copy)]
pub struct PlainServerSocketOps {
    /// `bind(SocketAddress)` and `bind(SocketAddress, int)` — the arity is read
    /// from `args`.
    pub bind: NativeCallback,
    /// `close()`.
    pub close: NativeCallback,
    /// `getLocalPort()` — `-1` when the socket was never bound.
    pub local_port: NativeCallback,
    /// `getLocalSocketAddress()` — `null` when the socket was never bound.
    pub local_socket_address: NativeCallback,
    /// `isBound()` — stays true after `close()`, as on HotSpot.
    pub is_bound: NativeCallback,
    /// `isClosed()`.
    pub is_closed: NativeCallback,
}

static OPS: OnceLock<PlainServerSocketOps> = OnceLock::new();

/// Install the plain-`ServerSocket` handlers. Called once from
/// `net_phase_e::register_re2_server_socket` at native registration time.
pub fn set(ops: PlainServerSocketOps) {
    let _ = OPS.set(ops);
}

/// Fetch the installed handlers, if any. `None` under
/// `CRATONVM_REAL=net-sockets` (the default), where no synthetic
/// `java/net/ServerSocket` native is registered at all and real JDK bytecode
/// drives `sun/nio/ch/Net` — so nothing consults this bridge.
pub fn get() -> Option<PlainServerSocketOps> {
    OPS.get().copied()
}
