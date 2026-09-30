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
//! `ss_back_ref_table`; identity hashes are GC-stable, but each row also
//! compares its receiver `ObjectRef`, which [`gc_update_after_gc`] remaps.)
//!
//! ## The rows are WEAK on their `ServerSocket` (gc-common w18-f)
//!
//! Until w18-f [`gc_scan_roots`] pushed every recorded receiver as a strong
//! root, so a bound `ServerSocket` dropped without `close()` could never be
//! collected: its listener and its port stayed bound until VM exit, where
//! HotSpot's cleaner closes the descriptor of a collected socket
//! (`common-w17a-dropped-open-server-socket-keeps-its-listener-and-row`).
//! [`gc_scan_roots`] now roots nothing, and [`gc_sweep_rows`] (called from
//! `native-builtins::net_phase_e::gc_sweep_net_socket_rows`, which runs at all
//! three weak-row sweep sites) drops the row of a receiver the collection did
//! not keep. The listener itself is closed by that caller, from its own
//! `ServerSocket` side table.

use cratonvm_types::ObjectRef;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
struct BoundServerSocket {
    /// The `vm_identity` whose heap `object` lives in: only that VM's
    /// collection roots and remaps the row (gc-common w9-b).
    vm: usize,
    object: ObjectRef,
    host: String,
    port: i32,
}

