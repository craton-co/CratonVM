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
// The null-base static store and the synthetic per-receiver field store. NOT
// loader-conditional. VM-scoped since gc-common w7-a: both stores used to be
// keyed without a VM (by offset, and by a per-heap identity hash every VM
// mints from the same start), so two VMs shared slots outright and each VM's
// collection rooted and remapped the other VM's values
// (`common-w6a-unsafe-synthetic-field-store-is-shared-across-vms`). The
// `Class$Atomic` slots that used to be scanned here too are the
// `class-atomic-slots` row below.
fn scan_unsafe(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::gc_scan_unsafe_side_store_roots(shared.vm_identity, roots);
}
fn remap_unsafe(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::gc_update_unsafe_side_store_refs(shared.vm_identity, map);
}

/// Root `value`, or — when this cycle licenses loader-conditional metadata
/// (`defer`, [`crate::memory::roots::loader_metadata_licence`]), the heap
/// says the marker that consumes this root set follows `metadata_pin` for this
/// address, and `owner` is a class a user loader defined — pin it to that
/// loader instead, so it lives exactly as long as the loader does.
///
/// The same rule `roots.rs` steps 2 (statics), 3 (class locks) and 6b (proxy
/// `Method`s) apply inline, and `phases_late::gc_scan_classvalue_cache_roots`
/// applies to `ClassValue` results. Rows that use it: `class-atomic-slots`,
/// `annotation-proxies`, and (since gc-common w5-b, which removed their own
/// copies of the rule) `indy-call-sites` and `osc-cache`, `type-variables`
/// (gc-common w30-b), `defined-packages` for its class-owned rows
/// (gc-common w31-a), and (interpreter round i1 wave 8) `lambda-singletons`.
/// It RECORDS the pin before it skips the
/// root, so it can never skip a root it did not replace. `owner == None` (a
/// primitive mirror, an uncached annotation proxy) and a built-in-loader owner
/// (no `loader_pin` row) are rooted outright.
fn defer_or_root(
    shared: &crate::vm::SharedVm,
    defer: bool,
    owner: Option<u32>,
    value: ObjectRef,
    roots: &mut Vec<ObjectRef>,
) {
    let addr = value.as_ptr() as usize;
    if defer && shared.mem.heap.metadata_pin_deferrable(addr) {
        // THIS VM's pin only: a colliding class id's row can belong to another
        // VM's heap (`roots::vm_loader_pin_addr`).
        if let Some(loader) =
            owner.and_then(|cid| crate::memory::roots::vm_deferral_owner(shared, cid))
        {
            cratonvm_types::metadata_pin::add_metadata_pin(shared.vm_identity, loader, addr);
            return;
        }
    }
    roots.push(value);
}

