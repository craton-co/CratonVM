// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VM-specific GC helpers.
//!
//! The core GC algorithm lives in the `cratonvm-gc` crate. This module
//! provides the VM-specific `update_all_roots` function and re-exports
//! the gc crate's types for backward compatibility.

use std::collections::HashMap;

// Re-export everything from the gc crate's gc module.
pub use cratonvm_gc::gc::*;

use crate::types::ObjectRef;
#[cfg(test)]
use crate::types::Value;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size};

/// Result of one VM metadata-unloading transaction.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ClassMetadataUnloadResult {
    pub loaders_unloaded: usize,
    pub classes_unloaded: usize,
    pub jit_entries_retired: usize,
}

/// Complete the metadata half of class-loader unloading after GC has proved
/// the defining loader objects unreachable.
///
/// ClassIds remain monotonic tombstones. Every VM-owned cache is either
/// invalidated by exact ClassId or conservatively flushed when it lacks an
/// ownership index.
pub fn unload_dead_class_metadata(
    shared: &crate::vm::SharedVm,
    dead_class_hints: &[u32],
) -> ClassMetadataUnloadResult {
    use crate::classloading::{ClassId, ClassLoaderId};
    use rustc_hash::FxHashSet;

    if dead_class_hints.is_empty() {
        return ClassMetadataUnloadResult::default();
    }

    let dead_ids: Vec<ClassId> = {
        let cm = shared.classes.class_manager.read();
        dead_class_hints
            .iter()
            .map(|id| ClassId::new(*id))
            .filter(|id| {
                cm.get_loader_id(*id)
                    .is_some_and(|loader| matches!(loader, ClassLoaderId::UserDefined(_)))
            })
            .collect()
    };
    if dead_ids.is_empty() {
        cratonvm_native_builtins::classloader::forget_unloaded_classes(
            shared.vm_identity,
            dead_class_hints,
        );
        return ClassMetadataUnloadResult::default();
    }

    let unloaded = {
        let mut cm = shared.classes.class_manager_write();
        cm.unload_user_classes(&dead_ids)
    };
    if unloaded.is_empty() {
        cratonvm_native_builtins::classloader::forget_unloaded_classes(
            shared.vm_identity,
            dead_class_hints,
        );
        return ClassMetadataUnloadResult::default();
    }

    let loaders_unloaded = unloaded
        .iter()
        .map(|class| class.loader_id)
        .collect::<FxHashSet<_>>()
        .len();
    let ids: FxHashSet<ClassId> = unloaded.iter().map(|class| class.id).collect();
    let raw_ids: Vec<u32> = unloaded.iter().map(|class| class.id.as_u32()).collect();

    shared
        .classes
        .statics
        .write()
        .retain(|id, _| !ids.contains(id));
    shared
        .classes
        .class_locks
        .write()
        .retain(|id, _| !ids.contains(id));
    shared
        .classes
        .field_descriptor_cache
        .write()
        .retain(|(id, _), _| !ids.contains(id));
    shared
        .classes
        .class_init_waiters
        .lock()
        .retain(|id, _| !ids.contains(id));
    shared
        .classes
        .lambda_proxies
        .write()
        .retain(|id, _| !ids.contains(id));
    shared
        .classes
        .lambda_proxy_hosts
        .write()
        .retain(|proxy, host| !ids.contains(proxy) && !ids.contains(host));

    let dead_mirrors: Vec<ObjectRef> = {
        let mut mirrors = shared.classes.class_mirrors.write();
        ids.iter().filter_map(|id| mirrors.remove(id)).collect()
    };
    shared
        .classes
        .class_mirrors_reverse
        .write()
        .retain(|_, id| !ids.contains(id));
    cratonvm_native_builtins::classloader::forget_unloaded_class_mirrors(&dead_mirrors);

    {
        // Second writer of the initiating-resolution memo; advance the
        // resolution generation so the interpreter's resolved-field site cache
        // treats its entries as stale (see
        // `runtime::interpreter::constants`'s `RESOLUTION_EPOCH`).
        crate::runtime::interpreter::bump_resolution_epoch();
        let mut cache = shared.classes.initiating_resolution_cache.write();
        cache.retain(|_, entries| {
            entries.retain(|_, id| !ids.contains(id));
            !entries.is_empty()
        });
    }

    // These caches are pure memoizers. A conservative clear is preferable to
    // retaining a value that mentions an unloaded class through an indirect
    // target not represented in its key.
    //
    // ARCH-2026-07-26 (request CR-LR-1 of
    // `arch-2026-07-26/stackwalk-and-vtable.md`): this used to
    // call `invalidate_all()`, which takes THREE write locks — but two of them
    // guard `SharedResolutionState::global_methods` / `global_fields`, whose
    // only writers (`cache_method` / `cache_field`) have no production callers,
    // so those maps are always empty here. Re-verified on this tree: the sole
    // external consumers of `shared_resolution` are this line and the
    // interpreter's `promoted_*` paths. `invalidate_promoted()` clears exactly
    // the live cache, and — more importantly — stops this call site implying
    // that the other two maps are live.
    shared.classes.shared_resolution.invalidate_promoted();
    shared.classes.osc_cache.remove_classes(&ids);

    let mut jit_entries_retired = 0;
    {
        // PERF (ARCH-2026-07-26, request CR-VT-1 of
        // `arch-2026-07-26/stackwalk-and-vtable.md`).
        // `unload_class` calls `invalidate_class`, which sweeps EVERY slot of
        // EVERY vtable in the VM — so a per-class loop here costs
        // O(unloaded x all_classes x slots_per_class) under the manager write
        // lock: for a few hundred unloaded classes in a VM holding tens of
        // thousands, hundreds of millions of slot visits at a moment when every
        // dispatching thread is waiting on this lock. `unload_classes` does one
        // sweep for the whole batch and is proven equivalent to the loop by
        // `vtable::tests::unload_classes_matches_a_loop_of_unload_class`.
        let dead: Vec<u64> = unloaded.iter().map(|c| c.id.as_u32() as u64).collect();
        let mut vtables = shared.classes.vtable_manager.write();
        vtables.unload_classes(&dead);
    }
    for class in &unloaded {
        shared
            .jit
            .jit_alloc_class_cache
            .invalidate(class.id.as_u32());
        shared.jit.profile_store.invalidate_class(class.id.as_u32());
        shared
            .jit
            .tiered_manager
            .invalidate_class(class.name.as_ref());
        // And purge the broker: an unloaded class must lose its tracked
        // requests as well as its epoch, so `purge_class` rather than
        // `invalidate`. In-flight records are deliberately KEPT by that
        // call so their pre-unload epoch refuses the body coming back.
        let _ = shared
            .jit
            .compilation_broker
            .lock()
            .purge_class(class.name.as_ref());
        shared.jit.deopt_log.lock().clear_class(class.name.as_ref());
        jit_entries_retired += shared
            .jit
            .jit_cache
            .invalidate_unloaded_class(class.id, class.name.as_ref());
    }
    shared.jit.invalidation_manager.lock().clear_all();
    // Same reason as `jit_alloc_class_cache` above: the multianewarray
    // per-site plans are keyed by a class id and hold component class ids,
    // and a recycled id would hand a site the wrong component classes.
    crate::runtime::interpreter::invalidate_multianewarray_plans();
    shared
        .jit
        .jit_skip_set
        .write()
        .retain(|(class_name, _, _)| {
            !unloaded
                .iter()
                .any(|class| class.name.as_ref() == class_name.as_ref())
        });

    cratonvm_native_builtins::classloader::forget_unloaded_classes(shared.vm_identity, &raw_ids);
    // The generated-`$ProxyN` cache holds `ClassId`s on BOTH sides of its
    // rows and nothing else invalidates it, so its rows outlive the classes
    // they name. A `$ProxyN` unloaded with its loader was handed straight back
    // out of that cache on the next `Proxy.newProxyInstance` with the same
    // (loader-namespace, interfaces) key, and the instance was allocated
    // against a `ClassId` this function had already removed from the class
    // store — after which its class resolves to nothing and the cast at the
    // call site fails with `ClassCastException: ? cannot be cast to …`.
    //
    // Deliberately here on the ONLY path that actually removed classes, not
    // inside `forget_unloaded_classes`: the two early exits above call that
    // helper with the *hints* on paths where nothing was unloaded, and
    // purging valid rows there would just churn a fresh `$ProxyN` per
    // collection for classes that are still perfectly alive.
    cratonvm_native_builtins::forget_unloaded_proxy_classes(shared.vm_identity, &raw_ids);
    // ...and the level below it: the annotation-proxy cache holds INSTANCES of
    // those generated `$ProxyN` classes, keyed by holder class id, and is
    // itself a GC root — so the collector keeps each cached proxy alive while
    // nothing keeps its class alive. A proxy whose class was just unloaded is
    // handed straight back out of that cache by the next
    // `getDeclaredAnnotations()`, and resolving its class throws
    // `NoClassDefFoundError: jdk/proxyN/$ProxyM`. Spring's `AnnotationsScanner`
    // catches that and returns NO annotations, so every `@AutoConfiguration` /
    // `@Conditional` / `@Bean` on the holder silently disappears — see
    // `lang_class::forget_unloaded_annotation_proxies` for the suite failure
    // this reproduced as. Same placement rationale as the call above: only on
    // the path that actually unloaded classes.
    let dead_set: FxHashSet<u32> = raw_ids.iter().copied().collect();
    let annotation_proxies_dropped =
        cratonvm_native_builtins::lang_class::forget_unloaded_annotation_proxies(
            shared.vm_identity,
            &dead_set,
            &|obj| {
                shared
                    .mem
                    .heap
                    .is_object_address(obj.as_ptr() as usize)
                    .map(|valid| shared.mem.heap.class_id_of(valid).as_u32())
            },
        );
    if annotation_proxies_dropped != 0
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIRRORPIN").is_some()
    {
        eprintln!(
            "[DBG_MIRRORPIN] dropped {annotation_proxies_dropped} annotation-proxy cache              row(s) whose class or holder was unloaded"
        );
    }
    shared
        .debug
        .diagnostic_counters
        .classes_unloaded
        .fetch_add(unloaded.len() as u64, std::sync::atomic::Ordering::Relaxed);

    ClassMetadataUnloadResult {
        loaders_unloaded,
        classes_unloaded: unloaded.len(),
        jit_entries_retired,
    }
}

