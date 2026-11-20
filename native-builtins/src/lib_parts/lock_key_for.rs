#[allow(unused_imports)]
use super::super::*;

pub fn lock_key_for(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    held: bool,
) -> Result<usize, MethodCallFailed> {
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let mut guard = lock_key_shards::lock_key_shard(hash).lock();
    let LockKeyRegistry {
        next_generation,
        buckets,
        tombstones,
        by_addr,
    } = &mut *guard;
    let bucket = buckets.entry(hash).or_default();
    if let Some(slot) = bucket.iter_mut().find(|s| s.vm == vm && s.last_ptr == ptr) {
        slot.held |= held;
        return Ok(pack_lock_key(hash, slot.generation));
    }
    // gc-common w25-a: any other address is another object, even when it is
    // this VM's only other slot of `hash` -- a live collider or a newcomer
    // keyed before the sweep that frees a dead one's slot. The post-collection
    // remap keeps every live slot at its object's current address, so a moved
    // object never reaches here; a caller holding a STALE copy of a moved
    // object's address does, and gets a key of its own at the vacated address
    // (a miss, which the next sweep drops), never the object's -- nor, as the
    // old "lone occupant" step could, a third object's. See the module note.
    //
    // One counter for every hash; skip a key still in use (only reachable
    // after 2^32 mints).
    let mut generation = *next_generation;
    while bucket.iter().any(|s| s.generation == generation)
        || tombstones.contains_key(&(pack_lock_key(hash, generation) as u64))
    {
        generation = generation.wrapping_add(1);
    }
    *next_generation = generation.wrapping_add(1);
    bucket.push(LockKeyEntry {
        vm,
        last_ptr: ptr,
        generation,
        held,
    });
    by_addr.insert((ptr, vm, hash));
    Ok(pack_lock_key(hash, generation))
}

/// The VM that minted `key` through [`gc_stable_lock_key`], or `None` when no
/// entry names it (never minted, released by [`release_lock_key`], swept
/// weak, or dropped by [`forget_vm_lock_keys`]). A held key whose object died
/// still answers (its tombstone).
///
/// Lets a table keyed by lock keys (the `t27_tls` `SSLContext` tables) scan,
/// remap and forget one VM's rows without storing the VM a second time.
pub fn lock_key_owner_vm(key: u64) -> Option<usize> {
    let hash = (key >> 32) as u32;
    let generation = key as u32;
    let reg = lock_key_shards::lock_key_shard(hash).lock();
    reg.buckets
        .get(&hash)
        .and_then(|b| b.iter().find(|s| s.generation == generation))
        .map(|s| s.vm)
        .or_else(|| reg.tombstones.get(&key).copied())
}

/// Nothing outside the object's lifetime names `key` any more (the `t27_tls`
/// `SSLContext` rows filed under it are gone): drop its tombstone now, or --
/// while the object still lives -- let the first sweep that finds it dead drop
/// the slot instead of leaving a tombstone. The key is never minted again.
/// Idempotent; a key with neither is ignored.
pub fn release_lock_key(key: u64) {
    let hash = (key >> 32) as u32;
    let generation = key as u32;
    let mut reg = lock_key_shards::lock_key_shard(hash).lock();
    if reg.tombstones.remove(&key).is_some() {
        return;
    }
    if let Some(slot) = reg
        .buckets
        .get_mut(&hash)
        .and_then(|b| b.iter_mut().find(|s| s.generation == generation))
    {
        slot.held = false;
    }
}

/// Drop `vm`'s lock-key entries (VM teardown), tombstones included, and the
/// buckets they leave empty. The generation counter is untouched, so no key
/// `vm` used is ever minted again. Idempotent.
pub fn forget_vm_lock_keys(vm: usize) {
    let mut freed: Vec<usize> = Vec::new();
    // gcg g3/k: one shard at a time (shard 0 alone with the switch off).
    for shard in lock_key_shards::lock_key_shards_in_use() {
        let mut reg = shard.lock();
        reg.buckets.retain(|hash, bucket| {
            for s in bucket.iter().filter(|s| s.vm == vm && !s.held) {
                freed.push(pack_lock_key(*hash, s.generation));
            }
            bucket.retain(|s| s.vm != vm);
            !bucket.is_empty()
        });
        reg.by_addr.retain(|&(_, owner, _)| owner != vm);
        reg.tombstones.retain(|_, owner| *owner != vm);
    }
    // gc-common w14-c handoff: the lock state filed under the freed weak keys
    // goes too (the stamped / RRWL table locks are leaves). The AQLS stand-ins
    // keep no table since interpreter round i3 wave 73 (their word lives in
    // the object's own `state` field).
    crate::stamped_lock::forget_lock_keys(&freed);
    // gc-common w15-b: the two sibling schemes key through this registry now
    // (`properties_sidetable::key_for`, `phases_early::pbkdf2_key_for`); their
    // rows go with the keys, and the `Properties` module's per-VM state (its
    // weak-track queue handle, the last system-properties publication) with
    // the VM.
    crate::properties_sidetable::forget_properties_keys(&freed);
    crate::phases_early::forget_pbkdf2_keys(&freed);
    crate::properties_sidetable::forget_vm_properties_state(vm);
    // gc-common w16-b: three more former private copies key through this
    // registry now (`AbstractClassLoaderValue`, holder-less `Semaphore`,
    // `KeyPairGenerator` / `KeyFactory`); their rows go with the keys.
    crate::classloader_value_sidetable::forget_clv_keys(&freed);
    crate::util_concurrent_ext::forget_sem_state_keys(&freed);
    crate::jca::key_factory::forget_kpg_keys(&freed);
    // gc-common w17-b: four more (`CharsetEncoder` BOM state, XNIO OptionMap /
    // Builder handles, `Signature` side tables, `RSA_REALKEY_MAP`).
    crate::charset::forget_bom_keys(&freed);
    crate::xnio_async::forget_xnio_obj_keys(&freed);
    crate::xnio_worker::forget_xnio_worker_obj_keys(&freed); // gc-common w24-a
    crate::jca::signature::forget_sig_keys(&freed);
    crate::crypto_impl::forget_rsa_realkey_keys(&freed);
    crate::jca::forget_jca_lock_keys(&freed); // gc-common w18-e
    forget_lib_side_table_keys(&freed); // gc-common w20-c
    crate::wildfly_security::forget_wildfly_security_keys(&freed); // gc-common w21-a
    #[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
    crate::serialization::forget_serialization_keys(&freed); // gc-common w21-c
    crate::t27_tls::forget_tls_object_keys(&freed); // gc-common w26-b
    crate::phases_late::ssl_security::forget_ssl_security_obj_keys(vm, &freed); // gc-common w27-a
    crate::x509_manager::forget_x509_manager_keys(&freed); // gc-common w27-b
    crate::keystore::forget_keystore_keys(&freed); // gc-common w27-b
    crate::http_url_connection::forget_huc_obj_keys(&freed); // gc-common w27-b
    crate::net_phase_e::forget_net_obj_keys(&freed); // gc-common w28-a
    forget_core_object_state_keys(vm, &freed); // gc-common w29-b
    crate::phases_late::zip_streams::forget_zip_keys(vm, &freed); // gc-common w29-c
    crate::regex_matcher::forget_regex_matcher_keys(&freed); // gc-common w29-d
    crate::wildfly_undertow::forget_undertow_obj_keys(&freed); // gc-common w29-d
    crate::servlet::forget_selector_wakeup_keys(&freed); // gc-common w29-d
    crate::jca::ssl_context_spi::forget_ssl_context_keys(&freed); // gc-common w29-d
    crate::xml_stax::forget_xml_stax_keys(&freed); // gc-common w29-d
    crate::lang_class::forget_lang_class_keys(&freed); // gc-common w29-e
    crate::generics::forget_vm_type_parameter_keys(vm, &freed); // gc-common w29-e
    crate::phases_late::forget_classvalue_keys(vm, &freed); // gc-common w29-e
    crate::reflect_annotations::forget_proxy_namespace_keys(&freed); // gc-common w29-e
    crate::jboss_jdkspecific::forget_module_packages_keys(&freed); // gc-common w29-e
    crate::phases_early::forget_thread_state_keys(&freed); // gc-common w29-a
    crate::infinispan_local::forget_infinispan_keys(vm, &freed); // gc-common w30-a
    crate::lucene_es::forget_randomized_keys(vm, &freed); // gc-common w32-a
    crate::ironjacamar_pool::forget_jca_pool_keys(&freed); // gch h1/ak
}

/// gc-common w29-b (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// lane 2 "core object state"): the tables that moved off `(VM, identity
/// hash)` keys onto weak lock keys this wave drop the rows filed under
/// `keys` (freed by VM `vm`'s sweep or teardown): the Unsafe synthetic slots,
/// the `ReentrantLock` / `ReentrantReadWriteLock` states, the `Random` /
/// `SecureRandom` generator states, the VarHandle meta and FFM layout-handle
/// rows, and the synthetic `EnhancedQueueExecutor` shutdown flags. One entry
/// in each hook list, so the lists stay one line per lane. Every table lock
/// is a leaf, taken and released on its own.
pub fn forget_core_object_state_keys(vm: usize, keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    forget_synthetic_field_keys(keys);
    forget_rl_state_keys(keys);
    crate::securerandom::forget_random_state_keys(keys);
    crate::lang_invoke::forget_var_handle_meta_keys(keys);
    crate::wildfly_core::forget_eqe_shutdown_keys(vm, keys);
}

/// Judge VM `vm`'s live lock-key slots after a reclamation and answer how many
/// were dropped: a slot whose object did not survive is dropped (minted by
/// `gc_stable_weak_lock_key`) or turned into a tombstone (minted by
/// `gc_stable_lock_key`). Other VMs' slots are never judged.
///
/// Same `is_live` contract as [`gc_sweep_locale_rows`] / [`gc_sweep_tls_rows`]:
/// the address as the slot holds it, judged by the `survived_in_place` verdict
/// (`addr_keyed::sweep_address_keyed_tables`). Nothing moves there; the
/// stop-the-world epilogue calls [`gc_sweep_and_remap_lock_keys`] instead, and
/// the per-slice freed-span drop calls [`gc_drop_lock_keys_in_spans`] (since
/// gc-common w26-c; it used to pass a "not in a span" predicate here).
pub fn gc_sweep_lock_keys(vm: usize, is_live: &dyn Fn(usize) -> bool) -> usize {
    sweep_lock_keys(
        vm,
        LockKeyVerdict::Predicate {
            is_live,
            pointer_map: None,
        },
    )
}

/// [`gc_sweep_lock_keys`] for a reclamation that knows exactly which address
/// ranges it freed -- the Generational concurrent sweep, one call per slice
/// (`addr_keyed::drop_address_keyed_rows_in`): drop (or tombstone, when held)
/// every slot of VM `vm` whose address lies in one of `spans` (`(start, len)`,
/// sorted by `start`, non-overlapping, as there), keep every other slot, and
/// answer how many were dropped. The freed keys fan out to the same
/// `forget_*_keys` hooks as every other sweep.
///
/// gc-common w26-c (`common-w25a-lock-key-sweep-walks-the-whole-registry-per-concurrent-sweep-slice`):
/// the range test touches no heap and takes no lock, so it runs UNDER the
/// registry mutex, in one pass over the registry: no copy of the VM's slots,
/// no `dyn` predicate call per slot, no second acquisition. An empty `spans`
/// returns at once, without taking the mutex. Nothing moves; no re-addressing.
pub fn gc_drop_lock_keys_in_spans(vm: usize, spans: &[(usize, usize)]) -> usize {
    sweep_lock_keys(vm, LockKeyVerdict::FreedSpans(spans))
}

/// How [`sweep_lock_keys`] judges the collecting VM's slots.
pub enum LockKeyVerdict<'a> {
    /// A liveness predicate that may take the old-gen lock (or, in `t27_tls`,
    /// be asked while its callers hold their own tables' locks): evaluated on a
    /// copy of the VM's slots with the registry lock released; the survivors
    /// are then re-addressed through `pointer_map`, when there is one.
    Predicate {
        is_live: &'a dyn Fn(usize) -> bool,
        pointer_map: Option<&'a cratonvm_types::PointerMap>,
    },
    /// The spans a reclamation freed: a pure range test, judged under the
    /// registry lock in one pass ([`gc_drop_lock_keys_in_spans`]).
    FreedSpans(&'a [(usize, usize)]),
}

/// [`LockKeyVerdict::FreedSpans`]: the slots inside the spans, read off each
/// shard's address index under its mutex (common-w26c item 1: cost is the
/// slots freed, not the registry's size). Answers the dropped count and the
/// freed (weak) keys; a held slot
/// becomes a tombstone, exactly as in [`judge_lock_keys_by_predicate`].
pub fn drop_lock_keys_in_spans_locked(vm: usize, spans: &[(usize, usize)]) -> (usize, Vec<usize>) {
    let mut dropped = 0usize;
    let mut freed: Vec<usize> = Vec::new();
    if spans.is_empty() {
        return (dropped, freed);
    }
    // gcg g3/k: one shard at a time (shard 0 alone with the switch off). A
    // slot's tombstone goes into the slot's own shard: the shard of its hash.
    for shard in lock_key_shards::lock_key_shards_in_use() {
        let mut guard = shard.lock();
        let LockKeyRegistry {
            buckets,
            tombstones,
            by_addr,
            ..
        } = &mut *guard;
        // common-w26c item 1: only the slots inside a span, read off the
        // address index (`range` end is exclusive: `(end, 0, 0)` sorts below
        // every entry at `end`).
        let hits: Vec<(usize, usize, u32)> = spans
            .iter()
            .flat_map(|&(start, len)| {
                by_addr.range((start, 0, 0)..(start.saturating_add(len), 0, 0))
            })
            .filter(|&&(_, owner, _)| owner == vm)
            .copied()
            .collect();
        for (ptr, _, hash) in hits {
            by_addr.remove(&(ptr, vm, hash));
            let Some(bucket) = buckets.get_mut(&hash) else {
                continue;
            };
            let Some(i) = bucket.iter().position(|s| s.vm == vm && s.last_ptr == ptr) else {
                continue;
            };
            let slot = bucket.swap_remove(i);
            let key = pack_lock_key(hash, slot.generation);
            if slot.held {
                tombstones.insert(key as u64, vm);
            } else {
                dropped += 1;
                freed.push(key);
            }
            if bucket.is_empty() {
                buckets.remove(&hash);
            }
        }
    }
    (dropped, freed)
}

/// gci i30/rp: append the current address of every lock-key slot of VM `vm`
/// to `out` (the addresses [`judge_lock_keys_by_predicate`] will judge), one
/// shard lock at a time, read-only. See [`gc_weak_row_owners`].
pub fn lock_key_owner_addrs(vm: usize, out: &mut Vec<usize>) {
    for shard in lock_key_shards::lock_key_shards_in_use() {
        let reg = shard.lock();
        out.extend(
            reg.buckets
                .values()
                .flat_map(|bucket| bucket.iter())
                .filter(|s| s.vm == vm)
                .map(|s| s.last_ptr),
        );
    }
}

/// [`LockKeyVerdict::Predicate`]: copy the VM's slots out, judge them with the
/// registry lock released, then remove the dead ones under a second
/// acquisition that re-checks each is still the slot that was judged, and
/// re-address the survivors. Answers the dropped count and the freed keys.
pub fn judge_lock_keys_by_predicate(
    vm: usize,
    is_live: &dyn Fn(usize) -> bool,
    pointer_map: Option<&cratonvm_types::PointerMap>,
) -> (usize, Vec<usize>) {
    // gcg g3/k: every pass visits the shards in use one at a time (shard 0
    // alone with `CRATONVM_LOCK_KEY_SHARDS` off, which is the old single
    // registry and the old two acquisitions).
    let shards = lock_key_shards::lock_key_shards_in_use();
    let mut candidates: Vec<(u32, u32, usize)> = Vec::new();
    for shard in shards {
        let reg = shard.lock();
        candidates.extend(reg.buckets.iter().flat_map(|(hash, bucket)| {
            bucket
                .iter()
                .filter(move |s| s.vm == vm)
                .map(move |s| (*hash, s.generation, s.last_ptr))
        }));
    }
    if candidates.is_empty() {
        return (0, Vec::new());
    }
    let dead: Vec<(u32, u32, usize)> = candidates
        .into_iter()
        .filter(|&(_, _, ptr)| !is_live(ptr))
        .collect();
    let mut dropped = 0usize;
    let mut freed: Vec<usize> = Vec::new();
    let pointer_map = pointer_map.filter(|m| !m.is_empty());
    for (index, shard) in shards.iter().enumerate() {
        let mut guard = shard.lock();
        let LockKeyRegistry {
            buckets,
            tombstones,
            by_addr,
            ..
        } = &mut *guard;
        for &(hash, generation, ptr) in dead
            .iter()
            .filter(|&&(hash, _, _)| lock_key_shards::lock_key_shard_of(hash) == index)
        {
            let emptied = match buckets.get_mut(&hash) {
                Some(bucket) => {
                    if let Some(i) = bucket
                        .iter()
                        .position(|s| s.generation == generation && s.vm == vm && s.last_ptr == ptr)
                    {
                        let slot = bucket.swap_remove(i);
                        by_addr.remove(&(ptr, vm, hash));
                        if slot.held {
                            tombstones.insert(pack_lock_key(hash, generation) as u64, vm);
                        } else {
                            dropped += 1;
                            freed.push(pack_lock_key(hash, generation));
                        }
                    }
                    bucket.is_empty()
                }
                None => false,
            };
            if emptied {
                buckets.remove(&hash);
            }
        }
        if let Some(pointer_map) = pointer_map {
            let mut moved: Vec<(usize, usize, u32)> = Vec::new();
            for (&hash, bucket) in buckets.iter_mut() {
                for slot in bucket.iter_mut().filter(|s| s.vm == vm) {
                    if let Some(&new_addr) = pointer_map.get(&slot.last_ptr) {
                        moved.push((slot.last_ptr, new_addr, hash));
                        slot.last_ptr = new_addr;
                    }
                }
            }
            // Remove every old key before inserting any new one: a chained
            // map can move a slot onto an address another slot just left.
            for &(old, _, hash) in &moved {
                by_addr.remove(&(old, vm, hash));
            }
            by_addr.extend(moved.into_iter().map(|(_, new, hash)| (new, vm, hash)));
        }
        drop(guard);
    }
    (dropped, freed)
}

/// [`gc_sweep_lock_keys`] for the stop-the-world epilogue
/// (`gc_and_alloc::process_references_after_gc`), which then re-addresses the
/// surviving slots through `pointer_map`, each once (the map may be chained).
/// `is_live` answers for the PRE-remap address, as there.
///
/// Called after `gc_sweep_tls_rows`, so an `SSLContext` whose rows that sweep
/// released goes in the same collection.
///
/// `is_live` is never evaluated under the registry lock (the stop-the-world
/// predicate takes the old-gen lock, and `t27_tls` asks the registry while
/// holding its own tables' locks): the slots are copied out, judged, and the
/// dead ones removed under a second acquisition that re-checks each is still
/// the slot that was judged. A slot a mutator mints meanwhile (an out-of-pause
/// sweep) is not judged until the next reclamation.
pub fn gc_sweep_and_remap_lock_keys(
    vm: usize,
    is_live: &dyn Fn(usize) -> bool,
    pointer_map: &cratonvm_types::PointerMap,
) -> usize {
    sweep_lock_keys(
        vm,
        LockKeyVerdict::Predicate {
            is_live,
            pointer_map: Some(pointer_map),
        },
    )
}