// `Class.reflectionData` (`SoftReference<ReflectionData>`), `annotationType`
// and `annotationData`, held by the `Class$Atomic` natives in a per-VM store
// tagged with the owning `ClassId` (gc-common w3-b). Rooting these outright —
// which the `unsafe-side-store` row did until then — made every class that was
// ever reflected on (`getDeclaredMethod(s)`, `getMethods`,
// `getDeclaredConstructors`, `getAnnotations`, `ObjectStreamClass.lookup`)
// immortal with its loader: `SoftReference -> ReflectionData -> Method ->
// Class -> loader`. See
// `docs/internal/gc-common-round-20260923/common-w2b-class-atomic-side-store-roots-every-reflected-class-FIXED-20260923.md`.
fn scan_class_atomic(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    cratonvm_native_builtins::gc_scan_class_atomic_side_store(shared.vm_identity, &mut |owner, v| {
        defer_or_root(shared, defer, owner, v, roots)
    });
}
fn remap_class_atomic(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::gc_update_class_atomic_side_store_refs(shared.vm_identity, map);
}
// The reflective `LambdaMetafactory` `CallSite` cache, keyed by
// `(vm_identity, bootstrap args)` since gc-common w8-a, and by the lookup
// class (the proxy's host) since interpreter round i1 wave 9. Loader-
// conditional through `defer_or_root` with the host as owner, like the
// singletons below: a row of a user-loader host no longer keeps that loader
// alive, and goes with the host (`invokedynamic::forget_unloaded_lambda_proxy_rows`).
fn scan_lambda_callsites(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    cratonvm_native_builtins::lang_invoke::gc_scan_lambda_callsite_cache_roots(
        shared.vm_identity,
        &mut |host, v| defer_or_root(shared, defer, host, v, roots),
    );
}
fn remap_lambda_callsites(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::lang_invoke::gc_update_lambda_callsite_cache_refs(
        shared.vm_identity,
        map,
    );
}
// Loader-conditional through `defer_or_root` (interpreter round i1 wave 8):
// the owner is the PROXY class id, whose `loader_pin` row names its host's
// user loader (`invokedynamic::pin_lambda_proxy_to_host_loader`). A proxy of a
// built-in host has no row and is rooted outright, as before.
fn scan_lambda_singletons(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    crate::runtime::invokedynamic::gc_scan_lambda_singleton_roots(
        shared.vm_identity,
        &mut |proxy_id, singleton| defer_or_root(shared, defer, Some(proxy_id), singleton, roots),
    );
}
fn remap_lambda_singletons(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    crate::runtime::invokedynamic::gc_update_lambda_singleton_refs(shared.vm_identity, map);
}
// Per-instruction `CallSite`s of generic (non-JDK-factory) `invokedynamic`
// linkages. Loader-conditional, like the condy values; see
// `runtime::invokedynamic::GENERIC_INDY_SITES`.
//
// The deferral itself is `defer_or_root`'s (gc-common w5-b). The scan used to
// carry its own copy that read the process-global `metadata_weak_mode()` and
// the VM-less `loader_pin_addr`, so under another VM's licence -- or another
// VM's user-loader class at the same id -- it pinned this VM's call site to a
// loader this VM's marker never visits.
fn scan_indy_call_sites(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    crate::runtime::invokedynamic::gc_scan_generic_indy_roots(
        shared.vm_identity,
        &mut |class_id, call_site| defer_or_root(shared, defer, Some(class_id), call_site, roots),
    );
}
fn remap_indy_call_sites(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    crate::runtime::invokedynamic::gc_update_generic_indy_refs(shared.vm_identity, map);
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
    //
    // NOT while a Generational concurrent old-gen mark is open: its remark
    // pause consumes this root set through `ConcurrentMarker`, which never calls
    // `external_roots_for_owner`, so an overlay deferred to owner propagation
    // there is marked by nobody and its old-gen backing nodes are swept while
    // the owning collection is live. See `roots::conditional_loader_metadata`.
    let conditional = match shared.config.gc_algorithm {
        crate::config::GcAlgorithm::Generational => {
            cratonvm_gc::gc_quiescence::young_marker_follows_side_tables()
                && !crate::memory::roots::generational_concurrent_mark_open(shared)
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
    // gc-common w12-c: THIS VM's overlay rows only. The provider registry is
    // process-wide, and the VM-less `scan_external_roots` handed this
    // collector every VM's overlay elements as roots
    // (`common-b-process-global-root-sources`, its `collection-overlays` row).
    if !conditional {
        cratonvm_gc::external_roots::scan_external_roots_for_vm(shared.vm_identity, roots);
    }
}
fn remap_collection_overlays(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    // This VM's pointer map, applied to this VM's overlay rows only
    // (gc-common w12-c).
    cratonvm_gc::external_roots::remap_external_roots_for_vm(shared.vm_identity, map);
}
fn scan_loaders_and_jmx(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::classloader::gc_scan_loader_singleton_roots(
        shared.vm_identity,
        roots,
    );
    #[cfg(feature = "management")]
    cratonvm_native_builtins::jmx::gc_scan_platform_mbean_server_root_for_vm(
        shared.vm_identity,
        roots,
    );
}
fn remap_loaders_and_jmx(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::classloader::gc_update_loader_singleton_refs(shared.vm_identity, map);
    #[cfg(feature = "management")]
    cratonvm_native_builtins::jmx::gc_update_platform_mbean_server_ref_for_vm(
        shared.vm_identity,
        map,
    );
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
// `vm`'s cached default Locales only (w10-a per VM; since w11-d the subtag
// rows are weak and swept, not roots).
fn scan_locale(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::gc_scan_locale_roots(shared.vm_identity, roots);
}
fn remap_locale(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::gc_update_locale_refs(shared.vm_identity, map);
}
fn scan_class_values(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    // THIS VM's licence, folded into the per-value predicate (gc-common w2-b,
    // the cheap subset of `common-b-process-global-root-sources-FIXED-20260923.md`). The scan
    // used to defer a value only when the process-global `metadata_weak_mode()`
    // AND this predicate agreed; that flag is one process-wide value every VM's
    // `collect_roots` overwrites, so VM B's scan could defer under VM A's
    // licence. Since the w5-b handoff this predicate is the WHOLE licence, and
    // the scan pins to this VM's own loader row. Step 1's licence for this
    // cycle, not a recomputation (`roots::loader_metadata_licence`, w5-b).
    let this_vm_defers = crate::memory::roots::loader_metadata_licence(shared);
    cratonvm_native_builtins::phases_late::gc_scan_classvalue_cache_roots(
        shared.vm_identity,
        roots,
        &|addr| this_vm_defers && shared.mem.heap.metadata_pin_deferrable(addr),
        // gcd d2/g: the owner through the unload classification, as
        // `defer_or_root` does for every other native row (a class the unload
        // transaction never removes roots its value).
        &|cid| crate::memory::roots::vm_deferral_owner(shared, cid),
    );
}
fn remap_class_values(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::phases_late::gc_update_classvalue_cache_refs(shared.vm_identity, map);
}
// `AbstractClassLoaderValue` rows (gc-common w17-d): the value of a
// `(loader, ClassLoaderValue)` row stands in for the JDK's
// `cl.classLoaderValueMap` field, so under THIS VM's licence it is pinned to
// its loader (`metadata_pin`) instead of rooted, exactly as `class-values`
// does for `ClassValue` results. A bootstrap-loader row, and every row on a
// cycle without the licence, is rooted. It used to be a var-handle root,
// which kept a custom `ModuleLayer`'s loader alive through
// `ServicesCatalog -> Module -> loader`.
fn scan_classloader_values(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let this_vm_defers = crate::memory::roots::loader_metadata_licence(shared);
    cratonvm_native_builtins::classloader_value_sidetable::gc_scan_clv_value_roots(
        shared.vm_identity,
        roots,
        &|addr| this_vm_defers && shared.mem.heap.metadata_pin_deferrable(addr),
    );
}
fn remap_classloader_values(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::classloader_value_sidetable::gc_update_clv_value_refs(
        shared.vm_identity,
        map,
    );
}
// The reflection `TypeVariable` caches (gc-common w30-b): the build cache
// behind `Class` / `Method.getTypeParameters()` and every generic-signature
// conversion (`generics::gc_scan_type_parameter_roots`), and the
// `Method.getTypeParameters()` array memo
// (`lang_class::gc_scan_method_type_params_roots`). Each value is owned by a
// class -- the declaration's class, the method's declaring class -- and under
// THIS VM's licence it is pinned to that class's user loader, exactly as
// `class-values` does for `ClassValue` results; a built-in-loader or unknown
// owner, and every cycle without the licence, is rooted. Both were strong JNI
// global roots, and a `TypeVariable` names its declaration, so every class
// asked for its type parameters kept its loader for the life of the VM
// (`common-w29e-reflection-type-variable-caches-pin-their-class-loaders`).
fn scan_type_variables(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    cratonvm_native_builtins::generics::gc_scan_type_parameter_roots(
        shared.vm_identity,
        &mut |owner, tv| defer_or_root(shared, defer, owner, tv, roots),
    );
    cratonvm_native_builtins::lang_class::gc_scan_method_type_params_roots(
        shared.vm_identity,
        &mut |owner, arr| defer_or_root(shared, defer, owner, arr, roots),
    );
}
fn remap_type_variables(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::generics::gc_update_type_parameter_refs(shared.vm_identity, map);
    cratonvm_native_builtins::lang_class::gc_update_method_type_params_refs(
        shared.vm_identity,
        map,
    );
}
// The defined-`Package` memo behind `Class.getPackage()` and
// `ClassLoader.getDefinedPackage(s)` (gc-common w31-a,
// `lang_class::gc_scan_defined_package_roots`). A user loader's `Package`
// stands in for the JDK's `loader.packages` entry, so under THIS VM's licence
// it is pinned to that loader (`metadata_pin`), exactly as
// `classloader-values` does; a row owned only by a class goes through
// `defer_or_root`; a built-in namespace's row, and every row on a cycle
// without the licence, is rooted. They were strong JNI global roots, and a
// user loader's `Package` reaches the loader through its `module`, so one
// `getPackage()` kept the loader for the life of the VM
// (`common-w30b-defined-package-memo-pins-user-loaders`).
fn scan_defined_packages(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    cratonvm_native_builtins::lang_class::gc_scan_defined_package_roots(
        shared.vm_identity,
        &mut |loader, class_owner, pkg| match loader {
            Some(loader) => {
                let addr = pkg.as_ptr() as usize;
                if defer && shared.mem.heap.metadata_pin_deferrable(addr) {
                    cratonvm_types::metadata_pin::add_metadata_pin(
                        shared.vm_identity,
                        loader,
                        addr,
                    );
                } else {
                    roots.push(pkg);
                }
            }
            None => defer_or_root(shared, defer, class_owner, pkg, roots),
        },
    );
}
fn remap_defined_packages(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::lang_class::gc_update_defined_package_refs(shared.vm_identity, map);
}
// Rows are stamped with the VM that wrote them (gc-common w8-a). A row two VMs
// wrote -- possible only through the process-global MSC shadow container --
// is scanned by both, as every row used to be.
fn scan_msc(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::jboss_msc::gc_scan_msc_service_roots(shared.vm_identity, roots);
}
fn remap_msc(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::jboss_msc::gc_update_msc_service_refs(shared.vm_identity, map);
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
//
// Loader-conditional through `defer_or_root` (gc-common w5-b). The cache's own
// copy of the rule read the process-global `metadata_weak_mode()`, the VM-less
// `loader_pin_addr`, and skipped `metadata_pin_deferrable` altogether -- so on
// Generational a still-YOUNG descriptor of a user-loader class was pinned to
// `metadata_pin`, which the young cycle does not seed from an old loader, and
// freed while the cache still handed it out.
fn scan_osc_cache(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    shared.classes.osc_cache.scan_roots(&mut |class_id, desc| {
        defer_or_root(shared, defer, Some(class_id.as_u32()), desc, roots)
    });
}
fn remap_osc_cache(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    shared.classes.osc_cache.remap_roots(map);
}
// Cached annotation proxies and their child roots are pinned to their HOLDER
// class's loader when the cycle licenses it (gc-common w3-b, handoff part B):
// rooted outright, a holder whose annotation type lives in its own user loader
// could never unload. The last-proxy interfaces array stays a plain root.
fn scan_annotations(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let defer = crate::memory::roots::loader_metadata_licence(shared);
    cratonvm_native_builtins::lang_class::gc_scan_annotation_proxy_entries(
        shared.vm_identity,
        &mut |owner, v| defer_or_root(shared, defer, owner, v, roots),
    );
    cratonvm_native_builtins::lang_class::gc_scan_annotation_proxy_roots(shared.vm_identity, roots);
}
fn remap_annotations(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::lang_class::gc_update_annotation_proxy_refs(shared.vm_identity, map);
}
// The three `net_phase_e` rows record the owning VM in each row (the
// `HttpServer` state, the `DatagramSocket` and `InetAddress` side tables) and
// walk only the collecting VM's rows (gc-common w8-a).
fn scan_http_handlers(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_re10_handler_roots(shared.vm_identity, roots);
}
fn remap_http_handlers(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::net_phase_e::gc_update_re10_handler_refs(shared.vm_identity, map);
}
fn scan_datagram_sockets(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_ds_roots(shared.vm_identity, roots);
}
fn remap_datagram_sockets(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::net_phase_e::gc_update_ds_refs(shared.vm_identity, map);
}
fn scan_inet_addresses(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::net_phase_e::gc_scan_inet_addr_roots(shared.vm_identity, roots);
}
fn remap_inet_addresses(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::net_phase_e::gc_update_inet_addr_refs(shared.vm_identity, map);
}
// Every `nio` row records the VM that created it (the selector registry slot,
// `sel_obj_ids`, `sk_table`, `chan_fields` and the two `ServerSocket` adaptor
// tables), so both halves walk only the collecting VM's rows, and
// `release_vm_native_state` drops a disposed VM's (gc-common w10-b).
fn scan_nio(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let vm = shared.vm_identity;
    cratonvm_native_io::nio_selector::gc_scan_selector_roots(vm, roots);
    cratonvm_native_io::socket_channel::gc_scan_channel_roots(vm, roots);
    cratonvm_native_io::socket_channel::gc_scan_ss_back_ref_roots(vm, roots);
    // `ServerSocketChannel.socket()`'s adaptor cache. It used to live in slot 5
    // of the channel object — really `AbstractSelectableChannel.keys` — so it
    // needed no roots and corrupted a JDK field instead; moving it to a side
    // table is what makes these two lines necessary
    // (W7-72-ssc-socket-and-filechannel.md).
    cratonvm_native_io::socket_channel::gc_scan_ssc_socket_cache_roots(vm, roots);
}
fn remap_nio(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    let vm = shared.vm_identity;
    cratonvm_native_io::nio_selector::sk_table_update_after_gc(vm, map);
    cratonvm_native_io::socket_channel::channel_fields_update_after_gc(vm, map);
    cratonvm_native_io::socket_channel::ss_back_ref_update_after_gc(vm, map);
    cratonvm_native_io::socket_channel::ssc_socket_cache_update_after_gc(vm, map);
}
// Each bound row records its VM (gc-common w9-b).
fn scan_server_ports(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_api::server_socket_ports::gc_scan_roots(shared.vm_identity, roots);
}
fn remap_server_ports(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_api::server_socket_ports::gc_update_after_gc(shared.vm_identity, map);
}
// Each task records its VM; a task registered through the VM-less
// `ScheduledRegistry::register` is still scanned by every VM until its callers
// pass one (`handoff-w8a-scheduled-pump-per-vm-callers.md`, gc-common w8-a).
fn scan_scheduled(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::scheduled_pump::gc_scan_scheduled_roots(shared.vm_identity, roots);
}
fn remap_scheduled(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::scheduled_pump::gc_update_scheduled_refs(shared.vm_identity, map);
}
// Futures, OptionMaps and Builders record the VM that created them (w8-a).
fn scan_xnio(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::xnio_async::gc_scan_xnio_future_roots(shared.vm_identity, roots);
}
fn remap_xnio(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::xnio_async::gc_update_xnio_future_refs(shared.vm_identity, map);
}
// Each upcall trampoline records the VM whose target it holds (w8-a).
fn scan_panama(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::panama::gc_scan_upcall_target_roots(shared.vm_identity, roots);
}
fn remap_panama(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::panama::gc_update_upcall_target_refs(shared.vm_identity, map);
}
// The `t27_tls` halves walk only the collecting VM's rows (gc-common w9-b):
// `SSLContext`-keyed rows name their VM through the lock-key registry, engine
// and session rows carry it in the key, and the default `SSLContext` /
// `SSLSocketFactory` slots are per VM, and each `x509_manager`
// `KeyManagerState` records the VM that published it.
fn scan_tls(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    let vm = shared.vm_identity;
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_trust_manager_roots(vm, roots);
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_key_manager_roots(vm, roots);
    cratonvm_native_builtins::t27_tls::gc_scan_default_ssl_context_root(vm, roots);
    // The live `PrivateKey` objects a `KeyManagerState` holds for keys with no
    // PKCS#8 encoding of their own (netty's `OpenSslPrivateKey`, PKCS#11 keys).
    // Nothing else references them once `KeyManagerFactory.init` returns.
    cratonvm_native_builtins::x509_manager::gc_scan_key_manager_roots(vm, roots);
}
fn remap_tls(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    let vm = shared.vm_identity;
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_trust_manager_refs(vm, map);
    cratonvm_native_builtins::t27_tls::gc_update_tls_ctx_key_manager_refs(vm, map);
    cratonvm_native_builtins::t27_tls::gc_update_default_ssl_context_ref(vm, map);
    cratonvm_native_builtins::x509_manager::gc_update_key_manager_refs(vm, map);
}
// Each `ForkJoinTask` entry and each common pool records its VM (gc-common
// w9-b). Every production writer passes its VM; an unscoped (VM 0) entry is
// made only by mock contexts.
fn scan_forkjoin(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::phases_early::gc_scan_forkjoin_roots(shared.vm_identity, roots);
}
fn remap_forkjoin(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::phases_early::gc_update_forkjoin_refs(shared.vm_identity, map);
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
// The WildFly / JBoss native side tables that hold heap objects with no other
// root (gc-common w10-d), both per VM:
// * the JNDI binding store: every value `InitialContext.bind` / `rebind` or
//   `ServiceBasedNamingStore.bind` put in the flat store. It had no row, so a
//   young collection freed a value only the store held and a moving one
//   stranded it (`common-w9b-jndi-bindings-store-holds-unrooted-object-refs`);
// * the `EnhancedQueueExecutor.execute` pending-Runnable queue, drained by
//   `AsyncFutureTask.await()`. It used to root each Runnable through the
//   var-handle-root registry, which never forgets one.
fn scan_wildfly_side_tables(shared: &crate::vm::SharedVm, roots: &mut Vec<ObjectRef>) {
    cratonvm_native_builtins::wildfly_naming::gc_scan_jndi_binding_roots(shared.vm_identity, roots);
    cratonvm_native_builtins::wildfly_core::gc_scan_eqe_pending_roots(shared.vm_identity, roots);
}
fn remap_wildfly_side_tables(shared: &crate::vm::SharedVm, map: &cratonvm_types::PointerMap) {
    cratonvm_native_builtins::wildfly_naming::gc_update_jndi_binding_refs(shared.vm_identity, map);
    cratonvm_native_builtins::wildfly_core::gc_update_eqe_pending_refs(shared.vm_identity, map);
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
    root_source!("class-atomic-slots", scan_class_atomic, remap_class_atomic),
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
        "indy-call-sites",
        scan_indy_call_sites,
        remap_indy_call_sites
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
    root_source!(
        "classloader-values",
        scan_classloader_values,
        remap_classloader_values
    ),
    root_source!("type-variables", scan_type_variables, remap_type_variables),
    root_source!(
        "defined-packages",
        scan_defined_packages,
        remap_defined_packages
    ),
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
    root_source!(
        "wildfly-side-tables",
        scan_wildfly_side_tables,
        remap_wildfly_side_tables
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

    /// gc-common w3-b: the `Class$Atomic` reflection slots have their own row
    /// (so they are loader-conditional and remapped exactly once), and the
    /// unconditional unsafe row no longer carries them.
    #[test]
    fn class_atomic_slots_are_their_own_root_source() {
        assert!(
            VM_ROOT_SOURCES.iter().any(|s| s.name == "class-atomic-slots"),
            "the Class$Atomic reflection slots must be a VM root source"
        );
    }

    /// `defer_or_root` records the pin BEFORE it skips the root, and only for
    /// a user-loader owner on a licensed cycle whose marker follows the pin;
    /// every other combination roots the value.
    #[test]
    fn defer_or_root_pins_only_a_user_loader_owner_on_a_licensed_cycle() {
        use crate::config::VmConfig;
        let shared = std::sync::Arc::new(crate::vm::SharedVm::new(VmConfig::default()));
        let loader = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let value = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let value_addr = value.as_ptr() as usize;
        // gen r5w4/conc8: the deferral owner also requires a class the unload
        // removes with its loader; these ids are in no class store, so the
        // scan's screen (empty: nothing refused) stands in for it.
        let _screen = crate::memory::roots::DeferralScreen::enter(
            shared.vm_identity,
            rustc_hash::FxHashSet::default(),
        );
        // A class id no other test registers a loader pin for.
        let user_cid: u32 = 0x7ff0_3b01;
        let builtin_cid: u32 = 0x7ff0_3b02;
        // Through the authoritative side table (which also writes the
        // `loader_pin` row): `defer_or_root` uses a pin only when THIS VM's own
        // defining-loader row agrees (`roots::vm_loader_pin_addr`, w4-b).
        cratonvm_native_builtins::classloader::register_defining_loader(
            shared.vm_identity,
            user_cid,
            loader,
        );
        let pinned = || {
            cratonvm_types::metadata_pin::roots_for_loader(loader.as_ptr() as usize)
                .is_some_and(|p| p.contains(&value_addr))
        };

        let mut roots = Vec::new();
        defer_or_root(&shared, false, Some(user_cid), value, &mut roots);
        assert_eq!(roots, vec![value], "no licence: rooted");
        assert!(!pinned());

        let mut roots = Vec::new();
        defer_or_root(&shared, true, None, value, &mut roots);
        assert_eq!(roots, vec![value], "no owner: rooted");

        let mut roots = Vec::new();
        defer_or_root(&shared, true, Some(builtin_cid), value, &mut roots);
        assert_eq!(roots, vec![value], "a built-in-loader owner has no loader_pin row: rooted");
        assert!(!pinned());

        let mut roots = Vec::new();
        defer_or_root(&shared, true, Some(user_cid), value, &mut roots);
        if shared.mem.heap.metadata_pin_deferrable(value_addr) {
            assert!(roots.is_empty(), "a user-loader owner's value must not be a root");
            assert!(pinned(), "...it must be pinned to the owner's loader instead");
        } else {
            assert_eq!(roots, vec![value], "the marker would not follow the pin: rooted");
            assert!(!pinned());
        }
        cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);

        // gcd d1/c (`gengc-r5w4-conc8-native-root-deferrals-ignore-the-unload-classification`):
        // a class the unload never removes (the scan's screen names it: a
        // built-in-namespace class a user loader defined) is ROOTED although
        // its loader has a `loader_pin` row, so its value cannot be left
        // unmarked when that loader dies.
        {
            let _refused = crate::memory::roots::DeferralScreen::enter(
                shared.vm_identity,
                [user_cid].into_iter().collect(),
            );
            let mut roots = Vec::new();
            defer_or_root(&shared, true, Some(user_cid), value, &mut roots);
            assert_eq!(roots, vec![value], "a never-unloaded class's value: rooted");
            assert!(!pinned());
        }
        cratonvm_types::loader_pin::forget_vm_loader_pins(shared.vm_identity);
        cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
    }

    /// **AN `ObjectStreamClass` DESCRIPTOR DEFERS ONLY TO ITS OWN VM'S LOADER,
    /// UNDER ITS OWN VM'S LICENCE** (gc-common w5-b).
    ///
    /// The `osc-cache` row used to decide in `OscCache::scan_roots` from the
    /// process-global `metadata_weak_mode()` and the VM-less `loader_pin_addr`
    /// with no `metadata_pin_deferrable` check. Now it goes through
    /// `defer_or_root` under `roots::loader_metadata_licence`. Four arms: a VM
    /// with no user-loader class at the id roots even on a licensed cycle
    /// (another VM's loader at the same id must not capture it); the owning VM
    /// pins iff the heap says the marker follows the pin; an unlicensed cycle
    /// roots; the scoped licence is restored afterwards.
    #[test]
    fn osc_cache_descriptors_defer_only_to_their_own_vms_loader() {
        use crate::config::VmConfig;
        use crate::memory::roots::{loader_metadata_licence, NativeScanLicence};
        let vm_a = std::sync::Arc::new(crate::vm::SharedVm::new(VmConfig::default()));
        let vm_b = std::sync::Arc::new(crate::vm::SharedVm::new(VmConfig::default()));
        // gen r5w4/conc8: see `defer_or_root_pins_only_a_user_loader_owner_on_a_licensed_cycle`.
        let _screen = crate::memory::roots::DeferralScreen::enter(
            vm_a.vm_identity,
            rustc_hash::FxHashSet::default(),
        );
        // A class id no other test registers a loader pin for.
        let cid: u32 = 0x7ff0_5b01;
        let alloc = |vm: &crate::vm::SharedVm| {
            vm.mem
                .heap
                .alloc_object(crate::classloading::ClassId::new(0), 0)
        };
        let loader_a = alloc(&*vm_a);
        let desc_a = alloc(&*vm_a);
        let desc_b = alloc(&*vm_b);
        cratonvm_native_builtins::classloader::register_defining_loader(
            vm_a.vm_identity,
            cid,
            loader_a,
        );
        vm_a.classes
            .osc_cache
            .insert_if_absent(cratonvm_types::ClassId::new(cid), desc_a);
        vm_b.classes
            .osc_cache
            .insert_if_absent(cratonvm_types::ClassId::new(cid), desc_b);
        let pinned = |desc: ObjectRef| {
            cratonvm_types::metadata_pin::roots_for_loader(loader_a.as_ptr() as usize)
                .is_some_and(|p| p.contains(&(desc.as_ptr() as usize)))
        };

        {
            let _licence = NativeScanLicence::enter(vm_b.vm_identity, true);
            assert!(loader_metadata_licence(&vm_b), "the scoped licence is read");
            let mut roots = Vec::new();
            scan_osc_cache(&vm_b, &mut roots);
            assert_eq!(
                roots,
                vec![desc_b],
                "B has no user-loader class at this id: rooted, never pinned to A's loader"
            );
            assert!(!pinned(desc_b));
        }

        {
            let _licence = NativeScanLicence::enter(vm_a.vm_identity, false);
            let mut roots = Vec::new();
            scan_osc_cache(&vm_a, &mut roots);
            assert_eq!(roots, vec![desc_a], "no licence: rooted");
            assert!(!pinned(desc_a));
        }

        {
            let _licence = NativeScanLicence::enter(vm_a.vm_identity, true);
            let mut roots = Vec::new();
            scan_osc_cache(&vm_a, &mut roots);
            if vm_a.mem.heap.metadata_pin_deferrable(desc_a.as_ptr() as usize) {
                assert!(roots.is_empty(), "A's own user-loader descriptor is not a root");
                assert!(pinned(desc_a), "...it is pinned to A's loader instead");
            } else {
                assert_eq!(
                    roots,
                    vec![desc_a],
                    "the marker would not follow the pin (a young Generational \
                     descriptor): rooted"
                );
            }
            {
                let _inner = NativeScanLicence::enter(vm_a.vm_identity, false);
                assert!(!loader_metadata_licence(&vm_a));
            }
            assert!(
                loader_metadata_licence(&vm_a),
                "a nested scope restores the outer licence"
            );
        }

        cratonvm_types::loader_pin::forget_vm_loader_pins(vm_a.vm_identity);
        cratonvm_types::metadata_pin::replace_metadata_pins(vm_a.vm_identity, &[]);
    }

    /// **THE PROCESS-GLOBAL ROWS ARE A RATCHET, NOT A HABIT**
    /// (`common-b-process-global-root-sources-FIXED-20260923.md`, gc-common w4-b).
    ///
    /// The module doc says every source is VM-scoped or a documented exception
    /// whose scan half ignores the `SharedVm` (`_:`). Those exceptions hand VM
    /// B's collector VM A's addresses in a multi-VM process (`libcratonvm`
    /// embedders, this crate's parallel unit tests). They cannot all be fixed
    /// from this file -- each backing store lives in `native-builtins` /
    /// `native-io` / `native-api` -- but the set must only SHRINK. This freezes
    /// it exactly: a new `_:` scan fails here (key its store on `vm_identity`
    /// instead, the `logmanager` / `security-manager` pattern), and a fixed
    /// row must be removed from `FROZEN` so the ratchet tightens.
    #[test]
    fn process_global_root_sources_only_shrink() {
        // gc-common w8-a: 13 -> 5. `lambda-callsites`, `jboss-msc`,
        // `http-handlers`, `datagram-sockets`, `inet-addresses`,
        // `scheduled-pump`, `xnio-futures` and `panama-upcalls` are VM-scoped.
        // gc-common w9-b: 5 -> 3. `forkjoin` and `tls` are VM-scoped (the
        // `tls` row's `x509_manager` half since the w9 handoffs).
        // gc-common w9 handoffs: 3 -> 2. `server-ports` is VM-scoped.
        // gc-common w10-a / w10-b: 2 -> 0. `locale` and `nio` are VM-scoped;
        // every scan half now takes the collecting VM.
        const FROZEN: &[&str] = &[];
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/memory/native_roots.rs");
        let src = std::fs::read_to_string(path)
            .expect("native_roots.rs is readable")
            .replace("\r\n", "\n");
        // Every row's scan half, `_:` or not: the parser's own sanity check.
        let rows = src
            .lines()
            .filter(|l| {
                l.strip_prefix("fn scan_")
                    .and_then(|rest| rest.split_once('('))
                    .is_some_and(|(_, tail)| tail.contains("&crate::vm::SharedVm"))
            })
            .count();
        let mut found: Vec<String> = src
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("fn scan_")?;
                let (name, tail) = rest.split_once('(')?;
                tail.starts_with("_: &crate::vm::SharedVm")
                    .then(|| format!("scan_{name}"))
            })
            .collect();
        found.sort_unstable();
        assert!(
            rows >= 20,
            "the row parse found only {rows} scan halves -- the parser is broken, \
             failing rather than passing vacuously"
        );
        assert_eq!(
            found, FROZEN,
            "the set of root sources whose scan ignores the collecting VM changed. \
             A new one must key its store on `vm_identity`; a fixed one must be \
             removed from FROZEN"
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

    #[test]
    fn classloader_value_rows_are_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES
                .iter()
                .any(|s| s.name == "classloader-values"),
            "AbstractClassLoaderValue values are rooted only by this row \
             (gc-common w17-d)"
        );
    }

    #[test]
    fn type_variable_caches_are_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES.iter().any(|s| s.name == "type-variables"),
            "the reflection TypeVariable caches are rooted only by this row \
             (gc-common w30-b)"
        );
    }

    #[test]
    fn defined_package_memo_is_a_registered_root_source() {
        assert!(
            VM_ROOT_SOURCES.iter().any(|s| s.name == "defined-packages"),
            "the defined-`Package` memo is rooted only by this row (gc-common w31-a)"
        );
    }
}
