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

use cratonvm_types::ObjectRef;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
struct BoundServerSocket {
    object: ObjectRef,
    host: String,
    port: i32,
}

fn table() -> &'static Mutex<HashMap<i32, Vec<BoundServerSocket>>> {
    static T: OnceLock<Mutex<HashMap<i32, Vec<BoundServerSocket>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a plain ServerSocket's port. The identity hash selects only a bucket:
/// the receiver ObjectRef prevents a collision from redirecting a later
/// getLocalPort/bind/close operation to a different listener.
pub fn record(identity_hash: i32, object: ObjectRef, port: i32) {
    record_addr(identity_hash, object, "0.0.0.0", port);
}

pub fn record_addr(identity_hash: i32, object: ObjectRef, host: &str, port: i32) {
    if let Ok(mut t) = table().lock() {
        let bucket = t.entry(identity_hash).or_default();
        if let Some(row) = bucket.iter_mut().find(|row| row.object == object) {
            row.host = host.to_string();
            row.port = port;
        } else {
            bucket.push(BoundServerSocket {
                object,
                host: host.to_string(),
                port,
            });
        }
    }
}

pub fn get(identity_hash: i32, object: ObjectRef) -> Option<i32> {
    table().lock().ok().and_then(|t| {
        t.get(&identity_hash)
            .and_then(|bucket| bucket.iter().find(|row| row.object == object))
            .map(|bound| bound.port)
    })
}

pub fn get_addr(identity_hash: i32, object: ObjectRef) -> Option<(String, i32)> {
    table().lock().ok().and_then(|t| {
        t.get(&identity_hash)
            .and_then(|bucket| bucket.iter().find(|row| row.object == object))
            .map(|bound| (bound.host.clone(), bound.port))
    })
}

pub fn remove(identity_hash: i32, object: ObjectRef) {
    if let Ok(mut t) = table().lock() {
        let remove_bucket = if let Some(bucket) = t.get_mut(&identity_hash) {
            bucket.retain(|row| row.object != object);
            bucket.is_empty()
        } else {
            false
        };
        if remove_bucket {
            t.remove(&identity_hash);
        }
    }
}

/// The registry is a live native edge while a ServerSocket is bound; retain and
/// remap its receivers across moving GC, exactly like the consumer side tables.
pub fn gc_scan_roots(out: &mut Vec<ObjectRef>) {
    if let Ok(t) = table().lock() {
        for bucket in t.values() {
            out.extend(bucket.iter().map(|row| row.object));
        }
    }
}

pub fn gc_update_after_gc(pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    if let Ok(mut t) = table().lock() {
        for bucket in t.values_mut() {
            for row in bucket {
                if let Some(&new_addr) = pointer_map.get(&(row.object.as_ptr() as usize)) {
                    row.object = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
}