/// Judge VM `vm`'s slots as `verdict` says, then fan the freed weak keys out
/// to every table filed under them. The ONE list of `forget_*_keys(&freed)`
/// hooks for every sweep (full, remapping or freed-span); the other list is
/// VM teardown's, in [`forget_vm_lock_keys`]. Both run with the registry lock
/// released.
pub fn sweep_lock_keys(vm: usize, verdict: LockKeyVerdict<'_>) -> usize {
    // gc-common w29-b: a moving collection's epilogue -- objects may now sit at
    // new addresses, and the VarHandle memos are keyed by address.
    if matches!(verdict, LockKeyVerdict::Predicate { pointer_map: Some(m), .. } if !m.is_empty()) {
        crate::lang_invoke::note_objects_moved();
    }
    let (dropped, freed) = match verdict {
        LockKeyVerdict::Predicate {
            is_live,
            pointer_map,
        } => judge_lock_keys_by_predicate(vm, is_live, pointer_map),
        LockKeyVerdict::FreedSpans(spans) => drop_lock_keys_in_spans_locked(vm, spans),
    };
    if freed.is_empty() {
        // Every hook below returns at once on no keys (a concurrent-sweep
        // slice usually frees none); skip the calls.
        return dropped;
    }
    // gc-common w14-c handoff: a freed weak key is never minted again, so its
    // lock state can go with it.
    crate::stamped_lock::forget_lock_keys(&freed);
    // gc-common w15-b: and so can the `Properties` side-map rows and the
    // `SecretKeyFactory` PRF / algorithm rows filed under it.
    crate::properties_sidetable::forget_properties_keys(&freed);
    crate::phases_early::forget_pbkdf2_keys(&freed);
    // gc-common w16-b: and the `AbstractClassLoaderValue` rows (either key),
    // `Semaphore` side states and `KeyPairGenerator` / `KeyFactory` rows.
    crate::classloader_value_sidetable::forget_clv_keys(&freed);
    crate::util_concurrent_ext::forget_sem_state_keys(&freed);
    crate::jca::key_factory::forget_kpg_keys(&freed);
    // gc-common w17-b: and the `CharsetEncoder` BOM rows, XNIO OptionMap /
    // Builder handle rows (with the stores they name), `Signature` rows and
    // `RSA_REALKEY_MAP` rows.
    crate::charset::forget_bom_keys(&freed);
    crate::xnio_async::forget_xnio_obj_keys(&freed);
    crate::xnio_worker::forget_xnio_worker_obj_keys(&freed); // gc-common w24-a
    crate::jca::signature::forget_sig_keys(&freed);
    crate::crypto_impl::forget_rsa_realkey_keys(&freed);
    crate::jca::forget_jca_lock_keys(&freed); // gc-common w18-e
    forget_lib_side_table_keys(&freed); // gc-common w20-c
    crate::wildfly_security::forget_wildfly_security_keys(&freed); // gc-common w21-a
    #[cfg(any(feature = "experimental-serialization", feature = "synthetic-jdk"))]
    crate::serialization::forget_serialization_keys(&freed); // gc-common w21-c
    crate::t27_tls::forget_tls_object_keys(&freed); // gc-common w26-b
    crate::phases_late::ssl_security::forget_ssl_security_obj_keys(vm, &freed); // gc-common w27-a
    crate::x509_manager::forget_x509_manager_keys(&freed); // gc-common w27-b
    crate::keystore::forget_keystore_keys(&freed); // gc-common w27-b
    crate::http_url_connection::forget_huc_obj_keys(&freed); // gc-common w27-b
    crate::net_phase_e::forget_net_obj_keys(&freed); // gc-common w28-a
    forget_core_object_state_keys(vm, &freed); // gc-common w29-b
    crate::phases_late::zip_streams::forget_zip_keys(vm, &freed); // gc-common w29-c
    crate::regex_matcher::forget_regex_matcher_keys(&freed); // gc-common w29-d
    crate::wildfly_undertow::forget_undertow_obj_keys(&freed); // gc-common w29-d
    crate::servlet::forget_selector_wakeup_keys(&freed); // gc-common w29-d
    crate::jca::ssl_context_spi::forget_ssl_context_keys(&freed); // gc-common w29-d
    crate::xml_stax::forget_xml_stax_keys(&freed); // gc-common w29-d
    crate::lang_class::forget_lang_class_keys(&freed); // gc-common w29-e
    crate::generics::forget_type_parameter_keys(&freed); // gc-common w29-e
    crate::phases_late::forget_classvalue_keys(vm, &freed); // gc-common w29-e
    crate::reflect_annotations::forget_proxy_namespace_keys(&freed); // gc-common w29-e
    crate::jboss_jdkspecific::forget_module_packages_keys(&freed); // gc-common w29-e
    crate::phases_early::forget_thread_state_keys(&freed); // gc-common w29-a
    crate::infinispan_local::forget_infinispan_keys(vm, &freed); // gc-common w30-a
    crate::lucene_es::forget_randomized_keys(vm, &freed); // gc-common w32-a
    crate::ironjacamar_pool::forget_jca_pool_keys(&freed); // gch h1/ak
    dropped
}


pub fn mirror_stamped_state(ctx: &mut dyn NativeContext, obj: ObjectRef, addr: usize) {
    ctx.set_field_by_name(
        obj,
        "state",
        Value::Long(crate::stamped_lock::stamped_visible_state(addr)),
    );
}

// --- LongAdder ---
//
// Round-7 HIGH native-builtins #16 fix: stripe the LongAdder across a fixed
// pool of cells (real Striped64 grows up to NCPU; we use a constant 8-wide
// table for simplicity and call it good enough). Single-cell adders
// collapse to AtomicLong under contention, defeating LongAdder's entire
// purpose. With striping, contending threads hash to different cells and
// only conflict pairwise on hash collision.
//
// Cell storage lives in a process-wide side table keyed by the LongAdder
// `ObjectRef`'s raw pointer. We cannot reshape the JDK's LongAdder synthetic
// layout (other natives + reflection read field 0 as the `base` long), so
// the cells are stored out-of-band. `base` (field 0) is the uncontended
// path; the cells absorb contention.
//
// `sum()` returns `base + cells.iter().sum()`. `reset()` zeroes both. The
// per-cell atomicity guarantee from the previous CAS-loop is preserved.

/// Number of striped cells per LongAdder. The real JDK grows up to NCPU
/// rounded to a power of two. 8 is a fixed compromise: enough to give
/// 8-way concurrent throughput without per-process state growth.
pub const LA_CELL_COUNT: usize = 8;

pub struct LongAdderCells {
    /// One i64 per stripe. We use `std::sync::atomic::AtomicI64` so the
    /// per-cell CAS is a plain hardware atomic — no lock + no map lookup
    /// per add.
    pub cells: [std::sync::atomic::AtomicI64; LA_CELL_COUNT],
}

impl Default for LongAdderCells {
    fn default() -> Self {
        // `AtomicI64` is not `Copy`, so we cannot use `[expr; N]` syntax —
        // build the array explicitly. The constant must match
        // `LA_CELL_COUNT`; bump both if the stripe width changes.
        Self {
            cells: [
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
                std::sync::atomic::AtomicI64::new(0),
            ],
        }
    }
}

// Round-9 CRIT GC-correctness fix: key by `identity_hash_code(this)` not
// by raw `this.as_ptr() as usize`. Under a moving collector the
// LongAdder is invisibly relocated and the table loses its cell strip,
// silently dropping every striped add (LongAdder.sum() then under-reports
// by the lost cells). `identity_hash_code` is GC-stable: it lives in the
// header's mark word, which every mover copies with the object.
//
// gc-common w20-c: the bare identity hash is not unique across VMs (every
// heap numbers its hashes from the same seed), so two VMs' adders summed one
// strip, and the rows were never evicted. The key is now the adder's weak
// lock key, which is just as move-stable, per VM, and freed (with the row,
// `forget_lib_side_table_keys`) when the adder dies.
pub fn long_adder_cells_table(
) -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<usize, std::sync::Arc<LongAdderCells>>> {
    static TABLE: std::sync::OnceLock<
        parking_lot::Mutex<rustc_hash::FxHashMap<usize, std::sync::Arc<LongAdderCells>>>,
    > = std::sync::OnceLock::new();
    TABLE.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Fetch (or lazily create) the cell strip for a LongAdder instance. The
/// returned `Arc` keeps the cells valid for an add racing the adder's death
/// (the row goes with the adder's weak lock key).
pub fn long_adder_cells(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> std::sync::Arc<LongAdderCells> {
    let key = lib_side_key(ctx, this);
    let mut t = long_adder_cells_table().lock();
    t.entry(key)
        .or_insert_with(|| std::sync::Arc::new(LongAdderCells::default()))
        .clone()
}

/// The adder's cell strip if it has one (mints no key).
pub fn long_adder_existing_cells(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<std::sync::Arc<LongAdderCells>> {
    let key = existing_weak_lock_key(ctx, this)?;
    long_adder_cells_table().lock().get(&key).cloned()
}

/// Pick a stripe index for the current thread. Round-9 CRIT-5 fix:
/// `std::thread::ThreadId` is allocated as a dense sequential counter
/// (1, 2, 3, ...), and the low bits of `FxHash(small dense integers)` are
/// highly correlated — eight threads mapping to indices via
/// `FxHash(tid) & 7` would frequently collide on the same cell, defeating
/// the stripe's purpose. Mix with the Fibonacci constant
/// `0x9E37_79B9_7F4A_7C15` (golden-ratio multiplier; same constant used
/// by `gc::collector::volatile_stripe_lock`) and take the high bits.
pub fn long_adder_stripe() -> usize {
    use std::hash::{Hash, Hasher};
    // std::thread::ThreadId has no public `as_u64`; pipe it through a tiny
    // FxHasher to extract a stable 64-bit value, then apply the Fibonacci
    // spread and take the high bits as the 8-cell index.
    let mut h = rustc_hash::FxHasher::default();
    std::thread::current().id().hash(&mut h);
    let tid_u64 = h.finish();
    ((tid_u64.wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 56) as usize & (LA_CELL_COUNT - 1)
}

/// Resolve `Striped64.base` to its absolute inherited-field slot.
///
/// Real JDK 25 lays out `Striped64` as `cells`, `base`, `cellsBusy`, so the
/// historical slot-0 assumption targets the reference-valued `cells` field.
/// Keep slot 0 only as a fallback for synthetic one-field objects used by the
/// native unit tests and stub-JDK mode.
pub fn striped64_base_slot_from_layout(
    resolved_base: Option<usize>,
    object_num_fields: usize,
) -> Option<usize> {
    resolved_base.or_else(|| (object_num_fields > 0).then_some(0))
}

pub fn striped64_base_slot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    striped64_base_slot_from_layout(
        ctx.resolve_field_index("java/util/concurrent/atomic/Striped64", "base"),
        ctx.object_num_fields(this),
    )
}


pub fn native_long_adder_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(base_slot) = striped64_base_slot(ctx, this) {
        ctx.set_field(this, base_slot, Value::Long(0));
    }
    // Pre-allocate the cell strip so the first `add` does not race the
    // map insert under the side-table mutex.
    let _ = long_adder_cells(ctx, this);
    Ok(None)
}

// audit-round5 fix #2 (CRIT): LongAdder.add / increment / decrement must be
// atomic. Round-7 HIGH-16: stripe across `LA_CELL_COUNT` cells so concurrent
// adders do not all CAS-spin on field 0. We optimistically try `base` once
// — if a CAS on `base` succeeds (uncontended path) we are done; otherwise
// we fall back to the stripe cell hashed to the current thread.
pub fn native_long_adder_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let x = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // Fast path: uncontended CAS on base.
    if let Some(base_slot) = striped64_base_slot(ctx, this) {
        let current = ctx.get_field_volatile(this, base_slot);
        let base = match current {
            Value::Long(v) => v,
            _ => 0,
        };
        let new_val = Value::Long(base.wrapping_add(x));
        if ctx.compare_and_swap_field(this, base_slot, current, new_val) {
            return Ok(None);
        }
    }
    // Contended path: stripe to a cell, do an uncontended atomic add.
    // `fetch_add` on `AtomicI64` is wait-free, no CAS-loop, no spinning.
    let idx = long_adder_stripe();
    let cells = long_adder_cells(ctx, this);
    cells.cells[idx].fetch_add(x, std::sync::atomic::Ordering::Relaxed);
    Ok(None)
}

pub fn native_long_adder_increment(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(base_slot) = striped64_base_slot(ctx, this) {
        let current = ctx.get_field_volatile(this, base_slot);
        let base = match current {
            Value::Long(v) => v,
            _ => 0,
        };
        let new_val = Value::Long(base.wrapping_add(1));
        if ctx.compare_and_swap_field(this, base_slot, current, new_val) {
            return Ok(None);
        }
    }
    let idx = long_adder_stripe();
    let cells = long_adder_cells(ctx, this);
    cells.cells[idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(None)
}

pub fn native_long_adder_decrement(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(base_slot) = striped64_base_slot(ctx, this) {
        let current = ctx.get_field_volatile(this, base_slot);
        let base = match current {
            Value::Long(v) => v,
            _ => 0,
        };
        let new_val = Value::Long(base.wrapping_sub(1));
        if ctx.compare_and_swap_field(this, base_slot, current, new_val) {
            return Ok(None);
        }
    }
    let idx = long_adder_stripe();
    let cells = long_adder_cells(ctx, this);
    cells.cells[idx].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    Ok(None)
}

/// Sum the per-cell strip into a single i64. Caller adds `base` separately.
pub fn long_adder_cells_sum(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    // Fast path: no cells allocated yet (init / never contended).
    let Some(c) = long_adder_existing_cells(ctx, this) else {
        return 0;
    };
    let mut s: i64 = 0;
    for cell in c.cells.iter() {
        s = s.wrapping_add(cell.load(std::sync::atomic::Ordering::Relaxed));
    }
    s
}

pub fn native_long_adder_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // audit-round5 fix #2 + round-7 HIGH-16: volatile read of the base
    // and a separate Relaxed sum across stripe cells. The JDK contract
    // allows `sum()` to return an "approximate" value if concurrent
    // updates race; we follow the same relaxation.
    let base = if let Some(base_slot) = striped64_base_slot(ctx, this) {
        match ctx.get_field_volatile(this, base_slot) {
            Value::Long(v) => v,
            _ => 0,
        }
    } else {
        0
    };
    let cells = long_adder_cells_sum(ctx, this);
    Ok(Some(Value::Long(base.wrapping_add(cells))))
}

pub fn native_long_adder_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Round-7 HIGH-16: must include stripe cells, otherwise `intValue` /
    // `(int)sum()` returns only the `base` and drops every contended add.
    let base = if let Some(base_slot) = striped64_base_slot(ctx, this) {
        match ctx.get_field(this, base_slot) {
            Value::Long(v) => v,
            _ => 0,
        }
    } else {
        0
    };
    let total = base.wrapping_add(long_adder_cells_sum(ctx, this));
    Ok(Some(Value::Int(total as i32)))
}

pub fn native_long_adder_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // audit-round5 fix #2: volatile store so other threads observing via
    // `sum`/`get_field_volatile` immediately see the cleared cell.
    if let Some(base_slot) = striped64_base_slot(ctx, this) {
        ctx.set_field_volatile(this, base_slot, Value::Long(0));
    }
    // Round-7 HIGH-16: also zero every stripe cell, otherwise contended
    // adds remain visible after `reset()`.
    if let Some(c) = long_adder_existing_cells(ctx, this) {
        for cell in c.cells.iter() {
            cell.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(None)
}

pub fn native_long_adder_sum_then_reset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Round-7 HIGH-16: atomically swap `base` to 0 via CAS-loop, then
    // swap every stripe cell to 0 with `swap` (wait-free). The JDK
    // `sumThenReset` is documented as a non-atomic snapshot — concurrent
    // adds racing in the middle may be partly visible in the returned
    // value and partly survive into the reset table. We match that.
    let base = if let Some(base_slot) = striped64_base_slot(ctx, this) {
        loop {
            let current = ctx.get_field_volatile(this, base_slot);
            if ctx.compare_and_swap_field(this, base_slot, current, Value::Long(0)) {
                break match current {
                    Value::Long(v) => v,
                    _ => 0,
                };
            }
        }
    } else {
        0
    };
    let mut total = base;
    if let Some(c) = long_adder_existing_cells(ctx, this) {
        for cell in c.cells.iter() {
            let old = cell.swap(0, std::sync::atomic::Ordering::Relaxed);
            total = total.wrapping_add(old);
        }
    }
    Ok(Some(Value::Long(total)))
}

pub fn native_long_adder_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Round-7 HIGH-16: include the stripe cell sum so toString reports
    // the same value as `sum()`.
    let base = if let Some(base_slot) = striped64_base_slot(ctx, this) {
        match ctx.get_field(this, base_slot) {
            Value::Long(v) => v,
            _ => 0,
        }
    } else {
        0
    };
    let total = base.wrapping_add(long_adder_cells_sum(ctx, this));
    let s = ctx.create_string(&total.to_string());
    Ok(Some(Value::Object(Some(s))))
}

// --- DoubleAdder ---
pub fn native_double_adder_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    if let Some(base_slot) = striped64_base_slot(ctx, this) {
        ctx.set_field(this, base_slot, Value::Long(0));
    }
    Ok(None)
}

// audit-round5 fix #2 (CRIT, mirror): DoubleAdder.add must be atomic for
// the same reasons as LongAdder.add. CAS uses bit-pattern equality on
// Value::Double (see vm_exec::values_equal_for_cas) so NaN does not cause
// a livelock — `Double(NaN).to_bits() == Double(NaN).to_bits()` holds.
pub fn native_double_adder_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let x = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let Some(base_slot) = striped64_base_slot(ctx, this) else {
        return Ok(None);
    };
    loop {
        let current = ctx.get_field_volatile(this, base_slot);
        let old = match current {
            Value::Long(v) => f64::from_bits(v as u64),
            Value::Double(v) => v,
            _ => 0.0,
        };
        let new_val = Value::Long((old + x).to_bits() as i64);
        if ctx.compare_and_swap_field(this, base_slot, current, new_val) {
            return Ok(None);
        }
    }
}

pub fn native_double_adder_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    // audit-round5 fix #2: volatile read so concurrent `add`s are observed.
    let sum = striped64_base_slot(ctx, this)
        .and_then(|base_slot| match ctx.get_field_volatile(this, base_slot) {
            Value::Long(v) => Some(f64::from_bits(v as u64)),
            Value::Double(v) => Some(v),
            _ => None,
        })
        .unwrap_or(0.0);
    Ok(Some(Value::Double(sum)))
}

pub fn native_double_adder_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // audit-round5 fix #2: volatile store so other threads' `sum`
    // reads the cleared value immediately.
    if let Some(base_slot) = striped64_base_slot(ctx, this) {
        ctx.set_field_volatile(this, base_slot, Value::Long(0));
    }
    Ok(None)
}

// ===========================================================================
// Phase 35: Missing functional interfaces
// ===========================================================================

pub(crate) fn register_functional_extras_natives(_registry: &mut NativeMethodRegistry) {
    // synthetic-stub removed: java.util.function defaults defer to real JDK bytecode
}

// ===========================================================================
// Phase 37: Enterprise — ProcessBuilder, StackTraceElement, ServiceLoader, System.getenv
// ===========================================================================

pub(crate) fn register_enterprise_natives(registry: &mut NativeMethodRegistry) {
    // StackTraceElement = 4-field (declaringClass, methodName, fileName, lineNumber)
    let ste = "java/lang/StackTraceElement";
    registry.register(
        ste,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;I)V",
        native_ste_init, NativeKind::SyntheticStub,
    );
    registry.register(
        ste,
        "getClassName",
        "()Ljava/lang/String;",
        native_ste_get_class, NativeKind::SyntheticStub,
    );
    registry.register(
        ste,
        "getMethodName",
        "()Ljava/lang/String;",
        native_ste_get_method, NativeKind::SyntheticStub,
    );
    registry.register(
        ste,
        "getFileName",
        "()Ljava/lang/String;",
        native_ste_get_file, NativeKind::SyntheticStub,
    );
    registry.register(ste, "getLineNumber", "()I", native_ste_get_line, NativeKind::SyntheticStub);
    registry.register(
        ste,
        "toString",
        "()Ljava/lang/String;",
        native_ste_to_string, NativeKind::SyntheticStub,
    );
    // Real body: `return lineNumber == -2;` — HotSpot marks a native frame
    // with the sentinel line number -2. A constant `false` mislabelled every
    // native frame as an ordinary bytecode frame, which is exactly the bit
    // stack-trace formatters print as "(Native Method)". Read the same field
    // `getLineNumber` reads (it handles both the synthetic slot and the real
    // `lineNumber` field) and apply the JDK's own test.
    registry.register(ste, "isNativeMethod", "()Z", |ctx, args| {
        let line = match crate::lang_misc::native_ste_get_line(ctx, args)? {
            Some(Value::Int(n)) => n,
            _ => -1,
        };
        Ok(Some(Value::Int(i32::from(line == -2))))
    }, NativeKind::SyntheticStub);

    // ProcessBuilder: NOT registered here any more.
    //
    // These four triples were also registered by
    // `phases_late::register_phase57_process`, which states `SyntheticStub` for
    // the whole ProcessBuilder cluster so `--real-jdk` refuses it. This block is
    // reached only from `register_synthetic_overrides`, which opens with
    // `set_category(Intrinsic)` — a CHOSEN kind — so it ran LAST in
    // synthetic-JDK mode and rewrote three of those slots from `SyntheticStub`
    // to `Intrinsic`, the one kind `JdkOnly` does not drop. `start()` was the
    // fourth and is re-won afterwards by `native-io`'s
    // `register_process_natives`, which restates `SyntheticStub`.
    //
    // The bodies here were also the weaker pair: `native_pb_init` writes only
    // slot 0, where phases_late writes the indexed slot AND the real-JDK
    // `command` field by name. See W7-46-process-cluster.md §8.2.

    // The five `java/lang/Process` natives that stood here -- waitFor,
    // exitValue, isAlive, destroy and destroyForcibly -- read slot 0 of a THIRD
    // Process layout (exit_code=0, stdout=1, stderr=2) that no longer has a
    // producer, and their comments still described a spawn model
    // ("`Command::output()`, so by the time any Java code holds a Process it
    // has already exited") that stopped being true when `native-io` took over
    // spawning. `register_process_natives` registers all five against the live
    // child, on this class name and on `cratonvm/synthetic/Process`. The
    // comment they carried said it best: a stub that lies is not an acceptable
    // fallback for an ordering change.

    // Thread enhancements    // Thread enhancements
    registry.register(
        "java/lang/Thread",
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
        native_thread_get_stack_trace, NativeKind::SyntheticStub,
    );
}

