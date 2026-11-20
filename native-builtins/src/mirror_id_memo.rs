// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread, per-VM memo of `class mirror -> ClassId`, the answer every
//! `java.lang.Class` native starts from.
//!
//! JIT round 17 wave 8, lane nio3 (page
//! `docs/feature-designs/native-dispatch-architecture.md`).
//!
//! `NativeContext::class_id_from_mirror` (the VM's `vm_object::class_id_from_mirror`)
//! reads `class_mirrors_reverse`, an `RwLock<FxHashMap<ObjectRef, ClassId>>`:
//! a shared reader count, a hash and a release on every `Class.isArray()`,
//! `isInterface()`, `isPrimitive()`, `getName()`. The map is keyed by the
//! mirror's ADDRESS, so a memo has to know when an address stops meaning the
//! same mirror.
//!
//! # Why the GC pause generation is the right key
//!
//! The reverse map changes in two ways only (`vm/src` writers, read
//! 2026-10-05):
//!
//! * a NEW row when a mirror is created (`vm_object.rs`
//!   `get_or_create_class_mirror`): it adds an address that had no row, and
//!   this memo never remembers a miss, so no entry can disagree with it;
//! * a re-key or a removal inside a collection (`memory/gc.rs`: the moved-mirror
//!   re-key, the unloaded-mirror sweep). Both run in a stop-the-world pause,
//!   and `GcBarrier::gc_generation` is bumped once at the end of every pause.
//!
//! An address is reused for a different object only after the object that
//! held it was collected, which is again a pause. So an entry filled under
//! generation `g` is exact for as long as the generation is still `g`. The
//! caller loads the generation BEFORE the slow lookup, so a pause that
//! overlaps the fill leaves the entry under an already stale generation.
//!
//! # Keys and switch
//!
//! An entry is `(vm_identity, generation, mirror address) -> class id`.
//! `vm_identity` is never reissued, and identity 0 (mocks, test contexts) is
//! never memoised. `CRATONVM_CLASS_ID_FROM_MIRROR_MEMO` (opt-in perf, default
//! OFF) is read once per thread; no static holds it.

use std::cell::Cell;

use cratonvm_types::ClassId;

/// Table size. A power of two; the slot is a hash of the address. A collision
/// evicts, which costs one locked lookup, never a wrong answer.
const WAYS: usize = 64;

#[derive(Clone, Copy)]
struct Entry {
    vm: usize,
    generation: u64,
    mirror: usize,
    class_id: u32,
}

impl Entry {
    /// `vm == 0` is never stored, so this entry never matches.
    const EMPTY: Entry = Entry {
        vm: 0,
        generation: 0,
        mirror: 0,
        class_id: 0,
    };
}

thread_local! {
    static MEMO: [Cell<Entry>; WAYS] = const { [const { Cell::new(Entry::EMPTY) }; WAYS] };
    /// 0 = not read yet on this thread, 1 = on, 2 = off.
    static MODE: Cell<u8> = const { Cell::new(0) };
}

/// Is the memo on (`CRATONVM_CLASS_ID_FROM_MIRROR_MEMO`, default OFF)?
pub fn enabled() -> bool {
    MODE.with(|mode| match mode.get() {
        1 => true,
        2 => false,
        _ => {
            let on = false;
            mode.set(if on { 1 } else { 2 });
            on
        }
    })
}

#[inline]
fn slot_of(mirror: usize) -> usize {
    // Objects are at least 8-byte aligned; fold the bits above the alignment.
    let a = mirror >> 3;
    (a ^ (a >> 6) ^ (a >> 12)) & (WAYS - 1)
}

/// Answer `mirror`'s class id in VM `vm` at pause generation `generation`
/// from the table, or compute it with `lookup` and remember a `Some`.
///
/// The caller must read `generation` BEFORE it could run `lookup` (see the
/// module doc). A `None` (a primitive mirror, a non-mirror object) is never
/// remembered.
pub fn lookup_or_fill(
    vm: usize,
    generation: u64,
    mirror: usize,
    lookup: impl FnOnce() -> Option<ClassId>,
) -> Option<ClassId> {
    if vm == 0 || mirror == 0 {
        return lookup();
    }
    let slot = slot_of(mirror);
    let hit = MEMO.with(|memo| {
        let e = memo[slot].get();
        (e.vm == vm && e.generation == generation && e.mirror == mirror).then_some(e.class_id)
    });
    if let Some(id) = hit {
        return Some(ClassId::new(id));
    }
    let id = lookup()?;
    MEMO.with(|memo| {
        memo[slot].set(Entry {
            vm,
            generation,
            mirror,
            class_id: id.as_u32(),
        })
    });
    Some(id)
}

// ---------------------------------------------------------------------------
// The per-VM, lock-free table (JIT round 18 wave 10, lane buffers)
// ---------------------------------------------------------------------------

