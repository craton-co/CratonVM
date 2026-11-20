#[allow(unused_imports)]
use super::*;

/// [`class_store_cut`] with the zs s9/ob6 switch given. `lock_free`: read the
/// published next `ClassId` (`ClassRealm::class_store_next_id`), which needs
/// no lock, so a collection whose edge finds the class lock held (a stopped
/// thread defining a class) still gets a census and later its `Metaspace`
/// key -- before, it got none and its `GcInfo` read the JMM's undefined
/// usage (rv5 residual 4 / rv6 X2). `false`: the s6/ob5 `try_read` (`None`
/// on a busy lock).
pub(crate) fn class_store_cut_with(shared: &SharedVm, lock_free: bool) -> Option<u32> {
    if lock_free {
        return Some(
            shared
                .classes
                .class_store_next_id
                .load(std::sync::atomic::Ordering::Acquire),
        );
    }
    let cm = shared.classes.class_manager.try_read_untracked()?;
    Some(cm.class_store().next_id().as_u32())
}


/// `CRATONVM_ZGC_METASPACE_BUSY_LOCK_BACKFILL` (zs s9/ob6; default ON, `=0`
/// kill: the s6/ob5 behaviour, where a busy class lock left a cycle `GcInfo`
/// with an undefined `Metaspace` usage). Two halves: the census at the
/// collection edge needs no lock ([`class_store_cut_with`]), and a
/// notification drain whose back-fill finds the lock busy holds the
/// still-keyless cycle records back instead of delivering them
/// (`gc_events.rs::run_gc_notifications`).
pub(crate) fn zgc_metaspace_busy_lock_backfill_enabled() -> bool {
    true
}

/// zs s6/ob5: give the ZGC cycle records that ran before the `Metaspace`
/// pool was handed out their `Metaspace` key
/// (`BackendGcBeans::backfill_metaspace`): one walk of the class store's
/// prefix up to the largest census waiting, only when one waits (otherwise
/// one load). `block`: take the class lock blocking (a Java thread in a JMX
/// native); otherwise only if it is free (a notification drain point),
/// leaving the entries for the next call when it is not.
///
/// zs s9/ob6: returns `true` when census entries still wait afterwards (the
/// non-blocking call found the class lock busy), so the drain can hold the
/// keyless cycle records back.
pub(crate) fn zgc_metaspace_backfill(shared: &SharedVm, block: bool) -> bool {
    let Some(b) = shared.mem.backend_gc_beans() else {
        return false;
    };
    let cuts = b.metaspace_deferred_cuts();
    if cuts.is_empty() {
        return false;
    }
    // zt t3/ob: each prefix's HotSpot-shaped figures (committed, class
    // space), the same walk.
    let figures = if block {
        let cm = shared.classes.class_manager.read();
        b.set_metaspace_max(cm.max_metaspace_bytes());
        class_metadata_prefix_figures(&cm, &cuts)
    } else {
        let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
            return true;
        };
        b.set_metaspace_max(cm.max_metaspace_bytes());
        class_metadata_prefix_figures(&cm, &cuts)
    };
    b.backfill_metaspace_figures(|cut| {
        figures
            .iter()
            .find(|&&(c, _)| c == cut)
            .map(|&(_, f)| f)
    });
    false
}

/// zs s6/ob5: the ZGC `Metaspace` pool's usage now: the memoised class-store
/// walk (one walk per loaded-class census, shared with the collection edges,
/// so a pool query and the next `GcInfo` agree), under a blocking read of
/// the class lock (a Java thread in a JMX native). `None` when the pool is
/// not modelled.
pub(crate) fn zgc_metaspace_pool_now(
    shared: &SharedVm,
    b: &cratonvm_gc::gc_metrics::BackendGcBeans,
) -> Option<cratonvm_gc::gc_metrics::JmxPoolUsage> {
    if !b.metaspace_pool_enabled() {
        return None;
    }
    let unloaded = shared
        .debug
        .diagnostic_counters
        .classes_unloaded
        .load(std::sync::atomic::Ordering::Relaxed);
    let cm = shared.classes.class_manager.read();
    b.set_metaspace_max(cm.max_metaspace_bytes());
    let key = (cm.loaded_count() as u64, unloaded);
    let used = b.metaspace_sample(key, || class_metadata_walk_bytes(&cm) as u64);
    Some(b.metaspace_pool_usage(used))
}

/// The carrier a JDK virtual thread `vthread` is mounted on, for a thread
/// dump's `Carrying virtual thread` line (gci i12/td): its
/// `VirtualThread.carrierThread`, which the JDK's `mount` sets and `unmount`
/// clears. `None` when unmounted, or for anything that is not a live heap
/// object with that field. Read as the dump's other field reads are
/// (`vm_exec::thread_dump_priority_and_daemon`): checked addresses, no pause.
pub(in crate::vm::vm_init) fn dump_carrier_thread_of(shared: &SharedVm, vthread: ObjectRef) -> Option<ObjectRef> {
    let heap = &shared.mem.heap;
    heap.is_heap_addr(vthread.as_ptr() as usize)?;
    let class_id = heap.class_id_of(vthread);
    let slot = {
        let cm = shared.classes.class_manager.read();
        super::vm_exec::resolve_field_index_in_hierarchy(class_id, "carrierThread", &cm.class_store)?
    };
    match heap.get_field(vthread, slot) {
        Value::Object(Some(carrier)) if heap.is_heap_addr(carrier.as_ptr() as usize).is_some() => {
            Some(carrier)
        }
        _ => None,
    }
}



/// The full collection a live-only attach heap walk runs first (gci i4/hd),
/// `kind` naming HotSpot's operation (`HeapInspection` for the histogram,
/// `HeapDump` for the dump). Nothing unless the switch is on.
///
/// gci i9/dc (`gci-i8rv-dcmd-live-walk-on-an-idle-vm-FIXED-20261017`): the
/// collection runs now, on a thread attached for it
/// (`run_dcmd_full_gc_on_attach_thread`), with HotSpot's cause (`Heap
/// Inspection Initiated GC` / `Heap Dump Initiated GC`), so an idle VM walks
/// at once, counts only live objects and is left with no request pending.
/// Only where that path does not apply (`CRATONVM_DCMD_GC_ON_ATTACH_THREAD=0`,
/// an unregistered unit VM, a caller that is itself a VM thread) or completed
/// no full collection (every attempt lost to allocation pauses) the old one:
/// `jcmd GC.run`'s latch (`request_diagnostic_command_gc`, cause `Diagnostic
/// Command`), waited for up to `DCMD_GC_RUN_WAIT`; a wait that runs out (no
/// allocating mutator took the request) leaves the walk counting what the
/// heap holds, and stderr says so.
pub(in crate::vm::vm_init) fn dcmd_live_heap_walk_collect(
    vm: &SharedVm,
    kind: crate::runtime::interpreter::gc_and_alloc::DcmdFullGc,
) {
    if crate::runtime::interpreter::gc_and_alloc::run_dcmd_full_gc_on_attach_thread(vm, kind)
        == Some(true)
    {
        return;
    }
    let collected = crate::runtime::interpreter::gc_and_alloc::request_diagnostic_command_gc(
        vm,
        crate::runtime::interpreter::gc_and_alloc::DCMD_GC_RUN_WAIT,
    );
    if !collected {
        eprintln!(
            "[dcmd] live heap walk: no full collection within {} ms; walking every object",
            crate::runtime::interpreter::gc_and_alloc::DCMD_GC_RUN_WAIT.as_millis()
        );
    }
}

/// gci i47w/dh47 (`gci-i47w-j45-hotspotdiagnostic-dumpheap0-unsatisfied-link-FIXED-20261016`):
/// `HotSpotDiagnosticMXBean.dumpHeap(path, live)`'s native half, HotSpot's
/// `HeapDumper(live).dump(path)`: the file opened exclusively FIRST (an
/// existing one is `File exists`, and nothing is collected), then -- only
/// when `live` -- the same `Heap Dump Initiated GC` full collection
/// `jcmd GC.heap_dump` runs ([`dcmd_live_heap_walk_collect`]), then the
/// HPROF walk at its own pause (`hprof::dump_heap_for_mxbean`). Called off
/// any Java thread (`vm_exec.rs` `dump_heap_for_mxbean`). `Err` is the
/// `IOException` message.
pub(crate) fn heap_dump_for_mxbean(vm: &SharedVm, path: &str, live: bool) -> Result<u64, String> {
    crate::runtime::hprof::dump_heap_for_mxbean(vm, path, live, &mut || {
        dcmd_live_heap_walk_collect(
            vm,
            crate::runtime::interpreter::gc_and_alloc::DcmdFullGc::HeapDump,
        )
    })
}

/// The `Metaspace` figure ([`VmDiagnosticState::heap_summary`]'s
/// `metaspace_used`, see the comment there): the class store's measured
/// metadata, summed per live class. O(classes + methods); the caller holds the
/// class-manager guard (read or write) it passes in.
///
/// gci i18/dk (`gci-i17rv-i16-default-path-review-findings-RETIRED-UNFIXED-20261018` finding
/// 8): split out of `heap_summary` unchanged, so
/// `vm_exec::jmx_check_non_heap_thresholds_armed` samples it under the one
/// `try_read` guard it took, without the rest of the heap summary and without
/// two more blocking acquisitions.
///
/// gci i32/mf (`gci-i31hp-metaspace-figure-is-not-hotspots-metaspace-used-FIXED-20261016`):
/// the figure is HotSpot 25's `MetaspaceUtils::used_bytes()` as HotSpot
/// `-Xshare:off` reports it (this VM has no CDS archive, so every class it
/// holds is a class HotSpot `-Xshare:off` would hold in metaspace), modelled
/// per class from HotSpot's metadata layout ([`hotspot_metaspace_used_bytes`]).
/// It was the size of this VM's reader structures
/// ([`metaspace_reader_struct_bytes`]), which no HotSpot number is. Every
/// consumer reads this one function (directly or through `heap_summary`): the
/// `Metaspace` memory pool and `getNonHeapMemoryUsage`, `-Xlog:gc+metaspace`,
/// the `gc,exit` / `GC.heap_info` / SIGQUIT heap lines, `jstat`'s perf data,
/// JFR `jdk.MetaspaceSummary` and the `gcThreshold` step fed by it, and the
/// non-heap usage-threshold check. `CRATONVM_METASPACE_HOTSPOT_FIGURE`
/// (default ON, `runtime_flag_default_on`; `=0` restores the reader-structure
/// figure).
///
/// [`VmDiagnosticState::heap_summary`]: crate::runtime::serviceability::VmDiagnosticState::heap_summary
pub(crate) fn metaspace_used_bytes(cm: &ClassManager) -> usize {
    if metaspace_hotspot_figure_enabled() {
        hotspot_metaspace_used_bytes(cm)
    } else {
        metaspace_reader_struct_bytes(cm)
    }
}

/// `CRATONVM_METASPACE_HOTSPOT_FIGURE` (gci i32/mf; default ON,
/// `runtime_flag_default_on`, `=0` kills): [`metaspace_used_bytes`] reports
/// the HotSpot-shaped figure. Read per walk (a serviceability call).
pub(in crate::vm::vm_init) fn metaspace_hotspot_figure_enabled() -> bool {
    true
}

/// `CRATONVM_METASPACE_HOTSPOT_SPACES` (gci i33/ms; default ON,
/// `runtime_flag_default_on`, `=0` kills): [`metaspace_stats`] states
/// HotSpot 25's two spaces -- the compressed class space split out of the
/// total, committed by 64 KiB granule per arena, HotSpot's reservations --
/// instead of one figure for used, committed and reserved with no class
/// space. Needs `CRATONVM_METASPACE_HOTSPOT_FIGURE` (the model's per-class
/// split); read per walk.
pub(in crate::vm::vm_init) fn metaspace_hotspot_spaces_enabled() -> bool {
    true
}

/// `CRATONVM_METASPACE_LIMIT_HOTSPOT_CHECK` (gci i33/ms; default ON,
/// `runtime_flag_default_on`, `=0` kills): `-XX:MaxMetaspaceSize` is checked
/// against the model's committed metaspace of every live class, boot classes
/// included, as HotSpot's `MetaspaceGC::allowed_expansion` checks
/// `MetaspaceUtils::committed_bytes()` ([`metaspace_limit_would_exceed`]);
/// `=0` checks the non-boot class-file bytes
/// (`ClassManager::metaspace_charged_bytes`), which refuses later than
/// HotSpot. Needs `CRATONVM_METASPACE_HOTSPOT_FIGURE`; read per check (only
/// with a limit set).
pub(in crate::vm::vm_init) fn metaspace_limit_hotspot_check_enabled() -> bool {
    true
}

/// gci i33/ms (`gci-i33ms-metaspace-spaces-and-limit-FIXED-20261016`): HotSpot 25's
/// metaspace statistics (`MetaspaceUtils::get_combined_statistics` /
/// `print_on`), in bytes. `used`, `committed` and `reserved` cover both
/// spaces; `class_space` is the compressed class space's part of each, as
/// HotSpot prints it on its own (`NonClass:` / `Class:` of `gc,metaspace`,
/// the `class space` line, the `Compressed Class Space` pool).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MetaspaceStats {
    pub(crate) used: u64,
    pub(crate) committed: u64,
    pub(crate) reserved: u64,
    /// The class space's `(used, committed, reserved)`; `None` is the
    /// no-class-space shape (`-XX:-UseCompressedClassPointers` on HotSpot;
    /// here `CRATONVM_METASPACE_HOTSPOT_SPACES=0` or
    /// `CRATONVM_METASPACE_HOTSPOT_FIGURE=0`).
    pub(crate) class_space: Option<(u64, u64, u64)>,
}

impl MetaspaceStats {
    /// The pre-i33 shape: one figure for used, committed and reserved, no
    /// class space.
    pub(crate) const fn flat(used: u64) -> Self {
        Self {
            used,
            committed: used,
            reserved: used,
            class_space: None,
        }
    }

    /// From an exit summary's `Metaspace` and `class space` triples.
    pub(crate) const fn from_triples(
        metaspace: (u64, u64, u64),
        class_space: Option<(u64, u64, u64)>,
    ) -> Self {
        Self {
            used: metaspace.0,
            committed: metaspace.1,
            reserved: metaspace.2,
            class_space,
        }
    }

    /// `(used, committed, reserved)` of both spaces.
    pub(crate) const fn triple(&self) -> (u64, u64, u64) {
        (self.used, self.committed, self.reserved)
    }