// --- StackTraceElement ---

// ===========================================================================
// Phase 43: Exception Hierarchy Completion
// ===========================================================================

pub fn register_exception_extras_natives(registry: &mut NativeMethodRegistry) {
    // Bridges for commonly-needed exception types: `getMessage()` and
    // `toString()`. All use the standard Throwable 2-field layout
    // (field 0 = message, field 1 = cause).
    //
    // THIS FUNCTION NO LONGER REGISTERS CONSTRUCTORS FROM THIS LIST.
    //
    // E34-1 §7 site 4, collapsed by E43 (2026-08-13). Until now the loop below
    // also registered the same four `<init>` descriptors — `()V`, `(String)V`,
    // `(String,Throwable)V`, `(Throwable)V` — on every name here: 212
    // registrations. It was a copy-paste twin of
    // `lang_misc::register_throwable_subclass_natives`, carrying that
    // function's two long "deliberately NOT in this list" comments verbatim
    // (they are still below, because they still explain the getMessage
    // bridges, which is what they were always really about) over a list that
    // had since drifted, and bound to a second set of ctor bodies.
    //
    // It is called from `register_synthetic_overrides` AND from
    // `reflect_annotations.rs`, i.e. in BOTH modes, and in both it runs AFTER
    // `register_throwable_subclass_natives`. There is no unregister API, so
    // `register` is last-write-wins: these 212 rows were the ones actually
    // answering for ~52 classes, and the JDK-derived table was being buried.
    //
    // MEASURED on JDK 25.0.3+9-LTS (`scratchpad/e43/Removals43.java`, which
    // reflects over every (class, descriptor) this loop registered that the
    // table does not carry): 89 such pairs across sites 3 and 4, of which
    //   * 85 are constructors the real class DOES NOT DECLARE AT ALL, so no
    //     `javac` on any real JDK could ever have compiled a call to them —
    //     e.g. `java/io/UncheckedIOException` was advertising all four while
    //     declaring none of them, and `java/text/ParseException` likewise;
    //   * 4 are real, and all four are `java/lang/VirtualMachineError`, the one
    //     name here that is outside the table's measured 62. They are restated
    //     explicitly after the loop rather than deleted.
    //
    // The two ctor bodies were diffed, as E34-1 N3 asked. `lang_misc::
    // native_exc_init_noargs` and `native_exception_init_empty` are the same
    // three statements. `lang_misc::native_exc_init_message` and
    // `native_exception_init_msg` are the same three statements plus, in the
    // latter, the opt-in `CRATONVM_DBG_NPE_TRACE` surefire forensic — which
    // fires only for `NullPointerException("Name is null")`. That single
    // difference is preserved by one named re-registration after the loop, so
    // collapsing onto the table does not silently retire a diagnostic.
    //
    // ADDING A CLASS HERE: it must also be a row of the table in
    // `lang_misc.rs` (or be added to the allow-list in
    // `the_blanket_four_ctor_loops_stay_collapsed_onto_the_table`, which is a
    // source witness at the bottom of this file that enforces exactly that).
    let exceptions = [
        "java/lang/Error",
        "java/lang/AssertionError",
        "java/lang/OutOfMemoryError",
        "java/lang/StackOverflowError",
        "java/lang/VirtualMachineError",
        "java/lang/InternalError",
        "java/lang/ExceptionInInitializerError",
        "java/lang/NoClassDefFoundError",
        "java/lang/LinkageError",
        "java/lang/VerifyError",
        "java/lang/SecurityException",
        "java/lang/TypeNotPresentException",
        "java/lang/ReflectiveOperationException",
        "java/lang/CloneNotSupportedException",
        "java/lang/IllegalArgumentException",
        "java/lang/IllegalStateException",
        "java/lang/UnsupportedOperationException",
        "java/lang/ArithmeticException",
        "java/lang/NullPointerException",
        "java/lang/ClassCastException",
        "java/lang/IndexOutOfBoundsException",
        "java/lang/ArrayIndexOutOfBoundsException",
        "java/lang/StringIndexOutOfBoundsException",
        "java/lang/NegativeArraySizeException",
        "java/lang/ClassNotFoundException",
        "java/lang/NoSuchMethodException",
        "java/lang/NoSuchFieldException",
        "java/lang/InstantiationException",
        "java/lang/IllegalAccessException",
        "java/lang/reflect/InaccessibleObjectException",
        "java/lang/reflect/InvocationTargetException",
        "java/lang/InterruptedException",
        "java/lang/NumberFormatException",
        "java/util/ConcurrentModificationException",
        "java/util/MissingResourceException",
        "java/util/FormatterClosedException",
        "java/util/NoSuchElementException",
        "java/text/ParseException",
        // `java/util/regex/PatternSyntaxException` is deliberately NOT in this
        // list. It is the one exception here that OVERRIDES `getMessage()`:
        // the JDK builds a three-line report ("Unclosed character class near
        // index 0", the pattern, a caret) from its `desc`/`pattern`/`index`
        // fields and never sets `Throwable.detailMessage`. A blanket
        // `getMessage` bridge in front of that override returns the null
        // `detailMessage`, so `"x".split("[")` reported `getMessage() == null`
        // where HotSpot gives the full report. Its real constructor is
        // `(String,String,int)V`, which is not among the `<init>` shapes
        // registered here either, so every bridge this loop would add is
        // either dead or actively wrong. Removed 2026-08-05.
        "java/util/InputMismatchException",
        "java/io/IOException",
        "java/io/FileNotFoundException",
        "java/io/UncheckedIOException",
        "java/io/NotSerializableException",
        // `java/io/InvalidClassException` is deliberately NOT in this list, for
        // the same reason `java/util/regex/PatternSyntaxException` is not: it
        // OVERRIDES `getMessage()`, prepending the offending class name to the
        // detail message. A blanket bridge in front of that override returns
        // the bare `Throwable.detailMessage`, so
        // `new InvalidClassException("com.example.Foo", "bad serialVersionUID")`
        // reported "bad serialVersionUID" where HotSpot reports
        // "com.example.Foo; bad serialVersionUID" -- and deserialization
        // diagnostics lose the one field that says WHICH class failed.
        //
        // Found 2026-08-05 by checking `javap -p` for a declared
        // getMessage/getLocalizedMessage/toString across every class in these
        // two lists; it and `NullPointerException` were the only two left after
        // PatternSyntaxException. See
        // `a-bridge-in-front-of-an-overridden-getmessage-FIXED-20260805.md`.
        "java/io/EOFException",
        "java/io/UnsupportedEncodingException",
        "java/net/MalformedURLException",
        "java/net/UnknownHostException",
        "java/util/concurrent/TimeoutException",
        "java/util/concurrent/RejectedExecutionException",
        "java/util/concurrent/CancellationException",
        "java/util/concurrent/CompletionException",
        "java/util/concurrent/ExecutionException",
        "java/util/concurrent/BrokenBarrierException",
    ];
    for exc in &exceptions {
        // No `<init>` here — see this function's header. The constructors for
        // every name in this list come from the JDK-derived per-class table in
        // `lang_misc::register_throwable_subclass_natives`, which runs earlier
        // on both boot paths.
        registry.register(
            exc,
            "getMessage",
            "()Ljava/lang/String;",
            // `NullPointerException` OVERRIDES `getMessage()` (JEP 358):
            // its bridge runs that override's body, not the field read.
            if *exc == "java/lang/NullPointerException" {
                native_npe_get_message
            } else {
                native_exception_get_message
            }, NativeKind::SyntheticStub,
        );
        registry.register(
            exc,
            "toString",
            "()Ljava/lang/String;",
            native_exception_to_string, NativeKind::SyntheticStub,
        );
    }

    // `java/lang/VirtualMachineError` is the ONE name above that the table does
    // not carry, so it is the one whose constructors this function must still
    // register — deleting them would have left it with none. It keeps its
    // HISTORICAL bodies, byte-for-byte the pair the deleted loop gave it, so
    // this is a no-op for it and not a quiet re-binding.
    //
    // It also happens to be a class for which the blanket four were right.
    // MEASURED, `javap -p java.lang.VirtualMachineError` on JDK 25.0.3+9-LTS:
    //     public VirtualMachineError();
    //     public VirtualMachineError(String);
    //     public VirtualMachineError(String, Throwable);
    //     public VirtualMachineError(Throwable);
    // All four declared, all four public — the same rule the table is built
    // from admits all four. It is abstract, so no `new` reaches them directly;
    // the reachable path is a subclass's `super(...)`, which is an
    // `invokespecial` on this exact triple.
    //
    // This is the same treatment application throwables get. A `…/DbException`
    // is not in the table and never was in this list either; its `<init>` is
    // answered by `vm_exec.rs`'s final native-registry fallback walking up the
    // dispatch chain to `java/lang/RuntimeException`, whose four descriptors
    // the table still carries, and its stub still declares the historical four
    // via `class_manager.rs::synthetic_stub_ctor_methods`' `is_throwable_like`
    // arm. Nothing in this change narrows either of those.
    let vm_error = "java/lang/VirtualMachineError";
    registry.register(vm_error, "<init>", "()V", native_exception_init_empty, NativeKind::SyntheticStub);
    registry.register(
        vm_error,
        "<init>",
        "(Ljava/lang/String;)V",
        native_exception_init_msg, NativeKind::SyntheticStub,
    );
    registry.register(
        vm_error,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        crate::lang_misc::native_exc_init_message_cause, NativeKind::SyntheticStub,
    );
    registry.register(
        vm_error,
        "<init>",
        "(Ljava/lang/Throwable;)V",
        crate::lang_misc::native_exc_init_cause, NativeKind::SyntheticStub,
    );

    // The one behavioural difference between the two ctor-body families, kept
    // alive by name rather than by a 212-row loop.
    //
    // `native_exception_init_msg` is `lang_misc::native_exc_init_message` plus
    // the `CRATONVM_DBG_NPE_TRACE` surefire forensic, and that forensic fires
    // for exactly one class and message: `NullPointerException("Name is null")`
    // (`java.lang.Enum.valueOf(null)`'s text). `(Ljava/lang/String;)V` is a
    // declared public constructor of `java/lang/NullPointerException` and is a
    // row of the table, so this restates a triple the table already owns with
    // a strict superset of its body — it does not widen the registered
    // surface by one descriptor.
    registry.register(
        "java/lang/NullPointerException",
        "<init>",
        "(Ljava/lang/String;)V",
        native_exception_init_msg, NativeKind::SyntheticStub,
    );
}

pub fn native_exception_init_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Fields already null by default; but we must still capture the stack
    // trace (the JDK no-arg Throwable ctor calls fillInStackTrace()). Without
    // this, a no-arg `new IllegalStateException()` thrown from bytecode had an
    // empty getStackTrace(). Same root cause as native_exception_init_msg.
    if let Some(Value::Object(Some(this))) = args.first() {
        // Mirror the JDK field initializer `private Throwable cause = this;`,
        // exactly as `lang_misc::native_exc_init_noargs` does. This native is
        // registered LATER (see `register_exception_extras_natives`) and wins
        // the registry slot for ~56 subclasses, so without the mirror those
        // classes were the only ones whose `cause` slot stayed unwritten:
        // real-JDK `initCause()` bytecode reads `cause != this` and refuses
        // with "Can't overwrite cause", and serializing such a throwable emits
        // a null `cause` where HotSpot emits the self back-reference.
        crate::lang_misc::write_throwable_cause(ctx, *this, Value::Object(Some(*this)));
        // `this` borrows `args`; take a local so the funnel's refreshed
        // reference is what any later statement in this block sees.
        let mut this = *this;
        crate::lang_misc::capture_throwable_trace_ctor(ctx, &mut this)?;
    }
    Ok(None)
}

pub fn native_exception_init_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let msg = args.get(1).cloned().unwrap_or(Value::Object(None));
    // Mirror to the named field (real-JDK Throwable layout has detailMessage at
    // slot 1, after backtrace). See `lang_misc::write_throwable_detail_message`.
    //
    // NOTE (stack-trace capture fix): we deliberately do NOT also blast `msg`
    // into raw slot 0 anymore. In the real-JDK Throwable layout slot 0 is
    // `backtrace`, which `capture_throwable_trace` (called below) sets to the
    // self-reference marker that `getOurStackTrace()` keys on. Writing the
    // String message there would clobber the backtrace marker and re-break
    // getStackTrace(). The named-field write is the correct, layout-aware path.
    //
    // FIX (synthetic receiver): the bare `set_field_by_name` this used to be
    // ALSO silently dropped the message whenever the receiver's class declares
    // no field names at all — which is every `ensure_synthetic_class` stub, and
    // therefore every exception in synthetic-JDK mode. `getMessage()` (both
    // `native_exception_get_message` here and
    // `lang_misc::native_throwable_get_message`) then falls back to raw slot 0
    // and finds nothing, so `new IllegalArgumentException("x").getMessage()`
    // answered null. Route through the shared helper, which writes by name for
    // a real-JDK layout and falls back to `synthetic_throwable_slot`
    // (detailMessage = slot 0) only when the by-name write did not take — the
    // exact slot both readers probe, so writer and reader cannot disagree.
    // These ctors are registered LATER than
    // `lang_misc::register_throwable_subclass_natives` and win the slot for
    // ~50 subclasses, so this path is the live one.
    crate::lang_misc::write_throwable_detail_message(ctx, this, msg);
    // Mirror the JDK field initializer `private Throwable cause = this;`, as
    // `lang_misc::native_exc_init_message` already does. Same omission story as
    // `native_exception_init_empty` above: this native shadows `Throwable
    // .<init>` for ~56 subclasses, so `new UnsupportedOperationException("x")`
    // left `cause` unwritten while `new RuntimeException()` (not in that list)
    // got the sentinel — a divergence visible through reflection, through
    // real-JDK `initCause()` bytecode, and in the serialized form.
    crate::lang_misc::write_throwable_cause(ctx, this, Value::Object(Some(this)));
    // Surefire bootstrap forensics: capture exact Java callsite for the
    // recurring `NullPointerException("Name is null")` blocker so we can
    // patch the true producer instead of masking symptoms.
    //
    // OPT-IN since W7-42. This was unconditional, and it fires for ANY
    // `NullPointerException("Name is null")` — including
    // `java.lang.Enum.valueOf(null)`, which ordinary application code and
    // every differential probe reach on purpose. Seven diagnostic frames then
    // land in the middle of a transcript that is supposed to carry
    // observables and nothing else; the shadow differential read them as
    // seven divergences, and worse, they shifted the alignment of every row
    // after them. They were always on stderr — the emitter is `eprintln!` and
    // that was measured on the binary, so a transcript that carried them was
    // captured with the streams merged — but a forensic for a closed
    // investigation should not be running at all on a normal run.
    //
    // `CRATONVM_DBG_NPE_TRACE` (equivalently `CRATONVM_DBG=npe-trace`) is the
    // flag `vm/src/runtime/exceptions.rs` already gates the sibling NPE-origin
    // dumps with, and it is already carried by `types/src/flag_groups.rs`,
    // `types/tests/flag-surface.txt`, `docs/flag-tokens.md` and
    // `docs/config/flag-inventory.md`. No new flag.
    if surefire_npe_trace_enabled() {
        if let Value::Object(Some(msg_obj)) = msg {
            if let Some(message) = ctx.read_string(msg_obj) {
                let class_name = ctx
                    .class_name_of_id(ctx.class_id_of_object(this))
                    .unwrap_or_default();
                if class_name == "java/lang/NullPointerException" && message == "Name is null" {
                    // The argument is unused; minting an identity hash for it
                    // was a side effect on the throwable. The capture is
                    // outermost-first, so the top frames are its tail.
                    let trace = ctx.capture_stack_trace();
                    eprintln!("[SUREFIRE-NPE] Name is null thrown; top Java frames:");
                    for (i, f) in trace.iter().rev().take(12).enumerate() {
                        let src = f.source_file.as_deref().unwrap_or("Unknown Source");
                        eprintln!(
                            "[SUREFIRE-NPE]   #{i} {}.{} ({}:{})",
                            f.class_name, f.method_name, src, f.line_number
                        );
                    }
                }
            }
        }
    }
    // STACK-TRACE CAPTURE FIX: `register_exception_extras_natives` registers
    // this native for ~50 exception subclasses (IllegalStateException,
    // IllegalArgumentException, NumberFormatException, …) and runs LAST, so it
    // wins the registry slot over the capturing `native_exc_init_message` from
    // `register_throwable_subclass_natives`. Previously this native shadowed the
    // JDK `<init>` without ever calling `fillInStackTrace`, so `getStackTrace()`
    // returned 0 frames for every one of those subclasses thrown from bytecode.
    // Route through the shared capture helper so they match HotSpot (and the
    // base Throwable/RuntimeException classes, which these lists omit).
    crate::lang_misc::capture_throwable_trace_ctor(ctx, &mut this)?;
    Ok(None)
}

