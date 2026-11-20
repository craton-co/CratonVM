// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 50-54 native method registrations.

use crate::util_concurrent_ext::{
    atomic_array_cas, atomic_array_index, atomic_array_new_length, atomic_array_raw_index,
    atomic_array_rmw,
};
use cratonvm_native_api::{NativeContext, NativeHandle, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};
#[cfg(feature = "legacy-synthetic-crypto")]
use crate::crypto_impl;
use crate::lang_class::{native_class_is_record, native_class_is_sealed};
use crate::lang_invoke::register_phase54_method_handle;
use crate::lang_misc::register_phase53_record;
use crate::lang_string::{
    native_string_hash_code, native_string_latin1_to_lower_case, native_string_to_lower_case,
    register_phase52_string_buffer,
};
use crate::{
    build_real_layout_string_hashset, native_noop, native_return_false,
    native_unsafe_ensure_class_initialized, obj_arg, try_alloc_concurrent_synthetic,
};
use crate::{
    native_return_first_arg, native_synchronized_collection, native_synchronized_list,
    native_synchronized_map, native_synchronized_set,
};

// Note: Locale and Charset natives are already registered in earlier phases

// ===========================================================================
// [PERF nb-phases-early-perf] java.util.Properties key->index lookup cache.
//
// The array-backed `Properties.getProperty`/`setProperty`/`containsKey`
// natives registered in `register_core_stdlib_extras` store entries as an
// interleaved key/value reference array (data[2i]=key, data[2i+1]=value) on
// field 0 of the Properties object, with the pair count on field 1. Their
// original lookup was a FULL LINEAR SCAN that called `ctx.read_string(k_ref)`
// — a heap read + UTF-8 decode + String allocation — on EVERY stored key per
// call, i.e. O(n) String allocations per `getProperty`. A typical app loads N
// properties once and then reads them thousands of times, so this is O(n) per
// read for the entire run.
//
// This cache is a PURE ACCELERATOR: the interleaved array remains the single
// source of truth (every other Properties native — `size`, `stringPropertyNames`,
// and the side-table-backed `put`/`load`/`entrySet`/… — is untouched). The
// cache maps each decoded key string to its FIRST pair index, exactly mirroring
// the linear scan's "first match wins, null key slots skipped" semantics. It is
// validated on every use against a GC-stable token `(array identity hash, size)`:
// if the backing array was reallocated (setProperty growth) or the size changed
// by any path, the entry is rebuilt with a single scan (same cost as the old
// code's worst case). The array identity hash — not the raw pointer — is the
// validity token because it is stable across a moving-GC relocation, avoiding
// the ABA hazard documented in properties_sidetable.rs::key_for.
//
// Behavior is byte-for-byte identical to the linear scan; only the per-lookup
// cost changes from O(n) decode+alloc to O(1) hash probe on the hot read path.
// ===========================================================================
mod props_index_cache {
    use super::*;
    use parking_lot::Mutex;
    use rustc_hash::FxHashMap;
    use std::collections::HashMap;
    use std::sync::OnceLock;

    /// One cached key->first-index map plus the validity token it was built
    /// against. `arr_token` is the backing array's identity hash (GC-stable);
    /// `size` is the pair count (field 1). The entry is valid iff BOTH still
    /// match the live object, otherwise it is rebuilt.
    struct Entry {
        arr_token: i32,
        size: usize,
        map: HashMap<String, usize>,
    }

