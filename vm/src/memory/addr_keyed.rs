// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Post-collection fixup for **address-keyed side-tables**.
//!
//! # The hazard
//!
//! `ObjectRef`'s `Hash`/`Eq` are address-based, so a
//! `HashMap<ObjectRef, _>` silently breaks across a moving collection.
//! Two distinct failures follow, and they need opposite remedies:
//!
//! * **A survivor that moved** leaves its entry stranded under the old
//!   address, and every later lookup misses. Harmless in isolation — the
//!   table merely stops working — so this half is a *performance* bug.
//! * **An entry whose object died** is the dangerous half. The collector
//!   hands the reclaimed address back to the allocator, so a later object
//!   can land on it and collide with the stale entry. The table then
//!   answers a lookup for the *new* object with the *old* object's value:
//!   a silent wrong answer, not a miss.
//!
//! Clearing the whole table on every collection closes both and is
//! sometimes the right trade, but it discards every live entry too.
//! [`remap_and_sweep`] keeps the live ones.
//!
//! # Why this is not an `external_roots` / `native_roots` provider
//!
//! Both registries pair a `scan` half (keep the referent alive) with a
//! `remap` half (keep the address valid), and both are driven from
//! `gc::update_all_roots` **after** its `pointer_map.is_empty()` early
//! return. That is the wrong shape for a *cache*:
//!
//! * A cache must not root its keys. An entry whose object is otherwise
//!   unreachable can never be looked up again — nothing is left to name
//!   it — so rooting it would convert the table into an immortality set
//!   and leak every object it ever saw.
//! * The sweep half has to run on a **non-moving** collection too, where
//!   `pointer_map` is empty and objects still die. A remap callback
//!   registered in either registry never runs on that path.
//!
//! So this runs before the early return, alongside
//! [`crate::memory::smuggled_longs::remap_and_sweep`], which sweeps its
//! own registry for exactly the same address-reuse reason.
//!
//! # Ordering within a cycle
//!
//! Callers pass the collector's old→new `pointer_map` and a liveness
//! predicate over **pre-remap** addresses. Both are evaluated against the
//! state the collector publishes at the end of the cycle, while the world
//! is still stopped, so no mutator can allocate onto a just-reclaimed
//! address before the sweep observes it as dead.

use crate::types::ObjectRef;
use rustc_hash::FxHashMap;
use std::collections::HashMap;

/// What one [`remap_and_sweep`] pass did, for tests and diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepStats {
    /// Entries whose key appeared in the pointer map and were re-keyed.
    pub moved: usize,
    /// Entries whose object survived without moving; key left alone.
    pub retained: usize,
    /// Entries whose object did not survive; dropped.
    pub dropped: usize,
}

/// Re-key `table` through the collector's relocation map and drop entries
/// whose object did not survive.
///
/// `is_live` answers "is this **pre-remap** address still a live object?"
/// — `VmHeap::is_object_address(addr).is_some()` is the usual
/// implementation. It is only consulted for keys absent from
/// `pointer_map`, since a key present there has already been proven to be
/// a relocated survivor.
///
/// Values are moved, never cloned, so `V` needs no bounds.
///
/// # Destination collisions
///
/// A key that moved is authoritative: it is inserted first, and an
/// unmoved entry is only kept if nothing already claimed its address.
/// This matters when object `A` is evacuated onto the address that dead
/// object `B` used to occupy. `is_live(B_addr)` is then true — the
/// address *is* live, but it belongs to `A` now — so a single-pass
/// implementation would keep `B`'s stale entry and let insertion order
/// decide which value survives. Today's collectors evacuate into regions
/// disjoint from the ones they free, so the collision is not reachable;
/// the two-pass order means correctness does not rest on that staying
/// true.
pub fn remap_and_sweep<V>(
    table: &mut FxHashMap<ObjectRef, V>,
    pointer_map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
) -> SweepStats {
    if table.is_empty() {
        return SweepStats::default();
    }

    let mut stats = SweepStats::default();
    let mut moved: Vec<(usize, V)> = Vec::new();
    let mut stayed: Vec<(usize, V)> = Vec::new();

    for (obj, value) in table.drain() {
        let addr = obj.as_ptr() as usize;
        match pointer_map.get(&addr) {
            Some(&new_addr) => moved.push((new_addr, value)),
            None if is_live(addr) => stayed.push((addr, value)),
            // Neither relocated nor still live: the object is gone. Drop
            // the entry so a future object allocated onto the reclaimed
            // address cannot collide with it.
            None => stats.dropped += 1,
        }
    }

    for (addr, value) in moved {
        table.insert(object_ref_at(addr), value);
        stats.moved += 1;
    }
    for (addr, value) in stayed {
        let key = object_ref_at(addr);
        if table.contains_key(&key) {
            // A relocated survivor already claimed this address, so this
            // entry's object is dead after all — see "Destination
            // collisions" above.
            stats.dropped += 1;
            continue;
        }
        table.insert(key, value);
        stats.retained += 1;
    }

    stats
}