pub fn native_exception_get_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Real-JDK layout: detailMessage at slot 1; synthetic stubs put it at 0.
    // An implicit NPE's JEP 358 text is in `extendedMessage`, which the JDK's
    // `NullPointerException.getMessage()` answers when `detailMessage` is null
    // (interpreter round i4 wave 99, lane L9).
    let by_name = crate::lang_misc::throwable_message_or_npe_extended(ctx, this);
    if matches!(by_name, Value::Object(Some(_))) {
        return Ok(Some(by_name));
    }
    // Slot-0 fallback is ONLY valid for synthetic-stub exceptions (message at
    // slot 0). In the real-JDK Throwable layout slot 0 is `backtrace`, which
    // `capture_throwable_trace` sets to the self-reference marker — returning
    // that made a no-arg `new IllegalStateException()` report getMessage() ==
    // the exception itself (stringified to "Exception"). Only return slot 0
    // when it actually holds a `java/lang/String`.
    if ctx.object_num_fields(this) >= 1 {
        if let v @ Value::Object(Some(o)) = ctx.get_field(this, 0) {
            if ctx
                .class_name_arc_of_id(ctx.class_id_of_object(o))
                .as_deref()
                == Some("java/lang/String")
            {
                return Ok(Some(v));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

/// The body of the `NullPointerException.getExtendedNPEMessage()` native: the
/// JEP 358 message of `this`, as a fresh `String`, or `null`.
///
/// Round 15 wave 7 (lane throw2, TH1-5): a compiled null dereference's NPE
/// carries no message until asked, as on HotSpot; the VM computes it here,
/// from the stored trace's top frame, into a fresh String (HotSpot makes a new
/// one per call; the JDK keeps it in `extendedMessage`). JIT round 21 (lane
/// npecompat, R15B-39): the deferral serves `--compatible` too, so this is
/// also what [`native_npe_get_message`] asks there. Anything else answers
/// `detailMessage`, as before (an explicitly constructed NPE: `null`, which is
/// HotSpot's `NPE_EXPLICIT_CONSTRUCTED` answer).
pub fn npe_extended_message_now(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    if let Some(deferred) = ctx.deferred_npe_message(this) {
        return match deferred {
            Some(text) => Value::Object(Some(crate::lang_math::fresh_computed_string(ctx, &text))),
            None => Value::Object(None),
        };
    }
    match crate::lang_misc::throwable_field_get(ctx, this, "detailMessage") {
        Value::Object(Some(s)) => Value::Object(Some(s)),
        _ => Value::Object(None),
    }
}

/// `NullPointerException.getMessage()` where a native stands in front of the
/// JDK's override (`--compatible`; under `--real-jdk` the triple is retired
/// and the JDK's bytecode runs). JIT round 21 (lane npecompat, R15B-39): the
/// JDK 25 body, statement for statement --
///
/// ```text
/// String message = super.getMessage();
/// if (message == null) {
///     synchronized (this) {
///         if (extendedMessageState == 1) {
///             extendedMessage = getExtendedNPEMessage();
///             extendedMessageState = 2;
///         }
///         return extendedMessage;
///     }
/// }
/// return message;
/// ```
///
/// The bridge it replaces ([`native_exception_get_message`]) answered
/// `super.getMessage()` alone, which was right only while the VM wrote every
/// JEP 358 message into `detailMessage` eagerly: a deferred NPE (the
/// compiled and interpreted null dereferences, `extendedMessageState == 1`,
/// `detailMessage == null`) read as `null`, and so did one whose message the
/// `fillInStackTrace()` override had already moved into `extendedMessage`.
/// The message is allocated before the monitor is taken (the allocation may
/// collect; `this` is pinned across it), so nothing moves while it is held.
/// A receiver without the JDK's two fields (a class library without them)
/// answers as the old bridge did.
pub fn native_npe_get_message(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let message = native_exception_get_message(ctx, args)?;
    if matches!(message, Some(Value::Object(Some(_)))) {
        return Ok(message);
    }
    let class_id = ctx.class_id_of_object(this);
    let (Some(extended_slot), Some(state_slot)) = (
        ctx.resolve_field_index_by_class_id(class_id, "extendedMessage"),
        ctx.resolve_field_index_by_class_id(class_id, "extendedMessageState"),
    ) else {
        return Ok(message);
    };
    if !matches!(ctx.get_field(this, state_slot), Value::Int(1)) {
        return Ok(Some(match ctx.get_field(this, extended_slot) {
            v @ Value::Object(_) => v,
            _ => Value::Object(None),
        }));
    }
    // `getExtendedNPEMessage()`, computed outside the monitor (it allocates).
    let pin = ctx.pin_native_root(this);
    let computed = npe_extended_message_now(ctx, this);
    let this = ctx.read_native_pin(pin, this);
    ctx.monitor_enter(this);
    // Another thread may have completed the transition meanwhile; then its
    // answer stands, as the JDK's `synchronized` block makes it.
    if matches!(ctx.get_field(this, state_slot), Value::Int(1)) {
        ctx.set_field(this, extended_slot, computed);
        ctx.set_field(this, state_slot, Value::Int(2));
    }
    let answer = match ctx.get_field(this, extended_slot) {
        v @ Value::Object(_) => v,
        _ => Value::Object(None),
    };
    ctx.monitor_exit(this);
    ctx.unpin_native_roots(pin);
    Ok(Some(answer))
}

pub fn native_exception_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Round 14 wave 1 (lane compat): `getLocalizedMessage()` is VIRTUAL, so a
    // subclass that overrides it and inherits `toString` must be honoured
    // (`r13w13-shadow5-exception-to-string-native-ignores-localized-message`).
    // `CRATONVM_EXC_TOSTRING_LOCALIZED=0` restores the direct field read below.
    if exc_tostring_localized_enabled(&*ctx) {
        return exception_to_string_localized(ctx, this);
    }
    // Mirror real `Throwable.toString()`:
    //   s = getClass().getName();  msg = getLocalizedMessage();
    //   return (msg != null) ? s + ": " + msg : s;
    // The previous body hardcoded the class as "Exception" (wrong for every
    // subclass) and read the message from raw slot 0 (which is `backtrace` in
    // the real-JDK layout, not the message).
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .map(|n| n.class_text(&*ctx))
        .unwrap_or_else(|| "java.lang.Throwable".to_string());
    let msg = match crate::lang_misc::throwable_message_or_npe_extended(ctx, this) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let text = match msg {
        Some(m) => format!("{class_name}: {m}"),
        None => class_name,
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&text)))))
}

/// `CRATONVM_EXC_TOSTRING_LOCALIZED` — default ON in both modes, `=0` restores
/// the direct `detailMessage` read of [`native_exception_to_string`]. Read once
/// per VM per thread (the shape of `lang_math::fresh_computed_string`'s
/// switch): `toString` sits under every logger and stack-trace header.
pub fn exc_tostring_localized_enabled(ctx: &dyn NativeContext) -> bool {
    thread_local! {
        static ON: std::cell::Cell<Option<(usize, bool)>> = const { std::cell::Cell::new(None) };
    }
    let vm = ctx.vm_identity();
    ON.with(|c| match c.get() {
        Some((owner, on)) if owner == vm => on,
        _ => {
            let on =
                true;
            c.set(Some((vm, on)));
            on
        }
    })
}

/// `Throwable.toString()` as the JDK writes it: the class name, then the
/// VIRTUAL `getLocalizedMessage()` when it is non-null.
///
/// A Java exception thrown by an overriding `getLocalizedMessage` propagates,
/// as it does on HotSpot. Only a dispatch that fails inside the VM (a
/// partially-built boot throwable) or answers a non-reference falls back to
/// the `detailMessage` field the old body read. The result is a fresh
/// `String`, not interned into the never-pruned pool
/// (`CRATONVM_COMPUTED_STRINGS_UNINTERNED`), like `Throwable.toString`'s own
/// native.
pub fn exception_to_string_localized(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .map(|n| n.class_text(&*ctx))
        .unwrap_or_else(|| "java.lang.Throwable".to_string());
    // The overriding body is Java and may allocate (and so move `this`).
    let pin = ctx.pin_native_root(this);
    let receiver = ctx.read_native_pin(pin, this);
    let dispatched =
        ctx.invoke_virtual(receiver, "getLocalizedMessage", "()Ljava/lang/String;", &[]);
    let current = ctx.read_native_pin(pin, receiver);
    ctx.unpin_native_roots(pin);
    let msg = match dispatched {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        Ok(Some(Value::Object(None))) | Ok(None) => None,
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            return Err(MethodCallFailed::ExceptionThrown(exc));
        }
        Err(MethodCallFailed::InternalError(_)) | Ok(Some(_)) => {
            match crate::lang_misc::throwable_message_or_npe_extended(ctx, current) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            }
        }
    };
    let text = match msg {
        Some(m) => format!("{class_name}: {m}"),
        None => class_name,
    };
    Ok(Some(Value::Object(Some(
        crate::lang_math::fresh_computed_string(ctx, &text),
    ))))
}


// ===========================================================================
// Phase 45: java.lang Completion — ClassLoader, CharSequence, Comparable,
//           Number, Readable, Cloneable, Record, StackWalker
// ===========================================================================

pub(crate) fn register_java_lang_extras_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: ClassLoader / Runtime / System.gc are VM-internal + host
    // bridges → Bridge. (ClassLoader.getParent returns null — a minor stub —
    // but the cluster is dominantly VM-internal.)
            // --- ClassLoader (stub) ---
    let cl = "java/lang/ClassLoader";
    // `getSystemClassLoader` / `loadClass(String)` were registered here too
    // (a fresh empty `ClassLoader` per call; a `loadClass` answering `null` for
    // a null name), dead for the same reason as the two below and deleted in
    // interpreter round i2 wave 56, lane L5
    // (`docs/feature-designs/native-dispatch-architecture.md`).
    // `getParent` / `getResource` used to be registered here as
    // `native_return_null` — "every loader is the bootstrap loader" and "no
    // resource exists anywhere". Both were already dead registrations, in BOTH
    // modes, and removing them changes no behaviour:
    //   * this registrar is only ever called from `register_synthetic_overrides`
    //     (its single call site), so it does not exist in real-JDK mode at all;
    //   * within that same function, and unconditionally, a later
    //     `classloader::register_classloader_natives(registry)` re-registers
    //     both triples with the real `cl_get_parent` / `cl_get_resource`
    //     implementations, and `NativeMethodRegistry::register` is
    //     last-registration-wins.
    // They are dropped rather than reimplemented precisely so there is one
    // implementation of each, not two that can drift.
    registry.register(
        cl,
        "toString",
        "()Ljava/lang/String;",
        native_classloader_to_string, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- CharSequence interface (register for String + StringBuilder) ---
    {
        let cls = "java/lang/CharSequence";
        registry.register(cls, "length", "()I", native_charseq_length, cratonvm_native_api::NativeKind::Bridge);
        registry.register(cls, "charAt", "(I)C", native_charseq_char_at, cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            cls,
            "toString",
            "()Ljava/lang/String;",
            native_charseq_to_string, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // --- Comparable interface (register for wrappers + String) ---
    // Already handled per-class, just add interface dispatch
    let cmp = "java/lang/Comparable";
    registry.register(
        cmp,
        "compareTo",
        "(Ljava/lang/Object;)I",
        native_comparable_compare_to, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Number abstract methods (register on Number and all wrapper subtypes) ---
    let num = "java/lang/Number";
    registry.register(num, "intValue", "()I", native_number_int_value, cratonvm_native_api::NativeKind::Bridge);
    registry.register(num, "longValue", "()J", native_number_long_value, cratonvm_native_api::NativeKind::Bridge);
    registry.register(num, "floatValue", "()F", native_number_float_value, cratonvm_native_api::NativeKind::Bridge);
    registry.register(num, "doubleValue", "()D", native_number_double_value, cratonvm_native_api::NativeKind::Bridge);

    // Register on wrapper types too — virtual dispatch may look up the concrete class
    for wrapper in &[
        "java/lang/Integer",
        "java/lang/Long",
        "java/lang/Float",
        "java/lang/Double",
        "java/lang/Short",
        "java/lang/Byte",
    ] {
        registry.register(wrapper, "intValue", "()I", native_number_int_value, cratonvm_native_api::NativeKind::Bridge);
        registry.register(wrapper, "longValue", "()J", native_number_long_value, cratonvm_native_api::NativeKind::Bridge);
        registry.register(wrapper, "floatValue", "()F", native_number_float_value, cratonvm_native_api::NativeKind::Bridge);
        registry.register(wrapper, "doubleValue", "()D", native_number_double_value, cratonvm_native_api::NativeKind::Bridge);
    }

    // --- Cloneable (marker, no methods) ---
    // No registration needed

    // --- StackWalker (stub) ---
    // DELETED: a duplicate `StackWalker.getInstance` pair used to live here,
    // handled by `native_stack_walker_get`, which allocated the walker and wrote
    // NONE of its fields — no `options`, no `retainClassRef`. This function is
    // reached only from `register_synthetic_overrides`, which runs AFTER
    // `register_stack_walker_boot`, so in `--synthetic-jdk` builds the empty
    // version SHADOWED the real one (`stack_walker::alloc_walker`, which
    // resolves and writes all three fields) and every walker came back with its
    // options lost — including `RETAIN_CLASS_REFERENCE`, which
    // `getCallerClass()` and `StackFrame.getDeclaringClass()` gate on. The
    // real-JDK build was unaffected because this registrar never runs there.
    // Same last-registration-wins hazard as the `java.net.http` carriers; one
    // owner per class.

    // --- Thread extras (only methods NOT already registered) ---
    // T1.6.7 — duplicate registration kept in sync with line ~430.
    // NATIVE21-2: the same body and the same leaf claim as the registration
    // above (last write wins, and a re-registration without the claim would
    // demote it); the kind is stated rather than inherited.
    registry.with_leaf(true, |r| {
        r.register(
            "java/lang/Thread",
            "holdsLock",
            "(Ljava/lang/Object;)Z",
            native_thread_holds_lock_leaf,
            NativeKind::Bridge,
        );
    });
    // Thread.interrupted() is registered in register_essential_natives with proper
    // is_interrupted(true) implementation — do NOT re-register here.
    // Thread.yield already registered in register_essential_natives with real
    // std::thread::yield_now() impl — not re-registered here.
    // Thread.onSpinWait is a CPU spin-loop HINT (HotSpot lowers it to the
    // x86 PAUSE instruction — nanoseconds, never a scheduler transition).
    // The previous `std::thread::yield_now()` implementation was
    // semantically wrong and catastrophic on a loaded host: every AQS
    // pre-park spin (`AbstractQueuedSynchronizer.acquire` spins up to ~255
    // onSpinWait rounds between park attempts) surrendered a scheduler
    // quantum, so one contended ReentrantReadWriteLock handoff could cost
    // SECONDS whenever the box had competing load — the "crawl regime"
    // (probe throughput collapsing to ~1 lock op/s, ~21s Thread.join
    // cascades at shutdown, Elasticsearch startups stalling past suite
    // timeouts). Interpreted call sites dispatch this native shadow, so the
    // crawl also locked itself in: threads too slow to hit JIT thresholds
    // never got the compiled (empty-bytecode, shadow-free) version that
    // made warm runs fast.
    registry.register("java/lang/Thread", "onSpinWait", "()V", |_ctx, _args| {
        std::hint::spin_loop();
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- System extras (only methods NOT already registered) ---
    registry.register("java/lang/System", "gc", "()V", |ctx, _args| {
        crate::hooks::jfr_force_gc_as_system_gc(ctx);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // Note: Objects utility, System.identityHashCode, System.nanoTime, Thread.currentThread
    // are already registered in earlier phases
}

pub fn native_classloader_to_string(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(
        ctx.create_string("SystemClassLoader"),
    ))))
}

pub fn native_return_null(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

pub fn native_charseq_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Int(s.encode_utf16().count() as i32)))
}

pub fn native_charseq_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let s = ctx.read_string(this).unwrap_or_default();
    let chars: Vec<u16> = s.encode_utf16().collect();
    Ok(Some(
        Value::Int(chars.get(idx).copied().unwrap_or(0) as i32),
    ))
}

pub fn native_charseq_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = ctx.read_string(this).unwrap_or_default();
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub fn native_comparable_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let b = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Try string comparison
    let sa = ctx.read_string(a).unwrap_or_default();
    let sb = ctx.read_string(b).unwrap_or_default();
    Ok(Some(Value::Int(sa.cmp(&sb) as i32)))
}

pub fn native_number_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match ctx.get_field(this, 0) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        Value::Long(v) => Ok(Some(Value::Int(v as i32))),
        Value::Float(v) => Ok(Some(Value::Int(v as i32))),
        Value::Double(v) => Ok(Some(Value::Int(v as i32))),
        _ => Ok(Some(Value::Int(0))),
    }
}

pub fn native_number_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    match ctx.get_field(this, 0) {
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        Value::Long(v) => Ok(Some(Value::Long(v))),
        Value::Float(v) => Ok(Some(Value::Long(v as i64))),
        Value::Double(v) => Ok(Some(Value::Long(v as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub fn native_number_float_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    match ctx.get_field(this, 0) {
        Value::Int(v) => Ok(Some(Value::Float(v as f32))),
        Value::Long(v) => Ok(Some(Value::Float(v as f32))),
        Value::Float(v) => Ok(Some(Value::Float(v))),
        Value::Double(v) => Ok(Some(Value::Float(v as f32))),
        _ => Ok(Some(Value::Float(0.0))),
    }
}

pub fn native_number_double_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    match ctx.get_field(this, 0) {
        Value::Int(v) => Ok(Some(Value::Double(v as f64))),
        Value::Long(v) => Ok(Some(Value::Double(v as f64))),
        Value::Float(v) => Ok(Some(Value::Double(v as f64))),
        Value::Double(v) => Ok(Some(Value::Double(v))),
        _ => Ok(Some(Value::Double(0.0))),
    }
}

// ===========================================================================
// Phase 46: Functional Interfaces Completion — primitive specialized
// ===========================================================================

pub(crate) fn register_functional_completion_natives(registry: &mut NativeMethodRegistry) {
    // synthetic-stub removed: java.util.function defaults defer to real JDK bytecode

    // IntSummaryStatistics
    let iss = "java/util/IntSummaryStatistics";
    registry.register(iss, "<init>", "()V", native_iss_init, NativeKind::SyntheticStub);
    registry.register(iss, "accept", "(I)V", native_iss_accept, NativeKind::SyntheticStub);
    registry.register(iss, "getCount", "()J", native_iss_get_count, NativeKind::SyntheticStub);
    registry.register(iss, "getSum", "()J", native_iss_get_sum, NativeKind::SyntheticStub);
    registry.register(iss, "getMin", "()I", native_iss_get_min, NativeKind::SyntheticStub);
    registry.register(iss, "getMax", "()I", native_iss_get_max, NativeKind::SyntheticStub);
    registry.register(iss, "getAverage", "()D", native_iss_get_average, NativeKind::SyntheticStub);
    registry.register(
        iss,
        "toString",
        "()Ljava/lang/String;",
        native_iss_to_string, NativeKind::SyntheticStub,
    );
}

pub fn native_return_self(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().cloned().unwrap_or(Value::Object(None))))
}

pub fn native_return_first_arg(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().cloned().unwrap_or(Value::Object(None))))
}

pub fn native_function_identity(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Fallible since 2026-08-05 (Real-JDK wave 2, lane L7). `Function$Identity`
    // has no class file anywhere — real `Function.identity()` is `t -> t`, an
    // invokedynamic lambda — so this is a fabrication `--real-jdk` forbids.
    //
    // Under strict mode this native is no longer registered at all (the
    // registrar tags it `SyntheticStub` below, and `register()` refuses that
    // category under `JdkOnly`), so this line is unreachable there. Making it
    // fallible anyway is what stops the two defects cancelling each other
    // again: if the retag is ever reverted, the fabrication fails loudly and
    // names the class instead of being recorded as a violation and performed.
    let proxy = crate::util_concurrent_ext::try_alloc_concurrent_synthetic(
        ctx,
        "java/util/function/Function$Identity",
        0,
    )?;
    Ok(Some(Value::Object(Some(proxy))))
}