    /// Per-Properties-object cache, keyed by the object's weak lock key
    /// (`crate::gc_stable_weak_lock_key`) so two distinct Properties never
    /// share an entry even if a moving GC reuses a raw address. A stale
    /// generation is simply overwritten on rebuild; the map is bounded by the
    /// same per-object property cap the side-table enforces (a Properties
    /// cannot hold unbounded keys), so this cannot grow without bound for a
    /// given object.
    ///
    /// gc-common w19-c: the key was the bare identity hash, and nothing ever
    /// removed a row. Every heap mints identity hashes from 1, so two VMs in
    /// one process shared rows (VM B's `Properties` with VM A's hash, array
    /// hash and size read A's key->index map and answered a different key's
    /// value), and every `Properties` object that was ever read left its row
    /// (a decoded copy of all its keys) for the life of the process. The lock
    /// key is VM-scoped and weak: when the object dies, or its VM is torn
    /// down, the registry frees the key and [`forget_keys`] (through
    /// `forget_pbkdf2_keys`, which both registry sweeps call) drops the row.
    fn cache() -> &'static Mutex<FxHashMap<usize, Entry>> {
        static C: OnceLock<Mutex<FxHashMap<usize, Entry>>> = OnceLock::new();
        C.get_or_init(|| Mutex::new(FxHashMap::default()))
    }

    /// The row key of `this`: its weak lock key (see [`cache`]). Computed
    /// before the cache lock is taken, so the registry lock and the cache
    /// lock are never held together.
    fn row_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
        crate::gc_stable_weak_lock_key(ctx, this)
            .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
    }

    /// Drop the rows filed under `keys`, which the lock-key registry has just
    /// freed (the `Properties` died, or its VM is being torn down). Leaf lock;
    /// no `ctx` call.
    pub(super) fn forget_keys(keys: &[usize]) {
        let mut c = cache().lock();
        for k in keys {
            c.remove(k);
        }
    }

    /// Test-only reader. (`cfg_attr`, not `#[cfg(test)]`: the stale-handle
    /// audit treats everything below a file's first `#[cfg(test)]` as test
    /// code.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn has_row_for_test(key: usize) -> bool {
        cache().lock().contains_key(&key)
    }

    /// Rebuild a fresh key->first-index map from the interleaved array,
    /// mirroring the linear scan exactly: ascending order, first occurrence of
    /// a decoded key wins (`or_insert`), null key slots skipped.
    fn build_map(ctx: &dyn NativeContext, data: ObjectRef, size: usize) -> HashMap<String, usize> {
        let mut map: HashMap<String, usize> = HashMap::with_capacity(size);
        for i in 0..size {
            if let Value::Object(Some(k_ref)) = ctx.get_array_element(data, i * 2) {
                if let Some(k) = ctx.read_string(k_ref) {
                    map.entry(k).or_insert(i);
                }
            }
        }
        map
    }

    /// Look up `key`'s pair index in O(1), (re)building the cache if its
    /// validity token no longer matches the live `(data, size)`. Returns the
    /// pair index `i` (the value lives at `data[i*2+1]`) or `None` if absent —
    /// identical to what the old linear scan would have found.
    pub(super) fn lookup_index(
        ctx: &dyn NativeContext,
        this: ObjectRef,
        data: ObjectRef,
        size: usize,
        key: &str,
    ) -> Option<usize> {
        let obj_token = row_key(ctx, this);
        let arr_token = ctx.identity_hash_code(data);
        let mut c = cache().lock();
        let needs_rebuild = match c.get(&obj_token) {
            Some(e) => e.arr_token != arr_token || e.size != size,
            None => true,
        };
        if needs_rebuild {
            let map = build_map(ctx, data, size);
            c.insert(
                obj_token,
                Entry {
                    arr_token,
                    size,
                    map,
                },
            );
        }
        // SAFETY of unwrap: just inserted-or-validated above.
        c.get(&obj_token).and_then(|e| e.map.get(key).copied())
    }

    /// After `setProperty` APPENDS a new pair at index `new_index` (so size is
    /// now `new_index + 1`) into `data`, fold that single key into the cached
    /// map without a full rebuild — keeping the amortized cost of a load+many
    /// reads at O(1) per op. The caller has already validated/populated the
    /// entry via a preceding `lookup_index`, so the cache entry exists; if the
    /// backing array was just grown, `data`/`arr_token` are the NEW array and
    /// we refresh the token.
    ///
    /// The indexed key is decoded from the stored array slot `data[new_index*2]`
    /// (NOT the caller's key string) so the cache always mirrors exactly what a
    /// full `build_map` rebuild would produce: a null key slot is skipped (the
    /// linear scan never matched it), and first-occurrence semantics are kept
    /// with `or_insert`. If the slot decode disagrees with the cache shape we
    /// simply drop the entry and let the next lookup rebuild — correct either
    /// way.
    pub(super) fn note_append(
        ctx: &dyn NativeContext,
        this: ObjectRef,
        data: ObjectRef,
        new_index: usize,
    ) {
        let obj_token = row_key(ctx, this);
        let arr_token = ctx.identity_hash_code(data);
        let new_size = new_index + 1;
        let mut c = cache().lock();
        match c.get_mut(&obj_token) {
            // Fast path: cache is contiguous with this append (it was built
            // for the pre-append size against the same backing array).
            Some(e) if e.size == new_index => {
                // Index the key exactly as build_map would: decode the stored
                // slot, skip a null key (unmatched by the old scan).
                if let Value::Object(Some(k_ref)) = ctx.get_array_element(data, new_index * 2) {
                    if let Some(k) = ctx.read_string(k_ref) {
                        e.map.entry(k).or_insert(new_index);
                    }
                }
                e.size = new_size;
                // Refresh the array token in case setProperty grew (copied)
                // the array — the contents are identical so the indices still
                // hold, only the array identity changed.
                e.arr_token = arr_token;
            }
            // Otherwise drop the entry; the next lookup rebuilds from scratch.
            _ => {
                c.remove(&obj_token);
            }
        }
    }
}

