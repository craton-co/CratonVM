/// One audited source file: its workspace-relative path, how many
/// address-keyed declarations it contains, and the GC disposition each of
/// its tables states in its own source.
pub(super) struct AuditedFile {
    pub path: &'static str,
    /// Lines declaring `HashMap<ObjectRef` / `HashSet<ObjectRef`, comments
    /// excluded. A table is usually two (the accessor signature and the
    /// `static` behind it), so this is a count of declarations, not of
    /// tables. It is here to make ADDING a table to an
    /// already-audited file fail too, not just adding a new file.
    pub declarations: usize,
    pub disposition: &'static str,
}

pub(super) const AUDITED: &[AuditedFile] = &[
    AuditedFile {
        path: "gc-core/src/heap.rs",
        declarations: 1,
        disposition: "gpu_pinned_refs: PINNED — membership is exactly what \
                      forbids relocation, so no key in it can go stale.",
    },
    AuditedFile {
        path: "vm/src/vm/realms/class_realm.rs",
        declarations: 1,
        disposition: "class_mirrors_reverse: REMAPPED + SWEPT — the moved \
                      rows are re-keyed by update_all_roots step 14 \
                      (gc::rekey_reverse_mirrors, full rebuild on drift) \
                      and dead mirrors' rows are pruned with the forward \
                      map by gc::reconcile_class_mirrors, which runs on \
                      non-moving cycles too (gc::prune_reverse_mirrors).",
    },
    AuditedFile {
        path: "vm/src/memory/gc.rs",
        declarations: 1,
        disposition: "NO TABLE OF ITS OWN (gc-common w1-b, 2026-09-23). \
                      One declaration is rekey_reverse_mirrors' `&mut` \
                      parameter: it borrows class_realm.rs's \
                      class_mirrors_reverse, whose REMAPPED + SWEPT \
                      disposition above covers it.",
    },
    AuditedFile {
        path: "vm/src/runtime/offload.rs",
        declarations: 4,
        disposition: "input_cache: REMAPPED + SWEPT — \
                      offload::input_cache::remap_and_sweep, which routes \
                      through this module and is driven from \
                      vm/src/memory/gc.rs. Still ONE table: the fourth \
                      declaration (2026-09-05) is drain_locked's `&mut` \
                      parameter: the drain takes the caller's guard, so \
                      that the DIRTY read-clear and the eviction it \
                      authorises happen under a single hold of the cache \
                      mutex. It borrows \
                      the same map rather than introducing another, so \
                      the remap+sweep disposition above covers it \
                      unchanged.",
    },
    AuditedFile {
        path: "native-builtins/src/net_phase_e/archive_stamp.rs",
        declarations: 7,
        disposition: "inet_addr_side_table, ds_side_table and \
                      ds_peer_table: WEAK — REMAPPED + SWEPT per VM \
                      (gc-common w17-a). Keys are not roots \
                      (gc_scan_inet_addr_roots / gc_scan_ds_roots push \
                      nothing); gc_sweep_net_socket_rows \
                      (sweep_object_keyed_rows) drops a dead key's rows at \
                      all three weak-row sites, a dropped-open socket's UDP \
                      fd is queued for closing, and gc_update_inet_addr_refs \
                      / gc_update_ds_refs re-key the survivors. Until w17-a \
                      all three were SCANNED + REMAPPED. (The previous \
                      entry named ds_side_table as covered by the \
                      inet_addr pair; it was not — that pair walks only \
                      inet_addr_side_table, and ds_side_table had no GC \
                      disposition at all until ds_peer_table's arrival \
                      made the count wrong and surfaced it.) The re10 \
                      handler roots live in this file too but are counted \
                      under their own pair. Seven declarations, six tables: the 
                      seventh is gc_update_ds_refs own rekey helper, which 
                      takes a HashMap<ObjectRef, V> parameter and so is 
                      counted by a matcher that reads declarations, not 
                      tables.",
    },
    AuditedFile {
        path: "native-builtins/src/locale_bootstrap.rs",
        declarations: 3,
        disposition: "synthetic_locale_data: WEAK — SWEPT + REMAPPED per VM \
                      (gc-common w11-d). Rows carry their VM (w10-a); keys \
                      are not roots (the cached defaults are rooted through \
                      CACHED_LOCALE), gc_sweep_locale_rows(vm) drops dead \
                      Locales' rows after every STW collection \
                      (process_references_after_gc) and gc_update_locale_refs \
                      re-keys the survivors. The third declaration is \
                      rekey_vm_rows, the per-VM rekey helper both locale \
                      tables use: a parameter, not a table.",
    },
    AuditedFile {
        path: "native-builtins/src/lib_parts/arrays_range_or_throw.rs",
        declarations: 2,
        disposition: "locale_data: WEAK — SWEPT + REMAPPED per VM, with the \
                      bootstrap table (gc-common w11-d: \
                      gc_sweep_locale_rows / gc_update_locale_refs; the keys \
                      are no longer roots). Rows carry their VM (w10-a).",
    },
    // gc-common w10-d: `wildfly_core.rs` left the census. `EQE_PENDING` is
    // now a per-VM FIFO of Runnables (no address key), scanned and
    // remapped by the `wildfly-side-tables` root row.
];