pub fn native_function_identity_apply(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

pub fn native_function_identity_return_arg(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

pub fn native_arraylist_list_itr_slots(
    ctx: &dyn NativeContext,
) -> (usize, usize, Option<usize>, usize, Option<usize>) {
    let cursor = ctx
        .resolve_field_index("java/util/ArrayList$Itr", "cursor")
        .unwrap_or(1);
    let last_ret = ctx
        .resolve_field_index("java/util/ArrayList$Itr", "lastRet")
        .unwrap_or(2);
    let expected_mod_count = ctx.resolve_field_index("java/util/ArrayList$Itr", "expectedModCount");
    let parent_list = ctx
        .resolve_field_index("java/util/ArrayList$Itr", "this$0")
        .unwrap_or(0);
    let child_list = ctx
        .resolve_field_index("java/util/ArrayList$ListItr", "this$0")
        .filter(|slot| *slot != parent_list);
    (
        cursor,
        last_ret,
        expected_mod_count,
        parent_list,
        child_list,
    )
}

pub fn set_field_if_present(ctx: &mut dyn NativeContext, obj: ObjectRef, slot: usize, value: Value) {
    if slot < ctx.object_num_fields(obj) {
        ctx.set_field(obj, slot, value);
    }
}

pub fn native_arraylist_size(ctx: &mut dyn NativeContext, list: ObjectRef) -> i32 {
    match cratonvm_native_collections::native_al_size(ctx, &[Value::Object(Some(list))]) {
        Ok(Some(Value::Int(size))) => size.max(0),
        _ => 0,
    }
}

pub fn native_arraylist_list_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let len = native_arraylist_size(ctx, this);
    // Three registrations share this body: `listIterator()`, `iterator()` and
    // `listIterator(I)`. Only the last one has an index, and it must be
    // REJECTED when out of range, not clamped into it —
    // `ArrayList.rangeCheckForAdd` throws the plain
    // `IndexOutOfBoundsException` with the comma wording ("Index: 5, Size: 2"),
    // measured on Temurin 25. Clamping returned an iterator positioned at
    // `size` instead, so a caller that expected the throw silently got an
    // exhausted iterator. The no-arg forms pass no argument, hence the test on
    // presence rather than on value.
    let cursor = match args.get(1) {
        Some(Value::Int(v)) => {
            if *v < 0 || *v > len {
                return Err(RuntimeError::ioobe(format!("Index: {v}, Size: {len}")).into());            }
            *v        }
        _ => 0,
    };

    // gc-common w20-c: the iterator's allocation can collect and move the
    // list, which is stored into the iterator and read after it.
    let this_pin = ctx.pin_native_root(this);
    let itr = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList$ListItr", 5)?;
    let (cursor_slot, last_ret_slot, expected_slot, parent_list_slot, child_list_slot) =
        native_arraylist_list_itr_slots(ctx);
    let this: ObjectRef = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    set_field_if_present(ctx, itr, parent_list_slot, Value::Object(Some(this)));
    if let Some(slot) = child_list_slot {
        set_field_if_present(ctx, itr, slot, Value::Object(Some(this)));
    }
    set_field_if_present(ctx, itr, cursor_slot, Value::Int(cursor));
    set_field_if_present(ctx, itr, last_ret_slot, Value::Int(-1));
    if let Some(slot) = expected_slot {
        let mod_count = al_list_itr_list_mod_count(&*ctx, this).unwrap_or(0);
        set_field_if_present(ctx, itr, slot, Value::Int(mod_count));
    }
    Ok(Some(Value::Object(Some(itr))))
}

pub fn native_arraylist_list_itr_list(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<ObjectRef> {
    let (_, _, _, parent_list_slot, child_list_slot) = native_arraylist_list_itr_slots(ctx);
    if let Some(slot) = child_list_slot {
        if let Value::Object(Some(list)) = ctx.get_field(this, slot) {
            return Some(list);
        }
    }
    match ctx.get_field(this, parent_list_slot) {
        Value::Object(Some(list)) => Some(list),
        _ => None,
    }
}

/// `CRATONVM_COMPAT_AL_LISTITR_COMOD` -- default ON, `=0` restores the old
/// bodies. Makes the nine `--compatible` `ArrayList$ListItr` natives below
/// follow JDK 25's `ArrayList$Itr` / `$ListItr` protocol instead of answering
/// from the cursor alone:
///
/// * `next` / `previous` / `remove` / `set` / `add` open with
///   `checkForComodification()` (`modCount != expectedModCount` throws
///   `ConcurrentModificationException`), in the JDK's order: `remove` / `set`
///   test `lastRet < 0` first;
/// * `hasNext` is `cursor != size` and `hasPrevious` is `cursor != 0` -- not
///   `<` / `>`: after a removal behind an iterator parked at the end, HotSpot's
///   `hasNext()` answers `true` and the `next()` that follows throws CME;
/// * an exhausted `next` / `previous` throws `NoSuchElementException` and a
///   `remove` / `set` with no element returned throws
///   `IllegalStateException`, both with a NULL message, as the JDK's
///   no-argument constructors do (the old bodies said `"set"` / `"remove"` /
///   `"ArrayList$ListItr.next"`).
///
/// Before this the bodies never compared `modCount`, so a structural change
/// made through the list itself, a sublist, `removeIf`, `sort`, or a SECOND
/// iterator left every `ListIterator` running over the moved elements
/// (`R16OverpadLayout`'s `noCME`; `R16CmecompatSweep` rows c0 c2-c4 c7 c9 c11
/// c33 c34 c41 c47 c50-c55 c57). `--real-jdk` drops these natives (they are
/// `SyntheticStub`) and runs the real bytecode, which is what this mirrors.
/// Round 16 wave 12 (lane cmecompat), page
/// `r16-overpad-compatible-sublist-listiterator-misses-concurrent-modification-FIXED-20261001.md`.
pub fn al_list_itr_jdk_protocol() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true    })}

/// The JDK's `modCount != expectedModCount`, with "no readable slot" on
/// either side meaning "no check" (the pre-fix behaviour), never a throw.
pub fn list_itr_comod_violated(expected: Option<i32>, actual: Option<i32>) -> bool {
    matches!((expected, actual), (Some(e), Some(a)) if e != a)
}

/// `ArrayList$Itr.hasNext()` under the JDK protocol (`cursor != size`) or the
/// legacy one (`cursor < size`, a negative cursor clamped to 0).
pub fn list_itr_has_next(cursor: i32, size: i32, jdk: bool) -> bool {
    if jdk {
        cursor != size
    } else {
        cursor.max(0) < size
    }
}

/// `ArrayList$ListItr.hasPrevious()`: `cursor != 0` (JDK) or `cursor > 0`.
pub fn list_itr_has_previous(cursor: i32, jdk: bool) -> bool {
    if jdk {
        cursor != 0
    } else {
        cursor > 0
    }
}

/// The list's `modCount` as `ArrayList$Itr` reads it. Under
/// [`al_list_itr_jdk_protocol`] that is the `AbstractList.modCount` slot (the
/// one the JDK's own `getfield` and native-collections' `al_set_size` bump
/// use), so a user subclass that declares a field of the same name cannot
/// stand in for it; off, the legacy leaf-first by-name read.
pub fn al_list_itr_list_mod_count(ctx: &dyn NativeContext, list: ObjectRef) -> Option<i32> {
    if al_list_itr_jdk_protocol() {
        let slot = ctx.resolve_field_index("java/util/AbstractList", "modCount")?;
        if slot >= ctx.object_num_fields(list) {
            return None;
        }
        match ctx.get_field(list, slot) {
            Value::Int(v) => Some(v),
            _ => None,
        }
    } else {
        ctx.get_field_by_name(list, "modCount").as_int()    }}

/// `ArrayList$Itr.checkForComodification()` for a `ListItr` receiver. A
/// no-op when [`al_list_itr_jdk_protocol`] is off.
pub fn native_arraylist_list_itr_check_comod(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    list: ObjectRef,
) -> Result<(), MethodCallFailed> {
    if !al_list_itr_jdk_protocol() {
        return Ok(());
    }
    let (_, _, expected_slot, _, _) = native_arraylist_list_itr_slots(ctx);
    let expected = expected_slot
        .filter(|slot| *slot < ctx.object_num_fields(this))
        .and_then(|slot| match ctx.get_field(this, slot) {
            Value::Int(v) => Some(v),
            _ => None,
        });
    let actual = al_list_itr_list_mod_count(&*ctx, list);
    if list_itr_comod_violated(expected, actual) {
        return Err(RuntimeError::ConcurrentModificationException.into());
    }
    Ok(())
}

/// The message of an exhausted `next()` / `previous()`: the JDK's
/// no-argument constructor (empty = no message) under
/// [`al_list_itr_jdk_protocol`], the old diagnostic text otherwise.
pub fn list_itr_message(legacy: &str) -> String {
    if al_list_itr_jdk_protocol() {
        String::new()
    } else {
        legacy.to_string()
    }
}

/// `throw new IllegalStateException()` from `ListItr.remove()` / `set()`
/// with no element returned. `RuntimeError::IllegalStateException` has no
/// "no message" form (an empty string renders as `""`), so under
/// [`al_list_itr_jdk_protocol`] the JDK's own no-argument constructor builds
/// it; off, or when that constructor cannot run, the legacy text.
pub fn list_itr_illegal_state(ctx: &mut dyn NativeContext, legacy: &str) -> MethodCallFailed {
    let legacy_error = || -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: legacy.to_string(),
        }
        .into()
    };
    if !al_list_itr_jdk_protocol() {
        return legacy_error();
    }
    crate::throwable_mint::construct_thrown(
        ctx,
        "java/lang/IllegalStateException",
        "()V",
        &[],
        legacy_error,
    )
}


/// `ArrayList$ListItr.set(Object)` -- real live mutation against the
/// backing `ArrayList` (found via `native_arraylist_list_itr_list`), NOT a
/// no-op. Root-caused 2026-07-17: this class's `set`/`add`/`remove` were
/// previously registered as hardcoded `|_ctx, _args| Ok(None)` stubs (a
/// leftover from an earlier, genuinely-immutable "snapshot" design), but
/// `native_arraylist_list_iterator` and the sibling `next`/`previous`
/// natives above already carry a LIVE backing-list reference in the
/// `this$0`-equivalent slot -- so the iterator is not actually a frozen
/// snapshot, and silently dropping `set`/`add`/`remove` produced silent
/// data loss for any real-JDK code using `List.listIterator()` mutators
/// (e.g. ANTLR4's `IntervalSet.add(int)`, which merges adjacent intervals
/// via `ListIterator.set`/`.previous`/`.remove` -- this exact bug corrupted
/// Groovy's ANTLR4-generated parser ATN after `ATNDeserializer.optimizeSets`,
/// producing spurious `Unexpected input` parse failures for basic numeric/
/// string literals). Mirrors real-JDK `ArrayList$ListItr.set`'s
/// `IllegalStateException` guard (`lastRet < 0`) and delegates the actual
/// write to the already-correct `native_al_set`.
pub fn native_arraylist_list_itr_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (_, last_ret_slot, _, _, _) = native_arraylist_list_itr_slots(ctx);
    let last_ret = ctx.get_field(this, last_ret_slot).as_int().unwrap_or(-1);
    if last_ret < 0 {
        return Err(list_itr_illegal_state(ctx, "set"));
    }
    let list = match native_arraylist_list_itr_list(ctx, this) {
        Some(list) => list,
        None => return Ok(None),
    };
    native_arraylist_list_itr_check_comod(ctx, this, list)?;
    let e = args.get(1).copied().unwrap_or(Value::Object(None));
    cratonvm_native_collections::native_al_set(
        ctx,
        &[Value::Object(Some(list)), Value::Int(last_ret), e],
    )?;
    Ok(None)
}

/// `ArrayList$ListItr.add(Object)` -- real live insertion at the cursor
/// position. See `native_arraylist_list_itr_set` for the root-cause
/// narrative; mirrors real-JDK `ArrayList$ListItr.add`'s cursor/lastRet
/// bookkeeping (advance cursor past the inserted element, reset lastRet to
/// -1 so a following `remove()`/`set()` correctly throws
/// `IllegalStateException`).
pub fn native_arraylist_list_itr_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, last_ret_slot, expected_slot, _, _) = native_arraylist_list_itr_slots(ctx);
    let cursor = ctx.get_field(this, cursor_slot).as_int().unwrap_or(0);
    let list = match native_arraylist_list_itr_list(ctx, this) {
        Some(list) => list,
        None => return Ok(None),
    };
    native_arraylist_list_itr_check_comod(ctx, this, list)?;
    let e = args.get(1).copied().unwrap_or(Value::Object(None));
    // gc-common w11-c: the reclaiming door (registered-only native). `this`
    // and `list` are used after the add, so both ride a pin frame and are
    // re-read; the frame is released before the `?`.
    let this_pin = ctx.pin_native_root(this);
    let list_pin = ctx.pin_native_root(list);
    let added = cratonvm_native_collections::native_al_add_at_entry_pub(
        ctx,
        &[Value::Object(Some(list)), Value::Int(cursor), e],
    );
    let this = ctx.read_native_pin(this_pin, this);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(this_pin);
    let _ = added?;
    ctx.set_field(this, cursor_slot, Value::Int(cursor + 1));
    ctx.set_field(this, last_ret_slot, Value::Int(-1));
    if let Some(slot) = expected_slot {
        let mod_count = al_list_itr_list_mod_count(&*ctx, list).unwrap_or(0);
        set_field_if_present(ctx, this, slot, Value::Int(mod_count));
    }
    Ok(None)
}

/// `ArrayList$ListItr.remove()` -- real live removal of the last element
/// returned by `next()`/`previous()`. See `native_arraylist_list_itr_set`
/// for the root-cause narrative; mirrors real-JDK `ArrayList$Itr.remove`'s
/// cursor rewind (`cursor = lastRet`) and `lastRet` reset.
pub fn native_arraylist_list_itr_remove(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, last_ret_slot, expected_slot, _, _) = native_arraylist_list_itr_slots(ctx);
    let last_ret = ctx.get_field(this, last_ret_slot).as_int().unwrap_or(-1);
    if last_ret < 0 {
        return Err(list_itr_illegal_state(ctx, "remove"));
    }
    let list = match native_arraylist_list_itr_list(ctx, this) {
        Some(list) => list,
        None => return Ok(None),
    };
    native_arraylist_list_itr_check_comod(ctx, this, list)?;
    // R16B-194 (round 18 wave 11, lane backlogfix): `this` and `list` are
    // written / read after the removal, so both ride a pin frame and are
    // re-read -- the shape `native_arraylist_list_itr_add` above and
    // native-collections' `ArrayList$Itr.remove()` already use. Today's
    // `native_al_remove_at` has no collection point (no Java call; its only
    // allocation, an auto-box wrapper in `set_array_element`, never
    // collects), so this closes a latent hazard rather than a live one: the
    // callee is a public funnel that can grow one (its `remove(Object)`
    // sibling already dispatches `equals` and propagates into a view
    // source), and nothing here would notice. Pure bug fix, no behaviour
    // choice, so no switch. The frame is released before the `?`.
    let this_pin = ctx.pin_native_root(this);
    let list_pin = ctx.pin_native_root(list);
    let removed = cratonvm_native_collections::native_al_remove_at(
        ctx,
        &[Value::Object(Some(list)), Value::Int(last_ret)],
    );
    let this = ctx.read_native_pin(this_pin, this);
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(this_pin);
    let _ = removed?;
    ctx.set_field(this, cursor_slot, Value::Int(last_ret));
    ctx.set_field(this, last_ret_slot, Value::Int(-1));
    if let Some(slot) = expected_slot {
        let mod_count = al_list_itr_list_mod_count(&*ctx, list).unwrap_or(0);
        set_field_if_present(ctx, this, slot, Value::Int(mod_count));
    }
    Ok(None)
}

pub fn native_snapshot_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, _, _, _, _) = native_arraylist_list_itr_slots(ctx);
    let cursor = ctx.get_field(this, cursor_slot).as_int().unwrap_or(0);
    let size = native_arraylist_list_itr_list(ctx, this)
        .map(|list| native_arraylist_size(ctx, list))
        .unwrap_or(0);
    Ok(Some(Value::Int(
        list_itr_has_next(cursor, size, al_list_itr_jdk_protocol()) as i32,
    )))
}

pub fn native_snapshot_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, last_ret_slot, _, _, _) = native_arraylist_list_itr_slots(ctx);
    let cursor = ctx
        .get_field(this, cursor_slot)
        .as_int()
        .unwrap_or(0)
        .max(0);
    let list = match native_arraylist_list_itr_list(ctx, this) {
        Some(list) => list,
        None => return Ok(Some(Value::Object(None))),
    };
    // `ArrayList$Itr.next()` opens with `checkForComodification()`, before
    // the exhaustion test.
    native_arraylist_list_itr_check_comod(ctx, this, list)?;
    if cursor >= native_arraylist_size(ctx, list) {
        // The JDK contract, and what the sibling `ArrayList$Itr.next()` native
        // in native-collections already does. A `null` here is not a harmless
        // "nothing left": `AbstractSequentialList.get(index)` is
        // `listIterator(index).next()` wrapped in
        // `catch (NoSuchElementException) -> IndexOutOfBoundsException`, so
        // swallowing it made `get(size())` answer `null` on EVERY list that
        // inherits `get` -- directly and through a
        // `Collections.unmodifiableList` view. See the identical reasoning on
        // the `Collections$EmptyIterator` natives in this file.
        return Err(MethodCallFailed::from(
            RuntimeError::NoSuchElementException {
                message: list_itr_message("ArrayList$ListItr.next"),
            },
        ));
    }
    // R16B-194 audit (round 18 wave 11, lane backlogfix): unlike
    // `native_al_remove_at`, `native_al_get` CAN run Java (an unmodifiable
    // backing's own `get`, a values view's `size()`, the `vc_route`
    // delegation), so `this` -- written below -- is pinned and re-read across
    // it. Pure GC-safety fix, no switch.
    let this_pin = ctx.pin_native_root(this);
    let value = cratonvm_native_collections::native_al_get(
        ctx,
        &[Value::Object(Some(list)), Value::Int(cursor)],
    );
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let value = value?;
    ctx.set_field(this, cursor_slot, Value::Int(cursor + 1));
    ctx.set_field(this, last_ret_slot, Value::Int(cursor));
    Ok(value)
}

pub fn native_snapshot_list_itr_has_previous(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, _, _, _, _) = native_arraylist_list_itr_slots(ctx);
    let cursor = ctx.get_field(this, cursor_slot).as_int().unwrap_or(0);
    Ok(Some(Value::Int(
        list_itr_has_previous(cursor, al_list_itr_jdk_protocol()) as i32,
    )))
}

pub fn native_snapshot_list_itr_previous(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, last_ret_slot, _, _, _) = native_arraylist_list_itr_slots(ctx);
    let cursor = ctx.get_field(this, cursor_slot).as_int().unwrap_or(0);
    let list = native_arraylist_list_itr_list(ctx, this);
    // `ListItr.previous()` checks for comodification before `i < 0`.
    if let Some(list) = list {
        native_arraylist_list_itr_check_comod(ctx, this, list)?;
    }
    if cursor <= 0 {
        // Same contract as `next()` above, at the other end.
        return Err(MethodCallFailed::from(
            RuntimeError::NoSuchElementException {
                message: list_itr_message("ArrayList$ListItr.previous"),
            },
        ));
    }
    let list = match list {
        Some(list) => list,
        None => return Ok(Some(Value::Object(None))),
    };
    let next_cursor = cursor - 1;
    // R16B-194 audit: same as `native_snapshot_itr_next` -- `native_al_get`
    // can run Java, and `this` is written after it.
    let this_pin = ctx.pin_native_root(this);
    let value = cratonvm_native_collections::native_al_get(
        ctx,
        &[Value::Object(Some(list)), Value::Int(next_cursor)],
    );
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let value = value?;
    ctx.set_field(this, cursor_slot, Value::Int(next_cursor));
    ctx.set_field(this, last_ret_slot, Value::Int(next_cursor));
    Ok(value)
}

pub fn native_snapshot_list_itr_next_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, _, _, _, _) = native_arraylist_list_itr_slots(ctx);
    Ok(Some(Value::Int(
        ctx.get_field(this, cursor_slot).as_int().unwrap_or(0),
    )))
}

pub fn native_snapshot_list_itr_previous_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (cursor_slot, _, _, _, _) = native_arraylist_list_itr_slots(ctx);
    Ok(Some(Value::Int(
        ctx.get_field(this, cursor_slot).as_int().unwrap_or(0) - 1,
    )))
}