/// Forget the `InheritableThreadLocal` `itl` (tests).
#[cfg(test)]
pub fn tl_inheritable_remove(ctx: &dyn NativeContext, itl: ObjectRef) {
    if let Some(key) = crate::existing_weak_lock_key(ctx, itl) {
        tl_inheritable_drop(&mut tl_side_tables().lock(), key);
    }
}

#[cfg(test)]
mod fjp_gc_tests;
#[cfg(test)]
mod r20_yhttp_isa_hash_tests;
/// PBKDF2 (PKCS#5 v2.0) over a 128-byte-block PRF (`SHA-384` / `SHA-512`) —
/// `hmac_block64` above hardcodes a 64-byte HMAC block, which is wrong for
/// these (RFC 2104 derives the block size from the underlying hash's own
/// block size, 128 bytes for SHA-384/512), so it would silently derive the
/// wrong key instead of throwing. Uses the `hmac` crate (already a dependency,
/// see `t27_tls_cbc.rs`) so the block size is correct by construction rather
/// than guessed. pgjdbc's SCRAM-SHA-256 client needs `PBKDF2WithHmacSHA384`
/// wired for `com.ongres.scram.common.ScramMechanism`'s static init to
/// succeed, even though SHA-384 itself is only used for the (unrelated)
/// SCRAM-SHA-256-PLUS channel-binding negotiation path.
macro_rules! pbkdf2_derive_wide_impl {
    ($name:ident, $digest:ty) => {
        fn $name(pw: &[u8], salt: &[u8], iters: u32, dklen: usize) -> Vec<u8> {
            use hmac::Mac;
            type HmacImpl = hmac::Hmac<$digest>;
            let mut out: Vec<u8> = Vec::with_capacity(dklen);
            let mut block_index: u32 = 1;
            while out.len() < dklen {
                let mut salt_i = salt.to_vec();
                salt_i.extend_from_slice(&block_index.to_be_bytes());
                let mut mac = HmacImpl::new_from_slice(pw).expect("Hmac accepts any key length");
                mac.update(&salt_i);
                let mut u = mac.finalize().into_bytes().to_vec();
                let mut t = u.clone();
                for _ in 1..iters.max(1) {
                    let mut mac =
                        HmacImpl::new_from_slice(pw).expect("Hmac accepts any key length");
                    mac.update(&u);
                    u = mac.finalize().into_bytes().to_vec();
                    for (a, b) in t.iter_mut().zip(u.iter()) {
                        *a ^= *b;
                    }
                }
                out.extend_from_slice(&t);
                block_index += 1;
            }
            out.truncate(dklen);
            out
        }
    };
}
pbkdf2_derive_wide_impl!(pbkdf2_derive_wide_sha384, sha2::Sha384);
#[cfg(test)]
mod nio21w2_latin1_inflate_fault_tests;
#[cfg(test)]
mod nio21w6_latin1_compare_fault_tests;
#[cfg(test)]
mod r21_arrays_support_mismatch_fault_tests;
// `Scanner.findWithinHorizon` used to be implemented here, over the model's
// slots 0 and 1, and registered by `register_t2_3_completion_natives` — which
// has no call site, so it was live in no configuration. Measured rather than
// assumed: `--dump-native-registry` shows no `findWithinHorizon` row among the
// 35 live `java/util/Scanner` entries, and the call reached real JDK bytecode
// that reads `buf`/`matcher`/`source` and threw `NullPointerException` in both
// modes (`probes/L3ScannerSearchProbe` on the pre-fix binary).
//
// It now lives in `native-io/src/lib.rs`, beside `findInLine` and `skip`: they
// share the Scanner state accessors and the same regex engine, and
// `register_scanner_natives` there is registered in every configuration. Moved
// rather than wired up in place — a second implementation over a second model
// is what made this lane's own brief describe `Scanner` as three fields wide.

