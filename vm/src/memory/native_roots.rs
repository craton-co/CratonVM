// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Uniform GC-root registry for process-global native side-tables.
//!
//! # Why this exists
//!
//! A number of native subsystems hold `ObjectRef`s in **process-global Rust
//! side-tables** that are invisible to the field-tracing root scan in
//! [`crate::memory::roots::collect_roots`]. Historically each such subsystem
//! had to be wired in by hand in TWO places:
//!
//!   * a `gc_scan_*_roots(&mut Vec<ObjectRef>)` call appended to `collect_roots`
//!     (so the held objects are marked live), and
//!   * a `gc_update_*_refs(&HashMap<usize, usize>)` call appended to
//!     `gc::update_all_roots` (so the held `ObjectRef`s are repointed to their
//!     relocated addresses after a moving/compacting collection).
//!
//! Forgetting **either** half is a use-after-free: a missing scan lets a moving
//! young-gen GC reclaim an object still referenced by the side-table; a missing
//! remap leaves the side-table pointing at a vacated from-space slot. The review
//! found ~8 subsystems in exactly this position (native-collection overlays, the
//! NIO `SelectionKey` table, the `ClassFileTransformer` chain, the
//! `ObjectStreamClass` cache, value-stack-smuggled jobjects, the scheduled pump,
//! and the xnio `IoFuture` table).
//!
//! # What this provides
//!
//! A single global registry. A subsystem calls
//! [`register_native_root_source`] **once** (typically from its lazy
//! initialization), supplying two top-level function pointers:
//!
//!   * a **scan** callback — `fn(&mut Vec<ObjectRef>)` — that pushes each live
//!     `ObjectRef` it currently holds (identical shape to the existing
//!     `gc_scan_*_roots` helpers, so a subsystem can register its existing
//!     function verbatim), and
//!   * a **remap** callback — `fn(&HashMap<usize, usize>)` — that rewrites each
//!     held `ObjectRef` using the collector's old→new relocation map (identical
//!     shape to the existing `gc_update_*_refs` helpers).
//!
//! [`scan_all_native_roots`] (driven from `collect_roots`) fans out to every
//! registered scan callback; [`remap_all_native_roots`] (driven from the
//! post-move fixup in `gc::update_all_roots`) fans out to every registered remap
//! callback. With an empty registry both are no-ops, so this is a **zero
//! behaviour-change** addition that is safe to merge before any subsystem
//! adopts it.
//!
//! # Thread / lifecycle safety
//!
//! Registration is safe from any thread, before or after the GC has started —
//! the registry is a `LazyLock<RwLock<…>>` (mirroring the JNI-native-method
//! table in `native::jni`). Registration is **idempotent**: re-registering the
//! same `(scan, remap)` function-pointer pair is silently ignored, so a
//! subsystem that registers lazily on first use cannot double-scan (which would
//! merely over-retain) or double-remap.
//!
//! The callbacks run while the world is stopped (root collection / post-move
//! fixup), exactly like every other entry in `collect_roots` /
//! `update_all_roots`, so they observe a quiescent heap.

use crate::types::ObjectRef;
use std::collections::HashMap;
use std::sync::LazyLock;

use parking_lot::RwLock;

/// A scan callback: append every live `ObjectRef` the subsystem currently holds
/// to `roots`. Shape-compatible with the existing `gc_scan_*_roots` helpers so
/// a subsystem can register one of those directly.
pub type ScanFn = fn(&mut Vec<ObjectRef>);

/// A remap callback: for each `ObjectRef` the subsystem holds, look its current
/// address up in `pointer_map` (the collector's old→new relocation map) and, if
/// present, rewrite it to the new address. Shape-compatible with the existing
/// `gc_update_*_refs` helpers. A well-behaved implementation early-returns on an
/// empty map (nothing moved — the non-moving sweep).
pub type RemapFn = fn(&HashMap<usize, usize>);

/// One registered native root source: a paired scan + remap callback. The two
/// halves describe the same set of held `ObjectRef`s — scanning keeps them live,
/// remapping keeps them valid across a move.
#[derive(Clone, Copy)]
struct NativeRootSource {
    scan: ScanFn,
    remap: RemapFn,
}