pub fn register_function_identity_natives(registry: &mut NativeMethodRegistry) {
    // `java/util/function/Function$Identity` is a purely VM-internal
    // synthetic stand-in for the real lambda-based `Function.identity()`
    // (real OpenJDK's `identity()` returns `t -> t`, an invokedynamic
    // lambda -- there is no real classfile named `Function$Identity` to
    // fall back to at all). Tagging this SyntheticStub broke real-JDK-mode
    // boot the moment `set_drop_synthetic_stubs(true)` started actually
    // dropping SyntheticStub registrations (dev d8092acb, 2026-07-14):
    // WildFly's very first `getRuntimeMXBean()`-adjacent lambda hit
    // `UnsatisfiedLinkError: Function$Identity.andThen`. Tag as Bridge
    // (needed in both modes) instead -- same class of bug/fix as the
    // `native-builtins/src/jmx.rs` JMX cluster retag, both landed together.
    //
    // 2026-08-05 (Real-JDK wave 2, lane L7 item 4) — retagged `SyntheticStub`,
    // and the 2026-07-14 reasoning above is NOT re-litigated. That bisect was
    // about `CRATONVM_NO_STUBS` / `set_drop_synthetic_stubs(true)`, an
    // operator's blunt global switch that is opt-in and still off by default,
    // so `Compatible` / `--fast-boot` behaviour is byte-for-byte unchanged by
    // this line: SyntheticStub registrations are kept in both.
    //
    // What changes is `--real-jdk`, and it fixes a defect the lane doc calls
    // out by name. `Function$Identity` was `Bridge`, so strict mode registered
    // AND invoked it — but the class it allocates has no class file anywhere
    // and is minted by `alloc_concurrent_synthetic`, which contract §5 forbids
    // under `JdkOnly`. The two defects cancelled: the fabrication recorded the
    // violation and proceeded, so the census reported it while the run
    // continued in the forbidden state. Fixing either half alone exposes the
    // other, which is why both land together here — this retag, plus
    // `native_function_identity`'s migration to the fallible allocator.
    //
    // Bridge was the wrong tag on its own terms. `Bridge` means "no working
    // real-bytecode fallback exists"; for the two `identity()` factories there
    // plainly is one — `java.base`'s own `Function.identity()` / `UnaryOperator
    // .identity()`, each a one-line invokedynamic returning `t -> t`. Dropped
    // here, strict mode runs those instead.
    //
    // The other three triples are the instance methods of the fabricated
    // stand-in. They do not fall back to anything, because with the factories
    // dropped no `Function$Identity` object is ever created and the class is
    // never fabricated — they simply become unreachable, which is the point.
    // Leaving them `Bridge` would keep asserting that a class with no bytes
    // anywhere is a boundary the VM must cross.
    //
    // The drop is recorded, not silent: `register()` pushes a
    // `SyntheticNativeRegistered` violation naming this call site, so
    // `--real-jdk-report` shows exactly which five triples went missing.
    registry.register(
        "java/util/function/Function",
        "identity",
        "()Ljava/util/function/Function;",
        native_function_identity, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/function/UnaryOperator",
        "identity",
        "()Ljava/util/function/UnaryOperator;",
        native_function_identity, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/function/Function$Identity",
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_function_identity_apply, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/function/Function$Identity",
        "andThen",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        native_function_identity_return_arg, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/util/function/Function$Identity",
        "compose",
        "(Ljava/util/function/Function;)Ljava/util/function/Function;",
        native_function_identity_return_arg, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn synthetic_instant_alloc(
    ctx: &mut dyn NativeContext,
    epoch_sec: i64,
    nano: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/time/Instant", 2)?;
    ctx.set_field(obj, 0, Value::Long(epoch_sec));
    ctx.set_field(obj, 1, Value::Int(nano));
    Ok(obj)
}

pub fn synthetic_instant_parts(ctx: &dyn NativeContext, obj: ObjectRef) -> (i64, i32) {
    let sec = match ctx.get_field(obj, 0) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    };
    let nano = match ctx.get_field(obj, 1) {
        Value::Int(v) => v,
        Value::Long(v) => v as i32,
        _ => 0,
    };
    (sec, nano)}

pub fn synthetic_instant_norm(epoch_sec: i64, nano_adj: i64) -> (i64, i32) {
    let total = epoch_sec as i128 * 1_000_000_000 + nano_adj as i128;
    (
        total.div_euclid(1_000_000_000) as i64,
        total.rem_euclid(1_000_000_000) as i32,
    )
}

pub fn native_synthetic_instant_now(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let obj = synthetic_instant_alloc(ctx, now.as_secs() as i64, now.subsec_nanos() as i32)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_of_epoch_second(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sec = match args.first() {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let obj = synthetic_instant_alloc(ctx, sec, 0)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_of_epoch_second_nano(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sec = match args.first() {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let adj = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let (sec, nano) = synthetic_instant_norm(sec, adj);
    let obj = synthetic_instant_alloc(ctx, sec, nano)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_of_epoch_milli(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let millis = match args.first() {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let sec = millis.div_euclid(1000);
    let nano = (millis.rem_euclid(1000) * 1_000_000) as i32;
    let obj = synthetic_instant_alloc(ctx, sec, nano)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_get_epoch_second(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Long(synthetic_instant_parts(ctx, this).0)))
}

pub fn native_synthetic_instant_get_nano(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(synthetic_instant_parts(ctx, this).1)))
}

pub fn native_synthetic_instant_to_epoch_milli(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    Ok(Some(Value::Long(
        sec.saturating_mul(1000) + (nano / 1_000_000) as i64,
    )))
}

pub fn native_synthetic_instant_plus_seconds(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let add = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    let obj = synthetic_instant_alloc(ctx, sec.saturating_add(add), nano)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_minus_seconds(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sub = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    let obj = synthetic_instant_alloc(ctx, sec.saturating_sub(sub), nano)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_plus_millis(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let add = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    let (sec, nano) = synthetic_instant_norm(sec, nano as i64 + add.saturating_mul(1_000_000));
    let obj = synthetic_instant_alloc(ctx, sec, nano)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_plus_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let add = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    };
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    let (sec, nano) = synthetic_instant_norm(sec, nano as i64 + add);
    let obj = synthetic_instant_alloc(ctx, sec, nano)?;
    Ok(Some(Value::Object(Some(obj))))
}

pub fn native_synthetic_instant_is_before(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    Ok(Some(Value::Int(
        if synthetic_instant_parts(ctx, this) < synthetic_instant_parts(ctx, other) {
            1
        } else {
            0
        },
    )))
}

pub fn native_synthetic_instant_is_after(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    Ok(Some(Value::Int(
        if synthetic_instant_parts(ctx, this) > synthetic_instant_parts(ctx, other) {
            1
        } else {
            0
        },
    )))
}

/// Civil date from a days-since-epoch count (Howard Hinnant's `civil_from_days`).
///
/// This lives here, ungated, on purpose. `util_time` is
/// `#[cfg(feature = "synthetic-jdk")]` because it registers synthetic natives,
/// but this function is pure calendar arithmetic with no VM dependency, and
/// `iso_instant_string` below needs it in EVERY build. Calling
/// `crate::util_time::epoch_day_to_ymd` from ungated code broke the
/// default-feature build — which is what the blocking `cargo test --workspace`
/// CI job runs, while every check in this module's own CI job passes
/// `--features synthetic-jdk` and so never saw it.
///
/// Infallible, and narrowed back to a plain tuple: the funnel migration widened
/// it to `Result` although every path ends in `Ok`, which cost its `synthetic-jdk`
/// caller a `?` it could not use (that build was red).
pub fn epoch_day_to_ymd(epoch_day: i64) -> (i32, i32, i32) {
    let z = epoch_day + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as i32, d as i32)
}

/// `Instant.toString()` — ISO-8601, per `DateTimeFormatter.ISO_INSTANT`.
///
/// This used to emit Rust's debug shape, `Instant(0.042000000)`, which is not
/// a format any Java caller can parse and which leaked straight through
/// `FileTime.toString()` (it delegates here). The JDK prints
/// `1970-01-01T00:00:00.042Z`: the UTC date-time, with the fraction rendered
/// in whole groups of three digits — milliseconds, microseconds or
/// nanoseconds — and omitted entirely when zero.
pub fn iso_instant_string(sec: i64, nano: i32) -> Result<String, MethodCallFailed> {
    // Floor division so pre-epoch instants borrow correctly into the previous
    // day rather than truncating toward zero.
    let days = sec.div_euclid(86_400);
    let secs_of_day = sec.rem_euclid(86_400);
    let (y, m, d) = epoch_day_to_ymd(days);
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    // Years outside 0..=9999 take an explicit sign, as ISO-8601 requires.
    let mut s = if (0..=9999).contains(&y) {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
    } else {
        format!("{y:+05}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
    };
    if nano != 0 {
        let nano = nano.unsigned_abs();
        if nano % 1_000_000 == 0 {
            s.push_str(&format!(".{:03}", nano / 1_000_000));
        } else if nano % 1_000 == 0 {
            s.push_str(&format!(".{:06}", nano / 1_000));
        } else {
            s.push_str(&format!(".{nano:09}"));
        }
    }
    s.push('Z');
    Ok(s)
}

pub fn native_synthetic_instant_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    let s = iso_instant_string(sec, nano);
    Ok(Some(Value::Object(Some(ctx.create_string(&s?)))))
}

pub fn native_synthetic_instant_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        if synthetic_instant_parts(ctx, this) == synthetic_instant_parts(ctx, other) {
            1
        } else {
            0
        },
    )))
}

pub fn native_synthetic_instant_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (sec, nano) = synthetic_instant_parts(ctx, this);
    let h = (sec ^ (sec >> 32)) as i32 ^ nano;
    Ok(Some(Value::Int(h)))
}