// ===========================================================================
// T2.3 unit tests — Random / StringTokenizer / ArraysSupport natives
// ===========================================================================
#[cfg(test)]
mod t2_tests;

/// gc-common w10-a: the `withInitial` supplier table and the
/// `InheritableThreadLocal` table are one per VM.
#[cfg(test)]
mod w10a_thread_local_side_table_vm_isolation_tests;

/// gc-common w29-a (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 1): ThreadLocal rows belong to their ThreadLocal, not to its identity
/// hash. The mock's identity hash is the address truncated to `i32`, so two
/// fake ThreadLocals 4 GiB apart share one (asserted first, lesson mm); the
/// ThreadLocal paths never dereference them. Each body runs on its own
/// `std::thread` (the map is thread-local) with a private VM identity.
#[cfg(test)]
mod w29a_thread_local_row_tests;

/// gc-common w15-b (`common-w14c-sibling-identity-key-registries-are-process-wide-and-never-swept`):
/// `SecretKeyFactory` rows keyed through the lib.rs lock-key registry. The
/// mock's identity hash is the address truncated to `i32`, so two addresses
/// 4 GiB apart share one; the factories are fake addresses these paths never
/// dereference (the `getAlgorithm` hit path reads only the table).
#[cfg(test)]
mod w15b_skf_key_tests;

/// gc-common w19-c: the rooted builders (stale-handle triage) and the three
/// tables that stopped being process-wide (the `Properties` index cache, the
/// `QName` parts, the `Security` state). The mock never moves an object, so
/// the builders are checked for their shapes and for releasing every root.
#[cfg(test)]
mod w19c_stale_handle_tests;

/// gc-common w19-d handoff: `URI.create(String)` roots the URI across every
/// component String and the by-name publish, and returns its address after
/// them. The mock never moves an object at an allocation, so this checks the
/// synthetic-layout slots and that no root is left behind.
#[cfg(test)]
mod w19d_uri_create_tests;

/// gc-common w20-b (stale-handle triage): natives that call Java in a loop
/// and used addresses from before the call. The move is simulated inside the
/// hooked `invoke_virtual`: the native roots are remapped from each old
/// address to a second object that plays the moved copy, and the vacated
/// copy is clobbered to what a collector leaves behind.
#[cfg(test)]
mod w20b_stale_handle_tests;