/// Process-global registry of native root sources.
///
/// Modelled on `native::jni::JNI_NATIVE_METHODS` (`LazyLock<RwLock<…>>`): cheap
/// to read (the GC pause path takes a read lock and iterates), rare to write
/// (subsystems register once at init). `parking_lot::RwLock` matches the rest of
/// the VM and is poison-free, so a panicking callback cannot brick the registry.
static REGISTRY: LazyLock<RwLock<Vec<NativeRootSource>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a native root source: a `scan` callback that yields every live
/// `ObjectRef` the subsystem holds, and a `remap` callback that repoints each
/// held `ObjectRef` after a moving collection.
///
/// Call this **once** per subsystem, typically from its lazy initialization.
/// Registration is idempotent: registering the same `(scan, remap)` pair more
/// than once is a no-op, so a subsystem that initializes lazily (and might race
/// to register from multiple threads) never ends up double-scanned.
///
/// Safe to call from any thread, before or after GC has started. The callbacks
/// are invoked only at a GC safepoint (root collection / post-move fixup) with
/// the world stopped, so they observe a quiescent heap — exactly the contract
/// the existing hand-wired `gc_scan_*` / `gc_update_*` helpers rely on.
///
/// # Function-pointer requirement
///
/// `scan` and `remap` are `fn` pointers (not `Box<dyn Fn>`): every subsystem
/// registers a top-level `fn`, which sidesteps lifetime/`'static`-closure
/// concerns and keeps the registry `Copy`-cheap to iterate. The matched pair is
/// deduplicated by comparing the raw code addresses of the two pointers.
pub fn register_native_root_source(scan: ScanFn, remap: RemapFn) {
    let mut reg = REGISTRY.write();
    // Idempotent: dedupe by the raw code addresses of BOTH pointers. Two
    // distinct `fn` items have distinct addresses; the same `fn` item compares
    // equal, so a subsystem that registers lazily on first use (possibly racing
    // across threads) registers exactly once.
    let scan_addr = scan as usize;
    let remap_addr = remap as usize;
    let already = reg
        .iter()
        .any(|s| s.scan as usize == scan_addr && s.remap as usize == remap_addr);
    if !already {
        reg.push(NativeRootSource { scan, remap });
    }
}

/// Run every registered scan callback, appending each subsystem's live
/// `ObjectRef`s to `roots`. Called from
/// [`crate::memory::roots::collect_roots`] alongside the hand-wired root
/// sources. No-op (zero behaviour change) when the registry is empty.
///
/// A read lock is held for the duration so registration cannot race the fan-out;
/// since callbacks only *read* their side-tables and *push* into `roots`, this
/// can never deadlock against another `scan_all` / `remap_all`.
pub fn scan_all_native_roots(roots: &mut Vec<ObjectRef>) {
    let reg = REGISTRY.read();
    for source in reg.iter() {
        (source.scan)(roots);
    }
}

/// Run every registered remap callback, repointing each subsystem's held
/// `ObjectRef`s through `pointer_map`. Called from
/// [`crate::memory::gc::update_all_roots`] alongside the hand-wired
/// `*_update_after_gc` hooks, on the post-move fixup path. No-op when the
/// registry is empty.
///
/// `pointer_map` is the collector's old-address → new-address relocation map (an
/// empty map means nothing moved — the non-moving sweep); each callback is
/// expected to honour that and early-return.
pub fn remap_all_native_roots(pointer_map: &HashMap<usize, usize>) {
    let reg = REGISTRY.read();
    for source in reg.iter() {
        (source.remap)(pointer_map);
    }
}

type VmScanFn = fn(&crate::vm::SharedVm, &mut Vec<ObjectRef>);
type VmRemapFn = fn(&crate::vm::SharedVm, &HashMap<usize, usize>);

#[derive(Clone, Copy)]
struct VmRootSource {
    name: &'static str,
    scan: VmScanFn,
    remap: VmRemapFn,
}

macro_rules! root_source {
    ($name:literal, $scan:ident, $remap:ident) => {
        VmRootSource {
            name: $name,
            scan: $scan,
            remap: $remap,
        }
    };
}