    /// `(used, committed, reserved)` of the non-class space (everything
    /// without a class space).
    pub(crate) fn non_class(&self) -> (u64, u64, u64) {
        match self.class_space {
            Some((u, c, r)) => (
                self.used.saturating_sub(u),
                self.committed.saturating_sub(c),
                self.reserved.saturating_sub(r),
            ),
            None => self.triple(),
        }
    }
}

/// gci i33/ms: [`MetaspaceStats`] now -- one class-store walk under the
/// guard the caller holds. With both switches on, HotSpot 25 `-Xshare:off`'s
/// figures from the per-class model ([`hs_metaspace_stats`]); otherwise
/// [`MetaspaceStats::flat`] over [`metaspace_used_bytes`] (the pre-i33
/// shape). `used` is [`metaspace_used_bytes`]'s figure either way.
pub(crate) fn metaspace_stats(cm: &ClassManager) -> MetaspaceStats {
    if metaspace_hotspot_figure_enabled() && metaspace_hotspot_spaces_enabled() {
        hs_metaspace_stats(&hs_metaspace_arenas(cm), cm.max_metaspace_bytes())
    } else {
        MetaspaceStats::flat(metaspace_used_bytes(cm) as u64)
    }
}

/// gci i33/ms: would defining a class of `additional` class-file bytes by
/// `loader` exceed `-XX:MaxMetaspaceSize`? Never without a limit (one
/// `Option` check). By default the model's COMMITTED metaspace of every live
/// class, boot classes included, plus the new class (its class-file bytes,
/// the nearest figure before it is parsed, in its loader's non-class arena),
/// compared with the limit as HotSpot's `MetaspaceGC::allowed_expansion`
/// compares `MetaspaceUtils::committed_bytes()`. The classes are summed
/// incrementally ([`hs_tally_catch_up`]): a define sums the classes defined
/// since the last check, not the store. `CRATONVM_METASPACE_LIMIT_HOTSPOT_CHECK=0`
/// (or `CRATONVM_METASPACE_HOTSPOT_FIGURE=0`): the non-boot class-file bytes
/// (`ClassManager::metaspace_would_exceed`), as before.
pub(crate) fn metaspace_limit_would_exceed(
    cm: &ClassManager,
    loader: cratonvm_types::ClassLoaderId,
    additional: u64,
) -> bool {
    let Some(max) = cm.max_metaspace_bytes() else {
        return false;
    };
    if !(metaspace_limit_hotspot_check_enabled() && metaspace_hotspot_figure_enabled()) {
        return cm.metaspace_would_exceed(additional);
    }
    let mut tally = cm.metaspace_arena_tally().lock();
    hs_tally_catch_up(&mut tally, cm);
    let mut boot = tally.boot;
    let mut used = tally.others_used;
    let mut footprint = tally.others_footprint;
    if loader == cratonvm_types::ClassLoaderId::Bootstrap {
        boot[0] = boot[0].saturating_add(additional);
    } else {
        let before = tally
            .loaders
            .get(&loader.to_native_id())
            .map_or(0, |a| a[0]);
        let after = before.saturating_add(additional);
        used[0] = used[0].saturating_add(additional);
        footprint[0] = footprint[0].saturating_add(
            hs_arena_footprint(after, &HS_STANDARD_NON_CLASS_CHUNKS)
                .saturating_sub(hs_arena_footprint(before, &HS_STANDARD_NON_CLASS_CHUNKS)),
        );
    }
    hs_stats_from_sums(boot, used, footprint, Some(max)).committed > max
}

// gci i32/mf: HotSpot 25 metadata sizes (x86_64, LP64, compressed class
// pointers), in bytes, from the HotSpot 25 sources' class layouts
// (`instanceKlass.hpp`, `klass.hpp`, `arrayKlass.hpp` / `objArrayKlass.hpp`,
// `constantPool.hpp`, `cpCache.hpp` + `resolvedFieldEntry.hpp` /
// `resolvedMethodEntry.hpp` / `resolvedIndyEntry.hpp`, `method.hpp`,
// `constMethod.hpp`, `annotations.hpp`, `recordComponent.hpp`, `array.hpp`).
// Every metaspace allocation is word aligned.
/// `sizeof(InstanceKlass)` (the `Klass` part included, before the embedded
/// vtable / itable / oop maps).
pub(in crate::vm::vm_init) const HS_INSTANCE_KLASS_BYTES: usize = 472;
/// `sizeof(ObjArrayKlass)` (a `TypeArrayKlass` is 8 bytes smaller; one figure).
pub(in crate::vm::vm_init) const HS_ARRAY_KLASS_BYTES: usize = 248;
/// `Universe::base_vtable_size()`: `java.lang.Object`'s five virtual methods,
/// the vtable of every interface and array class.
pub(in crate::vm::vm_init) const HS_BASE_VTABLE_WORDS: usize = 5;
/// `sizeof(ConstantPool)`, before its one word per slot.
pub(in crate::vm::vm_init) const HS_CONSTANT_POOL_BYTES: usize = 72;
/// `sizeof(ConstantPoolCache)` (JDK 25: no entries of its own, three arrays).
pub(in crate::vm::vm_init) const HS_CP_CACHE_BYTES: usize = 56;
/// `sizeof(ResolvedFieldEntry)` / `ResolvedMethodEntry` / `ResolvedIndyEntry`.
pub(in crate::vm::vm_init) const HS_RESOLVED_FIELD_ENTRY_BYTES: usize = 24;
pub(in crate::vm::vm_init) const HS_RESOLVED_METHOD_ENTRY_BYTES: usize = 24;
pub(in crate::vm::vm_init) const HS_RESOLVED_INDY_ENTRY_BYTES: usize = 16;
/// `sizeof(Method)`; a native method carries two more words (the native
/// function and the signature handler).
pub(in crate::vm::vm_init) const HS_METHOD_BYTES: usize = 88;
pub(in crate::vm::vm_init) const HS_NATIVE_METHOD_EXTRA_BYTES: usize = 16;
/// `sizeof(ConstMethod)`, before the bytecode and its inlined tables.
pub(in crate::vm::vm_init) const HS_CONST_METHOD_BYTES: usize = 56;
/// `sizeof(Annotations)` (four array pointers; a `MetaspaceObj`, no vptr).
pub(in crate::vm::vm_init) const HS_ANNOTATIONS_BYTES: usize = 32;
/// `sizeof(RecordComponent)`.
pub(in crate::vm::vm_init) const HS_RECORD_COMPONENT_BYTES: usize = 24;
/// Bytes per field in the compressed `FieldInfo` stream (name, signature,
/// offset, access and field flags as UNSIGNED5; one figure for the common
/// shapes), plus the stream's header and terminator.
pub(in crate::vm::vm_init) const HS_FIELD_INFO_BYTES: usize = 6;
pub(in crate::vm::vm_init) const HS_FIELD_INFO_STREAM_HEADER_BYTES: usize = 3;

// gci i33/ms: HotSpot 25's metaspace commit and reservation units
// (`metaspaceSettings.hpp`, `metaspace.cpp` `ergo_initialize`,
// `metaspaceArenaGrowthPolicy.cpp`), x86_64 defaults.
/// `Settings::commit_granule_bytes()`: memory is committed by 64 KiB granule.
pub(in crate::vm::vm_init) const HS_COMMIT_GRANULE_BYTES: u64 = 64 * 1024;
/// `Settings::virtual_space_node_default_word_size()` in bytes: the non-class
/// space reserves 64 MiB nodes.
pub(in crate::vm::vm_init) const HS_NON_CLASS_NODE_BYTES: u64 = 64 * 1024 * 1024;
/// `CompressedClassSpaceSize`'s default (the class space's reservation).
pub(in crate::vm::vm_init) const HS_COMPRESSED_CLASS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;
/// `Metaspace::reserve_alignment()`: a root chunk.
pub(in crate::vm::vm_init) const HS_METASPACE_RESERVE_ALIGNMENT: u64 = 4 * 1024 * 1024;
/// `ArenaGrowthPolicy` of a standard (non-boot) loader's arenas: the chunk
/// sizes it is handed, in order, the last one repeating. Chunks this small
/// sit inside granules other arenas' chunks share, so the space commits their
/// sum, by granule. (The boot loader's arenas take 4 MiB / 1 MiB chunks and
/// commit them lazily, granule by granule.)
pub(in crate::vm::vm_init) const HS_STANDARD_NON_CLASS_CHUNKS: [u64; 5] = [4096, 4096, 4096, 8192, 16384];
pub(in crate::vm::vm_init) const HS_STANDARD_CLASS_CHUNKS: [u64; 5] = [2048, 2048, 4096, 8192, 16384];

/// Round `n` up to a multiple of `unit` (> 0), saturating.
pub(in crate::vm::vm_init) fn hs_round_up(n: u64, unit: u64) -> u64 {
    n.div_ceil(unit).saturating_mul(unit)
}

/// The chunk bytes a standard arena holding `used` bytes was handed
/// (`chunks`, the last size repeating): the shortest prefix of the sequence
/// that covers `used`. 0 for an empty arena (none is created).
pub(in crate::vm::vm_init) fn hs_arena_footprint(used: u64, chunks: &[u64]) -> u64 {
    if used == 0 {
        return 0;
    }
    let mut total = 0u64;
    for &chunk in chunks {
        total = total.saturating_add(chunk);
        if total >= used {
            return total;
        }
    }
    let last = chunks.last().copied().unwrap_or(1).max(1);
    total.saturating_add(hs_round_up(used - total, last))
}

/// The compressed class space's reservation: `CompressedClassSpaceSize`, at
/// most 80% of `-XX:MaxMetaspaceSize` when one is set, aligned up to (and at
/// least) a root chunk (`Metaspace::ergo_initialize`). Also the
/// `Compressed Class Space` pool's `max`.
pub(in crate::vm::vm_init) fn hs_compressed_class_space_bytes(max_metaspace: Option<u64>) -> u64 {
    let ccs = max_metaspace.map_or(HS_COMPRESSED_CLASS_SPACE_BYTES, |m| {
        HS_COMPRESSED_CLASS_SPACE_BYTES.min(8 * (m / 10))
    });
    hs_round_up(ccs, HS_METASPACE_RESERVE_ALIGNMENT).max(HS_METASPACE_RESERVE_ALIGNMENT)
}

/// [`MetaspaceStats`] from the arena sums: the boot arenas' `[non-class,
/// class]` bytes (committed by granule), the other arenas' used bytes and
/// chunk footprints (their sum committed by granule), and the reservations
/// (the non-class space in 64 MiB nodes, at least one; the class space
/// [`hs_compressed_class_space_bytes`]). Pure.
pub(in crate::vm::vm_init) fn hs_stats_from_sums(
    boot: [u64; 2],
    others_used: [u64; 2],
    others_footprint: [u64; 2],
    max_metaspace: Option<u64>,
) -> MetaspaceStats {
    let granules = |b: u64| hs_round_up(b, HS_COMMIT_GRANULE_BYTES);
    let used_non_class = boot[0].saturating_add(others_used[0]);
    let used_class = boot[1].saturating_add(others_used[1]);
    let committed_non_class = granules(boot[0]).saturating_add(granules(others_footprint[0]));
    let committed_class = granules(boot[1]).saturating_add(granules(others_footprint[1]));
    let reserved_class = hs_compressed_class_space_bytes(max_metaspace);
    let reserved_non_class =
        hs_round_up(committed_non_class, HS_NON_CLASS_NODE_BYTES).max(HS_NON_CLASS_NODE_BYTES);
    MetaspaceStats {
        used: used_non_class.saturating_add(used_class),
        committed: committed_non_class.saturating_add(committed_class),
        reserved: reserved_non_class.saturating_add(reserved_class),
        class_space: Some((used_class, committed_class, reserved_class)),
    }
}

/// [`hs_stats_from_sums`] over a whole tally.
pub(in crate::vm::vm_init) fn hs_metaspace_stats(
    tally: &cratonvm_classloading::MetaspaceArenaTally,
    max_metaspace: Option<u64>,
) -> MetaspaceStats {
    hs_stats_from_sums(
        tally.boot,
        tally.others_used,
        tally.others_footprint,
        max_metaspace,
    )
}

/// Round `n` up to HotSpot's metaspace allocation unit (a word).
pub(in crate::vm::vm_init) const fn hs_align_word(n: usize) -> usize {
    (n + 7) & !7
}

/// The metaspace bytes of a HotSpot `Array<T>` of `n` elements of `elem`
/// bytes: an `int` length, the elements at their alignment, rounded to a
/// word. Empty arrays are HotSpot's shared empty arrays (no allocation).
pub(in crate::vm::vm_init) fn hs_array_bytes(elem: usize, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let header = if elem >= 8 { 8 } else { 4 };
    hs_align_word(header + elem * n)
}

/// The length of `value` in HotSpot's UNSIGNED5 encoding
/// (`unsigned5.hpp`: L = 192 one-byte values, H = 64 per continuation).
pub(in crate::vm::vm_init) fn hs_unsigned5_len(value: u32) -> usize {
    let mut sum = value;
    for len in 1..5 {
        if sum < 192 {
            return len;
        }
        sum = (sum - 192) >> 6;
    }
    5
}

/// HotSpot's signed-int encoding for the line-number stream
/// (`CompressedWriteStream::write_signed_int`: zig-zag, then UNSIGNED5).
pub(in crate::vm::vm_init) fn hs_signed_len(value: i32) -> usize {
    hs_unsigned5_len(((value << 1) ^ (value >> 31)) as u32)
}

/// The size of HotSpot's compressed line-number table for `entries`
/// (`CompressedLineNumberWriteStream::write_pair_inline` plus the terminator
/// byte): one byte for a pair whose bci delta fits 5 bits and line delta 3
/// unsigned bits (unless it is the 0xFF escape), else the escape and two
/// signed ints; a (0, 0) delta writes nothing. 0 without entries.
pub(in crate::vm::vm_init) fn hs_compressed_line_table_bytes<'a>(
    entries: impl Iterator<Item = &'a cratonvm_reader::attribute::LineNumberEntry>,
) -> usize {
    let (mut last_bci, mut last_line) = (0i32, 0i32);
    let mut bytes = 0usize;
    let mut any = false;
    for e in entries {
        any = true;
        let (bci, line) = (i32::from(e.start_pc), i32::from(e.line_number));
        let (db, dl) = (bci - last_bci, line - last_line);
        last_bci = bci;
        last_line = line;
        if db == 0 && dl == 0 {
            continue;
        }
        if (db & !0x1F) == 0 && (dl & !0x7) == 0 && ((db << 3) | dl) != 0xFF {
            bytes += 1;
        } else {
            bytes += 1 + hs_signed_len(db) + hs_signed_len(dl);
        }
    }
    if any {
        bytes + 1
    } else {
        0
    }
}

