//! Cross-crate hook for plain `java.net.ServerSocket.bind(SocketAddress[, int])`.
//!
//! Same sibling-crate split as [`crate::server_socket_ports`]: the *winning*
//! `bind` native lives in `cratonvm-native-io`
//! (`socket_channel::ss_wrapper_bind`), written for the
//! `ServerSocketChannel.socket()` adapter — for a *plain* `new ServerSocket()`
//! it has no channel back-ref and historically returned `Ok(None)` (a no-op),
//! so `new ServerSocket().bind(addr)` never bound a listener and
//! `getLocalPort()` stayed 0 (okhttp `MockWebServer.getPort()` = 0 →
//! `http://localhost:0` → every Spring HTTP client-factory test failed to
//! connect; BUG-04).
//!
//! The real binding logic (a `TcpListener` + the `s2` listener registry that
//! `accept()` reads, + recording the port via [`crate::server_socket_ports`])
//! lives in `cratonvm-native-builtins` (`net_phase_e::re2_bind_listener`), which
//! native-io cannot call directly (no dependency edge). native-builtins installs
//! its plain-bind handler here at registration time; native-io's winning `bind`
//! native invokes it for the no-back-ref (plain) case. Both crates depend only
//! on `cratonvm-native-api`, so this `NativeCallback` slot is the bridge.

use crate::registry::NativeCallback;
use std::sync::OnceLock;

static HOOK: OnceLock<NativeCallback> = OnceLock::new();

/// Install the plain-`ServerSocket` bind handler. Called once from
/// `net_phase_e::register_re2_server_socket` at native registration time.
pub fn set(cb: NativeCallback) {
    let _ = HOOK.set(cb);
}

/// Fetch the installed plain-bind handler, if any. Called from native-io's
/// winning `bind` native when the receiver has no channel back-ref.
pub fn get() -> Option<NativeCallback> {
    HOOK.get().copied()
}
