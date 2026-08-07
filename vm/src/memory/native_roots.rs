// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Uniform, **VM-scoped** GC-root registry for native side-tables.
//!
//! # Why this exists
//!
//! A number of native subsystems hold `ObjectRef`s in Rust side-tables that are
//! invisible to the field-tracing root scan in
//! [`crate::memory::roots::collect_roots`]. Historically each such subsystem
//! had to be wired in by hand in TWO places:
//!
//!   * a `gc_scan_*_roots(&mut Vec<ObjectRef>)` call appended to `collect_roots`
//!     (so the held objects are marked live), and
//!   * a `gc_update_*_refs(&cratonvm_types::PointerMap)` call appended to
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
//! [`VM_ROOT_SOURCES`] — a single compile-time inventory of named
//! `(scan, remap)` pairs. Adding a source means adding one `root_source!` row,
//! which cannot compile without supplying **both** halves; that is the whole
//! point of the table, since a half-wired source is exactly the use-after-free
//! described above. [`scan_all_roots`] and [`remap_all_roots`] fan out over it
//! from `collect_roots` and the post-move fixup respectively.
//!
//! # Every source is VM-scoped
//!
//! Both callbacks receive the **owning** [`crate::vm::SharedVm`]. That is not a
//! convenience: a process can own several heaps at once (the inline test
//! modules build a `SharedVm` per test, and `libcratonvm` can create more than
//! one VM), so a source that answers from process-global state hands VM B's
//! collector addresses belonging to VM A's heap, and lets VM B's post-move
//! fixup rewrite VM A's entries through VM B's relocation map.
//!
//! A source whose backing store is genuinely process-global therefore either
//! keys that store on `shared.vm_identity` (the logmanager, security-manager,
//! boxed-value and `java.lang.instrument` rows all do) or is a documented,
//! deliberate exception that ignores the `SharedVm` argument (`_:`).
//!
//! There used to be a second, **VM-agnostic** registry here —
//! `register_native_root_source(scan: fn(&mut Vec<ObjectRef>), remap:
//! fn(&cratonvm_types::PointerMap))`, a `LazyLock<RwLock<Vec<..>>>` that subsystems
//! joined lazily on first use. It is gone. Its callbacks had no way to know
//! which VM was collecting, so every subsystem that joined it was an isolation
//! bug by construction; its last two members (the `ObjectStreamClass` cache and
//! the `ClassFileTransformer` chain) have been re-keyed per VM and moved into
//! the table below. Do not reintroduce it: a new side table belongs in
//! `VM_ROOT_SOURCES`, keyed on `vm_identity` if it must live in a static.
//!
//! # Thread / lifecycle safety
//!
//! The callbacks run while the world is stopped (root collection / post-move
//! fixup), exactly like every other entry in `collect_roots` /
//! `update_all_roots`, so they observe a quiescent heap.

use crate::types::ObjectRef;
use std::collections::HashMap;


// ---------------------------------------------------------------------------
// CRATONVM_DBG_ROOTPROF=1 -- per-root-source timing.
//
// A generational young pause on a Spring workload was measured at 500-1538 ms
// with only ~2/3 of it attributable to the collector's own phases. The root
// fan-out here is one of the two unmeasured halves, and it contains a full
// walk of every overlay-backed collection in the process
// (`scan_collection_overlays` / `remap_collection_overlays`), which is
// proportional to the whole heap rather than to the young set. Off by
// default; when off this costs one `OnceLock` read per fan-out.
// ---------------------------------------------------------------------------
pub(crate) mod rootprof {
    use std::sync::OnceLock;

    static ON: OnceLock<bool> = OnceLock::new();