/// The class-file size of one `element_value` (JVMS 4.7.16.1): what HotSpot
/// copies into its annotation `Array<u1>`.
pub(in crate::vm::vm_init) fn hs_element_value_bytes(v: &cratonvm_reader::attribute::ElementValue) -> usize {
    use cratonvm_reader::attribute::ElementValue;
    1 + match v {
        ElementValue::Const { .. } | ElementValue::Class { .. } => 2,
        ElementValue::Enum { .. } => 4,
        ElementValue::AnnotationValue(a) => hs_annotation_bytes(a),
        ElementValue::Array(values) => 2 + values.iter().map(hs_element_value_bytes).sum::<usize>(),
    }
}

/// The class-file size of one `annotation` (JVMS 4.7.16).
pub(in crate::vm::vm_init) fn hs_annotation_bytes(a: &cratonvm_reader::attribute::Annotation) -> usize {
    4 + a
        .element_value_pairs
        .iter()
        .map(|p| 2 + hs_element_value_bytes(&p.value))
        .sum::<usize>()
}

/// The body size of a `Runtime*Annotations` attribute holding `list`.
pub(in crate::vm::vm_init) fn hs_annotations_attr_bytes(list: &[cratonvm_reader::attribute::Annotation]) -> usize {
    2 + list.iter().map(hs_annotation_bytes).sum::<usize>()
}

/// The annotation bytes HotSpot keeps for one decoded member attribute:
/// `Some(body size)` for the runtime-VISIBLE kinds (HotSpot drops the
/// invisible ones without `-XX:+PreserveAllAnnotations`), `None` otherwise.
pub(in crate::vm::vm_init) fn hs_visible_annotation_body(a: &cratonvm_reader::attribute::LazyAttribute) -> Option<usize> {
    use cratonvm_reader::attribute::{Attribute, LazyAttribute};
    match a {
        LazyAttribute::Decoded(Attribute::RuntimeVisibleAnnotations(list)) => {
            Some(hs_annotations_attr_bytes(list))
        }
        LazyAttribute::Decoded(Attribute::RuntimeVisibleParameterAnnotations(params)) => Some(
            1 + params
                .iter()
                .map(|p| hs_annotations_attr_bytes(p))
                .sum::<usize>(),
        ),
        LazyAttribute::Decoded(Attribute::RuntimeVisibleTypeAnnotations(list)) => Some(
            2 + list
                .iter()
                .map(|t| {
                    1 + t.target_info.len()
                        + 1
                        + 2 * t.type_path.len()
                        + hs_annotation_bytes(&t.annotation)
                })
                .sum::<usize>(),
        ),
        LazyAttribute::Decoded(Attribute::AnnotationDefault(v)) => Some(hs_element_value_bytes(v)),
        LazyAttribute::Raw(raw) => match &*raw.name {
            "RuntimeVisibleAnnotations"
            | "RuntimeVisibleParameterAnnotations"
            | "RuntimeVisibleTypeAnnotations"
            | "AnnotationDefault" => Some(raw.range.len()),
            _ => None,
        },
        LazyAttribute::Decoded(_) => None,
    }
}

/// One method's HotSpot metadata: its `Method` and `ConstMethod` (header,
/// bytecode, compressed line numbers, the inlined u2 tables -- checked
/// exceptions, local variables, exception handlers, generic signature,
/// method parameters -- word aligned, then one pointer per annotation
/// array, as `ConstMethod::size`), its stack-map `Array<u1>` and its
/// annotation arrays. `MethodCounters` / `MethodData` (created by execution
/// and profiling, not by loading) are not modelled.
pub(in crate::vm::vm_init) fn hs_method_bytes(m: &cratonvm_reader::method::ClassFileMethod) -> usize {
    use cratonvm_reader::attribute::{Attribute, LazyAttribute};
    let mut extra = 0usize; // bytecode + line numbers + the u2 tables
    let mut pointers = 0usize; // one per annotation array
    let mut arrays = 0usize; // separately allocated arrays
    for a in &m.attributes {
        if let Some(body) = hs_visible_annotation_body(a) {
            pointers += 8;
            arrays += hs_array_bytes(1, body);
            continue;
        }
        match a {
            LazyAttribute::Decoded(Attribute::Code(code)) => {
                extra += code.code.len();
                if !code.exception_table.is_empty() {
                    extra += 2 + 8 * code.exception_table.len();
                }
                extra += hs_compressed_line_table_bytes(
                    code.attributes
                        .iter()
                        .filter_map(|s| match s {
                            Attribute::LineNumberTable(e) => Some(e.iter()),
                            _ => None,
                        })
                        .flatten(),
                );
                // `LocalVariableTypeTable` entries are merged into the
                // local-variable table's elements (no extra size).
                let locals: usize = code
                    .attributes
                    .iter()
                    .map(|s| match s {
                        Attribute::LocalVariableTable(e) => e.len(),
                        _ => 0,
                    })
                    .sum();
                if locals > 0 {
                    extra += 2 + 12 * locals;
                }
                for s in &code.attributes {
                    if let Attribute::StackMapTable { entries } = s {
                        arrays += hs_array_bytes(1, entries.len());
                    }
                }
            }
            LazyAttribute::Decoded(Attribute::Exceptions { exception_indices })
                if !exception_indices.is_empty() =>
            {
                extra += 2 + 2 * exception_indices.len();
            }
            LazyAttribute::Decoded(Attribute::Signature(_)) => extra += 2,
            LazyAttribute::Decoded(Attribute::MethodParameters(p)) => extra += 2 + 4 * p.len(),
            // Undecoded (rare once a class is defined): the attribute body as
            // the nearest figure.
            LazyAttribute::Raw(raw) => match &*raw.name {
                "Code" | "Exceptions" => extra += raw.range.len(),
                "MethodParameters" => extra += raw.range.len() + 1,
                "Signature" => extra += 2,
                _ => {}
            },
            LazyAttribute::Decoded(_) => {}
        }
    }
    let const_method = hs_align_word(HS_CONST_METHOD_BYTES + hs_align_word(extra) + pointers);
    let method = HS_METHOD_BYTES + if m.is_native() { HS_NATIVE_METHOD_EXTRA_BYTES } else { 0 };
    method + const_method + arrays
}

/// `sun.cls.methodBytes`: HotSpot's `ClassLoadingService` method-size
/// variable, which the class-file parser raises by `Method::size()` words for
/// each method it creates and the unload lowers again -- the live classes'
/// `Method` bytes, the same model's `sizeof(Method)` (two more words for a
/// native) that [`hs_method_bytes`] starts from, with no `ConstMethod`. An
/// array class has no methods. One walk of the store.
pub(crate) fn hs_live_method_bytes(cm: &ClassManager) -> u64 {
    cm.class_store()
        .iter()
        .filter(|c| !hs_is_array_class(c))
        .flat_map(|c| c.methods.iter())
        .map(|m| {
            let extra = if m.is_native() {
                HS_NATIVE_METHOD_EXTRA_BYTES
            } else {
                0
            };
            (HS_METHOD_BYTES + extra) as u64 // Widening: usize -> u64
        })
        .sum()
}

/// Is `c` an array class? A reference-component array carries `array_info`;
/// a primitive one (`[I`, ...) does not (`Class::array_info`), only its name
/// says so (gci i33/ms: the i32 model sized `[I` as an `InstanceKlass`).
pub(in crate::vm::vm_init) fn hs_is_array_class(c: &cratonvm_classloading::Class) -> bool {
    c.array_info.is_some() || c.name.starts_with('[')
}

/// HotSpot's class-shape facts that depend on the hierarchy (vtable length,
/// transitive interfaces, whether instances hold references), memoized per
/// class id for one walk (gci i33/ms: hash maps, so a walk over a few new
/// classes -- the `-XX:MaxMetaspaceSize` check's -- costs nothing per store
/// slot). Depth-bounded: a malformed cycle ends the recursion instead of the
/// stack.
pub(in crate::vm::vm_init) struct HsKlassShapes<'a> {
    pub(in crate::vm::vm_init) store: &'a cratonvm_classloading::ClassStore,
    pub(in crate::vm::vm_init) vtable_memo: FxHashMap<u32, usize>,
    /// Sorted, deduplicated transitive interface slots.
    pub(in crate::vm::vm_init) interfaces_memo: FxHashMap<u32, Vec<u32>>,
    /// Does an instance hold a reference field?
    pub(in crate::vm::vm_init) oops_memo: FxHashMap<u32, bool>,
}

pub(in crate::vm::vm_init) const HS_SHAPE_MAX_DEPTH: u32 = 64;

impl<'a> HsKlassShapes<'a> {
    pub(in crate::vm::vm_init) fn new(store: &'a cratonvm_classloading::ClassStore) -> Self {
        Self {
            store,
            vtable_memo: FxHashMap::default(),
            interfaces_memo: FxHashMap::default(),
            oops_memo: FxHashMap::default(),
        }
    }