/// Post-GC reconciliation of the class-mirror cache (companion to
/// `roots.rs` step 6, which stops unconditionally rooting a user-defined
/// class's `java.lang.Class` mirror once `CRATONVM_LOADER_UNLOAD` is on —
/// see that comment for the full rationale). A mirror that did not survive
/// this collection must be dropped from `SharedVm::class_mirrors` /
/// `class_mirrors_reverse` here, BEFORE `update_all_roots`' own step 6/14
/// remap runs over the map — otherwise the cache would keep a stale
/// `ObjectRef` pointing at memory the collector already reclaimed or moved,
/// and the next `getClass()` on that class id would hand back a dangling
/// reference instead of lazily recreating the mirror.
///
/// `is_marked(addr)` MUST be the same survivor predicate reference
/// processing uses this cycle (mirrors `gc_reconcile_defining_loaders`),
/// so a mirror is pruned exactly when a weak/phantom reference to it would
/// be cleared. Safe to call unconditionally (including with
/// `CRATONVM_LOADER_UNLOAD=0`): every entry is still rooted in that mode, so
/// `is_marked` is always true and nothing is pruned.
pub fn reconcile_class_mirrors(
    shared: &crate::vm::SharedVm,
    is_marked: &dyn Fn(usize) -> bool,
    cycle_roots: Option<&[ObjectRef]>,
) {
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIRRORPIN").is_some();
    let mut mirrors = shared.classes.class_mirrors.write();
    if dbg {
        let cm = shared.classes.class_manager.read();
        for (&class_id, obj_ref) in mirrors.iter() {
            let name = cm
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            let addr = obj_ref.as_ptr() as usize;
            eprintln!(
                "[DBG_MIRRORPIN] reconcile class={:?} cid={:?} mirror_addr={:#x} is_marked={}",
                name,
                class_id,
                addr,
                is_marked(addr)
            );
        }
        // TEMP-DIAG: for a still-marked JSP mirror, name the chain that is
        // keeping it alive. "Still marked" alone cannot distinguish a direct
        // root from a live heap edge, and this test has been closed three times
        // on the direct-root half of that fork.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIRRORPIN_WHY").is_some() {
            for (&class_id, obj_ref) in mirrors.iter() {
                let name = cm
                    .get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                if !name.starts_with("org/apache/jsp/") {
                    continue;
                }
                let addr = obj_ref.as_ptr() as usize;
                // WHICH ARM of the survivor verdict says "live"? Every rooting
                // lever is inert and the object has no referrer, no instance
                // and no root, so the live possibility is that the mirror is
                // genuinely dead and the VERDICT is wrong.
                let (old_alloc, young_surv, region) = shared.mem.heap.liveness_arms(addr);
                eprintln!(
                    "[MIRRORWHY] {name} mirror={addr:#x} is_marked={} region={region} \
                     old_gen_allocated={old_alloc} young_survivor={young_surv}",
                    is_marked(addr)
                );
                if let Some(loader) = cratonvm_native_builtins::classloader::defining_loader_for(
                    shared.vm_identity,
                    class_id.as_u32(),
                ) {
                    let la = loader.as_ptr() as usize;
                    let (lo, ly, lr) = shared.mem.heap.liveness_arms(la);
                    eprintln!(
                        "[MIRRORWHY] {name} loader={la:#x} is_marked={} region={lr} \
                         old_gen_allocated={lo} young_survivor={ly}",
                        is_marked(la)
                    );
                }
                if !is_marked(addr) {
                    continue;
                }
                // `root_held_paths` stops a branch only at a zero-referrer
                // node — a genuine GC root and a side-table-propagated
                // object (mirror_pin/loader_pin/metadata_pin/an overlay
                // owner edge) both have zero heap referrers and land in the
                // exact same bucket, so a "no heap referrer, root=<not-a-
                // direct-root>" line can never tell them apart: it is what
                // BOTH a legitimately-live control loader and a wrongly-
                // retained one print. `cycle_roots` is this collection's
                // OWN root vector (the one actually handed to the marker,
                // threaded down from the `collect_roots` call site that
                // started this cycle) -- checking membership in it directly
                // is the one predicate that can actually say which of the
                // two this is.
                let real_root_addrs: std::collections::HashSet<usize> = cycle_roots
                    .map(|roots| roots.iter().map(|r| r.as_ptr() as usize).collect())
                    .unwrap_or_default();
                let is_real_root = |a: usize| real_root_addrs.contains(&a);
                let render = |paths: &Vec<Vec<(usize, u32)>>, tag: &str| {
                    eprintln!("[MIRRORWHY] {name} {tag} root_held_paths={}", paths.len());
                    for path in paths.iter().take(8) {
                        let rendered: Vec<String> = path
                            .iter()
                            .map(|&(a, cid)| {
                                let n = cm
                                    .get_class(cratonvm_types::ClassId::new(cid))
                                    .map(|c| c.name.to_string())
                                    .unwrap_or_else(|| format!("cid{cid}"));
                                format!("{n}@{a:#x}")
                            })
                            .collect();
                        // Name the ROOT SOURCE that put the head of this path
                        // in the root set (`CRATONVM_DBG_ROOT_SOURCE=1`).
                        // Without it the path says what holds the object and
                        // stops exactly where the answer is: a head with no
                        // parent is a root, and "which root" is the whole
                        // question. `<not-a-direct-root>` used to be printed
                        // for BOTH a genuine root this table doesn't name (a
                        // thread frame, a static field, a class mirror/lock)
                        // AND a side-table-propagated object with no heap
                        // referrer at all — `verified_root` below (this
                        // cycle's actual root vector, not an inference) is
                        // what tells those apart.
                        let head_addr = path.first().map(|&(a, _)| a);
                        let verified_root = head_addr.is_some_and(is_real_root);
                        let src = head_addr
                            .and_then(crate::memory::native_roots::root_source_of)
                            .unwrap_or(if verified_root {
                                "<root-not-named-by-VM_ROOT_SOURCES>"
                            } else {
                                "<NOT-A-ROOT-no-heap-referrer>"
                            });
                        eprintln!(
                            "[MIRRORWHY]   [root={src} verified_root={verified_root}] {}",
                            rendered.join(" -> ")
                        );
                    }
                };
                render(
                    &shared
                        .mem
                        .heap
                        .retention_paths(addr, &is_real_root, 200_000, 8),
                    &format!("mirror={addr:#x}"),
                );
                // The mirror has no heap referrer; it is marked because
                // `mirror_pin` propagates from its LOADER. So the real
                // question is what keeps the LOADER alive.
                if let Some(loader) = cratonvm_native_builtins::classloader::defining_loader_for(
                    shared.vm_identity,
                    class_id.as_u32(),
                ) {
                    let laddr = loader.as_ptr() as usize;
                    eprintln!(
                        "[MIRRORWHY] {name} loader={laddr:#x} loader_marked={}",
                        is_marked(laddr)
                    );
                    render(
                        &shared
                            .mem
                            .heap
                            .retention_paths(laddr, &is_real_root, 200_000, 8),
                        &format!("loader={laddr:#x}"),
                    );
                } else {
                    eprintln!("[MIRRORWHY] {name} loader=NONE (defining_loader_for pruned)");
                }
                // The mirror/loader having no heap referrer is EXPECTED and
                // says nothing: instance->class is the header's class_id, not
                // a ref slot, and instance->loader is `loader_pin`, a side
                // table. Both are invisible to a ref-slot walk. What actually
                // pins the loader is a live INSTANCE of one of its classes, so
                // enumerate those and say who holds them.
                let mut live_instances: Vec<(usize, u32)> = Vec::new();
                for (obj_ptr, _size) in shared.mem.heap.walk_objects() {
                    // SAFETY: walk_objects yields live object starts.
                    let cid = unsafe { &*(obj_ptr as *const cratonvm_gc::ObjectHeader) }
                        .class_id
                        .as_u32();
                    if cratonvm_types::loader_pin::loader_pin_addr(cid)
                        == cratonvm_native_builtins::classloader::defining_loader_for(
                            shared.vm_identity,
                            class_id.as_u32(),
                        )
                        .map(|l| l.as_ptr() as usize)
                    {
                        live_instances.push((obj_ptr as usize, cid));
                    }
                }
                eprintln!(
                    "[MIRRORWHY] {name} live_instances_of_this_loader={}",
                    live_instances.len()
                );
                // Zero live instances does not mean "nothing propagates to
                // it": the owner-based collection-overlay edge (fixed this
                // session, `external_roots_for_owner`) is invisible to both
                // `retention_paths`'s heap-field-only reverse walk AND to
                // the live-instance enumeration above, because it is a
                // native side-table edge, not a Java object field. Ask
                // directly whether the mirror/loader address is currently
                // an ELEMENT of any overlay-backed collection at all — the
                // unconditional scan sees every element regardless of which
                // owner (if any) is still alive, so a hit here says "some
                // overlay holds this," not yet "and that owner is live,"
                // but it is the one thing neither check above can rule out.
                let mut all_overlay_elems: Vec<crate::types::ObjectRef> = Vec::new();
                cratonvm_gc::external_roots::scan_external_roots(&mut all_overlay_elems);
                let mirror_in_overlay = all_overlay_elems
                    .iter()
                    .any(|r| r.as_ptr() as usize == addr);
                eprintln!(
                    "[MIRRORWHY] {name} mirror_is_overlay_element={mirror_in_overlay} \
                     total_overlay_elements={}",
                    all_overlay_elems.len()
                );
                if let Some(loader) = cratonvm_native_builtins::classloader::defining_loader_for(
                    shared.vm_identity,
                    class_id.as_u32(),
                ) {
                    let laddr = loader.as_ptr() as usize;
                    let loader_in_overlay = all_overlay_elems
                        .iter()
                        .any(|r| r.as_ptr() as usize == laddr);
                    eprintln!("[MIRRORWHY] {name} loader_is_overlay_element={loader_in_overlay}");
                }
                for &(iaddr, icid) in live_instances.iter().take(4) {
                    let iname = cm
                        .get_class(cratonvm_types::ClassId::new(icid))
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("cid{icid}"));
                    render(
                        &shared
                            .mem
                            .heap
                            .retention_paths(iaddr, &is_real_root, 200_000, 4),
                        &format!("instance {iname}@{iaddr:#x}"),
                    );
                }
            }
        }
    }
    mirrors.retain(|_class_id, obj_ref| is_marked(obj_ref.as_ptr() as usize));
}

/// Rebuild `cratonvm_types::mirror_pin`'s (loader address -> defined mirror
/// addresses) registry from the authoritative, just-pruned `class_mirrors`
/// cache and the defining-loader side-table, so the GC marker's mirror_pin
/// checks (`gen_heap.rs`, see `vm::memory::roots` step 6) see current
/// addresses on the next collection.
///
/// Call this AFTER `reconcile_class_mirrors` and
/// `gc_reconcile_defining_loaders` have both run this cycle. Cheap to call
/// unconditionally: bounded by `class_mirrors.len()`, each entry a single
/// `defining_loader_for` hash lookup that returns `None` (skipped) for every
/// built-in-loader class — the overwhelmingly common case.
pub fn rebuild_mirror_pins(shared: &crate::vm::SharedVm, pointer_map: &cratonvm_types::PointerMap) {
    let class_mirrors = shared.classes.class_mirrors.read();
    let mut entries: Vec<(usize, usize)> = Vec::new();
    for (&class_id, mirror_ref) in class_mirrors.iter() {
        if let Some(loader) = cratonvm_native_builtins::classloader::defining_loader_for(
            shared.vm_identity,
            class_id.as_u32(),
        ) {
            let old_mirror_addr = mirror_ref.as_ptr() as usize;
            let mirror_addr = pointer_map
                .get(&old_mirror_addr)
                .copied()
                .unwrap_or(old_mirror_addr);
            entries.push((loader.as_ptr() as usize, mirror_addr));
        }
    }
    drop(class_mirrors);
    cratonvm_types::mirror_pin::replace_mirror_pins(shared.vm_identity, &entries);
}

/// Update all root locations in the VM state after a GC collection.
///
// ALTRACE (julgc-rootscan investigation 20260721): env-gated helpers shared
// across the vm crate for the deterministic GC-stress corruption hunt.
pub(crate) fn altrace_enabled_vm() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ALTRACE").is_some())
}

pub(crate) fn watch_addr() -> Option<usize> {
    use std::sync::OnceLock;
    static W: OnceLock<Option<usize>> = OnceLock::new();
    *W.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_DBG_WATCHADDR")
            .ok()
            .and_then(|s| usize::from_str_radix(s.trim_start_matches("0x"), 16).ok())
    })
}

// GCPART (stw-residual-close 20260722): env-gated ring of the last few
// relocation pointer maps. A stale-ref capture probes it to distinguish
// "this address WAS relocated at epoch E to X but the holding slot missed
// the remap" from "never relocated in any recent cycle (root-scan miss /
// young-space reuse)". Gate: CRATONVM_DBG_GCPART; debug-only.
pub(crate) fn gcpart_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_GCPART").is_some())
}

#[allow(clippy::type_complexity)]
fn gcpart_ring() -> &'static std::sync::Mutex<Vec<(u64, cratonvm_types::PointerMap)>> {
    static R: std::sync::OnceLock<std::sync::Mutex<Vec<(u64, cratonvm_types::PointerMap)>>> =
        std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

pub(crate) fn gcpart_record(epoch: u64, map: &cratonvm_types::PointerMap) {
    if !gcpart_enabled() || map.is_empty() {
        return;
    }
    if let Ok(mut ring) = gcpart_ring().lock() {
        if ring.len() >= 8 {
            ring.remove(0);
        }
        ring.push((epoch, map.clone()));
    }
}