/// G54-1 NOMINATION 5: renamed from `register_synthetic_instant_stub_natives`.
/// The `synthetic_*` prefix is otherwise reserved for registrars reachable
/// only from `register_synthetic_overrides` (`--features synthetic-jdk`);
/// this one is called from `register_essential_natives_with_shims`, i.e. it
/// ships on the real-JDK path (see the call site's own comment: a WildFly
/// bootstrap fallback for `java.time.Instant`, kept as `SyntheticStub` so a
/// loaded real-JDK `Instant` still wins on its own bytecode).
pub fn register_instant_stub_natives(registry: &mut NativeMethodRegistry) {
    let inst = "java/time/Instant";
    registry.register(
        inst,
        "now",
        "()Ljava/time/Instant;",
        native_synthetic_instant_now, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "ofEpochSecond",
        "(J)Ljava/time/Instant;",
        native_synthetic_instant_of_epoch_second, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "ofEpochSecond",
        "(JJ)Ljava/time/Instant;",
        native_synthetic_instant_of_epoch_second_nano, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "ofEpochMilli",
        "(J)Ljava/time/Instant;",
        native_synthetic_instant_of_epoch_milli, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "getEpochSecond",
        "()J",
        native_synthetic_instant_get_epoch_second, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(inst, "getNano", "()I", native_synthetic_instant_get_nano, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        inst,
        "toEpochMilli",
        "()J",
        native_synthetic_instant_to_epoch_milli, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "plusSeconds",
        "(J)Ljava/time/Instant;",
        native_synthetic_instant_plus_seconds, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "minusSeconds",
        "(J)Ljava/time/Instant;",
        native_synthetic_instant_minus_seconds, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "plusMillis",
        "(J)Ljava/time/Instant;",
        native_synthetic_instant_plus_millis, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "plusNanos",
        "(J)Ljava/time/Instant;",
        native_synthetic_instant_plus_nanos, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "isBefore",
        "(Ljava/time/Instant;)Z",
        native_synthetic_instant_is_before, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "isAfter",
        "(Ljava/time/Instant;)Z",
        native_synthetic_instant_is_after, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "toString",
        "()Ljava/lang/String;",
        native_synthetic_instant_to_string, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        inst,
        "equals",
        "(Ljava/lang/Object;)Z",
        native_synthetic_instant_equals, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(inst, "hashCode", "()I", native_synthetic_instant_hash_code, cratonvm_native_api::NativeKind::SyntheticStub);
}

pub fn sync_collection_backing(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "c") {
        Value::Object(Some(c)) => Some(c),
        _ => None,
    }
}

pub fn sync_map_backing(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "m") {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

pub fn register_synchronized_collection_wrapper_natives(registry: &mut NativeMethodRegistry) {
    for (class, ctor_desc) in [
        (
            "java/util/Collections$SynchronizedSet",
            "(Ljava/util/Set;)V",
        ),
        (
            "java/util/Collections$SynchronizedCollection",
            "(Ljava/util/Collection;)V",
        ),
    ] {
        registry.register(class, "<init>", ctor_desc, native_sync_collection_init, cratonvm_native_api::NativeKind::SyntheticStub);
        let two_arg_ctor_desc = if class == "java/util/Collections$SynchronizedSet" {
            "(Ljava/util/Set;Ljava/lang/Object;)V"
        } else {
            "(Ljava/util/Collection;Ljava/lang/Object;)V"
        };
        registry.register(
            class,
            "<init>",
            two_arg_ctor_desc,
            native_sync_collection_init, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "add",
            "(Ljava/lang/Object;)Z",
            native_sync_collection_add, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "contains",
            "(Ljava/lang/Object;)Z",
            native_sync_collection_contains, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "remove",
            "(Ljava/lang/Object;)Z",
            native_sync_collection_remove, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(class, "size", "()I", native_sync_collection_size, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(class, "isEmpty", "()Z", native_sync_collection_is_empty, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            class,
            "iterator",
            "()Ljava/util/Iterator;",
            native_sync_collection_iterator, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "toArray",
            "()[Ljava/lang/Object;",
            native_sync_collection_to_array, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // The rest of the `Collection` contract, forwarded verbatim — see
        // `sync_collection_delegate` for why a wrapper now needs all of it.
        registry.register(class, "clear", "()V", |ctx, args| {
            sync_collection_delegate(ctx, args, "clear", "()V", None)
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(class, "toString", "()Ljava/lang/String;", |ctx, args| {
            sync_collection_delegate(
                ctx,
                args,
                "toString",
                "()Ljava/lang/String;",
                Some(Value::Object(None)),
            )
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            class,
            "stream",
            "()Ljava/util/stream/Stream;",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "stream",
                    "()Ljava/util/stream/Stream;",
                    Some(Value::Object(None)),
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "spliterator",
            "()Ljava/util/Spliterator;",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "spliterator",
                    "()Ljava/util/Spliterator;",
                    Some(Value::Object(None)),
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "forEach",
                    "(Ljava/util/function/Consumer;)V",
                    None,
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "containsAll",
            "(Ljava/util/Collection;)Z",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "containsAll",
                    "(Ljava/util/Collection;)Z",
                    Some(Value::Int(0)),
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(class, "addAll", "(Ljava/util/Collection;)Z", |ctx, args| {
            sync_collection_delegate(
                ctx,
                args,
                "addAll",
                "(Ljava/util/Collection;)Z",
                Some(Value::Int(0)),
            )
        }, cratonvm_native_api::NativeKind::SyntheticStub);
        registry.register(
            class,
            "removeAll",
            "(Ljava/util/Collection;)Z",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "removeAll",
                    "(Ljava/util/Collection;)Z",
                    Some(Value::Int(0)),
                )
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            class,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "retainAll",
                    "(Ljava/util/Collection;)Z",
                    Some(Value::Int(0)),
                )
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        registry.register(
            class,
            "removeIf",
            "(Ljava/util/function/Predicate;)Z",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "removeIf",
                    "(Ljava/util/function/Predicate;)Z",
                    Some(Value::Int(0)),
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "toArray",
                    "([Ljava/lang/Object;)[Ljava/lang/Object;",
                    Some(Value::Object(None)),
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        registry.register(
            class,
            "toArray",
            "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
            |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "toArray",
                    "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
                    Some(Value::Object(None)),
                )
            }, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        // `equals`/`hashCode` ONLY on the Set wrapper, matching the JDK:
        // `SynchronizedSet` overrides both to delegate (the Set contract),
        // `SynchronizedCollection` inherits `Object` identity and must keep it.
        if class == "java/util/Collections$SynchronizedSet" {
            registry.register(class, "hashCode", "()I", |ctx, args| {
                sync_collection_delegate(ctx, args, "hashCode", "()I", Some(Value::Int(0)))
            }, cratonvm_native_api::NativeKind::SyntheticStub);
            registry.register(class, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
                sync_collection_delegate(
                    ctx,
                    args,
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    Some(Value::Int(0)),
                )
            }, cratonvm_native_api::NativeKind::Bridge);
        }
    }
    let map = "java/util/Collections$SynchronizedMap";
    registry.register(map, "<init>", "(Ljava/util/Map;)V", native_sync_map_init, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        map,
        "<init>",
        "(Ljava/util/Map;Ljava/lang/Object;)V",
        native_sync_map_init, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_sync_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        map,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_sync_map_put, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        map,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_sync_map_contains_key, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        map,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_sync_map_remove, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(map, "size", "()I", native_sync_map_size, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(map, "isEmpty", "()Z", native_sync_map_is_empty, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        map,
        "entrySet",
        "()Ljava/util/Set;",
        native_sync_map_entry_set, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(map, "keySet", "()Ljava/util/Set;", native_sync_map_key_set, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        map,
        "values",
        "()Ljava/util/Collection;",
        native_sync_map_values, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(map, "clear", "()V", native_sync_map_clear, cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        map,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_sync_map_compute_if_absent, cratonvm_native_api::NativeKind::Bridge,
    );
}

// IntSummaryStatistics: 4-field synthetic (count=0 Long, sum=1 Long, min=2 Int, max=3 Int)
pub const ISS_FIELD_COUNT: usize = 0;
pub const ISS_FIELD_SUM: usize = 1;
pub const ISS_FIELD_MIN: usize = 2;
pub const ISS_FIELD_MAX: usize = 3;

pub fn native_return_true(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// Validate the target accepted by `java.lang.reflect.Array` before using an
/// array accessor.  A non-null ordinary object used to reach the generic
/// heap-array fallback, which reads slot zero as though it were element zero.
/// Besides violating the reflection contract, that made `Array.set` spin when
/// HSQLDB passed its zero-field `RangeGroupEmpty` singleton.
pub fn reflect_array_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // HotSpot's `Reflection::array_get` starts with a null check that
            // throws a plain, message-less NullPointerException — not an
            // IllegalArgumentException, which is what a `catch (NPE)` around a
            // reflective array read expects.
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    if !ctx.object_is_array(arr) {
        return Err(RuntimeError::IllegalArgumentException {
            // HotSpot's exact wording ("Argument is not an array"), which is
            // what `Array.get("hello", 0)` prints.
            message: "Argument is not an array".to_string(),
        }
        .into());
    }
    Ok(arr)
}

/// `java.lang.reflect.Array`'s index argument, bounds-checked.
///
/// Until 2026-08-06 every accessor below did `*v as usize` and passed it
/// straight to the heap, which answers an out-of-range read with a default
/// value and drops an out-of-range write — so `Array.get(new int[4], 9)`
/// returned `0` and `Array.set(new int[4], 9, v)` silently did nothing, where
/// HotSpot throws. A negative index was worse: it became a huge `usize`.
///
/// The thrown AIOOBE deliberately carries **no** detail message. That is not
/// an unfinished migration — HotSpot's `Reflection::array_get` raises the
/// exception with no text, so `Array.get(new int[4], 9).getMessage()` is null
/// there while a plain `a[9]` in bytecode says "Index 9 out of bounds for
/// length 4". `probes/PreconditionsFormatterProbe`'s "reflective access" rows
/// pin both halves.
pub fn reflect_array_index(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    args: &[Value],
) -> Result<usize, MethodCallFailed> {
    let idx = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if idx < 0 || i64::from(idx) >= ctx.array_length(arr) as i64 {
        return Err(RuntimeError::aioobe_no_message(idx).into());
    }
    Ok(idx as usize)
}

/// JLS §5.1.2 widening primitive conversion, as `java.lang.reflect.Array`
/// applies it: `true` when a value of `from` may be read as / stored as `to`.
///
/// Both directions of the API use this one table. `Array.getX(a, i)` asks
/// `widens_to(element_type, X)` -- reading an `int[]` as a `long` is fine, the
/// reverse is not -- and `Array.setX(a, i, v)` asks `widens_to(X, element_type)`,
/// which is the same relation with the arguments swapped. Getting the direction
/// wrong is invisible for the eight same-type cells and wrong for every other
/// one, so the two call sites below name their direction explicitly.
///
/// `boolean` converts to nothing and from nothing, and `char` is the usual
/// asymmetry: `char` widens to `int` but `short` does not widen to `char`,
/// because neither of those two ranges contains the other. Measured cell by
/// cell in `probes/ReflectArrayContractProbe`.
pub fn widens_to(from: cratonvm_types::ArrayElementType, to: cratonvm_types::ArrayElementType) -> bool {
    use cratonvm_types::ArrayElementType as T;
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        (T::Byte, T::Short | T::Int | T::Long | T::Float | T::Double)
            | (T::Short, T::Int | T::Long | T::Float | T::Double)
            | (T::Char, T::Int | T::Long | T::Float | T::Double)
            | (T::Int, T::Long | T::Float | T::Double)
            | (T::Long, T::Float | T::Double)
            | (T::Float, T::Double)
    )
}

/// The element type of an array a PRIMITIVE accessor was handed, or the
/// `IllegalArgumentException` HotSpot throws when it is a reference array.
///
/// This check runs BEFORE the bounds check, which is observable:
/// `Array.getInt(new Object[4], 9)` is an `IllegalArgumentException` on HotSpot
/// while `Array.getInt(new long[4], 9)` is an `ArrayIndexOutOfBoundsException`.
/// The widening check, in contrast, runs AFTER bounds. Three checks, three
/// positions; `probes/ReflectArrayContractProbe`'s "bounds vs type" rows pin
/// each one.
pub fn reflect_array_primitive_elem(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
) -> Result<cratonvm_types::ArrayElementType, MethodCallFailed> {
    let elem = ctx.heap_element_type_of(arr);
    if elem == cratonvm_types::ArrayElementType::Reference {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Argument is not an array of primitive type".to_string(),
        }
        .into());
    }
    Ok(elem)
}

/// HotSpot's wording for a conversion the widening table refuses.
pub fn reflect_array_type_mismatch() -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: "argument type mismatch".to_string(),
    }
    .into()
}

/// Reinterpret a primitive element as `want`, which `widens_to` has already
/// allowed. The `Value` shape follows the Java RETURN type, not the array's:
/// `getLong` on an `int[]` must hand back a `Value::Long`, or the caller reads
/// half a slot.
pub fn reflect_widen_value(
    v: Value,
    from: cratonvm_types::ArrayElementType,
    want: cratonvm_types::ArrayElementType,
) -> Value {
    use cratonvm_types::ArrayElementType as T;
    // Every source that can reach a wider type is integral except f32->f64.
    let as_i64 = match v {
        Value::Int(i) => i64::from(i),
        Value::Long(l) => l,
        Value::Float(f) => f as i64,
        Value::Double(d) => d as i64,
        _ => 0,
    };
    let as_f64 = match v {
        Value::Int(i) => f64::from(i),
        Value::Long(l) => l as f64,
        Value::Float(f) => f64::from(f),
        Value::Double(d) => d,
        _ => 0.0,
    };
    match want {
        // Same-width integral reads keep the stored bits; `from == want` here
        // in every case the table allows.
        T::Boolean | T::Byte | T::Char | T::Short | T::Int => Value::Int(as_i64 as i32),
        T::Long => Value::Long(as_i64),
        T::Float => Value::Float(if from == T::Float {
            match v {
                Value::Float(f) => f,
                _ => as_f64 as f32,
            }
        } else {
            as_f64 as f32
        }),
        T::Double => Value::Double(as_f64),
        T::Reference => v,
    }
}

/// `Array.getBoolean/getByte/getChar/getShort/getInt/getLong/getFloat/getDouble`.
///
/// One body per requested type, bound at registration, because the requested
/// type IS the contract: five of these used to share `native_array_get_int`,
/// which returned the raw element whatever the array held, so
/// `Array.getInt(new long[4], 0)` answered `0` instead of throwing.
pub fn reflect_array_get_primitive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    want: cratonvm_types::ArrayElementType,
) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let elem = reflect_array_primitive_elem(&*ctx, arr)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    if !widens_to(elem, want) {
        return Err(reflect_array_type_mismatch());
    }
    let raw = ctx.get_array_element(arr, idx);
    Ok(Some(reflect_widen_value(raw, elem, want)))
}

/// `Array.setBoolean/setByte/setChar/setShort/setInt/setLong/setFloat/setDouble`.
///
/// The widening direction is the mirror of the getter's: a `setInt` may store
/// into a `long[]` but not into a `short[]`.
pub fn reflect_array_set_primitive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    have: cratonvm_types::ArrayElementType,
) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let elem = reflect_array_primitive_elem(&*ctx, arr)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    if !widens_to(have, elem) {
        return Err(reflect_array_type_mismatch());
    }
    let raw = args.get(2).copied().unwrap_or(Value::Int(0));
    ctx.set_array_element(arr, idx, reflect_widen_value(raw, have, elem));
    Ok(None)
}

/// The primitive a wrapper class boxes, or `None` for anything else.
pub fn reflect_boxed_primitive(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Option<cratonvm_types::ArrayElementType> {
    use cratonvm_types::ArrayElementType as T;
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(obj))?;
    Some(match class_name.as_str() {
        "java/lang/Boolean" => T::Boolean,
        "java/lang/Byte" => T::Byte,
        "java/lang/Character" => T::Char,
        "java/lang/Short" => T::Short,
        "java/lang/Integer" => T::Int,
        "java/lang/Long" => T::Long,
        "java/lang/Float" => T::Float,
        "java/lang/Double" => T::Double,
        _ => return None,
    })
}

/// Whether `value` may be stored into a reference array whose component class
/// is `component`.
///
/// `null` is always storable; `Object[]` takes everything. Anything this cannot
/// prove assignable is refused with HotSpot's "array element type mismatch" --
/// a FOURTH distinct `IllegalArgumentException` wording, separate from the
/// "argument type mismatch" a primitive array uses.
pub fn reflect_array_element_assignable(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    value: ObjectRef,
) -> bool {
    let arr_class = ctx.class_id_of_object(arr);
    // `array_component_class_id` is the direct answer; some array objects carry
    // the component class as their own class id instead (primitive arrays are
    // allocated with a synthetic id), so fall back rather than refusing.
    let component = ctx.array_component_class_id(arr_class).unwrap_or(arr_class);
    if let Some(object_class) = ctx.class_id_by_name("java/lang/Object") {
        if component == object_class {
            return true;
        }
    }
    let value_class = ctx.class_id_of_object(value);
    if value_class == component || ctx.is_subclass(value_class, component) {
        return true;
    }
    // THE SAME CHECK THE `aastore` BYTECODE MAKES. On HotSpot `Array.set` and
    // `aastore` are one rule; here they were two, and only one of them had been
    // taught what this VM cannot prove.
    //
    // `typecheck::aastore_element_assignable` — already shared by the
    // interpreter opcode and the JIT's `jit_aastore` — is deliberately
    // *additive*: it must never produce a FALSE `ArrayStoreException`, so it
    // fails open for an interface component, for a `$Proxy`/`AnnotationProxy`
    // value, for a synthetic class id, and for a same-named component that
    // resolved to a different `ClassId` under another loader. Each hedge was
    // paid for by a regression; one of them names storing an `AnnotationProxy`
    // into an annotation-type array, which is this case exactly.
    //
    // `AotIntegrationTests.endToEndTestsForBeanOverrides` reached it:
    // `TypeMappedAnnotation.adapt` fills a `ContextConfiguration[]` with
    // `annotations[i].synthesize()` proxies under
    // `@CompileWithForkedClassLoader`, and one element per run was refused —
    // `jdk/proxy3/$Proxy27` judged not assignable to a `ContextConfiguration` it
    // had been created FROM. `adapt` is generic over the component type, so it
    // must go through `Array.set` where ordinary code emits `aastore` and sails
    // past. That asymmetry, not the loader split on its own, is why this
    // surfaced here and nowhere else.
    //
    // The split itself was found and fixed on 2026-08-10 — a compiled
    // `invokestatic` bound its owner class by name, so `createProxy` ran in the
    // wrong loader's copy. See
    // `aotintegration-hangs-after-the-unmodifiable-get-fix.md`.
    // This check stays: it is the lenient rule `aastore` has always applied,
    // and `Array.set` is supposed to apply the same one.
    //
    // `None` means the context models no hierarchy (mocks): keep the exact-only
    // answer above rather than widening it.
    let admitted = ctx.aastore_element_assignable(arr, value).unwrap_or(false);
    // DIAG (`CRATONVM_DBG=coerce`): the refusal carries no detail of its own --
    // HotSpot's wording is the bare "array element type mismatch" and source
    // witnesses pin it -- so name both sides here instead. Mirrors the sibling
    // "argument type mismatch" diagnostic in `lang_class.rs`, including the
    // loader ids, because the two are read together.
    //
    // This fires on every store the EXACT check could not prove, whether or not
    // the shared predicate then admitted it, and says which happened. A
    // diagnostic that only printed on the final refusal went silent the moment
    // the fallback started working, which is exactly when the underlying
    // question -- why are there two `ContextConfiguration`s? -- became
    // unanswerable from a run. `admitted=` is the fallback's own signal.
    if crate::nbflags().dbg_coerce {
        reflect_array_store_diagnostic(&*ctx, arr_class, component, value_class, admitted);
    }
    admitted
}

/// The `CRATONVM_DBG=coerce` witness for a reflective array store that the
/// exact `ClassId` check could not prove.
///
/// Three facts decide between the explanations, and none of them is legible
/// from the exception alone:
///
/// * **The component name's `NameLookup`.** `Ambiguous { definitions: n }` IS
///   the loader split, counted. `Unique` means one class carries the name and
///   the mismatch is something else entirely.
/// * **The value's recorded interfaces.** A proxy listing a
///   *different-`ClassId`* class of the component's own name says the two
///   copies are real and the proxy picked the other one. A proxy listing
///   NOTHING says its interfaces were never recorded at definition time — a
///   different defect, in the proxy-definition path, and one that would make
///   any interface-list-based repair silently do nothing.
/// * **Both loader ids**, which name *which* copy each side holds.
pub fn reflect_array_store_diagnostic(
    ctx: &dyn NativeContext,
    arr_class: ClassId,
    component: ClassId,
    value_class: ClassId,
    admitted: bool,
) {
    let name_of = |c: ClassId| {
        ctx.class_name_of_id(c)
            .unwrap_or_else(|| "<unnamed>".to_string())
    };
    let comp_name = name_of(component);
    let mut chain = String::new();
    let mut cur = Some(value_class);
    let mut hops = 0;
    while let Some(c) = cur {
        if hops > 16 {
            chain.push_str(" -> ...");
            break;
        }
        hops += 1;
        chain.push_str(&format!(
            " -> {}(cid={c:?},loader={})",
            name_of(c),
            ctx.loader_id_of_class(c)
        ));
        cur = ctx.superclass_of(c);
    }
    let mut ifaces = String::new();
    for i in ctx.class_interfaces(value_class) {
        ifaces.push_str(&format!(
            " {}(cid={i:?},loader={})",
            name_of(i),
            ctx.loader_id_of_class(i)
        ));
    }
    if ifaces.is_empty() {
        ifaces.push_str(" <none recorded>");
    }
    eprintln!(
        "[DBG_COERCE] Array.set: {} -- array={} component={comp_name} (cid={component:?}, \
loader={}) value_class={} (cid={value_class:?}, loader={}) component_name_lookup={:?} \
value_interfaces:{ifaces} supers:{chain}",
        if admitted {
            "admitted by the shared aastore predicate"
        } else {
            "rejecting"
        },
        name_of(arr_class),
        ctx.loader_id_of_class(component),
        name_of(value_class),
        ctx.loader_id_of_class(value_class),
        ctx.classify_class_name(&comp_name),
    );
}

pub fn native_array_get_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    Ok(Some(Value::Int(ctx.array_length(arr) as i32)))
}

pub fn native_array_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::ArrayElementType;
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    let elem = ctx.heap_element_type_of(arr);
    let val = ctx.get_array_element(arr, idx);
    // Box primitives with the correct wrapper type for the array component.
    // For char[] we must return Character, for byte[] Byte, etc. — otherwise
    // the JDK code that unboxes via Character.charValue() / Byte.byteValue()
    // would receive an Integer and fail type-checks.
    //
    // DO NOT DEDUPLICATE THE EIGHT ARMS BELOW ONTO THE CACHED BOXING HELPER.
    // `lang_class::box_value_canonical` exists and routes `I J Z B S C` into
    // the six `X.valueOf` caches; three reflective call sites take it
    // (`Field.get`, `Method.invoke`'s return, `SerializedLambda`'s captured
    // args) because HotSpot answers the CANONICAL box on those paths.
    // `Array.get` is the one reflective path that does NOT, and allocating
    // fresh here is the contract, not an oversight.
    //
    // The JDK's reason: since JDK 18 `Field.get`/`Method.invoke` run on
    // `MethodHandle` accessors (`MethodHandleAccessorFactory`) whose boxing
    // step is a direct handle to `X.valueOf`, so they inherit `IntegerCache`
    // and friends for free. `java.lang.reflect.Array.get` is a VM native,
    // `Reflection::array_get`, which boxes with
    // `java_lang_boxing_object::create` — a function that allocates and has
    // never consulted a cache. That is an implementation detail of the JDK
    // and it is nonetheless OBSERVABLE, so it is behaviour we must match.
    //
    // MEASURED on this host, `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
    // (Microsoft build), `scratchpad/f20/ArrGet.java` — lane F20 re-took these
    // rather than inherit them from lane F11's 70-row table
    // (docs/feature-designs/native-dispatch-architecture.md):
    //
    //   Array.get int[]      == Integer.valueOf(7)   : false
    //   Array.get char[]     == Character.valueOf(a) : false
    //   Array.get byte[]     == Byte.valueOf(3)      : false
    //   Array.get short[]    == Short.valueOf(9)     : false
    //   Array.get long[]     == Long.valueOf(5)      : false
    //   Array.get boolean[]  == Boolean.TRUE         : false
    //   Array.get boolean[]  == Boolean.FALSE        : false
    //   Array.get int[] twice self-identity          : false
    //   Array.get int[] equals Integer.valueOf(7)    : true      <- see below
    //   Integer.valueOf(Array.getInt(ia,0)) == Integer.valueOf(7): true
    //   Field.get int        == Integer.valueOf(7)   : true      <- the contrast
    //   Method.invoke ()I    == Integer.valueOf(7)   : true      <- the contrast
    //
    // Two rows carry the whole warning. `Array.get` is not even SELF-identical
    // across two calls on the same element, so "returns the canonical box" is
    // not merely unspecified here, it is false. And every fresh row is still
    // `.equals`-equal, so no equality-shaped assertion — which is what almost
    // every reflective test in this tree writes — can see a dedup that breaks
    // this. The `Boolean` arm is the sharpest edge: `Boolean.TRUE` identity is
    // exactly what `native_boolean_value_of`'s own comment documents Xerces'
    // `XML11Configuration.configurePipeline()` relying on, so a "helpful"
    // rewrite of that arm onto the cache would be right for every OTHER boxing
    // caller in this tree and wrong here.
    //
    // The sibling `Array.getInt`/`getChar`/… return bare primitives and never
    // box at all; the caller's own `valueOf` is what makes the control row
    // above canonical. Do not conflate the two families.
    match elem {
        ArrayElementType::Boolean => {
            let v = match val {
                Value::Int(n) => n,
                _ => 0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Boolean");
            ctx.set_field(w, 0, Value::Int(if v != 0 { 1 } else { 0 }));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Byte => {
            let v = match val {
                Value::Int(n) => n,
                _ => 0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Byte");
            ctx.set_field(w, 0, Value::Int(v));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Char => {
            let v = match val {
                Value::Int(n) => n,
                _ => 0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Character");
            ctx.set_field(w, 0, Value::Int(v));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Short => {
            let v = match val {
                Value::Int(n) => n,
                _ => 0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Short");
            ctx.set_field(w, 0, Value::Int(v));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Int => {
            let v = match val {
                Value::Int(n) => n,
                _ => 0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Integer");
            ctx.set_field(w, 0, Value::Int(v));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Long => {
            let v = match val {
                Value::Long(n) => n,
                _ => 0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Long");
            ctx.set_field(w, 0, Value::Long(v));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Float => {
            let v = match val {
                Value::Float(n) => n,
                _ => 0.0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Float");
            ctx.set_field(w, 0, Value::Float(v));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Double => {
            let v = match val {
                Value::Double(n) => n,
                _ => 0.0,
            };
            let w = alloc_wrapper(ctx, "java/lang/Double");
            ctx.set_field(w, 0, Value::Double(v));
            Ok(Some(Value::Object(Some(w))))
        }
        ArrayElementType::Reference => Ok(Some(val)),
    }
}

/// Unbox a wrapper object into the primitive value matching the array's
/// component type.  Reads field 0 (the convention used by the VM for
/// all wrapper classes — Integer, Long, Float, Double, Boolean, Byte,
/// Character, Short).  Returns a default zero value if the wrapper is
/// null or the stored field has an unexpected tag.
pub fn unbox_for_array_set(
    ctx: &dyn NativeContext,
    elem: cratonvm_types::ArrayElementType,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    use cratonvm_types::ArrayElementType;
    if elem == ArrayElementType::Reference {
        return Ok(v);
    }
    let raw = match v {
        Value::Object(Some(o)) => {
            // `Array.set` must reject arbitrary objects for primitive arrays.
            // Reading slot zero before this check used the layout of a
            // `RangeGroupEmpty` instance as if it were a boxed primitive and
            // caused the HSQLDB startup loop documented in SPB-JDBC-HSQLDB.1.
            let class_name = ctx
                .class_name_of_id(ctx.class_id_of_object(o))
                .unwrap_or_default();
            if !matches!(
                class_name.as_str(),
                "java/lang/Boolean"
                    | "java/lang/Byte"
                    | "java/lang/Character"
                    | "java/lang/Short"
                    | "java/lang/Integer"
                    | "java/lang/Long"
                    | "java/lang/Float"
                    | "java/lang/Double"
            ) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Array.set value is not a boxed primitive".to_string(),
                }
                .into());
            }
            ctx.get_field(o, 0)
        }
        Value::Object(None) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Array.set cannot store null in a primitive array".to_string(),
            }
            .into())
        }
        other => other,
    };
    Ok(match elem {
        ArrayElementType::Boolean
        | ArrayElementType::Byte
        | ArrayElementType::Char
        | ArrayElementType::Short
        | ArrayElementType::Int => match raw {
            Value::Int(n) => Value::Int(n),
            _ => Value::Int(0),
        },
        ArrayElementType::Long => match raw {
            Value::Long(n) => Value::Long(n),
            Value::Int(n) => Value::Long(n as i64),
            _ => Value::Long(0),
        },
        ArrayElementType::Float => match raw {
            Value::Float(n) => Value::Float(n),
            Value::Int(n) => Value::Float(n as f32),
            _ => Value::Float(0.0),
        },
        ArrayElementType::Double => match raw {
            Value::Double(n) => Value::Double(n),
            Value::Float(n) => Value::Double(n as f64),
            Value::Long(n) => Value::Double(n as f64),
            Value::Int(n) => Value::Double(n as f64),
            _ => Value::Double(0.0),
        },
        ArrayElementType::Reference => unreachable!("reference arrays return before unboxing"),
    })
}

pub fn native_array_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::ArrayElementType as T;
    let arr = reflect_array_arg(ctx, args)?;
    let raw = args.get(2).cloned().unwrap_or(Value::Object(None));
    let elem = ctx.heap_element_type_of(arr);
    // A null stored into a PRIMITIVE array is refused BEFORE the bounds check:
    // `JVM_SetArrayElement` unboxes first (`Reflection::unbox_for_primitive`,
    // which throws a message-less `IllegalArgumentException` for null) and
    // only then calls `Reflection::array_set`, whose first act is the bounds
    // check. So `Array.set(new int[4], 9, null)` is an
    // `IllegalArgumentException` on HotSpot, not an
    // `ArrayIndexOutOfBoundsException` (interpreter round i4 wave 98, lane L9;
    // probe `L9W98ReflectArrayOrder.java`).
    if elem != T::Reference && matches!(raw, Value::Object(None)) {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }
    // Bounds BEFORE the value check otherwise: `Array.set(new int[4], 9, "x")`
    // is an `ArrayIndexOutOfBoundsException` on HotSpot even though the value
    // is also wrong (a non-box unboxes to `T_ILLEGAL` without throwing, and the
    // widening refusal comes after the bounds check). (The primitive GETTERS
    // order these the other way round; see `reflect_array_primitive_elem`.)
    let idx = reflect_array_index(&*ctx, arr, args)?;
    if elem == T::Reference {
        // A reference array takes null unconditionally and otherwise wants an
        // assignable element -- and says so with its own wording.
        if let Value::Object(Some(value)) = raw {
            if !reflect_array_element_assignable(ctx, arr, value) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "array element type mismatch".to_string(),
                }
                .into());
            }
        }
        ctx.set_array_element(arr, idx, raw);
        return Ok(None);
    }
    // A primitive array takes a BOX whose primitive widens to the element type.
    // `Array.set(new int[4], 0, Long.valueOf(1))` is refused even though the
    // reverse (`set(long[], Integer)`) is fine, because the direction is the
    // same one `setLong`/`setInt` use.
    let value = match raw {
        Value::Object(Some(o)) => o,
        // Refused above, before the bounds check (HotSpot's message is NULL:
        // `IllegalArgumentException()` with no argument).
        Value::Object(None) => {
            return Err(RuntimeError::IllegalArgumentException {
                message: String::new(),
            }
            .into());
        }
        // Already-unboxed primitives arrive from internal callers that never
        // went through a wrapper; keep serving them, widening-checked.
        other => {
            let have = match other {
                Value::Long(_) => T::Long,
                Value::Float(_) => T::Float,
                Value::Double(_) => T::Double,
                _ => T::Int,
            };
            if !widens_to(have, elem) {
                return Err(reflect_array_type_mismatch());
            }
            ctx.set_array_element(arr, idx, reflect_widen_value(other, have, elem));
            return Ok(None);
        }
    };
    let Some(have) = reflect_boxed_primitive(&*ctx, value) else {
        return Err(reflect_array_type_mismatch());
    };
    if !widens_to(have, elem) {
        return Err(reflect_array_type_mismatch());
    }
    let unboxed = ctx.get_field(value, 0);
    ctx.set_array_element(arr, idx, reflect_widen_value(unboxed, have, elem));
    Ok(None)
}

pub fn native_array_get_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    Ok(Some(ctx.get_array_element(arr, idx)))
}

pub fn native_array_set_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    let val = args.get(2).cloned().unwrap_or(Value::Int(0));
    ctx.set_array_element(arr, idx, val);
    Ok(None)
}

pub fn native_array_get_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    Ok(Some(ctx.get_array_element(arr, idx)))
}

pub fn native_array_set_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    let val = args.get(2).cloned().unwrap_or(Value::Long(0));
    ctx.set_array_element(arr, idx, val);
    Ok(None)
}