    /// `klassVtable::compute_vtable_size_and_num_mirandas`: the superclass's
    /// length plus each own method `needs_new_vtable_entry` (not final -- nor
    /// in a final class --, private, static or `<init>`; package-private, or
    /// not overriding a public / protected method of a superclass), plus the
    /// new mirandas ([`Self::miranda_and_default_counts`], gci i33/ms).
    /// Interfaces and arrays: `Object`'s five.
    pub(in crate::vm::vm_init) fn vtable_words(&mut self, id: ClassId, depth: u32) -> usize {
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
        let slot = id.as_u32();
        if let Some(&v) = self.vtable_memo.get(&slot) {
            return v;
        }
        let store = self.store;
        let Some(c) = store.get(id) else {
            return HS_BASE_VTABLE_WORDS;
        };
        let words = if hs_is_array_class(c) || c.is_interface() || depth > HS_SHAPE_MAX_DEPTH {
            HS_BASE_VTABLE_WORDS
        } else {
            let base = match c.superclass {
                Some(s) if s != id => self.vtable_words(s, depth + 1),
                _ => 0,
            };
            let class_final = c.access_flags.contains(ClassAccessFlags::FINAL);
            let fresh = c
                .methods
                .iter()
                .filter(|m| {
                    let f = m.access_flags;
                    if class_final
                        || f.intersects(
                            MethodAccessFlags::FINAL
                                | MethodAccessFlags::PRIVATE
                                | MethodAccessFlags::STATIC,
                        )
                        || &*m.name == "<init>"
                    {
                        return false;
                    }
                    if c.superclass.is_none()
                        || !f.intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PROTECTED)
                    {
                        return true;
                    }
                    !hs_overrides_a_super_method(store, c.superclass, m)
                })
                .count();
            base + fresh + self.miranda_and_default_counts(c).0
        };
        self.vtable_memo.insert(slot, words);
        words
    }

    /// gci i33/ms (`i32-mf-report.md` review item 6): `(new mirandas, default
    /// methods)` of class `c` -- 0 for an interface or an array.
    ///
    /// * New mirandas (`klassVtable::get_mirandas` /
    ///   `add_new_mirandas_to_lists`): each instance, non-private method of
    ///   the class's transitive interfaces (abstract or default, one per name
    ///   and descriptor) that no instance method of the class or a superclass
    ///   implements (`is_miranda`) and that no interface of the superclass
    ///   declares (`lookup_method_in_all_interfaces`): a vtable word each.
    /// * Default methods (`DefaultMethods::generate_default_methods`'s empty
    ///   slots): the non-abstract ones among the same unimplemented methods,
    ///   the superclass's interfaces included -- the `_default_methods`
    ///   `Array<Method*>` and its `_default_vtable_indices` `Array<int>`.
    ///
    /// Overpass methods (generated for conflicting defaults) are not modelled.
    pub(in crate::vm::vm_init) fn miranda_and_default_counts(&mut self, c: &cratonvm_classloading::Class) -> (usize, usize) {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        if c.is_interface() || hs_is_array_class(c) {
            return (0, 0);
        }
        let all = self.interfaces(c.id, 0);
        if all.is_empty() {
            return (0, 0);
        }
        let super_interfaces = match c.superclass.filter(|s| *s != c.id) {
            Some(s) => self.interfaces(s, 1),
            None => Vec::new(),
        };
        let store = self.store;
        let instance = |m: &cratonvm_reader::method::ClassFileMethod| {
            !m.is_static() && !m.access_flags.contains(MethodAccessFlags::PRIVATE)
        };
        let super_declares = |name: &str, descriptor: &str| {
            super_interfaces.iter().any(|&s| {
                store.get(ClassId::new(s)).is_some_and(|i| {
                    i.methods.iter().any(|m| {
                        instance(m) && &*m.name == name && &*m.descriptor == descriptor
                    })
                })
            })
        };
        let mut seen: FxHashSet<(&str, &str)> = FxHashSet::default();
        let (mut mirandas, mut defaults) = (0usize, 0usize);
        for &slot in &all {
            let Some(i) = store.get(ClassId::new(slot)) else {
                continue;
            };
            for im in &i.methods {
                if !instance(im) || &*im.name == "<clinit>" {
                    continue;
                }
                let key = (&*im.name, &*im.descriptor);
                if !seen.insert(key) {
                    continue;
                }
                if hs_overrides_a_super_method(store, Some(c.id), im) {
                    continue;
                }
                if !im.is_abstract() {
                    defaults += 1;
                }
                if !super_declares(key.0, key.1) {
                    mirandas += 1;
                }
            }
        }
        (mirandas, defaults)
    }

    /// The class's transitive interfaces (`InstanceKlass::transitive_interfaces`):
    /// its superclass's (not for an interface), each direct interface and that
    /// interface's own.
    pub(in crate::vm::vm_init) fn interfaces(&mut self, id: ClassId, depth: u32) -> Vec<u32> {
        let slot = id.as_u32();
        if let Some(v) = self.interfaces_memo.get(&slot) {
            return v.clone();
        }
        let store = self.store;
        let mut out: Vec<u32> = Vec::new();
        if depth <= HS_SHAPE_MAX_DEPTH {
            if let Some(c) = store.get(id) {
                if !c.is_interface() {
                    if let Some(s) = c.superclass.filter(|s| *s != id) {
                        out.extend(self.interfaces(s, depth + 1));
                    }
                }
                for &i in &c.interfaces {
                    if i != id {
                        out.push(i.as_u32());
                        out.extend(self.interfaces(i, depth + 1));
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        self.interfaces_memo.insert(slot, out.clone());
        out
    }

    /// Does an instance hold a reference field (its own or inherited)? Then
    /// HotSpot embeds a nonstatic oop map (one block: a word; contiguous
    /// blocks of a subclass usually merge with the superclass's).
    pub(in crate::vm::vm_init) fn has_oops(&mut self, id: ClassId, depth: u32) -> bool {
        let slot = id.as_u32();
        if let Some(&oops) = self.oops_memo.get(&slot) {
            return oops;
        }
        let store = self.store;
        let Some(c) = store.get(id) else {
            return false;
        };
        let own = c
            .fields
            .iter()
            .any(|f| !f.is_static() && matches!(f.descriptor.as_bytes().first().copied(), Some(b'L' | b'[')));
        let oops = own
            || (depth <= HS_SHAPE_MAX_DEPTH
                && matches!(c.superclass, Some(s) if s != id && self.has_oops(s, depth + 1)));
        self.oops_memo.insert(slot, oops);
        oops
    }

    /// `klassItable::compute_itable_size`: one offset entry (two words) per
    /// transitive interface that has itable methods or superinterfaces, one
    /// more as the terminator, and a word per itable method. 0 for an
    /// interface.
    pub(in crate::vm::vm_init) fn itable_words(&mut self, c: &cratonvm_classloading::Class) -> usize {
        if c.is_interface() {
            return 0;
        }
        let store = self.store;
        let mut offsets = 1usize;
        let mut methods = 0usize;
        for slot in self.interfaces(c.id, 0) {
            let iid = ClassId::new(slot);
            let count = store.get(iid).map_or(0, |i| {
                i.methods
                    .iter()
                    .filter(|m| {
                        !m.is_static()
                            && !m.access_flags.contains(
                                cratonvm_reader::class_access_flags::MethodAccessFlags::PRIVATE,
                            )
                            && &*m.name != "<clinit>"
                            && &*m.name != "<init>"
                    })
                    .count()
            });
            if count > 0 || !self.interfaces(iid, 1).is_empty() {
                offsets += 1;
                methods += count;
            }
        }
        2 * offsets + methods
    }

    /// One class's HotSpot `-Xshare:off` metaspace bytes as `(non-class,
    /// class)`: gci i33/ms splits out what HotSpot 25 allocates in the
    /// compressed class space -- the `Klass` (with its embedded vtable,
    /// itable and oop map) of EVERY class, array, instance, abstract or
    /// interface alike (`Klass::operator new` with `use_class_space`);
    /// everything else is non-class metaspace. gci i36/ms (vi33-ms S1:
    /// class space per class 487 B ours vs 729 B HotSpot -Xshare:off, 0.668):
    /// i33 kept abstract classes' and interfaces' `Klass`es out of the class
    /// space after JDK-8338526 (`klass_needs_narrow_id`), which HotSpot 25
    /// does not have -- its diagnostic switch `UseClassMetaspaceForAllClasses`
    /// is absent from 25.0.3's `jvm.dll`, where every diagnostic flag name is
    /// present (`PrintMetaspaceStatisticsAtExit` is). `class_type_annotations`:
    /// the bytes of the class's `RuntimeVisibleTypeAnnotations`
    /// (`ClassManager::class_type_annotations`), 0 without.
    pub(in crate::vm::vm_init) fn class_parts(
        &mut self,
        c: &cratonvm_classloading::Class,
        class_type_annotations: usize,
    ) -> (usize, usize) {
        use cratonvm_reader::constant_pool::ConstantPoolEntry;
        if hs_is_array_class(c) {
            // A one-dimensional primitive array (`[I`, no `array_info`) is a
            // `TypeArrayKlass` whose secondary supers are the shared
            // `{Cloneable, Serializable}`; an object array gets its own
            // secondary-supers array (non-class metaspace).
            let primitive = c.name.len() == 2;
            let supers = if primitive { 0 } else { hs_array_bytes(8, 3) };
            return (supers, HS_ARRAY_KLASS_BYTES + 8 * HS_BASE_VTABLE_WORDS);
        }
        // The `InstanceKlass` with its embedded tables.
        let mut words = self.vtable_words(c.id, 0) + self.itable_words(c);
        if self.has_oops(c.id, 0) {
            words += 1;
        }
        if c.is_interface() {
            words += 1; // the implementor slot
        }
        let klass = HS_INSTANCE_KLASS_BYTES + 8 * words;

        // The constant pool, its tags, resolved-klass array, operands, and
        // its cache with the resolved field / method / indy entries and the
        // resolved-references map.
        let cp = &c.constant_pool;
        let slots = cp.len();
        let (mut klasses, mut field_refs, mut method_refs, mut indys, mut refs) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        for i in 1..slots {
            match cp.get(u16::try_from(i).unwrap_or(u16::MAX)) {
                Some(ConstantPoolEntry::ClassReference { .. }) => klasses += 1,
                Some(ConstantPoolEntry::FieldReference { .. }) => field_refs += 1,
                Some(
                    ConstantPoolEntry::MethodReference { .. }
                    | ConstantPoolEntry::InterfaceMethodReference { .. },
                ) => method_refs += 1,
                Some(ConstantPoolEntry::InvokeDynamic { .. }) => {
                    indys += 1;
                    refs += 1;
                }
                Some(
                    ConstantPoolEntry::StringReference { .. }
                    | ConstantPoolEntry::MethodHandle { .. }
                    | ConstantPoolEntry::MethodType { .. }
                    | ConstantPoolEntry::Dynamic { .. },
                ) => refs += 1,
                _ => {}
            }
        }
        let operands = if c.bootstrap_methods.is_empty() {
            0
        } else {
            2 * c.bootstrap_methods.len()
                + c.bootstrap_methods
                    .iter()
                    .map(|b| 2 + b.bootstrap_arguments.len())
                    .sum::<usize>()
        };
        let pool = HS_CONSTANT_POOL_BYTES
            + 8 * slots
            + hs_array_bytes(1, slots)
            + hs_array_bytes(8, klasses)
            + hs_array_bytes(2, operands)
            + HS_CP_CACHE_BYTES
            + hs_array_bytes(HS_RESOLVED_FIELD_ENTRY_BYTES, field_refs)
            + hs_array_bytes(HS_RESOLVED_METHOD_ENTRY_BYTES, method_refs)
            + hs_array_bytes(HS_RESOLVED_INDY_ENTRY_BYTES, indys)
            + hs_array_bytes(2, refs);

        // Methods: the `Array<Method*>` and each method.
        let methods = hs_array_bytes(8, c.methods.len())
            + c.methods.iter().map(hs_method_bytes).sum::<usize>();

        // Fields: the `FieldInfo` stream and the field-status array, and the
        // field annotations (an `Array<AnnotationArray*>` over every field).
        let nfields = c.fields.len();
        let mut fields = hs_array_bytes(
            1,
            HS_FIELD_INFO_STREAM_HEADER_BYTES + HS_FIELD_INFO_BYTES * nfields,
        ) + hs_array_bytes(1, nfields);
        let mut field_annotations = false;
        for f in &c.fields {
            for a in &f.attributes {
                if let Some(body) = hs_visible_annotation_body(a) {
                    field_annotations = true;
                    fields += hs_array_bytes(1, body);
                }
            }
        }
        if field_annotations {
            fields += hs_array_bytes(8, nfields);
        }

        // Interfaces: the local array, the transitive one when it is not the
        // local one, and the hashed secondary-supers array (JDK 23+: its own
        // array once there are two or more).
        let transitive = self.interfaces(c.id, 0).len();
        let local = c.interfaces.len();
        let mut supers = hs_array_bytes(8, local);
        if transitive != local {
            supers += hs_array_bytes(8, transitive);
        }
        if transitive >= 2 {
            supers += hs_array_bytes(8, transitive);
        }

        // The class's attribute arrays.
        let mut attributes = 0usize;
        if !c.inner_classes.is_empty() || c.enclosing_method.is_some() {
            attributes += hs_array_bytes(
                2,
                4 * c.inner_classes.len() + if c.enclosing_method.is_some() { 2 } else { 0 },
            );
        }
        attributes += hs_array_bytes(2, c.nest_members.len());
        attributes += hs_array_bytes(2, c.permitted_subclasses.len());
        if !c.record_components.is_empty() {
            attributes += hs_array_bytes(8, c.record_components.len());
            for rc in &c.record_components {
                attributes += HS_RECORD_COMPONENT_BYTES
                    + rc.annotations.as_ref().map_or(0, |a| hs_array_bytes(1, a.len()))
                    + rc.type_annotations.as_ref().map_or(0, |a| hs_array_bytes(1, a.len()));
            }
        }
        // Class annotations: `Class::annotations` holds the
        // `RuntimeVisibleAnnotations` only (every define path skips the
        // invisible ones, as HotSpot drops them) -- gci i33/ms: the i32
        // report's "over-count of invisible class annotations" does not
        // exist. The class's `RuntimeVisibleTypeAnnotations` are HotSpot's
        // `_class_type_annotations` (gci i33/ms: were not counted).
        let class_annotations = if c.annotations.is_empty() {
            0
        } else {
            hs_array_bytes(1, hs_annotations_attr_bytes(&c.annotations))        };
        let class_type = hs_array_bytes(1, class_type_annotations);
        if class_annotations > 0 || class_type > 0 || field_annotations {
            attributes += HS_ANNOTATIONS_BYTES + class_annotations + class_type;
        }
        // gci i33/ms: the inherited default methods' `_default_methods`
        // array and its `_default_vtable_indices`.
        let defaults = self.miranda_and_default_counts(c).1;
        attributes += hs_array_bytes(8, defaults) + hs_array_bytes(4, defaults);

        (pool + methods + fields + supers + attributes, klass)
    }
}

/// Does `m` override a public / protected / package-private instance method
/// of a superclass (from `start` up)? `klassVtable::needs_new_vtable_entry`'s
/// superclass search, by name and descriptor.
pub(in crate::vm::vm_init) fn hs_overrides_a_super_method(
    store: &cratonvm_classloading::ClassStore,
    start: Option<ClassId>,
    m: &cratonvm_reader::method::ClassFileMethod,
) -> bool {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let mut next = start;
    let mut depth = 0u32;
    while let Some(id) = next {
        if depth > HS_SHAPE_MAX_DEPTH {
            return false;
        }
        let Some(s) = store.get(id) else {
            return false;
        };
        if s.methods.iter().any(|sm| {
            !sm.access_flags.intersects(MethodAccessFlags::STATIC | MethodAccessFlags::PRIVATE)
                && sm.name == m.name
                && sm.descriptor == m.descriptor
        }) {
            return true;
        }
        next = s.superclass.filter(|n| *n != id);
        depth += 1;
    }
    false
}

/// gci i32/mf: HotSpot 25's `MetaspaceUtils::used_bytes()` under
/// `-Xshare:off`, modelled per live class from HotSpot's metadata layout
/// (see the `HS_*` sizes): the `InstanceKlass` with its vtable, itable and
/// oop map, the constant pool with its tags and cache, every `Method` /
/// `ConstMethod` with its stack map and annotations, the field stream and
/// the class's attribute arrays; an `ObjArrayKlass` / `TypeArrayKlass` per
/// array class. Symbols are not metaspace in HotSpot and are not counted.
/// O(classes + methods + superclass-chain lookups); the caller holds the
/// class-manager guard. gci i33/ms: the sum of [`hs_metaspace_arenas`] (the
/// class space included, as `used_bytes()` includes it).
pub(in crate::vm::vm_init) fn hotspot_metaspace_used_bytes(cm: &ClassManager) -> usize {
    let tally = hs_metaspace_arenas(cm);
    let used = tally.boot[0]
        .saturating_add(tally.boot[1])
        .saturating_add(tally.others_used[0])
        .saturating_add(tally.others_used[1]);
    usize::try_from(used).unwrap_or(usize::MAX)
}

/// gci i33/ms: the bytes of class `id`'s `RuntimeVisibleTypeAnnotations`
/// (`ClassManager::class_type_annotations`), 0 without.
pub(in crate::vm::vm_init) fn hs_class_type_annotations_len(cm: &ClassManager, id: ClassId) -> usize {
    cm.class_type_annotations.get(&id).map_or(0, |body| body.len())
}

/// gci i33/ms: every live class's model bytes, summed per arena -- the boot
/// loader's, and each other loader's (HotSpot gives every class loader its
/// own arenas; a hidden class is summed with its loader). One walk.
pub(in crate::vm::vm_init) fn hs_metaspace_arenas(cm: &ClassManager) -> cratonvm_classloading::MetaspaceArenaTally {
    let store = cm.class_store();
    let mut shapes = HsKlassShapes::new(store);
    let mut tally = cratonvm_classloading::MetaspaceArenaTally::default();
    for c in store.iter() {
        hs_tally_class(&mut tally, cm, &mut shapes, c);
    }
    tally.scanned_slots = store.slot_count();
    tally
}

/// Add class `c`'s model bytes to `tally`.
pub(in crate::vm::vm_init) fn hs_tally_class(
    tally: &mut cratonvm_classloading::MetaspaceArenaTally,
    cm: &ClassManager,
    shapes: &mut HsKlassShapes<'_>,
    c: &cratonvm_classloading::Class,
) {
    let (non_class, class) = shapes.class_parts(c, hs_class_type_annotations_len(cm, c.id));
    let bytes = [non_class as u64, class as u64];
    if c.loader_id == cratonvm_types::ClassLoaderId::Bootstrap {
        tally.boot[0] = tally.boot[0].saturating_add(bytes[0]);
        tally.boot[1] = tally.boot[1].saturating_add(bytes[1]);
    } else {
        let arena = tally
            .loaders
            .entry(c.loader_id.to_native_id())
            .or_insert([0, 0]);
        for (i, chunks) in [HS_STANDARD_NON_CLASS_CHUNKS, HS_STANDARD_CLASS_CHUNKS]
            .iter()
            .enumerate()
        {
            let before = arena[i];
            let after = before.saturating_add(bytes[i]);
            arena[i] = after;
            tally.others_used[i] = tally.others_used[i].saturating_add(bytes[i]);
            tally.others_footprint[i] = tally.others_footprint[i].saturating_add(
                hs_arena_footprint(after, chunks).saturating_sub(hs_arena_footprint(before, chunks)),
            );
        }
    }
    tally.classes += 1;
}

/// gci i33/ms: bring the class manager's running tally up to date for the
/// `-XX:MaxMetaspaceSize` check -- sum the classes defined since it last
/// looked (class ids are never reused: they are the slots past
/// `scanned_slots`), and sum every live class again when a class was
/// unloaded since (the live count no longer matches). A class redefined in
/// place keeps the bytes it was summed with until the next unload; the
/// reported figures are fresh walks ([`metaspace_stats`]).
pub(in crate::vm::vm_init) fn hs_tally_catch_up(tally: &mut cratonvm_classloading::MetaspaceArenaTally, cm: &ClassManager) {
    let store = cm.class_store();
    let slots = store.slot_count();
    if tally.scanned_slots > slots {
        *tally = cratonvm_classloading::MetaspaceArenaTally::default();
    }
    if tally.scanned_slots < slots {
        let mut shapes = HsKlassShapes::new(store);
        for slot in tally.scanned_slots..slots {
            let id = ClassId::new(u32::try_from(slot).unwrap_or(u32::MAX));
            if let Some(c) = store.get(id) {
                hs_tally_class(tally, cm, &mut shapes, c);
            }
        }
        tally.scanned_slots = slots;
    }
    if tally.classes != store.len() {
        *tally = hs_metaspace_arenas(cm);
    }
}

/// The pre-i32/mf figure (`CRATONVM_METASPACE_HOTSPOT_FIGURE=0`): the size of
/// this VM's own reader structures per live class -- the `Class` record, a
/// `ConstantPoolEntry` per slot, the member tables and the attribute bodies.
pub(in crate::vm::vm_init) fn metaspace_reader_struct_bytes(cm: &ClassManager) -> usize {
    cm.class_store().iter().map(class_metadata_bytes).sum()
}


impl crate::runtime::serviceability::VmDiagnosticState for SharedVm {
    /// `VmConfig::attach_listener_load` (interpreter round i2 wave 58, lane L1).
    fn attach_load_enabled(&self) -> bool {
        self.config.attach_listener_load
    }

    /// The attach `load` operation, into this VM
    /// (`runtime::instrument::attach_listener_load`; wave 58, lane L1).
    fn attach_load(&self, lib: &str, absolute: bool, options: &str) -> String {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => {
                crate::runtime::instrument::attach_listener_load(shared, lib, absolute, options)
            }
            None => "VM is shutting down".to_string(),
        }
    }

    /// `-XX:-EnableDynamicAgentLoading` (interpreter round i3 wave 81, lane L1).
    fn dynamic_agent_loading_disabled(&self) -> bool {
        self.config.enable_dynamic_agent_loading == Some(false)
    }

    /// `VmConfig::attach_listener_props` (interpreter round i2 wave 59, lane L1).
    fn attach_properties_enabled(&self) -> bool {
        true
    }

    /// `VmConfig::attach_listener_lazy` (interpreter round i2 wave 59, lane L1).
    fn attach_listener_lazy(&self) -> bool {
        self.config.attach_listener_lazy
    }

    /// `VmConfig::sigquit_thread_dump` (interpreter round i2 wave 60, lane L1).
    fn sigquit_thread_dump(&self) -> bool {
        // `-Xrs` installs no latch, so the listener has nothing to act on.
        self.config.sigquit_thread_dump && !self.config.reduce_signal_usage
    }

    /// `jcmd <pid> Thread.dump_to_file` (gci i12/td): the JDK's
    /// `ThreadDumper`, run on an attached "Attach Listener" thread as HotSpot
    /// runs its `ThreadDumpToFileDCmd`
    /// (`runtime::instrument::attach_listener_thread_dump`).
    fn thread_dump_to_file(&self, path: &str, overwrite: bool, json: bool) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => {
                crate::runtime::instrument::attach_listener_thread_dump(shared, path, overwrite, json)
            }
            None => Err("VM is shutting down".to_string()),
        }
    }

    /// The attach `properties` / `agentProperties` operations, in this VM
    /// (`runtime::instrument::attach_listener_properties`; wave 59, lane L1).
    fn attach_properties(&self, agent: bool) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => crate::runtime::instrument::attach_listener_properties(shared, agent),
            None => Err("VM is shutting down".to_string()),
        }
    }

    /// `jcmd <pid> help JFR.<x>` (gci i33/jf): the JDK's `getHelp()`, as
    /// HotSpot's `JfrDCmd::print_help`
    /// (`runtime::instrument::attach_listener_jfr_help`).
    fn jfr_help(&self, class: &str) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => crate::runtime::instrument::attach_listener_jfr_help(shared, class),
            None => Err("VM is shutting down".to_string()),
        }
    }

    /// `jcmd <pid> GC.finalizer_info` (gci i33/jf;
    /// `runtime::instrument::attach_listener_finalizer_info`).
    fn finalizer_info(&self) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => crate::runtime::instrument::attach_listener_finalizer_info(shared),
            None => Err("VM is shutting down".to_string()),
        }
    }

    /// `jcmd <pid> GC.run_finalization` (gci i33/jf;
    /// `runtime::instrument::attach_listener_run_finalization`).
    fn run_finalization(&self) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => crate::runtime::instrument::attach_listener_run_finalization(shared),
            None => Err("VM is shutting down".to_string()),
        }
    }

    /// The dump's threads in HotSpot 25's shape (interpreter round i3 wave 81,
    /// lane L6; page `i59-L1-sigquit-terminates-the-vm`, measured against a
    /// HotSpot 25.0.4 `SIGQUIT` dump, `jstack` and `jcmd Thread.print`):
    ///
    /// * the live threads only, in registry order -- the order they
    ///   registered, as HotSpot lists its threads in the order they were
    ///   added -- where a hash map's order (and dead threads) were printed;
    /// * `#<Thread.threadId()>` (was the registry's own id), the OS tid, the
    ///   thread's own `priority` and `daemon` (were 5 and a guess by name),
    ///   its CPU time and time since it registered, its `JvmThread` address;
    /// * a blocked thread's frames as `vm_exec::thread_dump_stack` lists them
    ///   (parked leaf natives, served `Thread.run` frames, overload lines,
    ///   the lambda proxy frame), each with its module, and the lock lines
    ///   (`waiting on`, `parking to wait for`, `waiting to lock`, `locked`).
    ///
    /// Not listed: HotSpot's own threads this VM does not have as Java
    /// threads (`Signal Dispatcher`, `Service Thread`, compiler threads,
    /// `Attach Listener`, ...) and its non-Java threads (`VM Thread`, GC
    /// threads).
    ///
    /// gci i18/dc (page
    /// `gci-i17jh-thread-print-shows-no-frames-for-a-running-thread-FIXED-20261015`):
    /// a RUNNING thread's frames too. Its stack is published only at a
    /// blocking point or a pause, so when some listed thread is running the
    /// dump first takes ONE frame-trace pause (`stw_publish_frame_traces`,
    /// initiator `PAUSE_OUTSIDER`: the attach / signal thread is no registered
    /// mutator), as HotSpot's `VM_ThreadDump` takes one safepoint, and then
    /// reads every thread's published trace. Only from a thread no registry
    /// entry claims and of a VM the JNI registry knows
    /// ([`thread_dump_wants_frame_trace_pause`]); a pause another
    /// stop-the-world pre-empted publishes nothing and the running threads
    /// print as before (no frame). Kill: `CRATONVM_THREAD_DUMP_ONE_PAUSE=0`
    /// (the Java-level dump's one pause, the same promise).
    fn thread_snapshots(&self) -> Vec<crate::runtime::serviceability::ThreadSnapshot> {
        use crate::runtime::serviceability::{
            dump_lock_lines, DumpMonitor, ThreadDumpFacts, ThreadSnapshot, ThreadState,
        };

        let registry = &self.threads.thread_registry;
        let entries = registry.thread_dump_entries();
        let mut snapshots = Vec::with_capacity(entries.len());
        // Interpreter round i2 wave 59, lane L6 (`CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`,
        // stage 1 of `i58-L1-proposal-one-published-thread-state-word`): the
        // dump reads the state every other reader reads
        // (`ThreadRegistry::java_block_state`), and a blocked thread's
        // published frames, where every live thread printed RUNNABLE with no
        // frame at all.
        let published = registry.blocked_in_native_runnable();
        let monitor = |obj: crate::types::ObjectRef| super::vm_exec::thread_dump_monitor(self, obj);

        // gci i11/vt item 9: a JDK-driven virtual thread's continuation has a
        // registry entry of its own (`vm::jdk_continuation`), but it is not a
        // `Thread.print` thread: HotSpot 25 lists platform threads only (its
        // carriers among them); virtual threads appear in
        // `Thread.dump_to_file` / `HotSpotDiagnosticMXBean.dumpThreads`, which
        // the JDK's `ThreadDumper` builds from thread containers, not from
        // here. A bound virtual thread (`CRATONVM_VT_CONTINUATIONS=0`, HotSpot's
        // `-XX:-VMContinuations` shape, where each is a JavaThread) stays.
        let vtm = &self.threads.virtual_thread_manager;
        let continuation_entry = |tid: crate::threading::jvm_thread::ThreadId| {
            vtm.may_have_virtual_threads() && vtm.is_jdk_continuation(tid.0)
        };
        // gci i12/td (item 9's remainder): HotSpot's `JavaThread::print_on`
        // prints `   Carrying virtual thread #<id>` in place of a carrier's
        // state line while a virtual thread is mounted on it. A continuation's
        // entry holds its virtual thread, whose `carrierThread` (set by the
        // JDK's `VirtualThread.mount`, cleared by `unmount`) names the carrier.
        let carried: Vec<(crate::types::ObjectRef, u64)> = if vtm.may_have_virtual_threads() {
            entries
                .iter()
                .filter(|e| e.alive && continuation_entry(e.thread_id))
                .filter_map(|e| {
                    let vthread = e.java_thread_obj?;
                    let carrier = dump_carrier_thread_of(self, vthread)?;
                    let vtid = e
                        .java_tid
                        .or_else(|| super::vm_exec::read_java_thread_tid(self, vthread))?;
                    Some((carrier, vtid))
                })
                .collect()
        } else {
            Vec::new()
        };
        // gci i18/dc: one frame-trace pause when a listed thread is running,
        // so its frames are published (see the doc above).
        let any_running = entries
            .iter()
            .any(|e| e.alive && !continuation_entry(e.thread_id) && !registry.is_blocked(e.thread_id));
        let caller_is_outsider = !crate::native::jni::is_foreign_attached()
            && registry.current_os_tid_claimed() == Some(false);
        // gci i46/lg (residual 2): `jcmd Thread.print` / SIGQUIT is HotSpot's `VM_PrintThreads`.
        let _print_threads = self.mem.gc_barrier.frame_trace_op_scope(Some("PrintThreads"));
        let traced = thread_dump_wants_frame_trace_pause(
            published,
            registry.thread_dump_one_pause(),
            caller_is_outsider,
            any_running,
        ) && crate::native::jni::vm_for_identity(self.vm_identity).is_some()
            && crate::runtime::interpreter::stw_publish_frame_traces(
                self,
                crate::runtime::interpreter::PAUSE_OUTSIDER,
            );
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[THREAD_PRINT_PAUSE] running={any_running} outsider={caller_is_outsider} traced={traced}");
        }
        for entry in entries
            .iter()
            .filter(|e| e.alive && !continuation_entry(e.thread_id))
        {
            let tid = entry.thread_id;
            // Interpreter round i5 wave 103, lane L6 (page
            // `i101-L6-main-thread-stays-alive-during-destroy-java-vm`): the
            // launcher's main entry once it carries its `DestroyJavaVM` mirror
            // (`vm_exec::list_destroy_java_vm_thread`) prints as HotSpot's
            // `DestroyJavaVM` thread: RUNNABLE, `waiting on condition`, no
            // frame and a zero stack address -- not the launcher's own wait
            // (its published block state and frames).
            let destroy_java_vm =
                registry.java_side_ended(tid) && registry.destroy_java_vm_listed();
            let state = if destroy_java_vm {
                ThreadState::Runnable
            } else if published {
                match registry.java_block_state(tid) {
                    1 => ThreadState::Waiting,
                    2 => ThreadState::Blocked,
                    3 => ThreadState::TimedWaiting,
                    _ => ThreadState::Runnable,
                }
            } else {
                ThreadState::Runnable
            };
            let blocked = published && !destroy_java_vm && registry.is_blocked(tid);
            // RUNNABLE yet blocked, and not in a native's own C code: a wait
            // inside the VM, HotSpot's `CONDVAR_WAIT` (`waiting on condition`).
            let vm_wait = destroy_java_vm
                || (blocked && state == ThreadState::Runnable && !registry.blocked_in_native(tid));
            // Frames are on OS thread stacks; a blocked thread's are published,
            // and since gci i18/dc a running one's by the pause above.
            let stack = if blocked || (traced && published) {
                super::vm_exec::thread_dump_stack(self, tid, entry.java_thread_obj, vm_wait)
            } else {
                Default::default()
            };
            // Interpreter round i3 wave 79, lane L6 (page
            // `i63-L1-thread-dump-has-no-wait-kind-suffix`): HotSpot prints
            // the wait's kind (`(sleeping)`, `(parking)`, `(on object
            // monitor)`) and a header word that depends on it; the registry
            // answers it from the published wait word, as the JVMTI reader.
            let wait_kind = match state {
                ThreadState::Blocked => {
                    Some(crate::runtime::serviceability::DumpWaitKind::MonitorEnter)
                }
                ThreadState::Waiting | ThreadState::TimedWaiting => {
                    Some(dump_wait_kind_of(registry.dump_wait_kind(tid)))
                }
                _ => None,
            };
            let (priority, daemon) = entry.java_thread_obj.map_or((None, None), |obj| {
                super::vm_exec::thread_dump_priority_and_daemon(self, obj)
            });
            let java_tid = entry.java_tid.or_else(|| {
                entry
                    .java_thread_obj
                    .and_then(|obj| super::vm_exec::read_java_thread_tid(self, obj))
            });
            let (contended, waiting) = registry
                .jmx_lock_snapshot(tid)
                .map_or((None, None), |snapshot| (snapshot.0, snapshot.1));
            let park_blocker = entry
                .java_thread_obj
                .filter(|_| !stack.frames.is_empty())
                .and_then(|obj| super::vm_exec::thread_dump_park_blocker(self, obj));
            let frame_locked: Vec<(usize, DumpMonitor)> = stack
                .frame_monitors
                .iter()
                .filter_map(|&(index, obj)| Some((index, monitor(obj)?)))
                .collect();
            let top = stack.frames.first().map(|e| {
                (
                    crate::runtime::serviceability::external_class_name(&e.class_name),
                    e.method_name.to_string(),
                )
            });
            let lock_lines = dump_lock_lines(
                top.as_ref().map(|(c, m)| (c.as_str(), m.as_str())),
                state,
                waiting.and_then(|o| monitor(o)).as_ref(),
                park_blocker.and_then(|o| monitor(o)).as_ref(),
                contended.and_then(|o| monitor(o)).as_ref(),
                &frame_locked,
            );
            let frame_modules = if stack.frames.is_empty() {
                Vec::new()
            } else {
                let cm = self.classes.class_manager.read();
                stack
                    .frames
                    .iter()
                    .map(|e| super::vm_exec::thread_dump_frame_module(&cm, e.class_id))
                    .collect()
            };
            let cpu_nanos = entry
                .os_tid
                .and_then(cratonvm_native_builtins::phases_late::management::os_thread_cpu_time_ns)
                .map(|(cpu, _)| u64::try_from(cpu).unwrap_or(0));
            let elapsed_nanos =
                u64::try_from(entry.registered_at.elapsed().as_nanos()).unwrap_or(u64::MAX);

            snapshots.push(ThreadSnapshot {
                id: tid.0 as u64,
                name: entry.name.clone(),
                daemon: daemon.unwrap_or(entry.daemon),
                priority: priority.unwrap_or(5),
                state,
                stack_frames: stack.frames.iter().map(dump_frame_info).collect(),
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind,
                dump: ThreadDumpFacts {
                    java_tid,
                    os_tid: entry.os_tid.map(u64::from),
                    cpu_nanos,
                    elapsed_nanos: Some(elapsed_nanos),
                    thread_addr: entry.jvm_thread_addr as u64,
                    stack_addr: if destroy_java_vm {
                        0
                    } else {
                        (entry.blocked_wait_lo & !0xfff) as u64
                    },
                    vm_wait,
                    frame_modules,
                    lock_lines,
                    // A VM thread of its own (the finalizer-delivery and
                    // concurrent-mark service threads) has no `java.lang.Thread`.
                    no_java_thread: entry.java_thread_obj.is_none(),
                    carrying_virtual_thread: entry.java_thread_obj.and_then(|obj| {
                        carried.iter().find(|(carrier, _)| *carrier == obj).map(|&(_, vtid)| vtid)
                    }),
                    // zt t28/mf: filled by `Thread.print -l` / `SIGQUIT`.
                    owned_synchronizers: None,
                },
            });
        }

        // Always include a "main" thread if none present
        if snapshots.is_empty() {
            snapshots.push(ThreadSnapshot {
                id: 0,
                name: "main".to_string(),
                daemon: false,
                priority: 5,
                state: ThreadState::Runnable,
                stack_frames: Vec::new(),
                lock_info: None,
                blocked_by: None,
                waiting_on: None,
                wait_kind: None,
                dump: ThreadDumpFacts::default(),
            });
        }

        snapshots
    }

    /// This VM's own `java.vm.*` properties, the registry's address as the
    /// thread list's, and its JNI global / weak global ref counts
    /// (interpreter round i3 wave 81, lane L6).
    fn thread_dump_header(&self) -> crate::runtime::serviceability::ThreadDumpHeader {
        let props = crate::runtime::serviceability::VmDiagnosticState::system_properties(self);
        let mut header = crate::runtime::serviceability::ThreadDumpHeader::from_properties(&props);
        header.thread_list_addr = &self.threads.thread_registry as *const _ as usize as u64;
        let refs = self.natives.jni_global_refs.lock();
        header.jni_global_refs = refs.count();
        header.jni_weak_refs = refs.weak_count();
        header
    }

    fn heap_summary(&self) -> crate::runtime::serviceability::HeapSummary {
        use crate::runtime::serviceability::HeapSummary;

        let (young_bump, young_cap) = self.mem.heap.young_gen_stats();
        // Young OCCUPANCY, not the bump cursor (gc-common w3-f, 2026-09-23).
        // On the generational backend `young_gen_stats().0` is the from-space
        // cursor, which a non-moving young sweep (the path every JIT-warm
        // workload takes) never retracts: it reclaims onto the free list. So
        // this summary reported the young generation as full as its
        // high-water mark after a sweep that emptied it, while the JMX heap
        // `used`, `Runtime.freeMemory()` and `-Xlog:gc` (all
        // `VmHeap::live_bytes_estimate`) reported the reclamation. Subtract
        // the free list, as `live_bytes_estimate` does; `young_occupancy` is
        // zeros on G1 and ZGC, so their arms are unchanged. (`young_gen_stats`
        // itself stays the cursor: the SoftReference policy keys on it
        // deliberately — allocatable, not unused, bytes.)
        let young_used = young_bump.saturating_sub(self.mem.heap.young_occupancy().1);
        let (old_used, old_cap) = self.mem.heap.old_gen_stats();
        // Class metadata, MEASURED rather than invented (gc-common w2-e,
        // 2026-09-23). This used to be `loaded_count() * 4096` with a constant
        // 64 MiB floor under the capacity, so a 3 KiB DTO and a 300 KiB
        // generated parser counted the same and a class-loader leak could not
        // be told from a healthy app with the same class count
        // (`common-d-metaspace-numbers-are-fabricated-FIXED-20260923.md`;
        // enforcement: `docs/feature-designs/real-jdk-mode.md`).
        //
        // NOT the figure `-XX:MaxMetaspaceSize` is enforced against (gc-common
        // w6-f). That is `ClassManager::metaspace_charged_bytes`: class-file
        // bytes of non-bootstrap classes, an O(1) counter kept at define and
        // unload — a conservative under-estimate of HotSpot's use, so a limit
        // that fits there fits here. This walk stays the reported figure: it
        // measures what the store actually holds, every class included.
        //
        // What is summed, per LIVE class in the store (an unloaded class's slot
        // is `None` and drops out, so the figure falls when a loader is
        // unloaded): the `Class` record itself, its name, one
        // `ConstantPoolEntry` per constant-pool slot, the field / interface /
        // method tables, and per method its name, descriptor and attribute
        // bodies — the undecoded ones by their byte range in the class file,
        // a decoded `Code` by its bytecode. It is a LOWER BOUND: interned
        // strings shared between classes, vtables, resolution caches and JIT
        // code live elsewhere and are not counted. There is no separate
        // reservation behind it (class metadata is Rust heap owned by
        // `ClassStore`), so capacity == used, which is also what an honest
        // "committed" is for memory with no reservation.
        //
        // O(classes + methods) under the class-manager read lock — this is a
        // serviceability (`jcmd`/`jmap`) call, never a hot path.
        //
        // gci i18/dk (rv finding 8): the walk is [`metaspace_used_bytes`], so
        // the non-heap threshold check can take it alone, under a guard it
        // already holds.
        //
        // gci i32/mf: the paragraph above describes the reader-structure
        // figure, now behind `CRATONVM_METASPACE_HOTSPOT_FIGURE=0`; by default
        // the walk models HotSpot `-Xshare:off`'s `MetaspaceUtils::used_bytes()`
        // per class (see [`metaspace_used_bytes`]). Capacity stays == used.
        //
        // gci i33/ms: capacity is the model's COMMITTED metaspace (HotSpot
        // commits by 64 KiB granule per arena; [`metaspace_stats`], same one
        // walk; == used again under `CRATONVM_METASPACE_HOTSPOT_SPACES=0`).
        // And `-XX:MaxMetaspaceSize` is now checked against that committed
        // model, boot classes included ([`metaspace_limit_would_exceed`]);
        // the class-file-bytes paragraph above is its `=0` arm.
        //
        // zs s3/ob2: the ZGC `Metaspace` pool's collection-edge sampler reads
        // the same figure (`class_metadata_walk_bytes`).
        let metaspace = metaspace_stats(&self.classes.class_manager.read());
        let metaspace_used = usize::try_from(metaspace.used).unwrap_or(usize::MAX);
        let metaspace_capacity = usize::try_from(metaspace.committed).unwrap_or(usize::MAX);

        HeapSummary {
            young_gen_used: young_used as u64,
            young_gen_capacity: young_cap as u64,
            old_gen_used: old_used as u64,
            old_gen_capacity: old_cap as u64,
            metaspace_used: metaspace_used as u64,
            metaspace_capacity: metaspace_capacity as u64,
            total_used: (young_used + old_used + metaspace_used) as u64,
            total_capacity: (young_cap + old_cap + metaspace_capacity) as u64,
        }
    }

    /// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
    /// item 6): `jcmd GC.heap_info` and the SIGQUIT `Heap` section in HotSpot
    /// 25's text, Serial's lines on Generational and G1's on G1 (the
    /// `gc,exit` summary's builder, `unified_logging::exit_heap_lines`);
    /// `None` on ZGC, which keeps the summary text.
    fn heap_info_lines(&self) -> Option<Vec<String>> {
        crate::runtime::unified_logging::exit_heap_lines(self)
    }

    /// `jcmd GC.class_histogram` / `jmap -histo` (gci i3/hd, page
    /// `gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-FIXED-20261012`): walked
    /// inside the dump's own non-moving pause (`hprof::class_histogram_for_dcmd`),
    /// in HotSpot's order and names. It used to walk and read every header
    /// from the attach thread with the mutators running. An empty histogram
    /// when no pause could be had (the trait has no error channel; the
    /// failure is on stderr).
    /// zt t27/lg1 (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`): a
    /// `setVMOption("PrintClassHistogram", ..)` written since start-up, else
    /// the command line's last `-XX:±PrintClassHistogram`.
    fn print_class_histogram_at_sigquit(&self) -> bool {
        crate::config::hotspot_flags::manageable_effect_flags_enabled()
            && self
                .mem
                .managed_flags
                .get_bool("PrintClassHistogram")
                .unwrap_or_else(|| {
                    crate::config::hotspot_flags::jvm_arg_bool(
                        &self.config,
                        "PrintClassHistogram",
                        false,
                    )
                })
    }

    /// zt t28/mf (`CRATONVM_JMX_MANAGEABLE_EFFECT_FLAGS`): a
    /// `setVMOption("PrintConcurrentLocks", ..)` written since start-up, else
    /// the command line's last `-XX:±PrintConcurrentLocks`.
    fn print_concurrent_locks_at_sigquit(&self) -> bool {
        crate::config::hotspot_flags::manageable_effect_flags_enabled()
            && self
                .mem
                .managed_flags
                .get_bool("PrintConcurrentLocks")
                .unwrap_or_else(|| {
                    crate::config::hotspot_flags::jvm_arg_bool(
                        &self.config,
                        "PrintConcurrentLocks",
                        false,
                    )
                })
    }

    /// zt t28/mf: the census at one outsider pause
    /// (`vm_exec::owned_synchronizer_lines_outside`).
    fn owned_synchronizer_lines(&self) -> Option<Vec<(u64, String)>> {
        super::vm_exec::owned_synchronizer_lines_outside(self)
    }

    fn class_histogram(&self) -> Vec<crate::runtime::serviceability::ClassHistogramEntry> {
        match crate::runtime::hprof::class_histogram_for_dcmd(self) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("GC.class_histogram: {e}");
                Vec::new()
            }
        }
    }

    /// `GC.class_histogram` without `-all` / `jmap -histo:live` (gci i4/hd,
    /// `gci-i3hd-jcmd-heap-walks-count-dead-objects-FIXED-20261013`): with
    /// `CRATONVM_DCMD_LIVE_HEAP_WALKS` (default ON since 2026-10-13; `=0` turns it off: it adds a full
    /// collection to every such command, as HotSpot does) the walk follows a
    /// full collection (`Heap Inspection Initiated GC` since gci i9/dc), as
    /// HotSpot's `VM_GC_HeapInspection` collects first; off, the walk counts what the heap holds, dead objects
    /// included, as before.
    fn class_histogram_live(
        &self,
        live: bool,
    ) -> Vec<crate::runtime::serviceability::ClassHistogramEntry> {
        if live {
            dcmd_live_heap_walk_collect(
                self,
                crate::runtime::interpreter::gc_and_alloc::DcmdFullGc::HeapInspection,
            );
            // gci i14/hd second pass (item 7): the walk is told it follows the
            // live collection (`hprof::class_histogram_for_dcmd_live`).
            return match crate::runtime::hprof::class_histogram_for_dcmd_live(self, true) {
                Ok(entries) => entries,
                Err(e) => {
                    eprintln!("GC.class_histogram: {e}");
                    Vec::new()
                }
            };
        }
        self.class_histogram()
    }

    /// `GC.heap_dump` without `-all` / `jmap -dump:live` (gci i4/hd): as
    /// [`Self::class_histogram_live`] (HotSpot's `VM_HeapDumper` collects
    /// first).
    fn heap_dump_live(&self, path: &str, live: bool) -> Result<u64, String> {
        if live {
            dcmd_live_heap_walk_collect(
                self,
                crate::runtime::interpreter::gc_and_alloc::DcmdFullGc::HeapDump,
            );
            // gci i14/hd second pass (item 7): as `heap_dump`, told it follows
            // the live collection.
            return crate::runtime::hprof::dump_heap_for_dcmd_live(self, path, false, true, true);
        }
        self.heap_dump(path)
    }

    /// `GC.heap_dump [-all] [-gz=<level>] [-overwrite] <file>` (gci i5/hd):
    /// [`Self::heap_dump_live`]'s collection, then the dump gzip-framed when
    /// `gz_level` is set, the file opened exclusively unless `overwrite`
    /// (`hprof::dump_heap_for_dcmd`).
    fn heap_dump_with(
        &self,
        path: &str,
        live: bool,
        gz_level: Option<u8>,
        overwrite: bool,
    ) -> Result<u64, String> {
        if live {
            dcmd_live_heap_walk_collect(
                self,
                crate::runtime::interpreter::gc_and_alloc::DcmdFullGc::HeapDump,
            );
        }
        // gci i14/hd second pass (item 7): `live` reaches the walk.
        crate::runtime::hprof::dump_heap_for_dcmd_live(self, path, gz_level.is_some(), overwrite, live)
    }

    /// `jcmd GC.run` (gci i2/oo): request HotSpot's `_dcmd_gc_run` full
    /// collection and wait, bounded, for it to complete. `true` only when it
    /// did; it used to latch `gc_requested` (a deferred YOUNG collection
    /// labelled `Allocation Failure`) and answer `true` at once. gci i9/dc:
    /// run now on a thread attached for it, as the live-only walks' collections
    /// (`run_dcmd_full_gc_on_attach_thread`), so an idle VM collects at once and
    /// keeps no request pending; the bounded latch wait only where that path
    /// does not apply.
    fn trigger_gc(&self) -> bool {
        if crate::runtime::interpreter::gc_and_alloc::run_dcmd_full_gc_on_attach_thread(
            self,
            crate::runtime::interpreter::gc_and_alloc::DcmdFullGc::GcRun,
        ) == Some(true)
        {
            return true;
        }
        crate::runtime::interpreter::gc_and_alloc::request_diagnostic_command_gc(
            self,
            crate::runtime::interpreter::gc_and_alloc::DCMD_GC_RUN_WAIT,
        )
    }

    fn uptime_secs(&self) -> f64 {
        self.debug.diagnostic_counters.uptime_secs()
    }

    fn command_line(&self) -> String {
        let mut parts = vec!["cratonvm".to_string()];
        if self.config.max_heap_size != 256 * 1024 * 1024 {
            parts.push(format!(
                "-Xmx{}m",
                self.config.max_heap_size / (1024 * 1024)
            ));
        }
        if !self.config.classpath.is_empty() {
            parts.push(format!("-cp {}", self.config.classpath.join(":")));
        }
        for (k, v) in &self.config.system_properties {
            parts.push(format!("-D{}={}", k, v));
        }
        parts.join(" ")
    }

    fn system_properties(&self) -> Vec<(String, String)> {
        let props = self.system_properties.read();
        props.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn vm_flags(&self) -> Vec<String> {
        // zt t24/x20: with the values a `setVMOption` wrote (`{management}`).
        // The running collector's row is `{ergonomic}` without its flag
        // (`CRATONVM_VM_FLAGS_ERGONOMIC_GC_ORIGIN`), so a default ZGC VM names
        // `-XX:+UseZGC` as HotSpot names its ergonomic `-XX:+UseG1GC`.
        let mut flags: Vec<String> = crate::config::hotspot_flags::resolved_flags_for(self)
            .into_iter()
            .filter(|row| row.origin != "{default}")
            .map(|row| row.command_line())
            .collect();
        // No `-XX:CompatibilityMode=` token: HotSpot prints no such flag, and
        // both modes answer as HotSpot 25 does (dev cabdb9f4ae dropped it; zt t24
        // keeps it dropped).
        flags
    }

    /// `jcmd VM.flags -all`: the `-XX:+PrintFlagsFinal` rows of the flags
    /// this VM models, as `-XX:+PrintFlagsFinal` prints them.
    fn vm_flags_final(&self) -> Vec<String> {
        crate::config::hotspot_flags::resolved_flags(&self.config)
            .iter()
            .map(|row| row.print_final())
            .collect()
    }

    /// `jcmd GC.heap_dump` / `jmap -dump` (gci i3/hd, page
    /// `gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-FIXED-20261012`): the
    /// `runtime::hprof` dumper (GC roots, reserved-tail skips, array classes)
    /// inside its own non-moving pause (`hprof::dump_heap_for_dcmd`). It used
    /// to walk the heap and read every object's slots from the attach thread
    /// with the mutators running, through a second writer.
    fn heap_dump(&self, path: &str) -> Result<u64, String> {
        // gci i5/hd: plain, and truncating an existing file as before (the
        // command's options reach `heap_dump_with`).
        crate::runtime::hprof::dump_heap_for_dcmd(self, path, false, true)
    }

    // --- JFR, reached from `jcmd <pid> JFR.*` -------------------------------
    //
    // 2026-09-01. See the trait's own note in `runtime/serviceability.rs` for
    // why these are trait methods and not an `Arc<SharedVm>` on the processor.
    //
    // What is deliberately NOT accepted by `JFR.start`, and why it is refused
    // rather than ignored: `filename=`, `dumponexit=`, `duration=`, `maxage=`,
    // `settings=` and `disk=`.
    //
    //  * `filename=` / `dumponexit=` would need somewhere to remember the path
    //    for a recording this processor started. The only slot that exists,
    //    `SharedVm::debug.jfr_dump_on_exit`, belongs to the
    //    `-XX:StartFlightRecording` boot path, and a jcmd command silently
    //    retargeting the boot recording's exit dump is a way to lose the file
    //    an operator's launch script was counting on. Pass `filename=` to
    //    `JFR.dump` / `JFR.stop` instead, where it is used immediately.
    //  * `duration=` / `maxage=` need the HotSpot duration grammar (`30s`,
    //    `5m`, `1h`), whose only parser lives in `vm-cli`'s option parser and
    //    is private to it. A second, subtly different copy of a units parser
    //    is worse than a clear refusal.
    //  * `settings=` names a `.jfc` profile; see `config::apply_jfr_event_setting`
    //    for why a `.jfc` name is refused rather than approximated.
    fn jfr_start(&self, args: &[String]) -> Result<String, String> {
        if !self.config.use_synthetic_jdk {
            let shared = crate::native::jni::vm_for_identity(self.vm_identity)
                .ok_or_else(|| "JFR: VM is unavailable".to_string())?;
            return crate::runtime::instrument::attach_listener_jfr_dcmd(shared, "DCmdStart", args);
        }
        let mut explicit_name: Option<String> = None;
        let mut max_events: Option<usize> = None;
        let mut maxsize_label: Option<String> = None;
        let mut maxage_dur: Option<std::time::Duration> = None;
        let mut duration_dur: Option<std::time::Duration> = None;
        let mut dump_on_exit = false;
        let mut filename: Option<String> = None;
        let mut enabled_events: Option<Vec<String>> = None;

        let parse_dur = |s: &str| -> Option<std::time::Duration> {
            let s = s.trim();
            if let Some(v) = s.strip_suffix("ns") {
                v.parse::<u64>().ok().map(std::time::Duration::from_nanos)
            } else if let Some(v) = s.strip_suffix("us") {
                v.parse::<u64>().ok().map(std::time::Duration::from_micros)
            } else if let Some(v) = s.strip_suffix("ms") {
                v.parse::<u64>().ok().map(std::time::Duration::from_millis)
            } else if let Some(v) = s.strip_suffix('s') {
                v.parse::<u64>().ok().map(std::time::Duration::from_secs)
            } else if let Some(v) = s.strip_suffix('m') {
                v.parse::<u64>()
                    .ok()
                    .map(|m| std::time::Duration::from_secs(m.saturating_mul(60)))
            } else if let Some(v) = s.strip_suffix('h') {
                v.parse::<u64>()
                    .ok()
                    .map(|h| std::time::Duration::from_secs(h.saturating_mul(3600)))
            } else if let Some(v) = s.strip_suffix('d') {
                v.parse::<u64>()
                    .ok()
                    .map(|d| std::time::Duration::from_secs(d.saturating_mul(86400)))
            } else {
                s.parse::<u64>().ok().map(std::time::Duration::from_secs)
            }
        };

        for token in args {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let Some((key, value)) = token.split_once('=') else {
                return Err(format!(
                    "JFR.start: option `{token}` is missing `=value` (expected \
                     `key=value` or `+<Event>#enabled=true`)"
                ));
            };
            if key.starts_with('+') {
                crate::config::apply_jfr_event_setting(&mut enabled_events, key, value)?;
                continue;
            }
            match key {
                "name" => explicit_name = Some(value.to_string()),
                "maxevents" => {
                    max_events =
                        Some(value.parse::<usize>().map_err(|_| {
                            format!("JFR.start: maxevents=`{value}` is not a number")
                        })?);
                }
                "maxsize" => {
                    maxsize_label = Some(value.to_string());
                }
                "maxage" => {
                    maxage_dur = parse_dur(value);
                }
                "duration" => {
                    duration_dur = parse_dur(value);
                }
                "dumponexit" => {
                    dump_on_exit = value.eq_ignore_ascii_case("true");
                }
                "filename" => {
                    filename = Some(value.to_string());
                }
                "settings" | "disk" | "delay" | "path-to-gc-roots" => {}
                other => return Err(format!("JFR.start: unrecognized option `{other}`.")),
            }
        }

        let initial_name = explicit_name.clone().unwrap_or_else(|| "1".to_string());
        let mut settings = cratonvm_jfr::RecordingSettings::new(&initial_name);
        settings.max_size = max_events;
        settings.max_age = maxage_dur;
        settings.duration = duration_dur;
        settings.dump_on_exit = dump_on_exit;

        let mut fr = self.debug.flight_recorder.lock();
        if let Some(requested) = enabled_events {
            let unknown: Vec<String> = requested
                .iter()
                .filter(|n| fr.type_registry.find_by_name(n.as_str()).is_none())
                .cloned()
                .collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "JFR.start: this VM defines no event named: {}. Nothing was \
                     started.",
                    unknown.join(", ")
                ));
            }
            settings.enabled_event_names = Some(requested.iter().cloned().collect());
        }

        let id = fr.new_recording(settings);
        let rec_name = explicit_name.unwrap_or_else(|| id.to_string());
        if let Some(rec) = fr.get_recording_mut(id) {
            rec.settings.name = rec_name.clone();
        }
        fr.start_recording(id);
        drop(fr);

        if let Some(path) = filename {
            *self.debug.jfr_dump_on_exit.lock() = Some((id, path));
        }

        let has_limit = max_events.is_some()
            || maxsize_label.is_some()
            || maxage_dur.is_some()
            || duration_dur.is_some();
        let limit_note = if has_limit {
            ""
        } else {
            " No limit specified, using maxsize=250MB as default."
        };
        let pid = std::process::id();
        Ok(format!(
            "Started recording {id}.{limit_note}\n\n\
             Use jcmd {pid} JFR.dump name={rec_name} filename=FILEPATH to copy recording data to file.\n"
        ))
    }

    fn jfr_dump(&self, args: &[String]) -> Result<String, String> {
        if !self.config.use_synthetic_jdk {
            let shared = crate::native::jni::vm_for_identity(self.vm_identity)
                .ok_or_else(|| "JFR: VM is unavailable".to_string())?;
            return crate::runtime::instrument::attach_listener_jfr_dcmd(shared, "DCmdDump", args);
        }
        let (name, filename) = jcmd_jfr_parse_target(args, "JFR.dump")?;
        let filename = filename.ok_or_else(|| {
            "JFR.dump: filename=<path> is required -- there is nowhere else for the \
             bytes to go, and a dump command that writes no file is the defect this \
             one replaced"
                .to_string()
        })?;
        let mut fr = self.debug.flight_recorder.lock();
        let id = jcmd_jfr_running_recording(&fr, name.as_deref())
            .map_err(|e| format!("JFR.dump: {e}"))?;
        let rec_name = fr
            .get_recording(id)
            .map(|r| r.settings.name.clone())
            .unwrap_or_else(|| id.to_string());
        let bytes = fr
            .dump_recording(id, std::path::Path::new(&filename))
            .map_err(|e| format!("JFR.dump: writing `{filename}` failed: {e}"))?;
        drop(fr);
        let abs = std::fs::canonicalize(&filename)
            .map(|p| {
                let s = p.to_string_lossy().to_string();
                s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
            })
            .unwrap_or_else(|_| filename.clone());
        let size_str = if bytes >= 1024 * 1024 {
            format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
        } else if bytes >= 1024 {
            format!("{:.1} kB", bytes as f64 / 1024.0)
        } else {
            format!("{bytes} B")
        };
        Ok(format!(
            "Dumped recording \"{rec_name}\", {size_str} written to:\n\n{abs}\n"
        ))
    }

    fn jfr_stop(&self, args: &[String]) -> Result<String, String> {
        if !self.config.use_synthetic_jdk {
            let shared = crate::native::jni::vm_for_identity(self.vm_identity)
                .ok_or_else(|| "JFR: VM is unavailable".to_string())?;
            return crate::runtime::instrument::attach_listener_jfr_dcmd(shared, "DCmdStop", args);
        }
        let (name, filename) = jcmd_jfr_parse_target(args, "JFR.stop")?;
        let mut fr = self.debug.flight_recorder.lock();
        let id = jcmd_jfr_running_recording(&fr, name.as_deref())
            .map_err(|e| format!("JFR.stop: {e}"))?;
        let rec_name = fr
            .get_recording(id)
            .map(|r| r.settings.name.clone())
            .unwrap_or_else(|| id.to_string());

        let written = match filename.as_ref() {
            Some(path) => Some((
                path.clone(),
                fr.dump_recording(id, std::path::Path::new(path))
                    .map_err(|e| format!("JFR.stop: writing `{path}` failed: {e}"))?,
            )),
            None => None,
        };
        fr.stop_recording(id);
        drop(fr);

        match written {
            Some((path, bytes)) => {
                let abs = std::fs::canonicalize(&path)
                    .map(|p| {
                        let s = p.to_string_lossy().to_string();
                        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
                    })
                    .unwrap_or(path);
                let size_str = if bytes >= 1024 * 1024 {
                    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
                } else if bytes >= 1024 {
                    format!("{:.1} kB", bytes as f64 / 1024.0)
                } else {
                    format!("{bytes} B")
                };
                Ok(format!(
                    "Dumped recording \"{rec_name}\", {size_str} written to:\n\n{abs}\n\n\
                     Stopped recording \"{rec_name}\".\n"
                ))
            }
            None => Ok(format!("Stopped recording \"{rec_name}\".\n")),
        }
    }

    fn jfr_check(&self, args: &[String]) -> Result<String, String> {
        if !self.config.use_synthetic_jdk {
            let shared = crate::native::jni::vm_for_identity(self.vm_identity)
                .ok_or_else(|| "JFR: VM is unavailable".to_string())?;
            return crate::runtime::instrument::attach_listener_jfr_dcmd(shared, "DCmdCheck", args);
        }
        let mut wanted_name: Option<&str> = None;
        for token in args {
            let token = token.trim();
            if let Some((k, v)) = token.split_once('=') {
                if k == "name" {
                    wanted_name = Some(v);
                }
            }
        }
        let fr = self.debug.flight_recorder.lock();
        let running = fr.running_recording_ids();
        if running.is_empty() {
            return Ok(format!(
                "No available recordings.\n\nUse jcmd {} JFR.start to start a recording.\n",
                std::process::id()
            ));
        }
        let mut out = String::new();
        for id in running {
            let Some(rec) = fr.get_recording(id) else {
                continue;
            };
            let name = rec.settings.name.as_str();
            if let Some(w) = wanted_name {
                if name != w && id.to_string() != w {
                    continue;
                }
            }
            let mut extras = String::new();
            if let Some(dur) = rec.settings.duration {
                extras.push_str(&format!(" duration={}s", dur.as_secs()));
            }
            if let Some(age) = rec.settings.max_age {
                extras.push_str(&format!(" maxage={}s", age.as_secs()));
            }
            let state = match rec.state {
                cratonvm_jfr::RecordingState::New => "new",
                cratonvm_jfr::RecordingState::Running => "running",
                cratonvm_jfr::RecordingState::Stopped => "stopped",
                cratonvm_jfr::RecordingState::Closed => "closed",
            };
            out.push_str(&format!("Recording {id}: name={name}{extras} ({state})\n"));
        }
        if out.is_empty() {
            return Ok("No available recordings.\n".to_string());
        }
        Ok(out)
    }

    fn jfr_view(&self, args: &[String]) -> Result<String, String> {
        if self.config.use_synthetic_jdk {
            return crate::runtime::serviceability::default_jfr_view(args);
        }
        let shared = crate::native::jni::vm_for_identity(self.vm_identity)
            .ok_or_else(|| "JFR: VM is unavailable".to_string())?;
        crate::runtime::instrument::attach_listener_jfr_dcmd(shared, "DCmdView", args)
    }

    fn thread_vthread_scheduler(&self) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => crate::runtime::instrument::attach_listener_vthread_scheduler(shared),
            None => Ok(
                "java.util.concurrent.ForkJoinPool@0[Running, parallelism = 1, size = 0, active = 0, running = 0, steals = 0, tasks = 0, submissions = 0]\n"
                    .to_string(),
            ),
        }
    }

    fn thread_vthread_pollers(&self) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => crate::runtime::instrument::attach_listener_vthread_pollers(shared),
            None => Ok("Read I/O pollers:\n[0] sun.nio.ch.WEPollPoller@0 [registered = 0]\n\nWrite I/O pollers:\n[0] sun.nio.ch.WEPollPoller@0 [registered = 0]\n".to_string()),
        }
    }

    fn management_agent_start(&self, args: &[String]) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => {
                crate::runtime::instrument::attach_listener_management_agent_start(shared, args)
            }
            None => Ok(String::new()),
        }
    }

    fn management_agent_start_local(&self) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => {
                crate::runtime::instrument::attach_listener_management_agent_start_local(shared)
            }
            None => Ok(String::new()),
        }
    }

    fn management_agent_stop(&self) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => {
                crate::runtime::instrument::attach_listener_management_agent_stop(shared)
            }
            None => Ok(String::new()),
        }
    }

    fn management_agent_status(&self) -> Result<String, String> {
        match crate::native::jni::vm_for_identity(self.vm_identity) {
            Some(shared) => {
                crate::runtime::instrument::attach_listener_management_agent_status(shared)
            }
            None => Ok("Agent: disabled\n".to_string()),
        }
    }

    /// gcj j2/ob2 (`gcj-j1st-string-table-logging-and-statistics-are-not-hotspots-FIXED-20261025`
    /// item 1; j1-st X3): `jcmd VM.stringtable [-verbose]` from the live
    /// interned-String table (`HeapRealm::string_pool`) on every collector:
    /// HotSpot's statistics block from the texts ([`sv::stringtable_census`]),
    /// or under `-verbose` HotSpot's dump alone (rows on j2a,
    /// `ob2_cap_st_verbose`). `CRATONVM_GEN_JCMD_STRINGTABLE_LIVE=0`: the
    /// trait default's constant 0-entry text, as before. The read lock is
    /// held only while the texts are counted.
    fn vm_stringtable(&self, args: &[String]) -> Result<String, String> {
        use crate::runtime::serviceability as sv;
        if !sv::jcmd_stringtable_live() {
            return Ok(sv::default_vm_stringtable_text(&sv::StringTableCensus::default()));
        }
        let verbose = sv::dcmd_verbose_option(args);
        // gcj j1/st: with the ledger's uncleaned entries and grown size.
        let t0 = std::time::Instant::now();
        let census = crate::memory::string_table::hotspot_stringtable_census(self, verbose);
        if verbose {
            return Ok(sv::hotspot_stringtable_verbose_text(&census.items));
        }
        crate::runtime::unified_logging::log_unified(
            &[crate::runtime::unified_logging::LogTag::StringTablePerf],
            crate::runtime::unified_logging::LogLevel::Debug,
            &format!("GetStatistics, {} secs", sv::c_format_g(t0.elapsed().as_secs_f64())),
        );
        Ok(sv::default_vm_stringtable_text(&census))
    }

    /// This VM's interned class-file UTF-8 values are not HotSpot's
    /// process-wide SymbolTable. Refuse the command rather than reporting
    /// counters for a different table as if they were HotSpot statistics.
    fn vm_symboltable(&self, _args: &[String]) -> Result<String, String> {
        Err(
            "java.lang.IllegalArgumentException: VM.symboltable is unavailable: this VM does not maintain a HotSpot SymbolTable"
                .to_string(),
        )
    }

    /// gcj j2/ob2 rows on j2a (`ob2_cap_vmuptime`): HotSpot's `VM.uptime`
    /// shape on every collector (`CRATONVM_GEN_JCMD_HOTSPOT_TEXTS`); `-date`
    /// adds the local ISO timestamp prefix.
    fn vm_uptime_text(&self, args: &[String]) -> String {
        use crate::runtime::serviceability as sv;
        let uptime = sv::VmDiagnosticState::uptime_secs(self);
        if sv::gen_jcmd_hotspot_texts() {
            return sv::hotspot_vm_uptime_with_date(uptime, sv::dcmd_boolean_option(args, "-date"));
        }
        format!("VM uptime: {uptime:.3} seconds")
    }

    /// gcj j2/ob2 rows on j2a (`ob2_cap_vmcmd`): HotSpot's `VM.command_line`
    /// shape on every collector (`CRATONVM_GEN_JCMD_HOTSPOT_TEXTS`), from
    /// HotSpot's own `jvm_args` (`VmConfig::jvm_args`) and the
    /// `sun.java.command`, `java.class.path` and `sun.java.launcher`
    /// properties.
    fn vm_command_line_text(&self) -> String {
        use crate::runtime::serviceability as sv;
        let props = sv::VmDiagnosticState::system_properties(self);
        let prop = |key: &str| {
            props
                .iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        let command = prop("sun.java.command");
        let class_path = prop("java.class.path").unwrap_or_default();
        let launcher = prop("sun.java.launcher");
        sv::hotspot_vm_command_line_text(
            &self.config.jvm_args,
            command.as_deref(),
            &class_path,
            launcher.as_deref().unwrap_or("SUN_STANDARD"),
        )
    }

    fn vm_classes(&self, _args: &[String]) -> Result<String, String> {
        let cm = self.classes.class_manager.read();
        let store = cm.class_store();
        let mut out = format!(
            "{:<20}Size  {:<22}Flags    ClassName\n",
            "KlassAddr", "State"
        );
        for c in store.iter() {
            let addr = 0x0000_0008_0000_0000u64 + (u64::from(c.id.as_u32()) + 1) * 0x200;
            let size = 64 + c.methods.len() * 8 + c.fields.len() * 4;
            let state = if hs_is_array_class(c) {
                " "
            } else {
                "fully_initialized"
            };
            let name = crate::runtime::serviceability::external_class_name(&c.name);
            out.push_str(&format!(
                "0x{addr:016x}  {size:4}  {state:<20}  ----     {name}\n"
            ));
        }
        Ok(out)
    }

    fn vm_classloader_stats(&self) -> Result<String, String> {
        let cm = self.classes.class_manager.read();
        let tally = hs_metaspace_arenas(&cm);
        let boot_used = tally.boot[0].saturating_add(tally.boot[1]);
        let boot_chunk = hs_round_up(tally.boot[0], HS_COMMIT_GRANULE_BYTES)
            .saturating_add(hs_round_up(tally.boot[1], HS_COMMIT_GRANULE_BYTES));
        let mut out = format!(
            "{:<20}{:<20}{:<19}Classes   ChunkSz   BlockSz  Type\n",
            "ClassLoader", "Parent", "CLD*"
        );
        out.push_str(&format!(
            "0x0000000000000000  0x0000000000000000  0x0000000000000001 {:7} {:9} {:9}  <boot class loader>\n",
            tally.classes, boot_chunk, boot_used
        ));
        let total_chunk = boot_chunk
            .saturating_add(tally.others_footprint[0])
            .saturating_add(tally.others_footprint[1]);
        let total_used = boot_used
            .saturating_add(tally.others_used[0])
            .saturating_add(tally.others_used[1]);
        let loaders = 1 + tally.loaders.len();
        out.push_str(&format!(
            "Total = {loaders:<52} {:7} {:9} {:9}\n",
            tally.classes, total_chunk, total_used
        ));
        out.push_str(&format!(
            "ChunkSz: Total size of all allocated metaspace chunks\n\
             BlockSz: Total size of all allocated metaspace blocks (each chunk has several blocks)\n"
        ));
        Ok(out)
    }

    fn vm_metaspace(&self, _args: &[String]) -> Result<String, String> {
        let cm = self.classes.class_manager.read();
        let stats = metaspace_stats(&cm);
        let classes = cm.class_store().len();
        let (nc_used, nc_comm, nc_res) = stats.non_class();
        let (c_used, c_comm, c_res) = stats.class_space.unwrap_or((0, 0, 0));
        let (b_used, b_comm, b_res) = stats.triple();
        Ok(format!(
            "Total Usage - 1 loaders, {classes} classes:\n\
             \x20 Non-Class: {:.2} MB capacity, {:.2} MB committed, {:.2} MB used, {:.2} MB reserved\n\
             \x20     Class: {:.2} MB capacity, {:.2} MB committed, {:.2} MB used, {:.2} MB reserved\n\
             \x20      Both: {:.2} MB capacity, {:.2} MB committed, {:.2} MB used, {:.2} MB reserved\n\
             \n\
             Virtual space:\n\
             \x20 Non-class space:    {:.2} MB reserved, {:.2} MB committed\n\
             \x20     Class space:    {:.2} MB reserved, {:.2} MB committed\n\
             \x20            Both:    {:.2} MB reserved, {:.2} MB committed\n\
             \n\
             Settings:\n\
             \x20 MaxMetaspaceSize: unlimited\n\
             \x20 CompressedClassSpaceSize: 1.00 GB\n\
             \x20 Initial GC threshold: 21.00 MB\n\
             \x20 Current GC threshold: 21.00 MB\n\
             \x20 CDS: off\n",
            nc_comm as f64 / (1024.0 * 1024.0),
            nc_comm as f64 / (1024.0 * 1024.0),
            nc_used as f64 / (1024.0 * 1024.0),
            nc_res as f64 / (1024.0 * 1024.0),
            c_comm as f64 / (1024.0 * 1024.0),
            c_comm as f64 / (1024.0 * 1024.0),
            c_used as f64 / (1024.0 * 1024.0),
            c_res as f64 / (1024.0 * 1024.0),
            b_comm as f64 / (1024.0 * 1024.0),
            b_comm as f64 / (1024.0 * 1024.0),
            b_used as f64 / (1024.0 * 1024.0),
            b_res as f64 / (1024.0 * 1024.0),
            nc_res as f64 / (1024.0 * 1024.0),
            nc_comm as f64 / (1024.0 * 1024.0),
            c_res as f64 / (1024.0 * 1024.0),
            c_comm as f64 / (1024.0 * 1024.0),
            b_res as f64 / (1024.0 * 1024.0),
            b_comm as f64 / (1024.0 * 1024.0),
        ))
    }
}


