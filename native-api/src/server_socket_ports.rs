//! Cross-crate bound-port registry for synthetic `java.net.ServerSocket`.
//!
//! The synthetic `ServerSocket` surface is split across sibling crates that
//! cannot call each other: the *binding* path lives in
//! `cratonvm-native-builtins` (`net_phase_e::re2_bind_listener`, which records
//! the listener + actual ephemeral port in a private side-table), while the
//! last-registered — and therefore winning — `getLocalPort()` native lives in
//! `cratonvm-native-io` (`socket_channel::ss_wrapper_local_port`, written for
//! the `ServerSocketChannel.socket()` adapter). For a *plain* `new
//! ServerSocket(0)` that channel native finds no back-ref and used to return
//! `0`, so `getLocalPort()` reported 0 even though a real ephemeral port was
//! bound — breaking every caller that advertises its port and is then connected
//! to (e.g. Narayana's `TransactionStatusManager` recovery listener → the
//! Hibernate JTA cluster hang).
//!
//! Object fields can't carry the port across: the real `ServerSocket` layout's
//! low field slots are reference-typed, so an `int` written via `set_field` does
//! not round-trip. Both crates DO depend on `cratonvm-native-api` and both can
//! compute a GC-stable `identity_hash_code`, so this tiny identity-keyed table
//! is the shared channel: the binder records the port here, the `getLocalPort`
//! winner reads it back. (Same identity-hash keying as native-io's
//! `ss_back_ref_table`; identity hashes are GC-stable, so no post-GC remap is
//! needed — worst case is a missed lookup, never a wild value.)

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn table() -> &'static Mutex<HashMap<i32, i32>> {
    static T: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the actual bound (ephemeral-resolved) local port of a plain synthetic
/// `ServerSocket`, keyed by its identity hash. Called from the binding native.
pub fn record(identity_hash: i32, port: i32) {
    if let Ok(mut t) = table().lock() {
        t.insert(identity_hash, port);
    }
}

/// Look up the bound local port previously recorded for this `ServerSocket`'s
/// identity hash, if any. Called from the winning `getLocalPort` native when it
/// has no channel back-ref (i.e. a plain `ServerSocket`).
pub fn get(identity_hash: i32) -> Option<i32> {
    table().lock().ok().and_then(|t| t.get(&identity_hash).copied())
}

/// Drop the recorded port for a closed `ServerSocket` (best-effort).
pub fn remove(identity_hash: i32) {
    if let Ok(mut t) = table().lock() {
        t.remove(&identity_hash);
    }
}