fn scan_value_caches(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_math::gc_scan_value_of_cache_roots(shared.vm_identity, roots);
}
fn remap_value_caches(shared: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::lang_math::gc_update_value_of_cache_refs(shared.vm_identity, map);
}
fn scan_unsafe(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::gc_scan_unsafe_side_store_roots(roots);
}
fn remap_unsafe(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::gc_update_unsafe_side_store_refs(map);
}
fn scan_lambda_callsites(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_invoke::gc_scan_lambda_callsite_cache_roots(roots);
}
fn remap_lambda_callsites(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::lang_invoke::gc_update_lambda_callsite_cache_refs(map);
}
fn scan_lambda_singletons(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    crate::runtime::invokedynamic::gc_scan_lambda_singleton_roots(shared.vm_identity, roots);
}
fn remap_lambda_singletons(shared: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    crate::runtime::invokedynamic::gc_update_lambda_singleton_refs(shared.vm_identity, map);
}
fn scan_collection_overlays(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let conditional = shared.config.gc_algorithm == crate::config::GcAlgorithm::Generational
        && (cratonvm_gc::gc_quiescence::is_active()
            || cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack()
            || cratonvm_gc::gc_quiescence::major_gc_requested());
    if !conditional {
        cratonvm_gc::external_roots::scan_external_roots(roots);
    }
}
fn remap_collection_overlays(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_gc::external_roots::remap_external_roots(map);
}
fn scan_loaders_and_jmx(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::classloader::gc_scan_loader_singleton_roots(roots);
    #[cfg(feature = "management")]
    cratonvm_native_builtins::jmx::gc_scan_platform_mbean_server_root(roots);
}
fn remap_loaders_and_jmx(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::classloader::gc_update_loader_singleton_refs(map);
    #[cfg(feature = "management")]
    cratonvm_native_builtins::jmx::gc_update_platform_mbean_server_ref(map);
}
fn scan_system(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_system::gc_scan_system_singleton_roots(roots);
}
fn remap_system(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::lang_system::gc_update_system_singleton_refs(map);
}
fn scan_locale(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::gc_scan_locale_roots(roots);
}
fn remap_locale(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::gc_update_locale_refs(map);
}
fn scan_class_values(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::phases_late::gc_scan_classvalue_cache_roots(roots, &|addr| {
        shared.mem.heap.metadata_pin_deferrable(addr)
    });
}
fn remap_class_values(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::phases_late::gc_update_classvalue_cache_refs(map);
}
fn scan_msc(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::jboss_msc::gc_scan_msc_service_roots(roots);
}
fn remap_msc(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::jboss_msc::gc_update_msc_service_refs(map);
}
// Scoped to THIS VM. The logmanager side-tables hold raw heap addresses, and
// the process can own several heaps at once (the inline test module builds a
// `SharedVm` per test). Reporting another VM's address as a root here would
// hand the collector a pointer into a heap it does not own.
fn scan_logmanager(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::logmanager::gc_scan_logmanager_roots(shared.vm_identity, roots);
}
fn remap_logmanager(shared: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::logmanager::gc_update_logmanager_refs(shared.vm_identity, map);
}
fn scan_annotations(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_class::gc_scan_annotation_proxy_roots(roots);
}
fn remap_annotations(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::lang_class::gc_update_annotation_proxy_refs(map);
}
fn scan_http_handlers(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_re10_handler_roots(roots);
}
fn remap_http_handlers(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::net_phase_e::gc_update_re10_handler_refs(map);
}
fn scan_inet_addresses(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_inet_addr_roots(roots);
}
fn remap_inet_addresses(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::net_phase_e::gc_update_inet_addr_refs(map);
}
fn scan_nio(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_io::nio_selector::gc_scan_selector_roots(roots);
    cratonvm_native_io::socket_channel::gc_scan_channel_roots(roots);
    cratonvm_native_io::socket_channel::gc_scan_ss_back_ref_roots(roots);
}
fn remap_nio(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_io::nio_selector::sk_table_update_after_gc(map);
    cratonvm_native_io::socket_channel::channel_fields_update_after_gc(map);
    cratonvm_native_io::socket_channel::ss_back_ref_update_after_gc(map);
}
fn scan_server_ports(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_api::server_socket_ports::gc_scan_roots(roots);
}
fn remap_server_ports(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_api::server_socket_ports::gc_update_after_gc(map);
}
fn scan_scheduled(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::scheduled_pump::gc_scan_scheduled_roots(roots);
}
fn remap_scheduled(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::scheduled_pump::gc_update_scheduled_refs(map);
}
fn scan_xnio(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::xnio_async::gc_scan_xnio_future_roots(roots);
}
fn remap_xnio(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::xnio_async::gc_update_xnio_future_refs(map);
}
fn scan_panama(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::panama::gc_scan_upcall_target_roots(roots);
}
fn remap_panama(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::panama::gc_update_upcall_target_refs(map);
}
fn scan_tls(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_trust_manager_roots(roots);
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_key_manager_roots(roots);
    cratonvm_native_builtins::t27_tls::gc_scan_default_ssl_context_root(roots);
}
fn remap_tls(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_trust_manager_refs(map);
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_key_manager_refs(map);
    cratonvm_native_builtins::t27_tls::gc_update_default_ssl_context_ref(map);
}
fn scan_forkjoin(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::phases_early::gc_scan_forkjoin_roots(roots);
}
fn remap_forkjoin(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::phases_early::gc_update_forkjoin_refs(map);
}
fn scan_upcalls(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    shared.natives.upcall_table.lock().collect_roots(roots);
}
fn remap_upcalls(shared: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    shared.natives.upcall_table.lock().update_after_gc(map);
}

