//! Cross-crate hook for plain `java.net.ServerSocket.close()`.
//!
//! Sibling-crate split, mirroring [`crate::plain_server_socket_bind`]: the
//! *winning* `close` native lives in `cratonvm-native-io`
//! (`socket_channel::ss_wrapper_close`), written for the
//! `ServerSocketChannel.socket()` adapter — for a *plain* `new ServerSocket()`
//! it has no channel back-ref and returned `Ok(None)` (a no-op). That left the
//! `TcpListener` registered in native-builtins' `s2` listener registry, so a
//! thread blocked in `ServerSocket.accept()` (which polls that registry) was
//! never interrupted: okhttp's `MockWebServer.close()` then waited 5 s for its
//! accept TaskRunner queue to drain and threw
//! `AssertionError: Gave up waiting for queue to shut down`.
//!
//! The real close logic (drop the listener from the `s2` registry so `accept()`
//! observes the close + clear the SO_TIMEOUT) lives in
//! `cratonvm-native-builtins` (`net_phase_e::re2_server_socket_close`), which
//! native-io cannot call directly (no dependency edge). native-builtins installs
//! its plain-close handler here at registration time; native-io's winning
//! `close` native invokes it for the no-back-ref (plain) case. Both crates
//! depend only on `cratonvm-native-api`, so this `NativeCallback` slot is the
//! bridge.

use crate::registry::NativeCallback;
use std::sync::OnceLock;

static HOOK: OnceLock<NativeCallback> = OnceLock::new();

/// Install the plain-`ServerSocket` close handler. Called once from
/// `net_phase_e::register_re2_server_socket` at native registration time.
pub fn set(cb: NativeCallback) {
    let _ = HOOK.set(cb);
}

/// Fetch the installed plain-close handler, if any. Called from native-io's
/// winning `close` native when the receiver has no channel back-ref.
pub fn get() -> Option<NativeCallback> {
    HOOK.get().copied()
}
