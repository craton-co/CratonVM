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
        *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOTPROF").is_some())
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

    // ── Conservative native-stack scan counters ────────────────────────────
    //
    // WHY THESE EXIST. `conservative_roots::scan_one_frame` and
    // `native_stack_has_jit_frame` are the two functions that walk raw native
    // stack memory a word at a time, and between them they were 3.3% of a
    // `DefaultCatalogAndSchemaTest` profile. Attributing that to a CALLER took
    // several rounds and never succeeded by sampling: the release build omits
    // frame pointers so `perf --call-graph fp` yields nothing, and `dwarf`
    // unwinding gives up on this VM's stack depths. Every candidate caller was
    // then argued from static call sites — and the arguments kept being wrong,
    // because the plausible drivers (`update_root_snapshot`, `collect_roots`,
    // `deposit_root_snapshot`) run at wildly different rates and only counting
    // separates them.
    //
    // So count. A relaxed `fetch_add` per CALL (never per word) is free next to
    // the bulk loop it measures, and the word totals turn "this function is 2%
    // of CPU" into "it is 2% because it reads N words of stack per second",
    // which is the number that says whether to make the scan cheaper or to
    // stop calling it.
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    pub static SCAN_FRAME_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static SCAN_FRAME_WORDS: AtomicU64 = AtomicU64::new(0);
    pub static SCAN_FRAME_HITS: AtomicU64 = AtomicU64::new(0);
    pub static JITPROBE_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static JITPROBE_WORDS: AtomicU64 = AtomicU64::new(0);

    /// Fold one `scan_one_frame` pass into the counters, and print a cumulative
    /// line every 4096 passes. No-op unless [`on`].
    pub fn note_stack_scan(words: u64, hits: u64) {
        if !on() {
            return;
        }
        SCAN_FRAME_WORDS.fetch_add(words, Relaxed);
        SCAN_FRAME_HITS.fetch_add(hits, Relaxed);
        let n = SCAN_FRAME_CALLS.fetch_add(1, Relaxed) + 1;
        if n % 4096 == 0 {
            let w = SCAN_FRAME_WORDS.load(Relaxed);
            eprintln!(
                "[rootprof] conservative-scan calls={n} words={w} hits={} avg_words={} | jitprobe calls={} words={}",
                SCAN_FRAME_HITS.load(Relaxed),
                w / n,
                JITPROBE_CALLS.load(Relaxed),
                JITPROBE_WORDS.load(Relaxed),
            );
            let c = scan_caller_counts();
            eprintln!(
                "[rootprof] scan_active_jit_frames by caller: gc-roots={} safepoint={} blocked-deposit={}",
                c[0], c[1], c[2],
            );
        }
    }

    /// Fold one `native_stack_has_jit_frame` probe into the counters.
    pub fn note_jit_probe(words: u64) {
        if !on() {
            return;
        }
        JITPROBE_WORDS.fetch_add(words, Relaxed);
        JITPROBE_CALLS.fetch_add(1, Relaxed);
    }

    /// Per-CALLER tally for `conservative_roots::scan_active_jit_frames`.
    ///
    /// Three call sites drive it and they run at rates three orders of
    /// magnitude apart — `collect_roots` once per collection, the safepoint
    /// `update_root_snapshot`, and `deposit_root_snapshot_inner` on every
    /// blocked-region entry. Sampling cannot separate them here (no frame
    /// pointers, dwarf unwinding fails on this VM's stack depths) and reading
    /// the call sites did not either. Index: 0 = gc-roots, 1 = safepoint,
    /// 2 = blocked-deposit.
    ///
    /// A fourth slot for "the scan `CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN=1`
    /// did NOT run" was added here and removed again: this reporter only fires
    /// every 4096 `note_stack_scan` passes, and `note_stack_scan` is driven by
    /// the scan the skip removes — so the one arm that needed the counter is
    /// exactly the arm that can never print it. `CRATONVM_DBG_JIT_SCAN_PROF`'s
    /// exit-time `scans=` is the engagement counter for that switch instead,
    /// and it answered cleanly: 8,495 with the skip off, 0 with it on.
    pub static SCAN_BY_CALLER: [AtomicU64; 3] =
        [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

    pub fn note_scan_caller(which: usize) {
        if on() {
            SCAN_BY_CALLER[which].fetch_add(1, Relaxed);
        }
    }

    /// `[gc-roots, safepoint, blocked-deposit]` call counts.
    pub fn scan_caller_counts() -> [u64; 3] {
        [
            SCAN_BY_CALLER[0].load(Relaxed),
            SCAN_BY_CALLER[1].load(Relaxed),
            SCAN_BY_CALLER[2].load(Relaxed),
        ]
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
    // `young_marker_follows_side_tables()` is the canonical predicate for
    // "this cycle's precise marker will do owner-based overlay propagation
    // itself" -- the same one `VmHeap::mirror_pin_deferrable` uses for the
    // analogous class-mirror case. The three-way OR this replaced
    // (`is_active` / `unregistered_jit_frame_on_stack` / `major_gc_requested`)
    // covered only the JIT-safety diversion reasons for non-moving young, not
    // an explicit `System.gc()` reaching the non-moving sweep by the
    // `explicit_full_gc` term `young_marker_follows_side_tables` already
    // folds in -- so a plain, no-JIT-frame `System.gc()` (this test's exact
    // shape) fell through to the unconditional scan, which roots every
    // element of every overlay-backed collection with no reachability gate
    // at all (see gc_and_alloc.rs's `scan_collection_overlay_roots` comment).
    //
    // ZGC unconditionally defers too (mirroring `mirror_pin_deferrable`'s own
    // `VmHeap::Zgc(_) => true` arm): its single STW mark-sweep closure is
    // always the precise, owner-based marker (`zgc.rs`'s `collect_garbage`
    // mark loop now calls `external_roots_for_owner` from every confirmed-live
    // object, the same shape as Generational's non-moving young marker and
    // old-gen BFS), so there is no non-precise ZGC cycle to protect against.
    let conditional = match shared.config.gc_algorithm {
        crate::config::GcAlgorithm::Generational => {
            cratonvm_gc::gc_quiescence::young_marker_follows_side_tables()
        }
        #[cfg(feature = "zgc")]
        crate::config::GcAlgorithm::Zgc => true,
        crate::config::GcAlgorithm::G1 => false,
    };
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OVERLAY_GATE").is_some() {
        eprintln!(
            "[OVERLAYGATE] conditional={conditional} gc_algo={:?} major_gc_requested={} is_active={} unregistered_jit={}",
            shared.config.gc_algorithm,
            cratonvm_gc::gc_quiescence::major_gc_requested(),
            cratonvm_gc::gc_quiescence::is_active(),
            cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack(),
        );
    }
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
    cratonvm_native_builtins::classloader::gc_update_loader_singleton_refs(shared.vm_identity, map);
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
    shared
        .classes
        .osc_cache
        .scan_roots(shared.vm_identity, roots);
}
fn remap_osc_cache(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    shared.classes.osc_cache.remap_roots(map);
}
fn scan_annotations(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::lang_class::gc_scan_annotation_proxy_roots(shared.vm_identity, roots);
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
fn remap_datagram_sockets(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
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
    // `ServerSocketChannel.socket()`'s adaptor cache. It used to live in slot 5
    // of the channel object — really `AbstractSelectableChannel.keys` — so it
    // needed no roots and corrupted a JDK field instead; moving it to a side
    // table is what makes these two lines necessary
    // (W7-72-ssc-socket-and-filechannel.md).
    cratonvm_native_io::socket_channel::gc_scan_ssc_socket_cache_roots(roots);
}
fn remap_nio(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_io::nio_selector::sk_table_update_after_gc(map);
    cratonvm_native_io::socket_channel::channel_fields_update_after_gc(map);
    cratonvm_native_io::socket_channel::ss_back_ref_update_after_gc(map);
    cratonvm_native_io::socket_channel::ssc_socket_cache_update_after_gc(map);
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
    // The live `PrivateKey` objects a `KeyManagerState` holds for keys with no
    // PKCS#8 encoding of their own (netty's `OpenSslPrivateKey`, PKCS#11 keys).
    // Nothing else references them once `KeyManagerFactory.init` returns.
    cratonvm_native_builtins::x509_manager::gc_scan_key_manager_roots(roots);
}
fn remap_tls(_: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_trust_manager_refs(map);
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_key_manager_refs(map);
    cratonvm_native_builtins::t27_tls::gc_update_default_ssl_context_ref(map);
    cratonvm_native_builtins::x509_manager::gc_update_key_manager_refs(map);
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
    if root_attribution_on() {
        // CRATONVM_DBG_ROOT_SOURCE: remember which NAMED source contributed
        // each root this cycle.
        //
        // The inventory has always had names and nothing has ever been able to
        // answer "which of them rooted THIS object". The retention questions in
        // this repo are almost always that question — the fourth
        // `TestDefaultInstanceManager` recurrence spent four investigations
        // eliminating root sources one at a time, by turning levers off and
        // re-running, because there was no way to just ask. A `Vec` of
        // (name, addr) built only under the flag turns that into one run.
        let mut attribution = Vec::new();
        for source in VM_ROOT_SOURCES {
            let before = roots.len();
            (source.scan)(shared, roots);
            for r in &roots[before..] {
                attribution.push((source.name, r.as_ptr() as usize));
            }
        }
        set_root_attribution(attribution);
        return;
    }
    for source in VM_ROOT_SOURCES {
        debug_assert!(!source.name.is_empty());
        (source.scan)(shared, roots);
    }
}

/// `CRATONVM_DBG_ROOT_SOURCE` — record which named root source contributed
/// each root, so a retained object can be attributed to one instead of having
/// every source eliminated by bisection.
pub fn root_attribution_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOT_SOURCE").is_some())
}

/// Last cycle's (source name, root address) pairs. Diagnostic only; written
/// once per collection and only when the flag is on.
static ROOT_ATTRIBUTION: std::sync::OnceLock<parking_lot::Mutex<Vec<(&'static str, usize)>>> =
    std::sync::OnceLock::new();

fn root_attribution() -> &'static parking_lot::Mutex<Vec<(&'static str, usize)>> {
    ROOT_ATTRIBUTION.get_or_init(|| parking_lot::Mutex::new(Vec::new()))
}

fn set_root_attribution(v: Vec<(&'static str, usize)>) {
    *root_attribution().lock() = v;
}

/// Which named root source contributed `addr` as a root this cycle, if any.
///
/// `None` means no source handed this exact address to the marker — the object
/// is reachable THROUGH something, not rooted directly, which is a different
/// finding and wants a different fix.
pub fn root_source_of(addr: usize) -> Option<&'static str> {
    if !root_attribution_on() {
        return None;
    }
    root_attribution()
        .lock()
        .iter()
        .find(|&&(_, a)| a == addr)
        .map(|&(n, _)| n)
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