/// Compile-time inventory of every VM/native side table that owns movable
/// references. Adding a source requires supplying scan and remap together.
static VM_ROOT_SOURCES: &[VmRootSource] = &[
    root_source!("boxed-value-caches", scan_value_caches, remap_value_caches),
    root_source!("unsafe-side-store", scan_unsafe, remap_unsafe),
    root_source!(
        "lambda-callsites",
        scan_lambda_callsites,
        remap_lambda_callsites
    ),
    root_source!(
        "lambda-singletons",
        scan_lambda_singletons,
        remap_lambda_singletons
    ),
    root_source!(
        "collection-overlays",
        scan_collection_overlays,
        remap_collection_overlays
    ),
    root_source!(
        "classloaders-and-jmx",
        scan_loaders_and_jmx,
        remap_loaders_and_jmx
    ),
    root_source!("system-singletons", scan_system, remap_system),
    root_source!("locale", scan_locale, remap_locale),
    root_source!("class-values", scan_class_values, remap_class_values),
    root_source!("jboss-msc", scan_msc, remap_msc),
    root_source!("logmanager", scan_logmanager, remap_logmanager),
    root_source!("annotation-proxies", scan_annotations, remap_annotations),
    root_source!("http-handlers", scan_http_handlers, remap_http_handlers),
    root_source!("inet-addresses", scan_inet_addresses, remap_inet_addresses),
    root_source!("nio", scan_nio, remap_nio),
    root_source!("server-ports", scan_server_ports, remap_server_ports),
    root_source!("scheduled-pump", scan_scheduled, remap_scheduled),
    root_source!("xnio-futures", scan_xnio, remap_xnio),
    root_source!("panama-upcalls", scan_panama, remap_panama),
    root_source!("tls", scan_tls, remap_tls),
    root_source!("forkjoin", scan_forkjoin, remap_forkjoin),
    root_source!("vm-upcalls", scan_upcalls, remap_upcalls),
];

pub fn scan_all_roots(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    for source in VM_ROOT_SOURCES {
        debug_assert!(!source.name.is_empty());
        (source.scan)(shared, roots);
    }
    scan_all_native_roots(roots);
}