#[cfg(test)]
mod i8_l2_typed_array_tests;

#[cfg(test)]
mod i9_l2_enum_identity_tests;

/// SBR round 2, lane `thread`: the `ThreadLocal` Intrinsics are registered by
/// default in `--compatible` and NOT under `--real-jdk`, where the JDK's own
/// `ThreadLocalMap` bytecode runs over the real `Thread.threadLocals` fields.
#[cfg(test)]
mod sbr2_thread_local_registration_tests;

#[cfg(test)]
mod i74_l5_mismatch_bulk_tests;

/// gci i18/vp: the real-field `ThreadLocal.get()` `Intrinsic`'s decline into
/// the JDK's `get(Thread)` is a transparent tail call
/// (`continuation_pin::tail_call`): `initialValue()` (a `withInitial`
/// supplier) that parks unmounts the virtual thread, as on HotSpot, instead
/// of being pinned by the intrinsic's native frame.
#[cfg(test)]
mod gci_i18vp_thread_local_tail_call_tests;

/// gci i27/vt (`gci-i20st-compatible-threadlocal-initial-value-pins-a-virtual-thread-FIXED-20261017`):
/// under `--compatible` the `withInitial` miss path is a transparent tail
/// call into `ThreadLocalInitial.supplied(tl, s)` (whose bytecode stores the
/// value), so a supplier that parks does not pin its virtual thread; under
/// `--real-jdk` the helper class is never defined.
#[cfg(test)]
mod gci_i27vt_thread_local_initial_tests;

/// gci i28/tl (`gci-i20st-compatible-threadlocal-initial-value-pins-a-virtual-thread-FIXED-20261017`,
/// the `initialValue()` arm): a `ThreadLocal` subclass overriding
/// `initialValue()` (and not `set`) misses through a transparent tail call
/// into the BOOT-loader helper `ThreadLocal$CratonInitial.initial(tl)`.
#[cfg(test)]
mod gci_i28tl_thread_local_initial_value_tests;

/// JIT round 21 (lane tl2, `CRATONVM_TL_REAL_TYPED_READS`): the real-field
/// `ThreadLocal` fast path's typed reads answer what `get_field` answers.
#[cfg(test)]
mod jr21_tl2_typed_read_tests;

/// j27 lane FBTAIL: `VM.initLevel()` answers the JDK's static, as the JDK body
/// and HotSpot do; only a VM with no static to read keeps the floor of 2.
#[cfg(test)]
mod j27_fbtail_vm_init_level_tests;

mod sis_read_deadline;
#[allow(unused_imports)]
pub use sis_read_deadline::*;
#[allow(unused_imports)]
use sis_read_deadline::*;
mod take_inherited_for_thread_object;
#[allow(unused_imports)]
pub use take_inherited_for_thread_object::*;
#[allow(unused_imports)]
use take_inherited_for_thread_object::*;
mod register_qname_natives;
#[allow(unused_imports)]
pub use register_qname_natives::*;
#[allow(unused_imports)]
use register_qname_natives::*;
mod initialize_real_thread_pool_executor;
#[allow(unused_imports)]
pub use initialize_real_thread_pool_executor::*;
#[allow(unused_imports)]
use initialize_real_thread_pool_executor::*;
mod register_phase52_byte_order;
#[allow(unused_imports)]
pub use register_phase52_byte_order::*;
#[allow(unused_imports)]
use register_phase52_byte_order::*;
mod register_synthetic_socket_stubs;
#[allow(unused_imports)]
pub use register_synthetic_socket_stubs::*;
#[allow(unused_imports)]
use register_synthetic_socket_stubs::*;
mod p54_huc_do_request_body;
#[allow(unused_imports)]
pub use p54_huc_do_request_body::*;
#[allow(unused_imports)]
use p54_huc_do_request_body::*;