/// gch h1/ob: the CPUs the OS has configured (HotSpot's `os::processor_count`).
#[cfg(unix)]
pub(crate) fn configured_cpu_count() -> Option<usize> {
    // SAFETY: `sysconf` takes a constant name and has no other preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    usize::try_from(n).ok().filter(|&n| n > 0)
}
#[cfg(windows)]
pub(crate) fn configured_cpu_count() -> Option<usize> {
    unsafe extern "system" {
        fn GetActiveProcessorCount(group: u16) -> u32;
    }
    // SAFETY: `ALL_PROCESSOR_GROUPS` (0xFFFF) is a valid argument; no pointers.
    let n = unsafe { GetActiveProcessorCount(0xFFFF) };
    usize::try_from(n).ok().filter(|&n| n > 0)
}
#[cfg(not(any(unix, windows)))]
pub(crate) fn configured_cpu_count() -> Option<usize> {
    None
}

/// zr z3/mt: the `Runtime Workers:` figure for `-Xlog:gc+init` -- an explicit
/// `-XX:ParallelGCThreads` as the ZGC heap applied it, `None` otherwise (and
/// always off ZGC, whose blocks print no such line).
pub(in crate::vm::vm_init) fn zgc_runtime_workers_fact(heap: &VmHeap) -> Option<usize> {
    if heap.is_zgc() { heap.parallel_worker_count() } else { None }
}