pub fn remap_all_roots(shared: &crate::vm::SharedVm, pointer_map: &HashMap<usize, usize>) {
    for source in VM_ROOT_SOURCES {
        debug_assert!(!source.name.is_empty());
        (source.remap)(shared, pointer_map);
    }
    remap_all_native_roots(pointer_map);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // The registry is process-global, so these tests register their OWN unique
    // top-level callbacks (each test uses a distinct `fn` so the dedupe key is
    // unique) and only assert on the refs THEY contribute — never on the total
    // count, which other registrations (real or test) may inflate. All test
    // refs use synthetic, 8-byte-aligned non-heap addresses; they are never
    // dereferenced, only compared by pointer value.

    /// Fabricate a never-dereferenced `ObjectRef` from a fixed aligned address.
    fn fake_ref(addr: usize) -> ObjectRef {
        debug_assert!(addr != 0 && addr % 8 == 0);
        // SAFETY: the address is non-null and 8-byte aligned; the ref is only
        // ever compared by pointer value in these tests, never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    // ----- source #1: a fixed pair of refs, remapped via the pointer map -----
    const REF_A: usize = 0xAAAA_0000;
    const REF_B: usize = 0xBBBB_0000;

    fn scan_src1(roots: &mut Vec<ObjectRef>) {
        roots.push(fake_ref(REF_A));
        roots.push(fake_ref(REF_B));
    }
    fn remap_src1(pointer_map: &HashMap<usize, usize>) {
        // Record into a side-channel that the remap ran and with what mapping,
        // so the test can verify the relocation function was applied.
        if let Some(&new_a) = pointer_map.get(&REF_A) {
            SRC1_REMAP_A.store(new_a, Ordering::SeqCst);
        }
    }
    static SRC1_REMAP_A: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn scan_all_visits_registered_refs() {
        register_native_root_source(scan_src1, remap_src1);

        let mut roots = Vec::new();
        scan_all_native_roots(&mut roots);

        assert!(
            roots.contains(&fake_ref(REF_A)),
            "scan_all must surface the source's first ref"
        );
        assert!(
            roots.contains(&fake_ref(REF_B)),
            "scan_all must surface the source's second ref"
        );
    }

    #[test]
    fn remap_all_applies_relocation() {
        register_native_root_source(scan_src1, remap_src1);

        let mut pointer_map = HashMap::new();
        let relocated_a = 0xCCCC_0000usize;
        pointer_map.insert(REF_A, relocated_a);

        SRC1_REMAP_A.store(0, Ordering::SeqCst);
        remap_all_native_roots(&pointer_map);

        assert_eq!(
            SRC1_REMAP_A.load(Ordering::SeqCst),
            relocated_a,
            "remap_all must invoke the source's remap with the relocation map"
        );
    }

    // ----- source #2: counts how many times its scan callback fired ----------
    static SRC2_SCANS: AtomicUsize = AtomicUsize::new(0);

    fn scan_src2(_roots: &mut Vec<ObjectRef>) {
        SRC2_SCANS.fetch_add(1, Ordering::SeqCst);
    }
    fn remap_src2(_pointer_map: &HashMap<usize, usize>) {}

    #[test]
    fn registration_is_idempotent() {
        // Register the SAME pair three times; the dedupe must keep exactly one.
        register_native_root_source(scan_src2, remap_src2);
        register_native_root_source(scan_src2, remap_src2);
        register_native_root_source(scan_src2, remap_src2);

        SRC2_SCANS.store(0, Ordering::SeqCst);
        let mut roots = Vec::new();
        scan_all_native_roots(&mut roots);

        assert_eq!(
            SRC2_SCANS.load(Ordering::SeqCst),
            1,
            "an idempotently-registered source must scan exactly once per fan-out"
        );
    }

    // ----- source #3: proves an empty pointer-map is handled gracefully ------
    fn scan_src3(_roots: &mut Vec<ObjectRef>) {}
    fn remap_src3(pointer_map: &HashMap<usize, usize>) {
        // A well-behaved remap early-returns on the non-moving sweep.
        if pointer_map.is_empty() {
            SRC3_SAW_EMPTY.store(true, Ordering::SeqCst);
        }
    }
    static SRC3_SAW_EMPTY: AtomicBool = AtomicBool::new(false);

    #[test]
    fn remap_all_with_empty_map_is_safe() {
        register_native_root_source(scan_src3, remap_src3);

        SRC3_SAW_EMPTY.store(false, Ordering::SeqCst);
        let empty: HashMap<usize, usize> = HashMap::new();
        remap_all_native_roots(&empty);

        assert!(
            SRC3_SAW_EMPTY.load(Ordering::SeqCst),
            "remap_all must still fan out (callbacks self-guard the empty map)"
        );
    }

    #[test]
    fn built_in_inventory_has_unique_named_scan_remap_pairs() {
        let mut names = HashSet::new();
        for source in VM_ROOT_SOURCES {
            assert!(!source.name.is_empty());
            assert!(
                names.insert(source.name),
                "duplicate built-in root source: {}",
                source.name
            );
            assert_ne!(source.scan as usize, 0);
            assert_ne!(source.remap as usize, 0);
        }
    }
}