    pub fn on() -> bool {
        *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOTPROF").is_some()
        })
    }

    /// Print `parts` as one line when `total_ms` clears the noise floor.
    pub fn report(what: &str, total_ns: u128, parts: &[(&'static str, u128, usize)]) {
        if total_ns < 20_000_000 {
            return;
        }
        let mut detail = String::new();
        for (name, ns, n) in parts {
            if *ns < 1_000_000 {
                continue;
            }
            detail.push_str(&format!(" {}={}ms/{}", name, ns / 1_000_000, n));
        }
        eprintln!(
            "[rootprof] {} took {}ms{}",
            what,
            total_ns / 1_000_000,
            detail
        );
    }
}

type VmScanFn = fn(&crate::vm::SharedVm, &mut Vec<ObjectRef>);
type VmRemapFn = fn(&crate::vm::SharedVm, &cratonvm_types::PointerMap);

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
fn remap_value_caches(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::lang_math::gc_update_value_of_cache_refs(shared.vm_identity, map);
}
fn scan_unsafe(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::gc_scan_unsafe_side_store_roots(roots);
}
fn remap_unsafe(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::gc_update_unsafe_side_store_refs(map);
}
fn scan_lambda_callsites(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_invoke::gc_scan_lambda_callsite_cache_roots(roots);
}
fn remap_lambda_callsites(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::lang_invoke::gc_update_lambda_callsite_cache_refs(map);
}
fn scan_lambda_singletons(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    crate::runtime::invokedynamic::gc_scan_lambda_singleton_roots(shared.vm_identity, roots);
}
fn remap_lambda_singletons(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
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
fn remap_collection_overlays(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_gc::external_roots::remap_external_roots(map);
}
fn scan_loaders_and_jmx(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::classloader::gc_scan_loader_singleton_roots(
        shared.vm_identity,
        roots,
    );
    #[cfg(feature = "management")]
    cratonvm_native_builtins::jmx::gc_scan_platform_mbean_server_root(roots);
}
fn remap_loaders_and_jmx(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::classloader::gc_update_loader_singleton_refs(
        shared.vm_identity,
        map,
    );
    #[cfg(feature = "management")]
    cratonvm_native_builtins::jmx::gc_update_platform_mbean_server_ref(map);
}
fn scan_system(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_system::gc_scan_system_singleton_roots(
        shared.vm_identity,
        roots,
    );
}
fn remap_system(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::lang_system::gc_update_system_singleton_refs(shared.vm_identity, map);
}
fn scan_locale(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::gc_scan_locale_roots(roots);
}
fn remap_locale(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::gc_update_locale_refs(map);
}
fn scan_class_values(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::phases_late::gc_scan_classvalue_cache_roots(
        shared.vm_identity,
        roots,
        &|addr| shared.mem.heap.metadata_pin_deferrable(addr),
    );
}
fn remap_class_values(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::phases_late::gc_update_classvalue_cache_refs(shared.vm_identity, map);
}
fn scan_msc(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::jboss_msc::gc_scan_msc_service_roots(roots);
}
fn remap_msc(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::jboss_msc::gc_update_msc_service_refs(map);
}
// Scoped to THIS VM. The logmanager side-tables hold raw heap addresses, and
// the process can own several heaps at once (the inline test module builds a
// `SharedVm` per test). Reporting another VM's address as a root here would
// hand the collector a pointer into a heap it does not own.
fn scan_logmanager(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::logmanager::gc_scan_logmanager_roots(shared.vm_identity, roots);
}
fn remap_logmanager(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::logmanager::gc_update_logmanager_refs(shared.vm_identity, map);
}
// Per-VM for the same reason as the logmanager above: the security manager, the
// default `Policy` object and the shared `Permissions` collection are keyed on
// `vm_identity` in `native-builtins`, so reporting another VM's address here
// would hand the collector a pointer into a heap it does not own.
//
// The remap half is the load-bearing one. These refs used to be cached in
// process-global statics the collector never rewrote — only the per-VM
// `var_handle_roots` registry entry was remapped, leaving the cached copy stale
// across a moving collection.
fn scan_security_manager(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::security_manager::gc_scan_security_manager_roots(
        shared.vm_identity,
        roots,
    );
}
fn remap_security_manager(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::security_manager::gc_update_security_manager_refs(
        shared.vm_identity,
        map,
    );
}
// The `ObjectStreamClass` descriptor cache. VM-scoped because the cache is a
// field of THIS VM's `ClassRealm`: `scan_roots` walks only the receiver's map.
//
// It used to register a process-global `Mutex<Vec<*const OscMap>>` of raw
// backing-map pointers through `register_native_root_source`, which has no VM
// parameter — so every VM's collection walked every live cache, reporting one
// heap's addresses to another heap's collector and rewriting one VM's entries
// through another VM's relocation map. See `runtime::serialization::oscache`.
fn scan_osc_cache(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    shared.classes.osc_cache.scan_roots(roots);
}
fn remap_osc_cache(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    shared.classes.osc_cache.remap_roots(map);
}
fn scan_annotations(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_class::gc_scan_annotation_proxy_roots(
        shared.vm_identity,
        roots,
    );
}
fn remap_annotations(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::lang_class::gc_update_annotation_proxy_refs(shared.vm_identity, map);
}
fn scan_http_handlers(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_re10_handler_roots(roots);
}
fn remap_http_handlers(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::net_phase_e::gc_update_re10_handler_refs(map);
}
fn scan_datagram_sockets(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_ds_roots(roots);
}
fn remap_datagram_sockets(_: &crate::vm::SharedVm, map: &HashMap<usize, usize>) {
    cratonvm_native_builtins::net_phase_e::gc_update_ds_refs(map);
}
fn scan_inet_addresses(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_inet_addr_roots(roots);
}
fn remap_inet_addresses(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::net_phase_e::gc_update_inet_addr_refs(map);
}
fn scan_nio(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_io::nio_selector::gc_scan_selector_roots(roots);
    cratonvm_native_io::socket_channel::gc_scan_channel_roots(roots);
    cratonvm_native_io::socket_channel::gc_scan_ss_back_ref_roots(roots);
}
fn remap_nio(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_io::nio_selector::sk_table_update_after_gc(map);
    cratonvm_native_io::socket_channel::channel_fields_update_after_gc(map);
    cratonvm_native_io::socket_channel::ss_back_ref_update_after_gc(map);
}
fn scan_server_ports(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_api::server_socket_ports::gc_scan_roots(roots);
}
fn remap_server_ports(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_api::server_socket_ports::gc_update_after_gc(map);
}
fn scan_scheduled(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::scheduled_pump::gc_scan_scheduled_roots(roots);
}
fn remap_scheduled(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::scheduled_pump::gc_update_scheduled_refs(map);
}
fn scan_xnio(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::xnio_async::gc_scan_xnio_future_roots(roots);
}
fn remap_xnio(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::xnio_async::gc_update_xnio_future_refs(map);
}
fn scan_panama(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::panama::gc_scan_upcall_target_roots(roots);
}
fn remap_panama(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::panama::gc_update_upcall_target_refs(map);
}
fn scan_tls(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_trust_manager_roots(roots);
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_key_manager_roots(roots);
    cratonvm_native_builtins::t27_tls::gc_scan_default_ssl_context_root(roots);
}
fn remap_tls(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_trust_manager_refs(map);
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_key_manager_refs(map);
    cratonvm_native_builtins::t27_tls::gc_update_default_ssl_context_ref(map);
}
fn scan_forkjoin(_: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::phases_early::gc_scan_forkjoin_roots(roots);
}
fn remap_forkjoin(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::phases_early::gc_update_forkjoin_refs(map);
}
fn scan_upcalls(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    shared.natives.upcall_table.lock().collect_roots(roots);
}
fn remap_upcalls(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    shared.natives.upcall_table.lock().update_after_gc(map);
}
// The `java.lang.instrument` `ClassFileTransformer` chain. Entries hold the
// live `ClassFileTransformer` mirrors registered by Mockito's MockMaker /
// JaCoCo's coverage agent, which are typically reachable from nowhere else.
//
// VM-scoped for the same reason as the logmanager and `ObjectStreamClass`
// caches: the chain used to be ONE process-global `Vec<TransformerEntry>`
// hooked in through the VM-agnostic `register_native_root_source` fan-out, so
// every VM's collection walked every VM's transformers — reporting one heap's
// addresses to another heap's collector and rewriting one VM's entries through
// another VM's relocation map. See `runtime::instrument`.
//
// It was the last `register_native_root_source` caller, and the VM-agnostic
// registry is gone with it.
fn scan_instrument_transformers(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    crate::runtime::instrument::scan_transformer_roots(shared.vm_identity, roots);
}
fn remap_instrument_transformers(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    crate::runtime::instrument::remap_transformer_refs(shared.vm_identity, map);
}

/// Compile-time inventory of every VM/native side table that owns movable
/// references. Adding a source requires supplying scan and remap together.
static VM_ROOT_SOURCES: &[VmRootSource] = &[
    root_source!(
        "security-manager",
        scan_security_manager,
        remap_security_manager
    ),
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
    root_source!("osc-cache", scan_osc_cache, remap_osc_cache),
    root_source!("annotation-proxies", scan_annotations, remap_annotations),
    root_source!("http-handlers", scan_http_handlers, remap_http_handlers),
    root_source!("inet-addresses", scan_inet_addresses, remap_inet_addresses),
    root_source!(
        "datagram-sockets",
        scan_datagram_sockets,
        remap_datagram_sockets
    ),
    root_source!("nio", scan_nio, remap_nio),
    root_source!("server-ports", scan_server_ports, remap_server_ports),
    root_source!("scheduled-pump", scan_scheduled, remap_scheduled),
    root_source!("xnio-futures", scan_xnio, remap_xnio),
    root_source!("panama-upcalls", scan_panama, remap_panama),
    root_source!("tls", scan_tls, remap_tls),
    root_source!("forkjoin", scan_forkjoin, remap_forkjoin),
    root_source!("vm-upcalls", scan_upcalls, remap_upcalls),
    root_source!(
        "instrument-transformers",
        scan_instrument_transformers,
        remap_instrument_transformers
    ),
];

pub fn scan_all_roots(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    if rootprof::on() {
        let t_all = std::time::Instant::now();
        let mut parts: Vec<(&'static str, u128, usize)> = Vec::new();
        for source in VM_ROOT_SOURCES {
            let before = roots.len();
            let t = std::time::Instant::now();
            (source.scan)(shared, roots);
            parts.push((source.name, t.elapsed().as_nanos(), roots.len() - before));
        }
        rootprof::report("scan_all_roots", t_all.elapsed().as_nanos(), &parts);
        return;
    }
    for source in VM_ROOT_SOURCES {
        debug_assert!(!source.name.is_empty());
        (source.scan)(shared, roots);
    }
}

pub fn remap_all_roots(shared: &crate::vm::SharedVm, pointer_map: &cratonvm_types::PointerMap) {
    if rootprof::on() {
        let t_all = std::time::Instant::now();
        let mut parts: Vec<(&'static str, u128, usize)> = Vec::new();
        for source in VM_ROOT_SOURCES {
            let t = std::time::Instant::now();
            (source.remap)(shared, pointer_map);
            parts.push((source.name, t.elapsed().as_nanos(), 0));
        }
        rootprof::report("remap_all_roots", t_all.elapsed().as_nanos(), &parts);
        return;
    }
    for source in VM_ROOT_SOURCES {
        debug_assert!(!source.name.is_empty());
        (source.remap)(shared, pointer_map);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

    /// Every root source must supply BOTH halves as *distinct* functions. A row
    /// that accidentally names the same function twice would either scan on the
    /// remap path or remap on the scan path; the type system cannot catch it
    /// (both halves are `fn(&SharedVm, ..)`-shaped once coerced), so assert it.
    #[test]
    fn every_root_source_pairs_two_distinct_halves() {
        for source in VM_ROOT_SOURCES {
            assert_ne!(
                source.scan as usize, source.remap as usize,
                "root source {} names the same function as both scan and remap",
                source.name
            );
        }
    }

    /// The `ClassFileTransformer` chain must be in the inventory. It was the
    /// last subsystem hooked in through the deleted VM-agnostic
    /// `register_native_root_source` fan-out; if this row is ever dropped, a
    /// moving collection reclaims or staleness-poisons every registered
    /// transformer with no compile error to show for it.
    /// The two `DatagramSocket`-keyed side tables in `net_phase_e` hold state
    /// the JDK requires to outlive `close()` — `getSoTimeout`, `getBroadcast`,
    /// and the connected peer behind `getPort`/`getInetAddress`/`isConnected`.
    /// Both are `HashMap<ObjectRef, _>`, so a moving young collection that
    /// relocates a socket strands its entry and every one of those getters
    /// silently reverts to its default.
    ///
    /// Measured with `probes/DsGcProbe`, 64 sockets across ~800k young-gen
    /// allocations: with this row present, `DSGCPROBE OK`; with it removed and
    /// nothing else changed, **288 failures** — `soTimeout 0`, `port -1`,
    /// `peer null`, `isConnected false`. Not a lookup miss: a silent wrong
    /// answer, which is why `addr_keyed`'s census exists.
    #[test]
    fn datagram_socket_side_tables_are_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES.iter().any(|s| s.name == "datagram-sockets"),
            "ds_side_table and ds_peer_table must be a VM root source"
        );
    }

    #[test]
    fn instrument_transformer_chain_is_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES
                .iter()
                .any(|s| s.name == "instrument-transformers"),
            "the java.lang.instrument transformer chain must be a VM root source"
        );
    }
}