/// `CRATONVM_CLASS_ID_FROM_MIRROR_TABLE` (opt-in perf, default OFF): answer
/// `NativeContext::class_id_from_mirror` from the VM's [`MirrorIdTable`]
/// instead of the `class_mirrors_reverse` read lock. Read once per VM, when
/// the table is built. Takes precedence over the per-thread memo above when
/// both are on.
pub fn table_enabled_from_env() -> bool {
    true
}

/// Entries in a VM's table: a power of two, direct-mapped. 1024 x 32 bytes =
/// 32 KiB per VM, allocated only when the switch is on.
const TABLE_WAYS: usize = 1024;

/// One direct-mapped cell, guarded by its own sequence counter (a per-cell
/// seqlock): odd while a writer is inside, bumped twice per write, so a
/// reader can never accept a mix of two writes (no ABA: the counter only
/// grows). Every field is an atomic, so a racing read is a stale value, never
/// undefined behaviour.
#[derive(Default)]
struct TableCell {
    seq: std::sync::atomic::AtomicU32,
    class_id: std::sync::atomic::AtomicU32,
    mirror: std::sync::atomic::AtomicUsize,
    generation: std::sync::atomic::AtomicU64,
}

/// A per-VM, lock-free `class mirror address -> ClassId` table, shared by
/// every thread of the VM (the memo above is per thread).
///
/// The soundness argument is the memo's (module doc): an entry filled under
/// GC pause generation `g` is exact while the generation is still `g`, and
/// the caller loads the generation BEFORE the slow lookup it fills from.
/// What this adds over the memo: one fill serves every thread, 1024 cells
/// instead of 64, and no thread-local access on the hit path. A hit costs the
/// generation load, the cell's two sequence loads and three relaxed loads; no
/// read-modify-write, so readers on different threads do not bounce a
/// reader-count cache line as the `RwLock` does.
///
/// Fills are serialised by a mutex (misses only: a new mirror, a new
/// generation, a collision), so two writers never interleave on one cell.
pub struct MirrorIdTable {
    cells: Box<[TableCell]>,
    fill: parking_lot::Mutex<()>,
}

impl MirrorIdTable {
    /// A table, or an empty (always-missing, never-allocating) one when the
    /// switch is off.
    pub fn new(enabled: bool) -> Self {
        let cells: Box<[TableCell]> = if enabled {
            (0..TABLE_WAYS).map(|_| TableCell::default()).collect()
        } else {
            Box::new([])
        };
        Self {
            cells,
            fill: parking_lot::Mutex::new(()),
        }
    }

    /// Built for this VM with the switch on?
    #[inline]
    pub fn is_enabled(&self) -> bool {
        !self.cells.is_empty()
    }

    #[inline]
    fn cell_of(&self, mirror: usize) -> &TableCell {
        let a = mirror >> 3;
        &self.cells[(a ^ (a >> 10) ^ (a >> 20)) & (TABLE_WAYS - 1)]
    }

    /// The table's answer for `mirror` at `generation`, without a lock.
    /// `None` is a miss (never "not a mirror": misses are not stored).
    #[inline]
    pub fn get(&self, generation: u64, mirror: usize) -> Option<ClassId> {
        use std::sync::atomic::{fence, Ordering};
        if self.cells.is_empty() || mirror == 0 {
            return None;
        }
        let cell = self.cell_of(mirror);
        let s1 = cell.seq.load(Ordering::Acquire);
        if s1 & 1 != 0 {
            return None;
        }
        let m = cell.mirror.load(Ordering::Relaxed);
        let g = cell.generation.load(Ordering::Relaxed);
        let id = cell.class_id.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        let s2 = cell.seq.load(Ordering::Relaxed);
        (s1 == s2 && m == mirror && g == generation).then(|| ClassId::new(id))
    }

    fn put(&self, generation: u64, mirror: usize, id: ClassId) {
        use std::sync::atomic::{fence, Ordering};
        let _writer = self.fill.lock();
        let cell = self.cell_of(mirror);
        let s = cell.seq.load(Ordering::Relaxed);
        cell.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        cell.mirror.store(mirror, Ordering::Relaxed);
        cell.generation.store(generation, Ordering::Relaxed);
        cell.class_id.store(id.as_u32(), Ordering::Relaxed);
        cell.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Answer `mirror`'s class id at pause generation `generation` from the
    /// table, or compute it with `lookup` (the locked reverse-map read) and
    /// remember a `Some`. The caller must load `generation` BEFORE `lookup`
    /// could run. A disabled table always runs `lookup`.
    pub fn lookup_or_fill(
        &self,
        generation: u64,
        mirror: usize,
        lookup: impl FnOnce() -> Option<ClassId>,
    ) -> Option<ClassId> {
        if !self.is_enabled() || mirror == 0 {
            return lookup();
        }
        if let Some(id) = self.get(generation, mirror) {
            return Some(id);
        }
        let id = lookup()?;
        self.put(generation, mirror, id);
        Some(id)
    }
}

impl Default for MirrorIdTable {
    /// The disabled table.
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests;