fn table() -> &'static Mutex<HashMap<i32, Vec<BoundServerSocket>>> {
    static T: OnceLock<Mutex<HashMap<i32, Vec<BoundServerSocket>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bumped under the table lock by every write ([`record_addr`], [`remove`],
/// [`gc_update_after_gc`]). [`gc_sweep_rows`] judges with the lock released, so
/// out of a pause a mutator may file a row for a NEW object at a just-freed
/// address meanwhile; a moved epoch defers the drop to the next sweep instead
/// of dropping that row (the w17-a `DS_WRITE_EPOCH` rule). Mutators are
/// stopped at the pause site, where the epoch never moves. Not compatibility
/// state: a change counter for a table that is already process-wide.
static WRITE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Record a plain ServerSocket's port. The identity hash selects only a bucket:
/// the receiver ObjectRef prevents a collision from redirecting a later
/// getLocalPort/bind/close operation to a different listener.
pub fn record(vm: usize, identity_hash: i32, object: ObjectRef, port: i32) {
    record_addr(vm, identity_hash, object, "0.0.0.0", port);
}

/// [`record`] with the bound host. `vm` is the binding VM's `vm_identity`.
///
/// `object` must be CURRENT (the receiver at the native's entry, or re-read
/// through a pin after the last allocation): the row is weak, and
/// [`gc_sweep_rows`] judges the address recorded here.
pub fn record_addr(vm: usize, identity_hash: i32, object: ObjectRef, host: &str, port: i32) {
    if let Ok(mut t) = table().lock() {
        WRITE_EPOCH.fetch_add(1, Ordering::Relaxed);
        let bucket = t.entry(identity_hash).or_default();
        if let Some(row) = bucket.iter_mut().find(|row| row.object == object) {
            row.vm = vm;
            row.host = host.to_string();
            row.port = port;
        } else {
            bucket.push(BoundServerSocket {
                vm,
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
        WRITE_EPOCH.fetch_add(1, Ordering::Relaxed);
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

/// Roots NOTHING since gc-common w18-f: a row no longer keeps its
/// `ServerSocket` alive (see the module doc and [`gc_sweep_rows`]). It stays,
/// empty, so the `server-ports` row in `vm/src/memory/native_roots.rs`
/// keeps its remap half ([`gc_update_after_gc`]) without a `vm/` change, the
/// shape w17-a gave `net_phase_e::gc_scan_inet_addr_roots`.
pub fn gc_scan_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    let _ = (vm, out);
}

/// The weak-row sweep of this table (gc-common w18-f): drop VM `vm`'s rows whose
/// `ServerSocket` the collection did not keep. Returns how many rows went.
///
/// The `addr_keyed` weak-row contract, as `net_phase_e`'s object-keyed sweep
/// applies it: receivers are PRE-remap here. A receiver in `pointer_map` moved
/// and its row is kept as it is ([`gc_update_after_gc`], run by
/// `update_all_roots` after the sweep, re-addresses it). Any other receiver of
/// `vm` is judged by `is_live` with the table lock RELEASED, and its row is
/// dropped when it is dead, unless the table was written since the receivers
/// were collected ([`WRITE_EPOCH`]; the drop then waits for the next sweep).
///
/// Only the row goes: the listener is closed by the caller
/// (`net_phase_e::gc_sweep_net_socket_rows`), from its own side table.
pub fn gc_sweep_rows(
    vm: usize,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> usize {
    // 1. Collect this VM's unmoved receivers, with the epoch.
    let (cands, epoch) = {
        let Ok(t) = table().lock() else {
            return 0;
        };
        let mut cands: Vec<(i32, usize)> = Vec::new();
        for (&hash, bucket) in t.iter() {
            for row in bucket.iter() {
                let addr = row.object.as_ptr() as usize;
                if row.vm == vm && !pointer_map.contains_key(&addr) {
                    cands.push((hash, addr));
                }
            }
        }
        (cands, WRITE_EPOCH.load(Ordering::Relaxed))
    };
    if cands.is_empty() {
        return 0;
    }
    // 2. Judge, with no lock held.
    let dead: Vec<(i32, usize)> = cands.into_iter().filter(|&(_, a)| !is_live(a)).collect();
    if dead.is_empty() {
        return 0;
    }
    // 3. Drop, only if the table is exactly as collected.
    let Ok(mut t) = table().lock() else {
        return 0;
    };
    if WRITE_EPOCH.load(Ordering::Relaxed) != epoch {
        return 0;
    }
    let mut dropped = 0usize;
    for (hash, addr) in dead {
        let emptied = match t.get_mut(&hash) {
            Some(bucket) => {
                let before = bucket.len();
                bucket.retain(|row| !(row.vm == vm && row.object.as_ptr() as usize == addr));
                dropped += before - bucket.len();
                bucket.is_empty()
            }
            None => false,
        };
        if emptied {
            t.remove(&hash);
        }
    }
    if dropped != 0 {
        WRITE_EPOCH.fetch_add(1, Ordering::Relaxed);
    }
    dropped
}

/// Post-move remap of VM `vm`'s rows (the `server-ports` remap half).
pub fn gc_update_after_gc(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    if let Ok(mut t) = table().lock() {
        WRITE_EPOCH.fetch_add(1, Ordering::Relaxed);
        for bucket in t.values_mut() {
            for row in bucket.iter_mut().filter(|row| row.vm == vm) {
                if let Some(&new_addr) = pointer_map.get(&(row.object.as_ptr() as usize)) {
                    row.object = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
}

/// Drop VM `vm`'s rows (VM teardown, from
/// `native-builtins::forget_vm_native_root_stores`).
pub fn forget_vm(vm: usize) {
    if let Ok(mut t) = table().lock() {
        for bucket in t.values_mut() {
            bucket.retain(|row| row.vm != vm);
        }
        t.retain(|_, bucket| !bucket.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gc-common w9-b (`handoff-w9b-server-ports-per-vm.md`): a row is
    /// remapped and forgotten only by the VM that bound it. Since gc-common
    /// w18-f no row is a root, for either VM.
    #[test]
    fn a_bound_row_belongs_to_its_vm() {
        const VM_A: usize = 0x9B50_0A01;
        const VM_B: usize = 0x9B50_0A02;
        let addr = 0x9B50_1000usize;
        // SAFETY: never dereferenced; only the address is compared.
        let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
        record_addr(VM_A, 0x9B51, obj, "127.0.0.1", 1111);
        record_addr(VM_B, 0x9B52, obj, "127.0.0.1", 2222);
        let scan = |vm: usize| {
            let mut out = Vec::new();
            gc_scan_roots(vm, &mut out);
            out.iter().filter(|o| o.as_ptr() as usize == addr).count()
        };
        assert_eq!((scan(VM_A), scan(VM_B)), (0, 0), "the rows are weak");

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(addr, addr + 0x100);
        gc_update_after_gc(VM_B, &map);
        assert_eq!(get(0x9B51, obj), Some(1111), "B's map must not move A's row");
        assert_eq!(get(0x9B52, obj), None, "B's row moved");

        forget_vm(VM_A);
        assert_eq!(get(0x9B51, obj), None);
        forget_vm(VM_B);
        assert_eq!(get(0x9B52, unsafe { ObjectRef::from_raw((addr + 0x100) as *mut u8) }), None);
    }

    /// Sweep until `want` rows of `vm` have gone. A parallel test's write
    /// between a sweep's collection and its drop defers that drop to the next
    /// sweep (the [`WRITE_EPOCH`] check), so one sweep is not guaranteed to
    /// finish. Bounded.
    fn sweep_expecting(
        vm: usize,
        map: &cratonvm_types::PointerMap,
        is_live: &dyn Fn(usize) -> bool,
        want: usize,
    ) -> usize {
        let mut gone = 0;
        for _ in 0..256 {
            gone += gc_sweep_rows(vm, map, is_live);
            if gone >= want {
                break;
            }
        }
        gone
    }

    /// gc-common w18-f (`common-w17a-dropped-open-server-socket-keeps-its-listener-and-row`):
    /// a dead receiver's row goes, a live one's stays, and another VM's
    /// collection judges none of this VM's rows.
    #[test]
    fn a_dead_receivers_row_goes_and_a_live_ones_stays() {
        const VM: usize = 0x18F0_0A01;
        const OTHER: usize = 0x18F0_0A02;
        let dead_addr = 0x18F0_1000usize;
        let live_addr = 0x18F0_2000usize;
        // SAFETY: never dereferenced; only the addresses are compared.
        let dead = unsafe { ObjectRef::from_raw(dead_addr as *mut u8) };
        let live = unsafe { ObjectRef::from_raw(live_addr as *mut u8) };
        record_addr(VM, 0x18F1, dead, "0.0.0.0", 4001);
        record_addr(VM, 0x18F2, live, "0.0.0.0", 4002);
        let map = cratonvm_types::PointerMap::default();
        let only_live = move |a: usize| a == live_addr;

        // Another VM's collection sees none of these rows.
        assert_eq!(gc_sweep_rows(OTHER, &map, &|_| false), 0);
        assert_eq!(get(0x18F1, dead), Some(4001));

        assert_eq!(sweep_expecting(VM, &map, &only_live, 1), 1);
        assert_eq!(get(0x18F1, dead), None, "the dead receiver's row went");
        assert_eq!(get(0x18F2, live), Some(4002), "the live receiver's row stayed");
        forget_vm(VM);
    }

    /// A receiver the collection MOVED is not judged at its vacated address:
    /// its row is kept for the remap, which then follows it.
    #[test]
    fn a_moved_receiver_is_kept_and_followed() {
        const VM: usize = 0x18F0_0A03;
        let from = 0x18F0_3000usize;
        let to = 0x18F0_4000usize;
        // SAFETY: never dereferenced; only the addresses are compared.
        let obj = unsafe { ObjectRef::from_raw(from as *mut u8) };
        let moved = unsafe { ObjectRef::from_raw(to as *mut u8) };
        record_addr(VM, 0x18F3, obj, "127.0.0.1", 4003);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(from, to);
        // Nothing is live at the OLD address; the row must survive anyway.
        assert_eq!(gc_sweep_rows(VM, &map, &|_| false), 0);
        gc_update_after_gc(VM, &map);
        assert_eq!(get(0x18F3, moved), Some(4003));
        assert_eq!(get(0x18F3, obj), None);
        forget_vm(VM);
    }
}