pub(in crate::vm::vm_init) fn fast_boot_jit_tuning_on(
    flag_on: bool,
    explicit: bool,
    available_cpus: Option<usize>,
) -> bool {
    flag_on && (explicit || available_cpus.map_or(true, |count| count <= 4))
}


/// HotSpot's `-XX:+MaxFDLimit`, on by default on Linux and macOS: raise the
/// process's soft `RLIMIT_NOFILE` to its hard limit (macOS: at most
/// `OPEN_MAX`, 10240, above which `setrlimit` refuses), once, as the VM starts.
///
/// A Java program is written against that limit. A shell's soft limit is
/// commonly 1024 while the hard one is far higher (1048576 on the Azure linux
/// host, where HotSpot 25's `-XX:+PrintFlagsFinal` reports `MaxFDLimit = true`),
/// and HotSpot never runs at 1024. Code that leaves descriptors to the GC --
/// Spring's `TestCompiler` never closes its `StandardJavaFileManager`, so every
/// compile keeps a `ZipFileSystem` channel per classpath jar until a Cleaner
/// runs -- stays well under HotSpot's limit and ran out of this VM's:
/// MEASURED, 1100 unclosed `Files.newByteChannel`s of one jar succeed on
/// HotSpot, and the AOT `TestCompiler` classes failed here with `Too many open
/// files` once `jar:` became the real `ZipFileSystem`. Both modes: netty's
/// `DataCompressionHttp2Test` peaks at ~2100 descriptors, and at 1024 an
/// `EMFILE` reading a class file surfaces as `NoClassDefFoundError`
/// (`docs/feature-designs/collections-interception.md`).
#[cfg(unix)]
pub(in crate::vm::vm_init) fn raise_open_file_limit_to_hard() {
    // SAFETY: `getrlimit`/`setrlimit` on a zeroed, correctly-typed `rlimit`;
    // a failure of either leaves the limit as it was, which is the fallback.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return;
        }
        let mut target = limit.rlim_max;
        if cfg!(target_os = "macos") {
            target = target.min(10240);
        }
        if limit.rlim_cur >= target {
            return;
        }
        limit.rlim_cur = target;
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
    }
}

/// Windows has no per-process descriptor limit to raise.
#[cfg(not(unix))]
pub(in crate::vm::vm_init) fn raise_open_file_limit_to_hard() {}
