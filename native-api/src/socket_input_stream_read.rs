//! Cross-crate hook for socket-backed `InputStream.read*` dispatch.
//!
//! The base `java.io.InputStream.read*` natives live in `cratonvm-native-io`,
//! while the socket stream owner side table and real read implementation live
//! in `cratonvm-native-builtins` (`net_phase_e`). Some real-JDK callsites
//! dispatch through the declared `InputStream` owner even when the receiver is
//! CratonVM's `java.net.Socket$SocketInputStream`, so the base fallback needs a
//! narrow bridge back to the socket reader.

use crate::registry::NativeCallback;
use std::sync::OnceLock;

static READ_ONE: OnceLock<NativeCallback> = OnceLock::new();
static READ_ARRAY: OnceLock<NativeCallback> = OnceLock::new();
static READ_BYTES: OnceLock<NativeCallback> = OnceLock::new();

pub fn set_read_one(cb: NativeCallback) {
    let _ = READ_ONE.set(cb);
}

pub fn set_read_array(cb: NativeCallback) {
    let _ = READ_ARRAY.set(cb);
}

pub fn set_read_bytes(cb: NativeCallback) {
    let _ = READ_BYTES.set(cb);
}

pub fn get_read_one() -> Option<NativeCallback> {
    READ_ONE.get().copied()
}

pub fn get_read_array() -> Option<NativeCallback> {
    READ_ARRAY.get().copied()
}

pub fn get_read_bytes() -> Option<NativeCallback> {
    READ_BYTES.get().copied()
}