/// Rebuild an `ObjectRef` from an address the collector just published.
///
/// # Panics
///
/// Debug builds assert the address is non-null and 8-byte aligned, the
/// same precondition `ObjectRef::from_raw` documents.
#[inline]
fn object_ref_at(addr: usize) -> ObjectRef {
    debug_assert!(addr != 0 && addr % 8 == 0, "bad object address {addr:#x}");
    // SAFETY: `addr` comes from the collector's own pointer map or from a
    // key it just confirmed live, so it denotes a real object. The ref is
    // stored as a key and is not dereferenced here.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// The workspace census of address-keyed `ObjectRef` tables.
///
/// `docs/threading/objectref-concurrency-contract.md` §7.3 records the gap this
/// closes. `ObjectRef`'s `Hash`/`Eq` are addresses, so every
/// `HashMap<ObjectRef, _>` in the tree needs a GC disposition — and "**Nothing
/// enumerates the tables that need it.**" A table added without one breaks
/// nothing at review time; it just starts answering a lookup for a new object
/// with a dead object's value once the allocator reuses the address.
///
/// This is that enumeration, and
/// `the_address_keyed_table_census_is_complete` enforces it.
#[cfg(test)]
mod census {
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
            path: "gc/src/heap.rs",
            declarations: 1,
            disposition: "gpu_pinned_refs: PINNED — membership is exactly what \
                          forbids relocation, so no key in it can go stale.",
        },
        AuditedFile {
            path: "vm/src/vm/realms/class_realm.rs",
            declarations: 1,
            disposition: "class_mirrors_reverse: REMAPPED — rebuilt inside the \
                          collection by vm/src/memory/gc.rs, ahead of \
                          update_all_roots' own mirror step.",
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
            path: "native-builtins/src/net_phase_e.rs",
            declarations: 7,
            disposition: "inet_addr_side_table: SCANNED + REMAPPED — \
                          gc_scan_inet_addr_roots / gc_update_inet_addr_refs. \
                          ds_side_table and ds_peer_table: SCANNED + REMAPPED — \
                          gc_scan_ds_roots / gc_update_ds_refs. (The previous \
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
            declarations: 2,
            disposition: "synthetic_locale_data: SCANNED + REMAPPED — \
                          gc_scan_locale_roots and its remap companion.",
        },
        AuditedFile {
            path: "native-builtins/src/lib.rs",
            declarations: 2,
            disposition: "locale_data: SCANNED + REMAPPED — covered by \
                          gc_scan_locale_roots alongside the bootstrap table.",
        },
        AuditedFile {
            path: "native-builtins/src/classloader.rs",
            declarations: 2,
            disposition: "class_data_store: TOLERATED UNDER A STATED CONDITION \
                          — effectively write-only (get_class_data has no \
                          production caller). The source names the \
                          re-key-by-identity-hash migration required before a \
                          reader may be added.",
        },
        AuditedFile {
            path: "native-builtins/src/wildfly_core.rs",
            declarations: 2,
            disposition: "EQE_PENDING: TOLERATED UNDER A STATED CONDITION — the \
                          queued Runnables are rooted by identity hash; a stale \
                          KEY only splits one EQE's tasks across two buckets, \
                          and drain_all_pending_runnables drains every bucket \
                          unconditionally, so no task is lost or misdispatched.",
        },
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk every workspace source tree and require each address-keyed
    /// `ObjectRef` declaration to be accounted for in [`census::AUDITED`].
    ///
    /// Scans DIRECTORIES rather than a list of file names on purpose: a module
    /// split renames files, and a gate keyed on file names goes quietly
    /// fail-open at exactly the moment the code it guards was reorganised. For
    /// the same reason it fails loudly — rather than passing vacuously — when
    /// it cannot find the workspace root, when the walk turns up implausibly
    /// few files, or when the pattern matches nothing at all.
    #[test]
    fn the_address_keyed_table_census_is_complete() {
        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    collect_rs(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }

        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the vm crate directory must have a parent")
            .to_path_buf();
        assert!(
            workspace.join("Cargo.toml").is_file(),
            "workspace root not found at {} — failing rather than scanning nothing",
            workspace.display()
        );

        let mut files = Vec::new();
        for entry in std::fs::read_dir(&workspace).expect("workspace root is readable") {
            let src = entry.expect("readable directory entry").path().join("src");
            if src.is_dir() {
                collect_rs(&src, &mut files);
            }
        }
        assert!(
            files.len() > 100,
            "only {} source files found under {} — the directory walk is broken",
            files.len(),
            workspace.display()
        );

        // Declarations, not mentions: the pattern inside a doc comment or a
        // prose note is not a table. Everything from the first `//` is prose.
        let declarations_in = |text: &str| {
            text.lines()
                .filter(|line| {
                    let code = line.split("//").next().unwrap_or("");
                    code.contains("HashMap<ObjectRef") || code.contains("HashSet<ObjectRef")
                })
                .count()
        };

        let mut findings: Vec<String> = Vec::new();
        let mut total_declarations = 0usize;
        let mut seen: Vec<&str> = Vec::new();
        for file in &files {
            let rel = file
                .strip_prefix(&workspace)
                .unwrap_or(file)
                .to_string_lossy()
                .replace('\\', "/");
            // This module is the fixup machinery and `value.rs` is where the
            // contract itself is argued; neither owns a table.
            if rel.ends_with("vm/src/memory/addr_keyed.rs") || rel.ends_with("types/src/value.rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let count = declarations_in(&text);
            if count == 0 {
                continue;
            }
            total_declarations += count;
            match super::census::AUDITED
                .iter()
                .find(|a| rel.ends_with(a.path))
            {
                None => findings.push(format!(
                    "{rel}: {count} address-keyed declaration(s), NOT in the census"
                )),
                Some(audited) => {
                    seen.push(audited.path);
                    if audited.declarations != count {
                        findings.push(format!(
                            "{rel}: census says {} declaration(s), found {count} — a \
                             table was added or removed here, so re-audit it and \
                             update the entry",
                            audited.declarations
                        ));
                    }
                }
            }
        }
        for audited in super::census::AUDITED {
            if !seen.contains(&audited.path) {
                findings.push(format!(
                    "{}: in the census but no longer declares an address-keyed \
                     table — drop the entry (or fix the path if the file moved)",
                    audited.path
                ));
            }
        }

        assert!(
            total_declarations > 0,
            "the census matched no declarations at all — the pattern stopped \
             matching, so this test was about to pass vacuously"
        );
        assert!(
            findings.is_empty(),
            "address-keyed `ObjectRef` table census is out of date.\n\
             `ObjectRef`'s Hash/Eq are addresses: a moving collection strands \
             live entries, and a DEAD entry collides with whatever object the \
             allocator next places on that address — a silent wrong answer, not \
             a miss (docs/threading/objectref-concurrency-contract.md §7.3).\n\
             Give the table a scan+remap pair (see \
             `net_phase_e::gc_scan_inet_addr_roots`), route it through \
             `addr_keyed::remap_and_sweep`, or state why it is safe — then \
             record it in `census::AUDITED`.\n  {}",
            findings.join("\n  ")
        );
    }

    const A: usize = 0x1_0000;
    const B: usize = 0x2_0000;
    const C: usize = 0x3_0000;

    fn key(addr: usize) -> ObjectRef {
        object_ref_at(addr)
    }

    fn table(entries: &[(usize, u32)]) -> FxHashMap<ObjectRef, u32> {
        entries.iter().map(|&(a, v)| (key(a), v)).collect()
    }

    fn nothing_moved() -> cratonvm_types::PointerMap {
        cratonvm_types::PointerMap::default()
    }

    #[test]
    fn moved_entry_is_rekeyed_and_keeps_its_value() {
        let mut t = table(&[(A, 7)]);
        let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

        let stats = remap_and_sweep(&mut t, &map, &|_| true);

        assert_eq!(stats.moved, 1);
        assert_eq!(t.get(&key(B)), Some(&7), "value must follow the object");
        assert!(!t.contains_key(&key(A)), "old address must not linger");
    }

    #[test]
    fn live_unmoved_entry_survives_a_nonmoving_sweep() {
        let mut t = table(&[(A, 7)]);

        let stats = remap_and_sweep(&mut t, &nothing_moved(), &|addr| addr == A);

        assert_eq!(
            stats,
            SweepStats {
                moved: 0,
                retained: 1,
                dropped: 0
            }
        );
        assert_eq!(t.get(&key(A)), Some(&7));
    }

    /// The correctness-critical case: a non-moving collection publishes an
    /// empty pointer map, so a remap-only fixup would be a no-op and the
    /// dead entry would stay to collide with whatever is allocated onto
    /// its reclaimed address next.
    #[test]
    fn dead_entry_is_dropped_even_when_nothing_moved() {
        let mut t = table(&[(A, 7)]);

        let stats = remap_and_sweep(&mut t, &nothing_moved(), &|_| false);

        assert_eq!(stats.dropped, 1);
        assert!(t.is_empty(), "reclaimed address must not stay keyed");
    }

    #[test]
    fn mixed_cycle_sorts_each_entry_into_the_right_bucket() {
        let mut t = table(&[(A, 1), (B, 2), (C, 3)]);
        // A relocated to 0x40000; B survived in place; C died.
        let map = cratonvm_types::PointerMap::from_iter([(A, 0x4_0000)]);

        let stats = remap_and_sweep(&mut t, &map, &|addr| addr == B);

        assert_eq!(
            stats,
            SweepStats {
                moved: 1,
                retained: 1,
                dropped: 1
            }
        );
        assert_eq!(t.get(&key(0x4_0000)), Some(&1));
        assert_eq!(t.get(&key(B)), Some(&2));
        assert_eq!(t.len(), 2);
    }

    /// A relocated survivor evacuated onto a dead entry's old address must
    /// win, whatever order the two entries come out of the table in.
    #[test]
    fn relocated_survivor_wins_a_destination_collision() {
        let mut t = table(&[(A, 1), (B, 2)]);
        // A moves onto B's address; B is dead, but `is_live(B)` now reports
        // true because A occupies that address.
        let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

        let stats = remap_and_sweep(&mut t, &map, &|_| true);

        assert_eq!(t.len(), 1);
        assert_eq!(t.get(&key(B)), Some(&1), "A's value, not B's stale one");
        assert_eq!(
            stats,
            SweepStats {
                moved: 1,
                retained: 0,
                dropped: 1
            }
        );
    }

    #[test]
    fn empty_table_is_a_no_op() {
        let mut t: FxHashMap<ObjectRef, u32> = FxHashMap::default();

        let stats = remap_and_sweep(
            &mut t,
            &cratonvm_types::PointerMap::from_iter([(A, B)]),
            &|_| true,
        );

        assert_eq!(stats, SweepStats::default());
        assert!(t.is_empty());
    }
}