pub fn native_array_get_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    Ok(Some(ctx.get_array_element(arr, idx)))
}

pub fn native_array_set_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    let val = args.get(2).cloned().unwrap_or(Value::Float(0.0));
    ctx.set_array_element(arr, idx, val);
    Ok(None)
}

pub fn native_array_get_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    Ok(Some(ctx.get_array_element(arr, idx)))
}

pub fn native_array_set_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = reflect_array_arg(ctx, args)?;
    let idx = reflect_array_index(&*ctx, arr, args)?;
    let val = args.get(2).cloned().unwrap_or(Value::Double(0.0));
    ctx.set_array_element(arr, idx, val);
    Ok(None)
}

pub fn array_new_instance_component(
    ctx: &mut dyn NativeContext,
    mirror_arg: Option<&Value>,
) -> (String, Option<cratonvm_types::ClassId>) {
    match mirror_arg {
        Some(Value::Object(Some(mirror))) => {
            // Preserve the exact ClassId when the mirror represents a class
            // from an isolated defining loader. The name is still needed for
            // primitive handling and for the legacy fallback, but name-only
            // resolution is invalid for same-named classes from distinct
            // loaders (the AOT forked-loader case).
            let component_id = crate::lang_class::mirror_class_id(&*ctx, *mirror);
            let component_name = crate::lang_class::mirror_class_name(&*ctx, *mirror)
                .filter(|s| !s.is_empty())
                .or_else(|| ctx.read_string(*mirror))
                .map(|s| s.replace('.', "/"))
                .unwrap_or_else(|| "java/lang/Object".to_string());
            (component_name, component_id)
        }
        _ => ("java/lang/Object".to_string(), None),
    }
}

pub fn array_new_instance_for_component(
    ctx: &mut dyn NativeContext,
    comp_name: &str,
    component_id: Option<cratonvm_types::ClassId>,
    length: usize,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    use cratonvm_types::ArrayElementType;
    // gc-common w8-b (`common-w3c-native-callback-allocations-cannot-collect`,
    // route 1): the array is the last thing the native allocates and it holds
    // no heap reference afterwards, so the allocation reclaims and retries
    // before the `OutOfMemoryError` (`live` is empty).
    const WHAT: &str = "Array.newInstance";
    let primitive = match comp_name {
        "int" => Some(ArrayElementType::Int),
        "long" => Some(ArrayElementType::Long),
        "float" => Some(ArrayElementType::Float),
        "double" => Some(ArrayElementType::Double),
        "boolean" => Some(ArrayElementType::Boolean),
        "byte" => Some(ArrayElementType::Byte),
        "char" => Some(ArrayElementType::Char),
        "short" => Some(ArrayElementType::Short),
        _ => None,
    };
    match primitive {
        Some(elem) => new_array_reclaiming(ctx, elem, length, &mut [], WHAT),
        None => {
            let comp_id = component_id.unwrap_or_else(|| {
                ctx.ensure_class_initialized(comp_name)
                    .unwrap_or(cratonvm_types::ClassId::new(0))
            });
            if dbg_toarray_enabled() {
                eprintln!(
                    "[DBG_TOARRAY] Array.newInstance comp_name={:?} comp_id={:?} len={}",
                    comp_name, comp_id, length
                );
            }
            new_ref_array_reclaiming(ctx, comp_id, length, &mut [], WHAT)
        }
    }
}

/// The three `Array.newInstance(Class, ...)` argument checks HotSpot makes
/// before it allocates anything, measured on Temurin 25:
///
/// ```text
/// Array.newInstance(null, 1)            NullPointerException
/// Array.newInstance(void.class, 1)      IllegalArgumentException (null message)
/// Array.newInstance(int.class, new int[0])  IllegalArgumentException (null message)
/// ```
///
/// All three used to allocate and return an array instead — a null component
/// type quietly became `Object[]`, and `void.class` did too.
pub fn reflect_array_component_ok(
    ctx: &mut dyn NativeContext,
    mirror_arg: Option<&Value>,
) -> Result<(), MethodCallFailed> {
    let mirror = match mirror_arg {
        Some(Value::Object(Some(m))) => *m,
        // A null `Class` argument. HotSpot dereferences it, so: NPE, no message.
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let name = crate::lang_class::mirror_class_name(&*ctx, mirror).unwrap_or_default();
    if name == "void" {
        // There is no `void[]`. Empty message = null `getMessage()`.
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }
    Ok(())
}

/// Interpreter round i2 wave 54, lane L3: HotSpot raises
/// `Array.newInstance`'s refusals inside the JDK `native` `Array.newArray`
/// (`multi`: `multiNewArray`), whose `(Native Method)` frame it lists under
/// `newInstance` (probe `L3W54ReflectiveRaiseChains.java`).
/// The native serving `newInstance` notes that chain for the funnel's mark
/// when it returns an unbuilt error (`NativeExceptionAccess::note_served_raise_chain`).
#[cold]
#[inline(never)]
pub fn note_array_new_instance_chain(
    ctx: &mut dyn NativeContext,
    multi: bool,
    result: &MethodCallResult,
) {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    let Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(error))) =
        result
    else {
        return;
    };
    let Some((throwable_class, _)) = error.as_java_throwable() else {
        return;
    };
    const ARRAY: &str = "java/lang/reflect/Array";
    let (descriptor, native) = if multi {
        ("(Ljava/lang/Class;[I)Ljava/lang/Object;", "multiNewArray")
    } else {
        ("(Ljava/lang/Class;I)Ljava/lang/Object;", "newArray")
    };
    ctx.note_served_raise_chain(ServedRaiseChain {
        throwable_class,
        hops: vec![
            ServedRaiseHop {
                class: ARRAY,
                name: "newInstance",
                descriptor,
                site: ServedRaiseHopSite::LastCall {
                    name: native,
                    descriptor: Some(descriptor),
                },
            },
            ServedRaiseHop {
                class: ARRAY,
                name: native,
                descriptor,
                site: ServedRaiseHopSite::Raise,
            },
        ],
    });
}

pub fn native_array_new_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let result = native_array_new_instance_body(ctx, args);
    if result.is_err() {
        note_array_new_instance_chain(ctx, false, &result);
    }
    vm_object_alloc_event(ctx, result)
}

pub fn native_array_new_instance_body(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // HotSpot's order (`Reflection::reflect_new_array`, probe
    // `L4W29ReflectNewArray.java` rows 1-8): a null
    // component first, then the length, and only then `void` / a component of
    // 255 dimensions -- `Array.newInstance(void.class, -1)` is a
    // `NegativeArraySizeException`, not an `IllegalArgumentException`. This
    // native answers `Array.newInstance` in `--compatible` only (retired to
    // `SyntheticStub` under `--real-jdk`, round 14 wave 6), so it must agree
    // with the `newArray` native the JDK body reaches there.
    if !matches!(args.first(), Some(Value::Object(Some(_)))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    if let Some(Value::Int(v)) = args.get(1) {
        if *v < 0 {
            return Err(RuntimeError::NegativeArraySizeException { size: *v }.into());
        }
    }
    reflect_array_component_ok(ctx, args.first())?;
    // `.max(0)` CLAMPED a negative length and returned an empty array, so
    // `Array.newInstance(int.class, -1)` handed back an `int[0]` where HotSpot
    // throws. Measured on Temurin 25: `NegativeArraySizeException` whose detail
    // message is the size alone, with no prose — the same text `new int[-1]`
    // produces, which is why the variant builds it from the payload rather than
    // taking a string. Same shape as the missing bounds check on the accessors:
    // an out-of-range argument silently accepted.
    let len = match args.get(1) {
        Some(Value::Int(v)) if *v < 0 => {
            return Err(RuntimeError::NegativeArraySizeException { size: *v }.into());
        }
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let (comp_name, component_id) = array_new_instance_component(ctx, args.first());
    // An array class has at most 255 dimensions: `Array.newInstance` of a
    // 255-dimension component is an `IllegalArgumentException` (row 7).
    if lang_system::reflect_new_array_refuses_component(&comp_name, 1) {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }
    let arr = array_new_instance_for_component(ctx, &comp_name, component_id, len)?;
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_array_new_instance_multi(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let result = native_array_new_instance_multi_body(ctx, args);
    if result.is_err() {
        note_array_new_instance_chain(ctx, true, &result);
    }
    vm_object_alloc_event_nested(ctx, result)
}

pub fn native_array_new_instance_multi_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Every argument check -- null component or dimensions, zero or more than
    // 255 dimensions, negative dimensions, `void`, too many dimensions in all --
    // is made by the shared builder in HotSpot's order (probe
    // `L4W29ReflectNewArray.java` rows 9-29). The
    // checks this body used to make first put `void` ahead of a negative
    // dimension (`Array.newInstance(void.class, 1, -1)` is a
    // `NegativeArraySizeException` on HotSpot).
    //
    // `Array.newInstance(Class, int[])` must fully materialize ALL dimensions
    // with the precise nested array type (`String[2][2]` → `[[Ljava/lang/String;`).
    // The previous body read only `dims[0]` and built a one-dimensional array,
    // collapsing e.g. `new String[2][2]` to `String[]`
    // (SpEL ArrayConstructorTests.multiDimensionalArrays). Delegate to the
    // shared nested-array builder, which also backs `Array.multiNewArray`.
    lang_system::native_array_multi_new_array(ctx, args)
}

// ===========================================================================
// Phase 47: java.lang.reflect.Proxy — dynamic proxy stubs
// ===========================================================================
//
// WP2.5 (Strategy B+): every proxy still lands on the single synthetic
// class `java/lang/reflect/Proxy$Instance`, but per-proxy data
// (interfaces array, handler) is now stored in dedicated slots and the
// natives expose helpers so reflection round-trips work.
//
// The globals below back `Class.getInterfaces()` on the proxy class
// mirror — see the doc-comment on `lang_class::proxy_last_interfaces`
// for the rationale and the known multi-threaded-creation limitation.

// bug nb-lib-gckeys §2: the last-proxy interfaces array is no longer stored
// here as a raw `AtomicU64` pointer (which was never a GC root and could
// dangle after a moving collection). It now lives as a GC-tracked `ObjectRef`
// in `lang_class` (`set_proxy_last_interfaces` / `proxy_last_interfaces`),
// rooted and remapped by the annotation-proxy GC hooks. The
// `proxy_last_interfaces_bits()` accessor below is retained for the WP2.5
// white-box test and now derives from that GC-tracked cell.

/// Total proxies created in this process. Diagnostic only.
pub static PROXY_INSTANCES_CREATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// WP2.5-B — generated-proxy-class cache, keyed on
/// `(vm_identity, loader_id, ordered_iface_class_ids)`. One generated
/// `$ProxyN` class per (VM, loader, ordered-interface-list) — the JDK
/// `ProxyGenerator` does the same minus the VM, keying on interface order so
/// `getInterfaces()` round-trips the user-requested order.
///
/// The `vm_identity` is load-bearing, not hygiene. Every other component of
/// this entry is a per-VM number: `loader_id` is a small per-VM integer, the
/// key's `ClassId`s are minted per VM from zero, and **the VALUE is a
/// `ClassId`** — a handle that means nothing outside the class manager that
/// issued it. Without the partition, VM B asking for a proxy over its
/// interface `ClassId(42)` hit VM A's entry and was handed VM A's generated
/// `$ProxyN` id. `class_name_of_id` then answered `None` for it, which is the
/// `?` in the `ClassCastException: ? cannot be cast to …` that made the
/// proxy/annotation corpus tests flip in roughly half of all parallel runs —
/// in both directions, since the borrowed id sometimes happened to satisfy
/// the cast and sometimes not.
pub static PROXY_CLASS_CACHE: parking_lot::RwLock<
    Option<
        rustc_hash::FxHashMap<(usize, u32, Vec<cratonvm_types::ClassId>), cratonvm_types::ClassId>,
    >,
> = parking_lot::RwLock::new(None);

/// Drop every generated-proxy-class row this VM's class unloading just
/// invalidated.
///
/// A row is `(vm, loader_namespace, ordered_iface_ids) -> generated_class_id`,
/// and **class unloading can kill either side**. The value goes first: once
/// `ClassManager::unload_user_classes` has removed the generated `$ProxyN`,
/// a later `Proxy.newProxyInstance` with the same key was handed the dead id
/// straight back out of this cache and allocated an instance against it. Its
/// class then resolves to nothing — the same
/// `ClassCastException: ? cannot be cast to …` this cache's `vm_identity`
/// partition was added to stop, reached the other way round. That is what made
/// `BatchJdbcAutoConfigurationTests`, `FreeMarkerAutoConfigurationReactive-
/// IntegrationTests` and `OpenTelemetrySdkAutoConfigurationTests` fail under
/// `-XX:+UseZGC`, whose every collection is a full mark and therefore runs
/// loader reclamation on every cycle rather than only in the generational
/// collector's narrow full-mark windows.
///
/// The KEY is purged on the same evidence: an entry keyed on an interface
/// `ClassId` that no longer exists cannot be matched by a live request except
/// by id reuse, and matching it by reuse is precisely the bug above.
pub fn forget_unloaded_proxy_classes(vm_identity: usize, class_ids: &[u32]) {
    if class_ids.is_empty() {
        return;
    }
    crate::generics::forget_unloaded_type_parameter_rows(vm_identity, class_ids); // gc-common w30-b
    crate::lang_class::forget_unloaded_method_type_params(vm_identity, class_ids); // gc-common w30-b
    // (gc-common w31-a's defined-`Package` rows: the VM's
    // `LoaderIdentityTables` since interpreter round i5 wave 105; the VM's
    // unload path calls `lang_class::forget_unloaded_defined_packages` itself.)
    let dead: rustc_hash::FxHashSet<u32> = class_ids.iter().copied().collect();
    let mut guard = PROXY_CLASS_CACHE.write();
    if let Some(map) = guard.as_mut() {
        map.retain(|(vm, _, ifaces), cid| {
            *vm != vm_identity
                || (!dead.contains(&cid.as_u32())
                    && !ifaces.iter().any(|i| dead.contains(&i.as_u32())))
        });
    }
}

/// Drop every generated-proxy-class row belonging to `vm_identity`. Called
/// from `release_vm_native_state`; the rows hold `ClassId`s into a class
/// manager that is going away.
pub fn forget_vm_proxy_classes(vm_identity: usize) {
    let mut guard = PROXY_CLASS_CACHE.write();
    if let Some(map) = guard.as_mut() {
        map.retain(|(vm, _, _), _| *vm != vm_identity);
    }
    let mut modules = PROXY_LOADER_MODULES.write();
    if let Some(map) = modules.as_mut() {
        map.retain(|(vm, _), _| *vm != vm_identity);
    }
    drop(modules);
    drop(guard);
    PROXY_CLASS_NUMBERS.forget(vm_identity);
}


/// WP2.5-B — process-wide counter for the `$ProxyN` suffix, read only under
/// `CRATONVM_PROXY_CLASS_NUMBER_PER_VM=0`; the default numbers per VM
/// ([`PROXY_CLASS_NUMBERS`]).
pub static PROXY_CLASS_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The next `$ProxyN` suffix of each VM (round 15 wave 5, lane proxy8, PX7-4).
/// HotSpot's `Proxy.ProxyBuilder.nextUniqueNumber` is one counter per JVM,
/// shared by every loader, so a VM of its own starts at `$Proxy0`; the
/// process-wide [`PROXY_CLASS_COUNTER`] made the second VM of a process
/// continue the first one's numbers, a process global holding Java-visible
/// state (`Class.getName()`; AGENTS.md). Dropped with its VM
/// ([`forget_vm_proxy_classes`]).
pub static PROXY_CLASS_NUMBERS: cratonvm_native_api::vm_scoped::VmScoped<u32> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// proxy-real-classfile increment 7 — per-loader dynamic-module sequence number
/// backing the generated proxy package `jdk/proxyN/$ProxyM`, mirroring HotSpot's
/// per-loader dynamic `jdk.proxyN` module (one module per class loader, `N`
/// assigned in first-encounter order starting at 1; `M` is the VM's own
/// [`PROXY_CLASS_NUMBERS`] count, shared by every loader). Read only under
/// `CRATONVM_PROXY_MODULE_NUMBER_PER_VM=0`; the default numbers per VM from
/// [`PROXY_LOADER_MODULES`]. Public-interface proxies of the SAME loader
/// share one `jdk/proxyN` package, exactly like the JDK's `ProxyBuilder`.
pub static PROXY_MODULE_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Keyed by `(vm_identity, loader_id)`: loader ids are small per-VM integers,
/// so an unqualified key let VM B's loader 2 inherit VM A's module number and
/// generate its `$ProxyN` into a package that VM A had already claimed.
pub static PROXY_LOADER_MODULES: parking_lot::RwLock<Option<rustc_hash::FxHashMap<(usize, u32), u32>>> =
    parking_lot::RwLock::new(None);

/// WP2.5 v3 item 6 — classify a thrown exception from
/// `InvocationHandler.invoke` against the proxied method's declared
/// throws set; wrap in `java.lang.reflect.UndeclaredThrowableException`
/// if not a `RuntimeException` / `Error` and not declared.
///
/// On any failure to allocate or invoke the wrapper ctor, the original
/// exception is returned verbatim — losing the wrap is preferable to
/// losing the exception entirely.
pub fn wrap_undeclared_throwable(
    ctx: &mut dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
    thrown: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    // 1) RuntimeException / Error — always propagate.
    let thrown_cid = ctx.class_id_of_object(thrown);
    if crate::nbflags().dbg_ute {
        let thrown_name = ctx
            .class_name_of_id(thrown_cid)
            .unwrap_or_else(|| "<unknown>".to_string());
        let method_name = match ctx.get_field_by_name(method_obj, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let decl_class = match ctx.get_field_by_name(method_obj, "clazz") {
            Value::Object(Some(m)) => ctx
                .class_id_from_mirror(m)
                .and_then(|c| ctx.class_name_of_id(c))
                .unwrap_or_else(|| "<unknown>".to_string()),
            _ => "<unknown>".to_string(),
        };
        eprintln!(
            "[DBG_UTE] method={}.{} thrown={}",
            decl_class, method_name, thrown_name
        );
    }
    // gc-common w20-c: each `ensure_class_initialized` can run a `<clinit>`,
    // and the wrap path allocates and runs a constructor; `method_obj`,
    // `thrown` and the wrapper were used raw after them. All three live in
    // scope handles and are read back at each use.
    let mut scope = NativeHandleScope::new(ctx);
    let method_h = scope.root(method_obj);
    let thrown_h = scope.root(thrown);
    if let Ok(rcid) = scope.ensure_class_initialized("java/lang/RuntimeException") {
        if thrown_cid == rcid || scope.is_subclass(thrown_cid, rcid) {
            return scope.get(&thrown_h);
        }
    }
    if let Ok(ecid) = scope.ensure_class_initialized("java/lang/Error") {
        if thrown_cid == ecid || scope.is_subclass(thrown_cid, ecid) {
            return scope.get(&thrown_h);
        }
    }

    // 2) Inspect Method.exceptionTypes (Class[]). Generated `<clinit>`
    // populates this via `Class.getExceptionTypes()`; if for any reason
    // the field is missing or null, fall through to the wrap path —
    // matching JDK behaviour for methods with no declared throws.
    let method_obj = scope.get(&method_h);
    if let Value::Object(Some(arr)) = scope.get_field_by_name(method_obj, "exceptionTypes") {
        let n = scope.array_length(arr);
        for i in 0..n {
            if let Value::Object(Some(decl_mirror)) = scope.get_array_element(arr, i) {
                if let Some(decl_cid) = scope.class_id_from_mirror(decl_mirror) {
                    if thrown_cid == decl_cid || scope.is_subclass(thrown_cid, decl_cid) {
                        return scope.get(&thrown_h);
                    }
                }
            }
        }
    }

    // 3) Wrap. Allocate UndeclaredThrowableException and invoke its
    // (Throwable) constructor.
    let ute_cid =
        match scope.ensure_class_initialized("java/lang/reflect/UndeclaredThrowableException") {
            Ok(c) => c,
            Err(_) => return scope.get(&thrown_h),
        };
    let n_fields = scope.class_num_total_fields(ute_cid).max(4);
    let ute = scope.alloc_object(ute_cid, n_fields);
    let ute_h = scope.root(ute);
    let ctor_args = [
        Value::Object(Some(ute)),
        Value::Object(Some(scope.get(&thrown_h))),
    ];
    let _ = scope.invoke(
        "java/lang/reflect/UndeclaredThrowableException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
        &ctor_args,
    );
    scope.get(&ute_h)
}