/// For each recorded recent map, oldest first: (epoch, moved_to-if-key,
/// map_len, appears-as-destination).
#[allow(clippy::type_complexity)]
pub(crate) fn gcpart_probe(addr: usize) -> Vec<(u64, Option<usize>, usize, bool)> {
    if !gcpart_enabled() {
        return Vec::new();
    }
    match gcpart_ring().lock() {
        Ok(ring) => ring
            .iter()
            .map(|(e, m)| {
                (
                    *e,
                    m.get(&addr).copied(),
                    m.len(),
                    m.values().any(|&v| v == addr),
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn remap_handle_slots(
    slots: &mut [Option<ObjectRef>],
    pointer_map: &cratonvm_types::PointerMap,
) -> usize {
    let mut rewritten = 0;
    for slot in slots.iter_mut().flatten() {
        let old_addr = slot.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: relocation maps contain live, aligned object addresses.
            *slot = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            rewritten += 1;
        }
    }
    rewritten
}

/// Post-move half of root section §10: the per-thread **single-slot** object
/// references. Returns how many were relocated.
///
/// Each of these is one `Option<ObjectRef>` field on `JvmThread` in which some
/// subsystem parks a live reference between two bytecodes, with nothing else in
/// the heap keeping it reachable. Every slot listed here MUST also be pushed by
/// the matching scan in `memory/roots.rs` §10 — a slot with only one of the two
/// halves is either a collected object (scan missing) or a from-space address
/// handed back to running code (remap missing), and both surface far away from
/// here. Factored out of [`update_all_roots`] so the pairing is unit-testable
/// without standing up a VM, exactly like [`remap_handle_slots`].
///
/// `jit_pending_exception` is the newest member and the reason this exists: it
/// spent its life as a `Cell<Option<ObjectRef>>` inside the `JIT_SIGNALS`
/// `thread_local!` in `jit/helpers.rs`, where neither half could reach it — TLS
/// belongs to the mutator, and every `VM_ROOT_SOURCES` callback runs on the
/// collector. See `fixed-bugs/jit-signals-root-gap.md`.
pub(crate) fn remap_thread_object_slots(
    thread: &mut crate::threading::jvm_thread::JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
) -> usize {
    let mut rewritten = 0;
    for slot in [
        &mut thread.java_thread_obj,
        &mut thread.pending_async_exception,
        &mut thread.jit_pending_exception,
        &mut thread.uncaught_exception_pending,
    ] {
        let Some(obj_ref) = slot.as_mut() else {
            continue;
        };
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: relocation maps contain live, aligned object addresses.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            rewritten += 1;
        }
    }
    // The remap half for the JIT's stashed deopt / exceptional frames. The scan
    // half is in `roots.rs` §10 and the two must land together — the visitor
    // carries a debug assertion that this thread scanned at least once whenever
    // there is anything to remap, precisely so the half-wiring that shipped
    // once already trips instead of going quiet.
    cratonvm_jit::deopt::remap_stashed_deopt_objects(|addr| {
        pointer_map.get(&(addr as usize)).map(|&to| to as u64)
    });
    rewritten
}

/// Remap the proxy-dispatch `Method` cache
/// (`SharedVm::classes::proxy_method_cache`) through a relocation map.
///
/// This is the remap half of `memory/roots.rs` §6b. That section pushes every
/// cached `Method` as an unconditional root, so the object is kept alive and —
/// under a MOVING collector — evacuated to a new address. Nothing then wrote
/// the new address back into the cache: the map is keyed by
/// `(proxy class, name, descriptor)` and its VALUES are raw `ObjectRef`s, so
/// the next dispatch for that key served a from-space pointer to running Java
/// code. Reading `Method.name` off reset from-space memory yields `0`, and
/// `Method.getName()` returns `null` — which javac's String-switch lowering
/// (`astore <localN>; aload <localN>; invokevirtual String.hashCode()`) turns
/// into `NullPointerException: Cannot invoke "String.hashCode()" because
/// "<localN>" is null` inside any `InvocationHandler` that branches on the
/// method name. Spring's `SynthesizedMergedAnnotationInvocationHandler.invoke`
/// is exactly that shape, which is how this reached
/// `FlywayAutoConfigurationTests` / `IntegrationAutoConfigurationTests` under
/// `-XX:+UseGenerationalGC` while ZGC (non-moving) and G1 stayed green.
///
/// A scan with no matching remap is the standing hazard this file already
/// names above `remap_thread_object_slots`; `proxy_method_cache` was the one
/// `shared.*` side table in `roots.rs` that had only the scan half. Factored
/// out so the pairing is unit-testable without standing up a VM, exactly like
/// [`remap_handle_slots`]. Returns the number of entries rewritten.
pub(crate) fn remap_proxy_method_cache<K, S: std::hash::BuildHasher>(
    cache: &mut std::collections::HashMap<K, ObjectRef, S>,
    pointer_map: &cratonvm_types::PointerMap,
) -> usize {
    let mut rewritten = 0;
    for obj_ref in cache.values_mut() {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            // SAFETY: relocation maps contain live, aligned object addresses.
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            rewritten += 1;
        }
    }
    rewritten
}

/// Scans thread frames (locals + operand stacks), static fields, class locks,
/// and printed values, updating any ObjectRef whose old address appears in
/// the pointer map.
pub fn update_all_roots(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
) {
    let __rp_guard = crate::memory::native_roots::rootprof::on().then(|| {
        struct G(std::time::Instant, usize);
        impl Drop for G {
            fn drop(&mut self) {
                let ns = self.0.elapsed().as_nanos();
                if ns >= 20_000_000 {
                    eprintln!(
                        "[rootprof] update_all_roots took {}ms pointer_map={}",
                        ns / 1_000_000,
                        self.1
                    );
                }
            }
        }
        G(std::time::Instant::now(), pointer_map.len())
    });
    let _ = &__rp_guard;
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_PRECISE").is_some() {
        eprintln!(
            "[PRECISE] update_all_roots called, pointer_map.len()={}",
            pointer_map.len()
        );
    }
    // Lever #3 (bug 04): keep this thread's rootsnap frozen-frame cache valid
    // across this collection (remap relocated cached roots + re-tag the gen).
    // Done BEFORE the empty-map early return so a non-relocating sweep also keeps
    // the cache rather than forcing a full rebuild. Opt-in/default-OFF + no-op
    // unless the rootsnap cache is enabled. See `remap_rs_cache_after_gc`.
    crate::runtime::interpreter::remap_rs_cache_after_gc(thread, pointer_map, &shared.mem.heap);
    // Long-smuggle mint registry: relocate registered handles through this
    // cycle's pointer map and drop entries whose referent died. BEFORE the
    // empty-map early return so non-relocating sweeps still sweep dead
    // entries (a reclaimed address must not stay registered — a future
    // primitive long colliding with the reused address would otherwise pass
    // the rewrite gate).
    crate::memory::smuggled_longs::remap_and_sweep(pointer_map, &shared.mem.heap);
    // Throwable backtraces are VM-wide, non-owning side data. Keep the stored
    // object handle in sync with a move and prune traces for collected
    // throwables before any early return for a non-relocating sweep.
    shared.remap_and_sweep_throwable_stack_traces(pointer_map);
    // GPU input-residency cache: an `ObjectRef`-keyed table of device
    // buffers that survives across kernel submissions. Same placement
    // rationale as the two above — the sweep half must run on a
    // non-relocating collection as well, or a dead array's key stays in
    // the map and the next array allocated onto its reclaimed address
    // gets served that array's device buffer. Not a root source: see
    // `memory::addr_keyed`.
    #[cfg(feature = "gpu-offload")]
    crate::runtime::offload::input_cache::remap_and_sweep(
        shared.vm_identity,
        pointer_map,
        &shared.mem.heap,
    );
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ALTRACE").is_some() {
        eprintln!(
            "[altrace GC] count={} moved={} tid={}",
            shared.mem.heap.collection_count(),
            pointer_map.len(),
            thread.thread_id.0
        );
    }
    if pointer_map.is_empty() {
        return;
    }
    gcpart_record(shared.mem.heap.collection_count(), pointer_map);
    // `CRATONVM_DBG_VACATED_FRAMES` — remember what this collection moved
    // objects away FROM, so the next safepoint's frame audit can name any slot
    // still holding one. No-op unless the flag is set.
    cratonvm_gc::gc_quiescence::record_vacated(pointer_map);
    crate::runtime::interpreter::remap_trace_push(
        shared,
        thread,
        "initiator",
        &format!("map={}", pointer_map.len()),
    );

    // JNI local references (INT-2): rewrite THIS thread's `JNI_LOCAL_FRAMES`
    // handles through the pointer map. The scan half
    // (`jni::collect_local_ref_roots`, roots.rs) has always kept the objects
    // alive, so a moving collection RELOCATES them — but this matching remap
    // half had no production caller (only a #[cfg(test)] one), leaving every
    // JNI local a dangling from-space pointer after any moving GC. The
    // storage is thread-local, so this covers the GC-initiating thread; the
    // safepoint-resume and blocked-wake paths call it for their own threads
    // (`apply_pointer_map_to_thread` / `check_post_block_gc`).
    crate::native::jni::update_local_refs_after_gc(pointer_map);

    // JNI keep-alive pin set (INT-10): re-key checked-out-array pins whose
    // object moved this collection, so the matching Release/unpin (keyed by
    // the object base) still finds them and `is_pinned` answers correctly
    // for the object's new address.
    cratonvm_gc::pinned::update_after_gc(pointer_map);

    // BUG-03 trace (gated CRATONVM_DBG_BUG03): per-GC coverage for main (tid 0).
    // Logs, at each relocating GC, whether main's Thread mirror moves THIS GC and
    // who the initiator is + main's blocked state — to find the GC where the mirror
    // moves but main's frames are not remapped (the concurrent-spawn stale-`parent`
    // root cause). Read the mirror's CURRENT (pre-step-21) registry address.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some() {
        let epoch = shared.mem.heap.collection_count();
        let main_old = shared
            .threads
            .thread_registry
            .java_thread_obj(crate::threading::jvm_thread::ThreadId(0))
            .map(|o| o.as_ptr() as usize)
            .unwrap_or(0);
        let moves = main_old != 0 && pointer_map.contains_key(&main_old);
        let blocked = shared
            .threads
            .thread_registry
            .dump_blocked_states()
            .into_iter()
            .find(|(tid, _, _)| *tid == 0)
            .map(|(_, b, _)| b)
            .unwrap_or(false);
        eprintln!(
            "[BUG03-gc] e{} initiator=tid{} main_mirror=0x{:x} moves_this_gc={} main_blocked={}",
            epoch, thread.thread_id.0, main_old, moves, blocked
        );
        // On a move where main is the initiator, dump main's frame stack +
        // whether any frame slot still holds the OLD mirror addr (which SHOULD be
        // remapped here). Pinpoints whether `parent` is in a scanned frame (tag/
        // slot bug) or absent (held in a native/Rust transient).
        if moves && thread.thread_id.0 == 0 {
            use std::fmt::Write as _;
            let mut s = String::new();
            for f in thread.frames.iter().rev().take(6) {
                let _ = write!(s, " {}.{}", f.class_name(), f.method_name());
            }
            eprintln!("[BUG03-gc]   main frames (top-first):{}", s);
            // Locate the OLD mirror addr in main's frames — the slot that SHOULD be
            // remapped here. If found in a LONG/DOUBLE-kind slot, that is the
            // mis-tagged object ref the kind-gated remap skips.
            for (fi, f) in thread.frames.iter().enumerate() {
                if let Some(loc) = f.dbg_locate_addr(main_old) {
                    eprintln!(
                        "[BUG03-gc]   FOUND main_mirror old=0x{:x} in frame#{} {}.{} @ {}",
                        main_old,
                        fi,
                        f.class_name(),
                        f.method_name(),
                        loc
                    );
                }
            }
        }
        // EVERY GC (main): trace slot 7 value + pc for each Thread.<init> frame, to
        // see `parent`'s trajectory (when it diverges from the field) regardless of
        // in-map status.
        if thread.thread_id.0 == 0 {
            for (fi, f) in thread.frames.iter().enumerate() {
                if f.method_name() == "<init>"
                    && f.class_name() == "java/lang/Thread"
                    && f.locals_len() > 7
                {
                    let s7 = match f.get_local(7) {
                        crate::types::Value::Object(Some(o)) => {
                            let a = o.as_ptr() as usize;
                            format!("obj=0x{:x} in_map={}", a, pointer_map.contains_key(&a))
                        }
                        crate::types::Value::Object(None) => "null".to_string(),
                        other => format!("{other:?}"),
                    };
                    eprintln!(
                        "[BUG03-l7] e{} frame#{} Thread.<init> pc={} local7={} stack:{}",
                        epoch,
                        fi,
                        f.pc,
                        s7,
                        f.dbg_stack_dump()
                    );
                }
            }
        }
    }

    if let Some(w) = watch_addr() {
        let fwd = pointer_map.get(&w).copied();
        let back = pointer_map
            .iter()
            .find_map(|(k, v)| (*v == w).then_some(*k));
        eprintln!(
            "[watch] GC#{} addr=0x{w:x} moved_to={fwd:x?} moved_from={back:x?}",
            shared.mem.heap.collection_count()
        );
    }
    // 1. Thread frames — locals and operand stacks (SoA layout)
    // See `JvmThread::last_heal_collection`.
    thread.last_heal_collection = shared.mem.heap.collection_count();
    for frame in &mut thread.frames {
        frame.update_local_refs(pointer_map, &shared.mem.heap);
        frame
            .stack
            .update_object_refs(pointer_map, &shared.mem.heap);
        // Forward the synchronized-method monitor object too. A `synchronized`
        // method records the object it locked on entry in `monitor_on_exit` and
        // releases it on frame-pop. If a GC during the method body relocates
        // that object (its locals/operand-stack copies are forwarded above, and
        // the monitor table is remapped by the collector), a stale
        // `monitor_on_exit` would make the implicit `monitorexit` target the
        // old address — surfacing as "thread does not own the monitor"
        // (observed in BouncyCastle's synchronized `X9ECParametersHolder.
        // getParameters` / `X9ECPoint.getPoint`, which allocate inside the
        // locked region). Keep it consistent with the relocated object.
        if let Some(ref mut obj_ref) = frame.monitor_on_exit {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // DIAGNOSTIC-ONLY (cceres3): initiator-side counterpart of the
    // ARRIVE-STALE / WAKE-STALE frame verifiers.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BLOCKGC").is_some() {
        for (fi, fr) in thread.frames.iter().enumerate() {
            for li in 0..fr.locals_len() {
                if let crate::types::Value::Object(Some(o)) = fr.get_local(li as u16) {
                    let a = o.as_ptr() as usize;
                    if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                        eprintln!(
                            "[blockgc] INITIATOR-STALE tid={} frame#{fi} {}.{} pc={} local[{li}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
            for si in 0..fr.stack.len() {
                if let crate::types::Value::Object(Some(o)) = fr.stack.peek_at(si) {
                    let a = o.as_ptr() as usize;
                    if let Some(new) = shared.mem.heap.debug_forwarded_target(a) {
                        eprintln!(
                            "[blockgc] INITIATOR-STALE tid={} frame#{fi} {}.{} pc={} stack[{si}] 0x{a:x}->0x{new:x} in_map={}",
                            thread.thread_id.0, fr.class_name(), fr.method_name(), fr.pc,
                            pointer_map.contains_key(&a),
                        );
                    }
                }
            }
        }
    }
    // Stage 3 (precise oop maps) — relocate oop slots of active JIT frames on
    // this thread, the JIT analogue of the interpreter-frame remap above. Inert
    // unless CRATONVM_PRECISE_JIT_MAPS compiled the frame (sp_id_slot_off != 0);
    // it is the piece that lets a moving collector run while JIT frames are live
    // (see fixed-suite-bugs/app-jvm-bugs/precise-jit-stack-maps-design.md, Stage 3).
    crate::jit::conservative_roots::remap_active_jit_frames(pointer_map);
    crate::jit::conservative_roots::remap_register_image_words(pointer_map, Some(shared));
    crate::jit::conservative_roots::report_stale_after_remap(pointer_map, Some(shared));

    // Shadow-stack precise remap (CRATONVM_SHADOW_STACK) — the rewritable
    // counterpart to the marking scan in roots.rs. Every pushed slot is a known
    // oop, so relocating it via `pointer_map` is unconditionally safe (no
    // is-it-really-a-pointer ambiguity, unlike the conservative JIT scan). After
    // the call returns, JIT codegen reloads each oop from its (now-updated) slot,
    // so a moved object's new address flows back into the compiled code's
    // registers. This is what makes the moving collector correct under JIT.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        let _rewritten = thread.shadow_stack.remap(pointer_map);
        if crate::runtime::env_cache::dbg_shadow() && _rewritten > 0
        {
            eprintln!(
                "[SHADOW] remap: depth={} rewritten={}",
                thread.shadow_stack.depth(),
                _rewritten
            );
        }
    }

    for obj_ref in &mut thread.native_pin_roots {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }

    // Handle-scope slots (arch/handles) are roots too: without this remap a
    // RootedHandle read after a moving collection would decode the pre-move
    // address. Mirrors the native_pin_roots loop above.
    remap_handle_slots(&mut thread.handle_slots, pointer_map);

    if let Some(ref mut obj_ref) = thread.native_pending_return {
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }

    for entry in &mut thread.jit_hashmap_string_node_cache {
        for obj_ref in [&mut entry.map, &mut entry.node] {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        if let Some(key_object) = entry.key_object.as_mut() {
            let old_addr = key_object.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *key_object = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    for entry in &mut thread.string_case_cache {
        for obj_ref in [&mut entry.source, &mut entry.first, &mut entry.second] {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        if let Some(locale) = entry.locale.as_mut() {
            let old_addr = locale.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *locale = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 2. Static fields
    //
    // SLOTS FIRST. `collect_roots` already visited every static of every class
    // a few microseconds ago and knows exactly which slots hold an object; a
    // `StaticsBlock` is a leaked `Box<[Value]>` whose base is stable for the
    // life of the VM, so those slot addresses are still valid here even if the
    // owning map has rehashed. Walking them is the same work this loop did,
    // minus a second sweep of every primitive slot in the process and minus
    // taking the `statics` WRITE lock across all of it.
    //
    // See `memory::roots::STATIC_REF_SLOTS` for why this exists beyond the
    // saving: it is the first place in this VM where a root is carried as a
    // SLOT rather than a value, which is the shape that would let a relocating
    // collector fix roots in place and retire the `PointerMap` and its ~35
    // hand-written remap companions.
    //
    // The take is what makes it safe: an `update_all_roots` not preceded by a
    // scan on this thread gets `None` and falls through to the full walk below,
    // so a stale list cannot be applied. `CRATONVM_GC_STATIC_ROOT_SLOTS=0`
    // forces that path in one binary.
    match crate::memory::roots::take_static_ref_slots() {
        Some(slots) => {
            // The `statics` lock is deliberately NOT taken. These are raw
            // addresses into leaked blocks the map does not own the storage of,
            // and this runs stop-the-world with every mutator parked -- the same
            // conditions under which the scan read them.
            let covered = slots.len();
            for addr in slots {
                // SAFETY: recorded by `collect_roots` earlier in THIS pause as
                // the address of a `Value` slot inside a leaked `StaticsBlock`.
                // Such a block is never freed (`grow_to` leaves the old one
                // allocated on purpose), so the pointer cannot dangle, and no
                // mutator is running to write it concurrently.
                let val = unsafe { &mut *(addr as *mut cratonvm_types::Value) };
                update_value_ref(val, pointer_map);
            }
            // `CRATONVM_DBG_STATIC_SLOT_VERIFY=1` -- re-walk every static the
            // slow way and report any slot the recorded list did not cover.
            //
            // A missed slot here is not a wrong number, it is a live static
            // field left pointing at a vacated address: a use-after-free that
            // surfaces arbitrarily far from this function. The set the scan
            // records and the set this loop would have visited must be equal,
            // and the only way to know that on a real workload rather than in a
            // unit test is to run both and diff them -- the same shape
            // `CRATONVM_DBG_ROOTSNAP_VERIFY` uses for the frozen-frame cache.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STATIC_SLOT_VERIFY").is_some() {
                let mut missed = 0usize;
                let mut statics = shared.classes.statics.write();
                for fields in statics.values_mut() {
                    for val in fields.iter_mut() {
                        // Anything the slot walk covered is already remapped, so
                        // a second `update_value_ref` on it is a no-op (the new
                        // address is not itself a key). A slot that still
                        // resolves through the map is one the recorded list
                        // missed.
                        if let cratonvm_types::Value::Object(Some(o)) = *val {
                            if pointer_map.get(&(o.as_ptr() as usize)).is_some() {
                                missed += 1;
                                update_value_ref(val, pointer_map);
                            }
                        }
                    }
                }
                // The SHAPE, not just the failures. A verifier that prints only
                // when it finds something cannot distinguish "covered
                // everything" from "the fast path never ran" -- and on this
                // path the second one is the likelier way to get a silent zero,
                // because `take_static_ref_slots` returns `None` whenever the
                // scan recorded nothing and this whole arm is skipped. Printing
                // the covered count alongside the miss count is what makes a
                // zero mean something.
                eprintln!(
                    "[static-slot-verify] covered={covered} missed={missed}                      moved={moved} (covered = slots the scan recorded; moved =                      entries in this collection's pointer map)",
                    moved = pointer_map.len(),
                );
            }
        }
        None => {
            crate::memory::roots::note_static_slot_fallback();
            let mut statics = shared.classes.statics.write();
            for fields in statics.values_mut() {
                for val in fields.iter_mut() {
                    update_value_ref(val, pointer_map);
                }
            }
        }
    }

    // 3. Class lock objects
    {
        let mut class_locks = shared.classes.class_locks.write();
        for obj_ref in class_locks.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 4. Thread printed values
    for val in &mut thread.printed {
        update_value_ref(val, pointer_map);
    }

    // 5. Interned string pool
    {
        let mut string_pool = shared.mem.string_pool.write();
        for obj_ref in string_pool.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 6. Class mirror cache
    {
        let mut class_mirrors = shared.classes.class_mirrors.write();
        for obj_ref in class_mirrors.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 6a. Cached proxy-dispatch `Method` objects — the remap half of the
    //     unconditional root scan in `roots.rs` §6b. See
    //     [`remap_proxy_method_cache`] for why a rooted-but-unremapped entry
    //     hands running Java a from-space `Method` whose `name` reads null.
    {
        let mut proxy_methods = shared.classes.proxy_method_cache.write();
        remap_proxy_method_cache(&mut proxy_methods, pointer_map);
    }

    // 6b. VarHandle permanent roots (B-J) — remap the registry entries so the
    //     canonical VarHandle ref tracks the object across a move (the holder
    //     `static final` slot is remapped via the `statics` block above using
    //     the same pointer-map entry, now that the VarHandle is traced/copied).
    {
        let mut var_handle_roots = shared.mem.var_handle_roots.write();
        for obj_ref in var_handle_roots.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 6c. Pre-allocated singleton OutOfMemoryError. The root scan keeps it
    //     ALIVE (memory/roots.rs step 8c), but the holder is a bare
    //     `SharedVm` field no other remap step covers — without this, the
    //     first relocating GC that moves the singleton leaves
    //     `shared.mem.singleton_oom` dangling, and a later true OOM throws a
    //     reclaimed/zeroed object that surfaces as the unreadable
    //     `Exception in thread "main" unknown` (observed deterministically on
    //     the SteadyChurn recreation under sustained G1 churn).
    {
        let mut oom = shared.mem.singleton_oom.write();
        if let Some(ref mut obj_ref) = *oom {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 6d. Cached "main" java.lang.ThreadGroup singleton
    //     (`SharedVm::main_thread_group`). The root scan keeps it ALIVE
    //     (memory/roots.rs step 8d), but — exactly like `singleton_oom`
    //     just above — the cache is a bare `SharedVm` field that no other
    //     remap step covers, so without this a moving GC that relocates the
    //     group after it is published leaves the cache pointing at
    //     from-space. Every later reader of the cache (including
    //     `get_or_create_main_thread_group`'s own fast path, and
    //     `build_thread_field_holder`'s use of its return value) then hands
    //     out a dangling `ObjectRef`, which crashes the next
    //     `FieldHolder.<init>` field-setter that stores it into
    //     `holder.group` (`is_forwarded`/`get_header:1558`, confirmed live
    //     on a `cratonvm-aio-dispatch-N` thread). try_write mirrors the
    //     system-streams convention below: `get_or_create_main_thread_group`
    //     only ever holds the write lock for the single final-store
    //     assignment (never across an allocation), so a locked slot here
    //     means that store is mid-flight and its value is about to be
    //     overwritten anyway.
    {
        if let Some(mut tg) = shared.threads.main_thread_group.try_write() {
            if let Some(ref mut obj_ref) = *tg {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }

    // 7. System streams (System.out, System.err)
    // 7. System streams (System.out, System.err, System.in)
    //
    // try_write, NOT write: the singleton initializers (e.g.
    // `ensure_system_stdin_object`) hold the WRITE guard across allocating
    // calls, and an allocation-triggered GC on that same thread would
    // self-deadlock on the non-reentrant RwLock (mirrors the try_read in
    // roots.rs step 7). A locked slot is mid-initialization: it holds None
    // (populated only at the end, from a pinned — hence already remapped —
    // local), so there is nothing to remap.
    {
        if let Some(mut out) = shared.system_out.try_write() {
            if let Some(ref mut obj_ref) = *out {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    // Safety: new_addr was produced by the GC's pointer map and
                    // should point into the to-space. The debug_assert verifies
                    // this during development.
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
        if let Some(mut err) = shared.system_err.try_write() {
            if let Some(ref mut obj_ref) = *err {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    // Safety: new_addr was produced by the GC's pointer map and
                    // should point into the to-space. The debug_assert verifies
                    // this during development.
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
        // gcstress residual face fix — remap `system_in` too (it was missing
        // from this step AND from the roots.rs scan while out/err had both,
        // so the cached System.in went stale on the first moving GC).
        if let Some(mut sin) = shared.system_in.try_write() {
            if let Some(ref mut obj_ref) = *sin {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    // Safety: same contract as out/err above.
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }

    // 8. Primitive type Class mirrors
    {
        let mut prim_mirrors = shared.classes.primitive_mirrors.write();
        for obj_ref in prim_mirrors.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                // Safety: new_addr was produced by the GC's pointer map and
                // should point into the to-space. The debug_assert verifies
                // this during development.
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 8a. Canonical java.lang.Module mirrors (companion to root scan in
    //     roots.rs section 8a).
    {
        let mut module_mirrors = shared.classes.module_mirrors.write();
        for obj_ref in module_mirrors.values_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 9. JNI global references — update stored ObjectRefs inside each Box<ObjectRef>.
    {
        shared
            .natives
            .jni_global_refs
            .lock()
            .update_after_gc(pointer_map);
    }

    // 9a and 15-22. Paired post-move half of the VM/native root inventory.
    // The historical notes below document the individual registered sources.
    crate::memory::native_roots::remap_all_roots(shared, pointer_map);

    // 9a. Native upcall table — rewrite each live slot's callback `target` to its
    //     post-relocation address so the legacy `pe_upcall_invoke` dispatch path
    //     does not read a stale pointer after a moving collection (root scan in
    //     roots.rs section 9a).
    // 9b. NIO selector side-table — the `sun.nio.ch.SelectionKeyImpl` registry
    // (nio_selector `sk_table` + per-selector key state) stores raw channel /
    // selector / attachment / key ObjectRefs. Without remapping them after a
    // moving collection, `SelectionKey.channel()` returns a stale (relocated)
    // channel, and the Apache NIO reactor closing that session then dereferences
    // a moved-away object → `monitorenter ... null` on its `closeLock` and the
    // reactor worker dies (ES testManyAsyncRequests under burst load). The helper
    // early-returns when nothing moved (non-moving GC).

    // 10. Per-thread ObjectRef slots — java_thread_obj, pending_async_exception,
    //     jit_pending_exception. Paired with the scan in `roots.rs` §10; each of
    //     these is a single `Option<ObjectRef>` field that some subsystem parks a
    //     live reference in between two bytecodes, so it must be BOTH pushed as a
    //     root there and rewritten here.
    remap_thread_object_slots(thread, pointer_map);

    // 11. Root snapshot
    {
        let mut snapshot = thread.root_snapshot.lock();
        for obj_ref in snapshot.iter_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    // 12. Scoped value bindings (JEP 446)
    //
    // Round-9 GC fix: also remap the ScopedValue KEY ObjectRef when present
    // (previous code only remapped values). Without this, a moving GC
    // would leave the key pointing at a stale post-compaction address —
    // a use-after-free on the next `Carrier.get` traversal.
    for (_key_id, key_ref, val) in &mut thread.scoped_values {
        if let Some(obj_ref) = key_ref {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        update_value_ref(val, pointer_map);
    }

    // 13. Resolution cache — CONSTANT_Dynamic values may hold ObjectRefs
    {
        let mut cache = shared.classes.resolution_cache.write();
        cache.update_condy_refs(pointer_map);
    }

    // 14. Class mirrors reverse map — rebuild keys from updated forward map
    {
        let class_mirrors = shared.classes.class_mirrors.read();
        let mut reverse = shared.classes.class_mirrors_reverse.write();
        reverse.clear();
        for (&class_id, obj_ref) in class_mirrors.iter() {
            reverse.insert(*obj_ref, class_id);
        }
    }

    // 15. Round-9 CRIT GC-correctness fix: re-point the process-global
    //     Integer.valueOf / Boolean.TRUE/FALSE caches living in
    //     `native-builtins/src/lang_math.rs`. They are reported as roots
    //     by `roots.rs` step 15, so the cached objects survive GC — but
    //     under a moving collector their addresses change and we must
    //     remap them here, otherwise the next cache lookup returns a
    //     stale pointer.

    // 15a. Unsafe / Class$Atomic synthetic-offset side stores (scanned in
    //      `roots.rs` step 15a). A moving collection relocates the stored refs
    //      and selective promotion may tenure them; repoint them here so the
    //      next side-store load/CAS sees the live address instead of a dangling
    //      one. See `gc_scan_unsafe_side_store_roots`.

    // 16. Round-9 perf + GC fix: re-point the process-global LambdaMetafactory
    //     CallSite cache living in `native-builtins/src/lang_invoke.rs`.
    //     Same scan/update contract as the Integer.valueOf cache.

    // 16a. Re-point the zero-capture lambda proxy singleton cache
    //      (scanned in `roots.rs` step 16a). Same scan/update contract as
    //      the Integer.valueOf cache.

    // 17. Overlay-backed collections (LinkedList / LinkedHashMap / TreeMap /
    //     TreeSet) keep their backing arrays + nodes in process-global Rust
    //     side-tables. Their roots are scanned in `roots.rs` step 17; repoint
    //     the stored ObjectRefs to their relocated addresses here.

    // 18. Singleton built-in class loaders (app / platform) cached in
    //     process-global mutexes in `native-builtins/src/classloader.rs`.
    //     Scanned as roots in `roots.rs` step 18; repoint the stored ObjectRefs
    //     to their relocated addresses here so `getClassLoader()` keeps
    //     returning the live loader after a moving GC (fixes the intermittent
    //     stale-ClassLoader → `String.loadClass` cryptoProvider failure).

    // 18a. Process-global `System.getenv()` / `System.getProperties()`
    //      singletons (companion to roots.rs step 18a). Repoint the cached
    //      Map/Properties ObjectRefs to their relocated addresses so the next
    //      `getenv()`/`getProperties()` returns the live object after a move.

    // 18b. Process-global Locale caches (companion to roots.rs step 18b).
    //      Repoint the cached default Locale + synthetic Locale side-table keys
    //      to their relocated addresses so `Locale.getDefault()` keeps returning
    //      the live object after a moving GC (fixes the intermittent stale
    //      Locale → SIGSEGV).

    // 18c. `java.lang.ClassValue` memoization cache (companion to roots.rs
    //      step 18c). Repoint cached `computeValue(Class)` results to their
    //      relocated addresses so `ClassValue.get()` keeps returning the live
    //      object after a moving GC.

    // 19. JBoss MSC container-held service objects (the `Service` instance,
    //     synthetic `ServiceController` mirror, child `ServiceTarget`, in-flight
    //     `StartContext`) cached in a process-global side-table in
    //     `native-builtins/src/jboss_msc.rs`. Scanned as roots in `roots.rs`
    //     step 19; repoint the stored ObjectRefs to their relocated addresses
    //     here so the container can safely invoke `start()`/`stop()` on the held
    //     service after a moving GC.

    // 19b. logmanager.rs cached LogManager / Logger / LogContext singletons and
    //      the attachments table (Round-4 B4) — repoint the stored addresses to
    //      their relocated locations after a moving GC. Scanned as roots in
    //      `roots.rs` step 19b.

    // 19c. Class-level annotation-proxy identity cache in
    //      `native-builtins/src/lang_class.rs` (per-class `getAnnotation` /
    //      `getDeclaredAnnotations` proxies). Scanned as roots in `roots.rs`
    //      step 20; repoint the stored ObjectRefs to their relocated addresses
    //      after a moving GC so cached annotation instances stay live.

    // 19d. Synthetic `com.sun.net.httpserver` server registry handler refs
    //      (`native-builtins/src/net_phase_e.rs`). Scanned as roots in `roots.rs`;
    //      re-point the stored `HttpHandler` ObjectRefs to their relocated
    //      addresses after a moving GC so the per-request dispatcher invokes the
    //      live handler instead of a vacated from-space slot (else
    //      `NoSuchMethodError: java/lang/Object.handle` storm under GC pressure).

    // Process-global InetAddress side table (companion to roots.rs, right
    // after the re10 handler scan). Repoint the mirror's (hostName,
    // ipAddress) entry to its relocated key after a moving GC so
    // getHostAddress()/getAddress()/toString() keep resolving the real
    // address instead of falling back to "0.0.0.0".

    //      ScheduledThreadPoolExecutor pending runnables + XNIO IoFuture
    //      notifier/attachment/result refs: relocate the stored ObjectRefs after
    //      a moving GC so the pump / future-settle invokes the live object, not a
    //      vacated from-space slot. Root-scan companions in `roots.rs`; the NIO
    //      sk_table remap is wired separately above (`sk_table_update_after_gc`).
    // FFM/Panama upcall targets (Step 5 GAP C) — rewrite the leaked upcall
    // userdata's `target` in place so the trampoline dispatches to the moved
    // object; scan companion `panama::gc_scan_upcall_target_roots` in `roots.rs`.
    // TLS SSLContext TrustManager[] objects; scan companion
    // `t27_tls::gc_scan_tls_ctx_trust_manager_roots` in `roots.rs`.
    // TLS SSLContext KeyManager[] objects (client-cert resolver); scan
    // companion `t27_tls::gc_scan_tls_ctx_key_manager_roots` in `roots.rs`.
    // Process-wide default SSLContext; scan companion
    // `t27_tls::gc_scan_default_ssl_context_root` in `roots.rs`.

    // ForkJoinTask done/result side-table; scan companion
    // `phases_early::gc_scan_forkjoin_roots` in `roots.rs`.

    // 20. Blocked-thread root maintenance (the H2 TestScript stale-receiver
    //     SEGV fix). Threads parked in a blocking native (Object.wait /
    //     Thread.join / LockSupport.park / ReferenceQueue.remove) are
    //     excluded from the STW barrier and cannot apply this pointer map
    //     themselves; without this step their deposited root_snapshot goes
    //     stale after the first missed moving GC (and the NEXT collection
    //     then evacuates garbage through the stale addresses — a heap
    //     corruptor), and their frames resume with recycled from-space
    //     addresses. The fold remaps each blocked thread's snapshot in
    //     place and composes this map into its pending wake-time frame
    //     fixup (applied in `check_post_block_gc`).
    shared
        .threads
        .thread_registry
        .fold_pointer_map_into_blocked_audited(pointer_map, Some(&shared.mem.heap));

    // 21. Registry java.lang.Thread mirrors + the unpark(Thread) reverse
    //     index (keyed by mirror address). Scanned as roots in roots.rs
    //     step 10b; without the remap the registry serves stale mirrors
    //     back into bytecode and `LockSupport.unpark(Thread)` lookups by
    //     the relocated address silently miss (lost wakeups).
    shared
        .threads
        .thread_registry
        .update_thread_objs_after_gc(pointer_map);

    // 22. Uniform native-root registry — the post-move companion to
    //     `roots.rs` step 21, driven above via `native_roots::remap_all_roots`.
    //     Fans out over `native_roots::VM_ROOT_SOURCES`, repointing each held
    //     ObjectRef through `pointer_map`. Each remap self-guards the empty
    //     (non-moving) map, and each receives the OWNING `SharedVm` so one VM's
    //     fixup cannot rewrite another VM's entries.

    // Post-GC verification: check that no frame refs still point to relocated addresses.
    verify_no_stale_refs(thread, pointer_map);
    // Opt-in (CRATONVM_DBG_HEAP_STALE=1) deep heap-walk: catch un-forwarded /
    // reclaimed reference fields in OTHER objects (not just this thread's
    // frames) — where the residual ClassLoader/Locale stale-ref actually lives.
    verify_heap_object_fields(shared, pointer_map);
    // THE SCAN INVENTORY AND THE REMAP INVENTORY ARE TWO LISTS
    // (`CRATONVM_DBG_ROOT_REMAP_AUDIT=1`).
    //
    // `roots.rs` decides what the collector MARKS from; `native_roots.rs`
    // decides what gets REWRITTEN afterwards. A side table present in the first
    // and missing from the second keeps its object alive and then keeps naming
    // the address the collector moved it away from — which is the exact
    // signature the H2 MVStore-writer residual has left after the heap slots
    // and both frame-remap sites were verified complete.
    //
    // So ask directly: re-run the scan and look for an address this collection
    // moved. Destinations are excluded for the reason the frame verifier
    // excludes them — a survivor slides INTO a vacated address, and a root
    // legitimately naming that survivor is not a finding.
    //
    // `CRATONVM_DBG_ROOT_SOURCE=1` alongside this names WHICH source, which is
    // the fix's address.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOT_REMAP_AUDIT").is_some() {
        let destinations: rustc_hash::FxHashSet<usize> = pointer_map.values().copied().collect();
        let after = crate::memory::roots::collect_roots(shared, thread);
        let mut reported = 0usize;
        for (index, r) in after.iter().enumerate() {
            let a = r.as_ptr() as usize;
            if destinations.contains(&a) {
                continue;
            }
            if let Some(&new) = pointer_map.get(&a) {
                reported += 1;
                if reported <= 8 {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        obj = format!("{a:#x}"),
                        moved_to = format!("{new:#x}"),
                        source = crate::memory::native_roots::root_source_of(a)
                            .unwrap_or("<not-attributed>"),
                        // Which SECTION of the scan produced it. `root_source_of`
                        // only covers the uniform native-root registry; this
                        // covers the other forty.
                        scan_section = crate::memory::roots::scan_section_of(index),
                        "a ROOT this collection just scanned still names the address it moved                          the object away from — the scan inventory contains a source the remap                          inventory does not."
                    );
                }
            }
        }
        if reported > 0 {
            tracing::error!(
                target: "cratonvm::gc::guard",
                unremapped_roots = reported,
                scanned = after.len(),
                map = pointer_map.len(),
                "root remap audit: that many scanned roots were left naming a vacated address"
            );
        }
    }
}

/// Opt-in young-object size validator (`CRATONVM_DBG_VALIDATE_NEW=1`). Walks
/// every live plain object and compares its header `num_slots` (and
/// `array_length`) against the authoritative `num_total_fields` for its class
/// (what the interpreter allocates with). A mismatch is the smoking gun for a
/// JIT `new` that wrote a wrong-size/typed header (the JUnitCore.main miscompile
/// → heap-walk desync). Reports the FIRST offenders in address order (the
/// earliest is the root; later entries may be walk-desync garbage). Call after
/// each GC so it fires on the non-moving (JIT-active) sweep path too.
pub fn validate_object_sizes(shared: &crate::vm::SharedVm) {
    use cratonvm_types::{ObjectHeader, ObjectKind};

    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VALIDATE_NEW").is_none() {
        return;
    }
    let heap = &shared.mem.heap;
    let cm = shared.classes.class_manager.read();
    // One-shot: dump the class_id -> (name, num_total_fields) table for the
    // low class_ids that show up in the JUnitCore-corruption walks (6, 12, 34,
    // 36, ...), so the corrupted object types can be identified by name.
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static DUMPED: AtomicBool = AtomicBool::new(false);
        if !DUMPED.swap(true, Ordering::Relaxed) {
            for raw in 0u32..64 {
                let id = cratonvm_types::ClassId::new(raw);
                if let Some(c) = cm.get_class(id) {
                    eprintln!(
                        "[classid] {} -> {} (num_total_fields={})",
                        raw, c.name, c.num_total_fields,
                    );
                }
            }
        }
    }
    let mut reported = 0usize;
    const CAP: usize = 25;
    for (ptr, _size) in heap.walk_objects() {
        if reported >= CAP {
            break;
        }
        let hdr = unsafe { &*(ptr as *const ObjectHeader) };
        if hdr.kind() != ObjectKind::Object {
            continue;
        }
        let cid = hdr.class_id;
        let actual = hdr.num_slots() as usize;
        let arrlen = hdr.array_length();
        match cm.get_class(cid) {
            Some(c) => {
                if actual != c.num_total_fields || arrlen != 0 {
                    eprintln!(
                        "[young-validate] BAD {} (cid={}) num_slots={} EXPECTED={} array_length={} @0x{:x}",
                        c.name, cid.as_u32(), actual, c.num_total_fields, arrlen, ptr as usize,
                    );
                    reported += 1;
                }
            }
            None => {
                eprintln!(
                    "[young-validate] BAD <unknown class> cid={} num_slots={} array_length={} @0x{:x}",
                    cid.as_u32(), actual, arrlen, ptr as usize,
                );
                reported += 1;
            }
        }
    }
}

/// Opt-in deep heap-stale verifier (`CRATONVM_DBG_HEAP_STALE=1`). After a
/// moving GC, walks EVERY live plain object and checks each reference field for
/// a dangling target — the proven method for pinning the residual intermittent
/// stale-ref (e.g. a `ClassLoader` field that later derefs as a `String`,
/// surfacing as `String.loadClass` / "Not able to load any cryptoProvider").
///
/// Flags three signatures:
///   (a) target address still a KEY in `pointer_map` → the field was NOT
///       forwarded (the copier / old→young remembered-set missed this referrer
///       edge) — the most actionable signal;
///   (b) target is not a live heap address (points into the reset young
///       from-space) → the target was reclaimed and its slot freed;
///   (c) target has an all-zero (reclaimed) header.
///
/// Reports the referrer class + field index so the missing barrier/root edge
/// can be pinned. Output is capped per GC to avoid flooding. Reference arrays
/// are intentionally skipped here (already covered by the resurrection-drain
/// remembered-set fix); this pass targets plain object fields.
pub fn verify_heap_object_fields(
    shared: &crate::vm::SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) {
    use crate::types::Value;
    use cratonvm_types::{
        ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE,
        REF_ELEMENT_SIZE,
    };

    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_HEAP_STALE").is_none() {
        return;
    }
    let heap = &shared.mem.heap;
    let class_name = |cid: cratonvm_types::ClassId| -> String {
        shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("cid#{}", cid.as_u32()))
    };
    let mut reported = 0usize;
    const CAP: usize = 40;
    // Recycled-destination filter — the same ambiguity `verify_no_stale_refs`
    // documents at length, which this pass was missing.
    //
    // An address that is BOTH a key and a value in `pointer_map` was vacated by
    // one object and handed out again as the DESTINATION of another. A slot the
    // remap rewrote correctly then points at a map KEY, so the `contains_key`
    // test below reports it as UN-FORWARDED even though it is a fresh, correct
    // reference to that address's NEW occupant.
    //
    // On the major-GC path this is not a rare corner: `pointer_map` is the
    // composition of the young map with `OldGen::compact`'s, and a SLIDING
    // compactor moves survivors DOWN into space its predecessors just vacated —
    // so key∩value overlap is the normal case, not the exception. Reporting it
    // manufactured a "the collector leaves reference fields un-forwarded"
    // finding out of a correctly-collected heap. Build the destination set once
    // (this whole pass is already an opt-in full heap walk).
    let destinations: std::collections::HashSet<usize> = pointer_map.values().copied().collect();
    // Classify a reference target. Returns Some(reason) if it is dangling
    // (un-forwarded / off-heap / zeroed = wrongly reclaimed by the sweep).
    let classify = |addr: usize| -> Option<&'static str> {
        if addr == 0 {
            return None;
        }
        if pointer_map.contains_key(&addr) && !destinations.contains(&addr) {
            return Some("UN-FORWARDED");
        }
        if heap.is_heap_addr(addr).is_none() {
            return Some("OFF-HEAP(reclaimed)");
        }
        let h = unsafe { &*(addr as *const ObjectHeader) };
        if h.class_id.as_u32() == 0
            && h.mark_word.load(std::sync::atomic::Ordering::Relaxed) == 0
            && h.num_slots() == 0
            && h.array_length() == 0
        {
            return Some("ZEROED(reclaimed)");
        }
        None
    };
    for (ptr, _size) in heap.walk_objects() {
        if reported >= CAP {
            break;
        }
        let hdr = unsafe { &*(ptr as *const ObjectHeader) };
        let r_cid = hdr.class_id;
        if hdr.kind() == ObjectKind::Object {
            let referrer = unsafe { ObjectRef::from_raw(ptr) };
            let nf = heap.num_fields(referrer);
            for i in 0..nf {
                if let Value::Object(Some(target)) = heap.get_field(referrer, i) {
                    if let Some(reason) = classify(target.as_ptr() as usize) {
                        eprintln!(
                            "[heap-stale] {} OBJ {} field[{}] -> 0x{:x}",
                            reason,
                            class_name(r_cid),
                            i,
                            target.as_ptr() as usize,
                        );
                        reported += 1;
                        if reported >= CAP {
                            break;
                        }
                    }
                }
            }
        } else if hdr.kind() == ObjectKind::Array
            && hdr.element_type() == ArrayElementType::Reference
        {
            // Reference array (Object[]): elements are 8-byte compact pointers.
            let len = hdr.array_length() as usize;
            for i in 0..len {
                let s_ptr =
                    unsafe { (ptr as *const u8).add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                let raw = unsafe { read_ref_slot(s_ptr) } as usize;
                if let Some(reason) = classify(raw) {
                    eprintln!(
                        "[heap-stale] {} ARR {}[{}] -> 0x{:x}",
                        reason,
                        class_name(r_cid),
                        i,
                        raw,
                    );
                    reported += 1;
                    if reported >= CAP {
                        break;
                    }
                }
            }
        }
    }
    if reported > 0 {
        eprintln!(
            "[heap-stale] ^ {} stale field(s) this GC (pointer_map size={})",
            reported,
            pointer_map.len(),
        );
    }
}

/// Opt-in (`CRATONVM_DBG_HEAP_STALE=1`) audit of the collection-overlay SIDE
/// TABLES — the one place every other verifier is blind to.
///
/// `verify_heap_object_fields` walks Java objects and `verify_no_stale_refs`
/// walks thread frames; an overlay-backed collection whose backing was
/// reclaimed is neither, so it shows up in neither. That blind spot is where
/// the reclaimed-`LinkedHashMap$Node` / `TreeMap size 0` family lives.
///
/// Deliberately called from the GC epilogue in `interpreter.rs` rather than
/// from `verify_heap_object_fields`: the latter runs only from
/// `update_all_roots`, which EARLY-RETURNS on an empty `pointer_map` — i.e.
/// never on the non-moving sweep, which is exactly the path a `System.gc()`
/// takes (`explicit_full_gc`) and exactly where `ROverlaySystemGcStress` fails.
/// A verifier that cannot run on the failing path is worse than none.
///
/// The classifier here is deliberately NOT `is_addr_live` — that is a range
/// check, so a freed old-gen block reads as live. It reports the two
/// unambiguous signatures instead: off-heap, and an all-zero header.
pub fn audit_overlay_refs(shared: &crate::vm::SharedVm) {
    use cratonvm_types::ObjectHeader;

    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_HEAP_STALE").is_none() {
        return;
    }
    let heap = &shared.mem.heap;
    let classify = |addr: usize| -> Option<&'static str> {
        if heap.is_heap_addr(addr).is_none() {
            return Some("OFF-HEAP(reclaimed)");
        }
        // SAFETY: `is_heap_addr` placed `addr` inside a mapped heap region, so
        // the header bytes are readable.
        let h = unsafe { &*(addr as *const ObjectHeader) };
        if h.class_id.as_u32() == 0
            && h.mark_word.load(std::sync::atomic::Ordering::Relaxed) == 0
            && h.num_slots() == 0
            && h.array_length() == 0
        {
            return Some("ZEROED(reclaimed)");
        }
        None
    };
    let mut reported = 0usize;
    const CAP: usize = 40;
    cratonvm_native_collections::gc_audit_overlay_refs(&classify, &mut |reason, table, addr| {
        if reported < CAP {
            eprintln!("[overlay-stale] {reason} {table} -> 0x{addr:x}");
        }
        reported += 1;
    });
    if reported > 0 {
        eprintln!("[overlay-stale] ^ {reported} dangling overlay ref(s) after this GC");
    }
}

/// Post-GC verification: warns if any thread frame local or operand stack value
/// still holds an ObjectRef whose address appears in the pointer_map (i.e. was
/// supposed to be relocated), OR points to zeroed-out memory (i.e. was garbage
/// collected because it wasn't in the root set).
///
/// Recycled-destination exception (the SpinPollMark `-Xmx80m` OOM-pressure
/// false alarm, 2026-07-11): under the G1 evacuation-failure retry
/// (`retry_after_evacuation_failure`), a drain pass allocates its to-space
/// from regions the FIRST pass just evacuated and freed — so a first-pass
/// FROM-address (a pointer_map key) can be handed out again as a drain
/// DESTINATION (a pointer_map value) for a *different* object. A slot the
/// remap correctly rewrote to such a recycled destination still matches a
/// key, but it is a fresh, correct reference to the address's NEW occupant,
/// not a missed remap (`CRATONVM_DBG_BUG03` traces show the slot being
/// rewritten `old → recycled` in the very pause that would flag `recycled`).
/// Such addresses are therefore skipped; with `CRATONVM_GC_VERIFY_STALE=1`
/// they are reported separately as benign. The same ambiguity is inherent
/// to the heavy STALE-DEST scan below — a value-and-key address cannot be
/// classified as "intermediate hop" vs "recycled epoch" from the map alone.
///
/// Returns the number of genuine (non-recycled) stale reports, for tests.
fn verify_no_stale_refs(
    thread: &crate::threading::jvm_thread::JvmThread,
    pointer_map: &cratonvm_types::PointerMap,
) -> usize {
    use crate::types::Value;
    use cratonvm_types::ObjectHeader;

    let mut genuine_reports = 0usize;
    // Lazily-built set of this pause's destination addresses (map VALUES),
    // used to recognise recycled destinations. Built only when a candidate
    // stale slot is actually found, so the common (clean) path pays nothing.
    let mut destinations: Option<std::collections::HashSet<usize>> = None;

    // Allow opt-in heavy diagnostic that walks every Object slot and checks
    // for a zeroed header (class_id=0 && identity_hash_code=0 && num_slots=0).
    // Such a slot is the in-memory signature of the heavy-trees bug: a
    // pointer at an address inside the just-reset young-from semispace.
    let heavy = cratonvm_types::flags::runtime_var("CRATONVM_GC_VERIFY_STALE")
        .ok()
        .as_deref()
        == Some("1");

    // Build the set of "stale destination addresses" — addresses that appear
    // as VALUES in pointer_map but ALSO as KEYS. These are intermediate
    // forwarding points: minor GC promoted an object to addr A, then major
    // GC compacted A to B. A slot still pointing at A is silently stale —
    // the header check above might NOT trigger (A could be overwritten by
    // a slid object's data), but the slot is wrong.
    let stale_destinations: std::collections::HashSet<usize> = if heavy {
        pointer_map
            .values()
            // A genuine intermediate point is an address that something moved TO
            // and that was THEN relocated elsewhere. A self-forward (`k -> k`)
            // makes `k` both a value and a key but represents NO movement, so
            // exclude it (`pointer_map[v] != v`) to avoid a false STALE-DEST flag.
            .filter(|v| pointer_map.get(v).is_some_and(|nv| nv != *v))
            .copied()
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    for (fi, frame) in thread.frames.iter().enumerate() {
        let cname = frame.class_name();
        let mname = frame.method_name();
        // Check locals
        for li in 0..frame.locals_len() {
            let val = frame.get_local(li as u16);
            if let Value::Object(Some(obj_ref)) = val {
                let addr = obj_ref.as_ptr() as usize;
                // A `key == value` entry is a SELF-FORWARD (evacuation failure:
                // the object stayed in place because to-space was exhausted), so
                // a slot pointing at it is correct, not stale. Only flag entries
                // that actually relocated the object.
                if let Some(&new_addr) = pointer_map.get(&addr) {
                    if new_addr != addr {
                        let dests = destinations
                            .get_or_insert_with(|| pointer_map.values().copied().collect());
                        if dests.contains(&addr) {
                            // Recycled destination — correctly-rewritten slot
                            // (see fn doc). Benign; report only under the
                            // heavy opt-in flag.
                            if heavy {
                                eprintln!(
                                    "POST-GC RECYCLED-DEST LOCAL (benign): frame[{}] {}.{} \
                                     local[{}] holds 0x{:x} — a drain-pass destination that \
                                     is also an earlier-pass key (-> 0x{:x})",
                                    fi, cname, mname, li, addr, new_addr,
                                );
                            }
                        } else {
                            genuine_reports += 1;
                            eprintln!(
                                "POST-GC STALE LOCAL: frame[{}] {}.{} local[{}] still points to \
                                 relocated addr 0x{:x} (should be 0x{:x})",
                                fi, cname, mname, li, addr, new_addr,
                            );
                        }
                    }
                }
                if heavy && addr != 0 {
                    if stale_destinations.contains(&addr) {
                        eprintln!(
                            "POST-GC STALE-DEST LOCAL: frame[{}] {}.{} local[{}] pc={} \
                             points to intermediate addr 0x{:x} which was further relocated to 0x{:x}",
                            fi, cname, mname, li, frame.pc,
                            addr, pointer_map[&addr],
                        );
                    }
                    // SAFETY: read-only probe of an aligned address; if the
                    // slot is corrupt we'll see it in the diagnostic. This is
                    // an opt-in debug path.
                    let h = unsafe { &*(addr as *const ObjectHeader) };
                    if h.class_id.as_u32() == 0
                        && h.mark_word.load(std::sync::atomic::Ordering::Relaxed) == 0
                        && h.num_slots() == 0
                        && h.array_length() == 0
                    {
                        eprintln!(
                            "POST-GC ZERO-HEADER LOCAL: frame[{}] {}.{} local[{}] pc={} \
                             points to ZEROED header at 0x{:x} (kind={:?}, gc_flags=0x{:x})",
                            fi,
                            cname,
                            mname,
                            li,
                            frame.pc,
                            addr,
                            h.kind(),
                            h.gc_flags(),
                        );
                    }
                }
            }
        }
        // Check stack
        for si in 0..frame.stack.len() {
            let val = frame.stack.get_value(si);
            if let Value::Object(Some(obj_ref)) = val {
                let addr = obj_ref.as_ptr() as usize;
                // Skip self-forwards (`key == value`): the object did not move,
                // so this slot is correct (see the locals check above).
                if let Some(&new_addr) = pointer_map.get(&addr) {
                    if new_addr != addr {
                        let dests = destinations
                            .get_or_insert_with(|| pointer_map.values().copied().collect());
                        if dests.contains(&addr) {
                            // Recycled destination — benign (see fn doc).
                            if heavy {
                                eprintln!(
                                    "POST-GC RECYCLED-DEST STACK (benign): frame[{}] {}.{} \
                                     stack[{}] holds 0x{:x} — a drain-pass destination that \
                                     is also an earlier-pass key (-> 0x{:x})",
                                    fi, cname, mname, si, addr, new_addr,
                                );
                            }
                        } else {
                            genuine_reports += 1;
                            eprintln!(
                                "POST-GC STALE STACK: frame[{}] {}.{} stack[{}] still points to \
                                 relocated addr 0x{:x} (should be 0x{:x})",
                                fi, cname, mname, si, addr, new_addr,
                            );
                        }
                    }
                }
                if heavy && addr != 0 {
                    if stale_destinations.contains(&addr) {
                        eprintln!(
                            "POST-GC STALE-DEST STACK: frame[{}] {}.{} stack[{}] pc={} \
                             points to intermediate addr 0x{:x} which was further relocated to 0x{:x}",
                            fi, cname, mname, si, frame.pc,
                            addr, pointer_map[&addr],
                        );
                    }
                    // SAFETY: see locals comment above.
                    let h = unsafe { &*(addr as *const ObjectHeader) };
                    if h.class_id.as_u32() == 0
                        && h.mark_word.load(std::sync::atomic::Ordering::Relaxed) == 0
                        && h.num_slots() == 0
                        && h.array_length() == 0
                    {
                        eprintln!(
                            "POST-GC ZERO-HEADER STACK: frame[{}] {}.{} stack[{}] pc={} \
                             points to ZEROED header at 0x{:x} (kind={:?}, gc_flags=0x{:x})",
                            fi,
                            cname,
                            mname,
                            si,
                            frame.pc,
                            addr,
                            h.kind(),
                            h.gc_flags(),
                        );
                    }
                }
            }
        }
    }
    genuine_reports
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::memory::heap::Heap;

    /// Helper to create a test heap with small capacity for GC testing.
    fn small_heap() -> Heap {
        // 8 KB total (4 KB per semi-space) — forces GC quickly
        Heap::with_capacity(8 * 1024)
    }

    /// Test-only `StopTheWorldToken`. Single-threaded test harness.
    #[inline]
    fn stw() -> cratonvm_gc::collector::StopTheWorldToken {
        unsafe { cratonvm_gc::collector::StopTheWorldToken::new() }
    }

    #[test]
    fn moving_gc_rewrites_live_handle_slots_in_place() {
        // SAFETY: these aligned non-null addresses are never dereferenced.
        let old = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
        let unmoved = unsafe { ObjectRef::from_raw(0x3000usize as *mut u8) };
        let mut slots = vec![Some(old), None, Some(unmoved)];
        let pointer_map = cratonvm_types::PointerMap::from_iter([(0x1000usize, 0x2000usize)]);

        assert_eq!(remap_handle_slots(&mut slots, &pointer_map), 1);
        assert_eq!(slots[0].unwrap().as_ptr() as usize, 0x2000);
        assert!(slots[1].is_none());
        assert_eq!(slots[2].unwrap().as_ptr() as usize, 0x3000);
    }

    /// The remap half of `roots.rs` §6b. The scan half pushes every cached
    /// proxy-dispatch `Method` as an unconditional root, so under a moving
    /// young collection the object IS evacuated — and the cache kept serving
    /// the from-space address to every later dispatch on the same key. The
    /// object Java then received had a zeroed body, so `Method.getName()`
    /// returned `null` and Spring's String-switch on it threw
    /// `NullPointerException: Cannot invoke "String.hashCode()"`.
    ///
    /// The third entry is the load-bearing one: an UNMOVED cache value must be
    /// left exactly as it is. A remap that rewrote every value (rather than
    /// only pointer-map keys) would corrupt entries the collector pinned.
    #[test]
    fn moving_gc_rewrites_the_proxy_method_cache_in_place() {
        // SAFETY: these aligned non-null addresses are never dereferenced —
        // the remap is pure address arithmetic against the pointer map.
        let moved = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
        let also_moved = unsafe { ObjectRef::from_raw(0x2000usize as *mut u8) };
        let pinned = unsafe { ObjectRef::from_raw(0x3000usize as *mut u8) };

        let mut cache: std::collections::HashMap<&'static str, ObjectRef> =
            std::collections::HashMap::new();
        cache.insert("Api.alpha()Ljava/lang/String;", moved);
        cache.insert(
            "Api.bravo(Ljava/lang/String;)Ljava/lang/String;",
            also_moved,
        );
        cache.insert("Api.charlie(II)I", pinned);

        let pointer_map = cratonvm_types::PointerMap::from_iter([
            (0x1000usize, 0x8000usize),
            (0x2000usize, 0x9000usize),
        ]);

        assert_eq!(remap_proxy_method_cache(&mut cache, &pointer_map), 2);
        assert_eq!(
            cache["Api.alpha()Ljava/lang/String;"].as_ptr() as usize,
            0x8000,
            "a relocated cached Method must follow the move, or the next              dispatch serves Java a from-space object whose `name` reads null"
        );
        assert_eq!(
            cache["Api.bravo(Ljava/lang/String;)Ljava/lang/String;"].as_ptr() as usize,
            0x9000,
        );
        assert_eq!(
            cache["Api.charlie(II)I"].as_ptr() as usize,
            0x3000,
            "an entry absent from the pointer map did not move and must not be rewritten"
        );
    }

    /// §10 post-move fixup covers ALL FOUR per-thread single-slot references.
    ///
    /// `jit_pending_exception` is here because it once lived in a
    /// `thread_local!` this function could not name, so a moving collection
    /// left the interpreter's drain reading a from-space address.
    /// `uncaught_exception_pending` is here for the same shape one level up:
    /// the launcher held the fatal throwable in a Rust LOCAL across
    /// shutdown-hook execution, where neither half could reach it, and a
    /// collection inside a hook reclaimed it — the render then read a zeroed
    /// header as `ClassId(0)` and printed `Exception in thread "main"
    /// java/lang/Object`. See
    /// `fixed-suite-bugs/h2-suite-bugs/bug-h2-testopenclose-throwable-is-java-lang-object-FIXED-20260830.md`.
    ///
    /// The count below is the point of the test: a new slot added to
    /// `remap_thread_object_slots` without being added here leaves the guard
    /// asserting a stale total, which is how a half-wired slot gets in.
    #[test]
    fn moving_gc_rewrites_every_per_thread_object_slot() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        // SAFETY: these aligned non-null addresses are never dereferenced —
        // the remap is pure address arithmetic against the pointer map.
        let mirror = unsafe { ObjectRef::from_raw(0x1000usize as *mut u8) };
        let async_exc = unsafe { ObjectRef::from_raw(0x2000usize as *mut u8) };
        let jit_exc = unsafe { ObjectRef::from_raw(0x3000usize as *mut u8) };
        let uncaught = unsafe { ObjectRef::from_raw(0x4000usize as *mut u8) };

        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.java_thread_obj = Some(mirror);
        thread.pending_async_exception = Some(async_exc);
        thread.jit_pending_exception = Some(jit_exc);
        thread.uncaught_exception_pending = Some(uncaught);

        let pointer_map = cratonvm_types::PointerMap::from_iter([
            (0x1000usize, 0x8000usize),
            (0x2000usize, 0x9000usize),
            (0x3000usize, 0xA000usize),
            (0x4000usize, 0xB000usize),
        ]);

        assert_eq!(remap_thread_object_slots(&mut thread, &pointer_map), 4);
        assert_eq!(
            thread.uncaught_exception_pending.unwrap().as_ptr() as usize,
            0xB000,
            "the launcher's parked throwable must come back at its post-move address"
        );
        assert_eq!(
            thread.java_thread_obj.unwrap().as_ptr() as usize,
            0x8000,
            "java_thread_obj must follow the move"
        );
        assert_eq!(
            thread.pending_async_exception.unwrap().as_ptr() as usize,
            0x9000,
            "pending_async_exception must follow the move"
        );
        assert_eq!(
            thread.jit_pending_exception.unwrap().as_ptr() as usize,
            0xA000,
            "the JIT's pending throwable must follow the move — a stale address \
             here is handed straight to the interpreter's post-JIT drain"
        );
    }

    /// A slot whose object did not move, and an empty slot, must both come
    /// through untouched — the remap must not invent or drop references.
    #[test]
    fn per_thread_object_slot_remap_leaves_unmoved_and_empty_slots_alone() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        // SAFETY: never dereferenced; see the sibling test.
        let unmoved = unsafe { ObjectRef::from_raw(0x5000usize as *mut u8) };

        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.jit_pending_exception = Some(unmoved);

        // A non-empty map that simply does not mention our object.
        let pointer_map = cratonvm_types::PointerMap::from_iter([(0x1000usize, 0x8000usize)]);

        assert_eq!(remap_thread_object_slots(&mut thread, &pointer_map), 0);
        assert_eq!(
            thread.jit_pending_exception.unwrap().as_ptr() as usize,
            0x5000
        );
        assert!(thread.java_thread_obj.is_none());
        assert!(thread.pending_async_exception.is_none());
    }

    /// End-to-end: stash a JIT pending exception on the thread, run a *real*
    /// moving collection with that object as the only root, and check the
    /// thread's slot now names the object's post-copy address — and that the
    /// object is genuinely still there (its field survives the copy).
    ///
    /// This is the shape the bug had: the reference is parked in a slot while a
    /// collection relocates the object, and the drain that follows must not see
    /// the pre-move address.
    #[test]
    fn jit_pending_exception_survives_and_follows_a_moving_collection() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let heap = small_heap();
        let monitor_table = crate::threading::monitor::MonitorTable::new();

        // Stand in for the throwable the JIT helper would have constructed.
        let exc = heap.alloc_object(ClassId::new(7), 1);
        heap.set_field(exc, 0, Value::Int(0x5EED));

        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.jit_pending_exception = Some(exc);

        // The scan half: §10 of `roots.rs` pushes this slot. Model it directly
        // (that file is the collector's root list, not this module's) so the
        // object is reachable for the copy.
        let mut roots = vec![exc];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

        let moved_to = roots[0];
        assert_ne!(
            moved_to.as_ptr(),
            exc.as_ptr(),
            "the collection must actually relocate the object, or this test \
             proves nothing about the remap"
        );

        // The remap half: what this module owns.
        remap_thread_object_slots(&mut thread, &result.pointer_map);

        let drained = thread
            .jit_pending_exception
            .take()
            .expect("the pending exception must survive the collection");
        assert_eq!(
            drained.as_ptr(),
            moved_to.as_ptr(),
            "the drain must see the post-move address"
        );
        assert_eq!(
            heap.get_field(drained, 0).as_int(),
            Some(0x5EED),
            "the relocated throwable must still be a live, readable object"
        );
    }

    #[test]
    fn gc_basic_copy_single_object() {
        let heap = small_heap();
        let obj = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(100));

        let mut roots = vec![obj];
        let (mut from, mut to) = heap.lock_spaces();
        let result = collect(&mut from, &mut to, &mut roots);

        assert_eq!(result.stats.objects_copied, 1);

        // Root should be updated
        let new_obj = roots[0];
        assert_ne!(new_obj.as_ptr(), obj.as_ptr());

        // Read fields from to-space
        let header = unsafe { &*(new_obj.as_ptr() as *const crate::memory::heap::ObjectHeader) };
        assert_eq!(header.class_id, ClassId::new(1));
        assert_eq!(header.num_slots(), 2);

        // Read field values from to-space via raw pointers
        let field0_ptr = unsafe { new_obj.as_ptr().add(crate::memory::heap::HEADER_SIZE) };
        let field0: Value = unsafe { std::ptr::read(field0_ptr as *const Value) };
        assert_eq!(field0.as_int(), Some(42));

        let field1_ptr = unsafe {
            new_obj
                .as_ptr()
                .add(crate::memory::heap::HEADER_SIZE + crate::memory::heap::SLOT_SIZE)
        };
        let field1: Value = unsafe { std::ptr::read(field1_ptr as *const Value) };
        assert_eq!(field1.as_long(), Some(100));
    }

    #[test]
    fn gc_full_cycle_via_collect_garbage() {
        let heap = small_heap();
        let monitor_table = crate::threading::monitor::MonitorTable::new();

        let obj = heap.alloc_object(ClassId::new(1), 2);
        heap.set_field(obj, 0, Value::Int(42));
        heap.set_field(obj, 1, Value::Long(100));

        let mut roots = vec![obj];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

        assert_eq!(result.stats.objects_copied, 1);
        assert_eq!(roots.len(), 1);

        let new_obj = roots[0];
        assert_ne!(new_obj.as_ptr(), obj.as_ptr());

        assert_eq!(heap.get_field(new_obj, 0).as_int(), Some(42));
        assert_eq!(heap.get_field(new_obj, 1).as_long(), Some(100));
    }

    #[test]
    fn gc_full_cycle_with_monitor_remap() {
        use crate::threading::jvm_thread::ThreadId;

        let heap = small_heap();
        let monitor_table = crate::threading::monitor::MonitorTable::new();
        let tid = ThreadId(1);

        let obj = heap.alloc_object(ClassId::new(0), 0);
        monitor_table.enter(obj, tid);

        let mut roots = vec![obj];
        let _result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

        let new_obj = roots[0];
        assert_ne!(new_obj.as_ptr(), obj.as_ptr());

        assert!(monitor_table.exit(new_obj, tid).is_ok());
        assert!(monitor_table.exit(new_obj, tid).is_err());
    }

    #[test]
    fn update_value_ref_updates_known_ptr() {
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000usize, 0x2000usize);

        let obj_ref = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
        let mut val = Value::Object(Some(obj_ref));
        update_value_ref(&mut val, &map);

        match val {
            Value::Object(Some(r)) => assert_eq!(r.as_ptr() as usize, 0x2000),
            _ => panic!("expected updated reference"),
        }
    }

    /// Recycled-destination false-alarm regression (SpinPollMark `-Xmx80m`
    /// OOM pressure, 2026-07-11): under the G1 evacuation-failure retry a
    /// drain pass can hand out a first-pass FROM-address as its to-space
    /// destination, so the composed pointer map legitimately holds both
    /// `K -> V` (drain move into the recycled region) and `V -> C` (the
    /// first pass moving V's PREVIOUS occupant out). A slot correctly
    /// rewritten to `V` must NOT be reported stale; a slot holding a key
    /// that is NOT also a destination is a genuine missed remap and must be.
    #[test]
    fn verify_no_stale_refs_ignores_recycled_destinations() {
        use crate::runtime::frame::Frame;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        map.insert(0x20013B00000, 0x20010200000); // drain: K -> V (V recycled)
        map.insert(0x20010200000, 0x20013DC02E8); // pass 1: V -> C (old occupant)
        map.insert(0x50000000, 0x60000000); // unrelated genuine move

        // local[0] = a correctly-rewritten recycled destination -> benign;
        // local[1] = a from-address nothing rewrote -> genuine stale.
        let recycled = unsafe { ObjectRef::from_raw(0x20010200000usize as *mut u8) };
        let missed = unsafe { ObjectRef::from_raw(0x50000000usize as *mut u8) };

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[Value::Object(Some(recycled)), Value::Object(Some(missed))],
        );
        thread.frames.push(frame);

        assert_eq!(
            verify_no_stale_refs(&thread, &map),
            1,
            "recycled destination must be benign; unrewritten key must report"
        );
    }

    #[test]
    fn gc_update_all_roots_integration() {
        use crate::memory::vm_heap::{GcBackend, VmHeap};
        use crate::runtime::frame::Frame;
        use crate::threading::jvm_thread::{JvmThread, ThreadId};

        // B10: `update_object_refs` requires a `&VmHeap` for the heap-
        // membership filter; construct one alongside the legacy semi-space
        // `Heap` used by the older standalone GC tests.
        let vm_heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
        let heap = small_heap();
        let monitor_table = crate::threading::monitor::MonitorTable::new();

        let obj = heap.alloc_object(ClassId::new(0), 1);
        heap.set_field(obj, 0, Value::Int(42));

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[Value::Object(Some(obj)), Value::Int(99)],
        );
        frame.stack.push(Value::Object(Some(obj))).unwrap();
        thread.frames.push(frame);

        thread.printed.push(Value::Object(Some(obj)));

        let old_ptr = obj.as_ptr();

        let mut roots = vec![obj, obj, obj];
        let result = heap.collect_garbage(&stw(), &mut roots, &monitor_table);

        thread.frames[0].update_local_refs(&result.pointer_map, &vm_heap);
        thread.frames[0]
            .stack
            .update_object_refs(&result.pointer_map, &vm_heap);
        for val in &mut thread.printed {
            update_value_ref(val, &result.pointer_map);
        }

        match thread.frames[0].get_local(0) {
            Value::Object(Some(r)) => {
                assert_ne!(r.as_ptr(), old_ptr);
                assert_eq!(heap.get_field(r, 0).as_int(), Some(42));
            }
            other => unreachable!("expected updated object ref in locals, got {other:?}"),
        }
        assert_eq!(thread.frames[0].get_local(1).as_int(), Some(99));

        match thread.frames[0].stack.get_value(0) {
            Value::Object(Some(r)) => {
                assert_ne!(r.as_ptr(), old_ptr);
            }
            other => unreachable!("expected updated object ref in stack, got {other:?}"),
        }

        match &thread.printed[0] {
            Value::Object(Some(r)) => {
                assert_ne!(r.as_ptr(), old_ptr);
            }
            other => unreachable!("expected updated object ref in printed, got {other:?}"),
        }
    }
}
