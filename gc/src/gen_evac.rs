// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Parallel evacuation with promotion buffers for the MOVING young collection
//! (gen-gc-five, 2026-09-02).
//!
//! # What this replaces
//!
//! `GenerationalHeap::collect_garbage_inner`'s transitive closure was a single
//! thread alternating a linear Cheney scan over to-space with a drain of the
//! promoted-object worklist, forwarding every reference through
//! `forward_object`. After `gen-gc-minor-pause-20260902` removed the two
//! whole-space walks from the pause, that copy became its largest phase, and
//! two of its costs had nothing to do with copying:
//!
//! * it ran on ONE core, while the mark side of the non-moving sweep
//!   (`young_mark::drain_parallel`) had been parallel since 2026-07-25;
//! * every promotion took `OldGen::alloc` on its own — a walk up the size
//!   buckets, a best-fit scan inside one, a re-push of the split remainder,
//!   an invalidation of the sorted free-list cache, and a `memset` of the
//!   block that the copy then overwrote byte for byte.
//!
//! # Shape
//!
//! `threads` workers (from `young_mark::young_gc_threads`, so
//! `CRATONVM_GC_PAR_THREADS` and `CRATONVM_GC_PAR_MIN_BYTES` size this exactly
//! as they size the sweep) share one stack of "objects whose reference slots
//! still need scanning". Each worker owns:
//!
//! * a to-space CHUNK it bump-allocates young survivors into, carved from
//!   `young_to` under a lock [`TO_CHUNK_BYTES`] at a time and shrunk as the
//!   arena fills so the tails of `threads` chunks can never exhaust a
//!   to-space sized exactly for its survivors;
//! * an old-gen PROMOTION BUFFER (a PLAB), carved from `OldGen` UNZEROED —
//!   the copy overwrites every byte — and doubling from [`PLAB_MIN`] to
//!   [`PLAB_MAX`] as the worker keeps promoting, so a worker that promotes
//!   little wastes little;
//! * its own `(from, to)` pairs for the pointer map, its own deferred
//!   old->young card list and its own counters. Nothing is shared per object
//!   except the from-space headers.
//!
//! Forwarding is claimed by a compare-and-swap on the source's mark word:
//! copy first, then CAS `NEUTRAL / LOCKED / INFLATED -> FORWARDED(dest)`, which
//! is the ordering `ObjectHeader::make_forwarded` documents (the destination
//! must carry the intact lock word before the source is clobbered). A worker
//! that loses the race reads the winner's destination out of the mark word
//! its CAS returned and gives its own copy back: by retreating its buffer
//! cursor when the copy was that buffer's last allocation, else by stamping a
//! dead filler over it so every linear walker still parses the buffer end to
//! end.
//!
//! # What it leaves behind, so the rest of the cycle is unchanged
//!
//! * `young_to` is object-covered from offset 0 to `used()`: each chunk's
//!   unused tail becomes the same filler `int[]` the TLAB retire path writes
//!   (or the 8-byte GAP sentinel when there is no room for a header), and each
//!   chunk start is recorded as an allocator anchor.
//! * old gen holds no partially-written region: every PLAB's unused tail goes
//!   back through `OldGen::release_unused_tail` BEFORE [`evacuate`] returns,
//!   so a `walk_objects` later in the same pause sees objects and free blocks
//!   only. A PLAB never ends 8 bytes short of its end (see [`Lab::alloc`]),
//!   because an 8-byte span is below the free list's minimum block.
//! * The pointer map, the deferred card list and the copy tallies are merged
//!   by the caller, and the sequential phases after the drain (finalizer
//!   resurrection, verification, the major GC) run against the same state the
//!   sequential drain would have produced, plus the fillers.
//!
//! `CRATONVM_GC_PAR_EVAC=0` keeps the previous sequential engine byte for
//! byte; `CRATONVM_GC_PAR_THREADS=1` runs THIS engine on one worker, which
//! separates "the engine" from "the parallelism" in an A/B.

use std::sync::atomic::Ordering;
use std::sync::{Condvar, Mutex};

use crate::arena::Arena;
use crate::gen_heap::{
    forward_ref_slots, fwd_resolve_strict, fwdguard_enabled, gen_object_total_size,
    validate_copy_source_cells, PROMOTION_AGE,
};
use crate::heap::{
    array_element_type_from_tag, object_kind_from_tag, ArrayElementType, ObjectHeader, ObjectKind,
    GC_FLAG_OLD_GEN, HEADER_SIZE,
};
use crate::old_gen::OldGen;
use crate::tlab::{GAP_FILLER_CLASS_ID, TLAB_FILLER_CLASS_ID};
use crate::young_mark::ObjectStartBits;
use cratonvm_types::{element_type_tag_at, kind_tag_at};

/// A worker's to-space chunk. Sized so a worker refills a few hundred times
/// per gigabyte of survivors rather than per object, and small enough that
/// eight idle tails are noise against any young generation this engine runs
/// on (`CRATONVM_GC_PAR_MIN_BYTES`, 16 MiB by default).
pub(crate) const TO_CHUNK_BYTES: usize = 256 * 1024;
/// A worker's first promotion buffer.
pub(crate) const PLAB_MIN: usize = 16 * 1024;
/// The promotion buffer's ceiling; each refill doubles up to this.
pub(crate) const PLAB_MAX: usize = 256 * 1024;
/// An object at least this large bypasses both buffers and takes the shared
/// allocator directly, so one large array cannot strand most of a buffer.
const DIRECT_MIN: usize = 32 * 1024;

/// Objects a worker takes from the shared stack per acquisition.
const ACQUIRE_CHUNK: usize = 128;
/// Local stack depth at which a worker publishes surplus work.
const SPILL_HIGH: usize = 1024;
/// Depth a worker keeps for itself when spilling.
const SPILL_KEEP: usize = 256;

/// A bump buffer a worker owns outright: `[start, end)` was carved from a
/// shared allocator, `[start, cursor)` holds objects.
#[derive(Clone, Copy, Default)]
struct Lab {
    start: usize,
    cursor: usize,
    end: usize,
}

impl Lab {
    /// Bump `size` bytes (8-aligned by the caller). With `no_eight_tail` the
    /// buffer refuses an allocation that would leave exactly 8 bytes, because
    /// an 8-byte remainder can neither be returned to the old-gen free list
    /// (minimum block is `HEADER_SIZE`) nor carry an `int[]` filler; refusing
    /// retires the buffer with a tail of at least 24 bytes instead.
    #[inline]
    fn alloc(&mut self, size: usize, no_eight_tail: bool) -> Option<usize> {
        let after = self.cursor.checked_add(size)?;
        if after > self.end {
            return None;
        }
        if no_eight_tail && self.end - after == 8 {
            return None;
        }
        let p = self.cursor;
        self.cursor = after;
        Some(p)
    }

    #[inline]
    fn owns(&self, addr: usize) -> bool {
        addr >= self.start && addr < self.end
    }
}

struct WorkState {
    stack: Vec<usize>,
    idle: usize,
    done: bool,
}

/// Everything the workers share for one drain. Built by the collector with
/// the arena guards it already holds; the `&mut` borrows end when this is
/// dropped, before the sequential phases resume.
pub(crate) struct EvacShared<'a> {
    young_from: &'a Arena,
    starts: &'a ObjectStartBits,
    to_alloc: Mutex<&'a mut Arena>,
    old_alloc: Mutex<&'a mut OldGen>,
    to_base: usize,
    to_end: usize,
    old_base: usize,
    old_end: usize,
    force_promote_all: bool,
    loader_pin_on: bool,
    threads: usize,
    work: Mutex<WorkState>,
    cv: Condvar,
}

impl<'a> EvacShared<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        young_from: &'a Arena,
        starts: &'a ObjectStartBits,
        young_to: &'a mut Arena,
        old_gen: &'a mut OldGen,
        force_promote_all: bool,
        loader_pin_on: bool,
        threads: usize,
        seed: Vec<usize>,
    ) -> Self {
        let to_base = young_to.base_ptr() as usize;
        let to_end = to_base + young_to.capacity();
        let old_base = old_gen.base_ptr() as usize;
        let old_end = old_base + old_gen.capacity();
        Self {
            young_from,
            starts,
            to_alloc: Mutex::new(young_to),
            old_alloc: Mutex::new(old_gen),
            to_base,
            to_end,
            old_base,
            old_end,
            force_promote_all,
            loader_pin_on,
            threads: threads.max(1),
            work: Mutex::new(WorkState {
                stack: seed,
                idle: 0,
                done: false,
            }),
            cv: Condvar::new(),
        }
    }

    #[inline]
    fn in_to(&self, a: usize) -> bool {
        a >= self.to_base && a < self.to_end
    }

    #[inline]
    fn in_old(&self, a: usize) -> bool {
        a >= self.old_base && a < self.old_end
    }
}

/// One worker's contribution, merged by the collector after the drain.
#[derive(Default)]
pub(crate) struct EvacOutcome {
    /// `(from, to)` for every object this worker forwarded OR re-encountered
    /// — the same `or_insert` semantics `forward_object` gave the map.
    pub map: Vec<(usize, usize)>,
    /// Old-gen objects this worker scanned that still reference young.
    pub deferred_cards: Vec<usize>,
    /// Objects this worker copied (the `objects_copied` term).
    pub objects_copied: usize,
    /// The `COPY_TALLY` layout: promoted bytes/count, young bytes/count,
    /// re-encounters, copies.
    pub tally: [u64; 6],
    /// Bytes of to-space chunk tails stamped as fillers.
    pub to_filler_bytes: usize,
    /// To-space chunks carved.
    pub to_chunks: usize,
    /// Promotion buffers carved.
    pub plabs: usize,
    /// Copies this worker made and then discarded because another worker
    /// forwarded the same object first.
    pub lost_races: usize,
}

struct Worker<'s, 'a> {
    sh: &'s EvacShared<'a>,
    to_lab: Lab,
    old_lab: Lab,
    next_plab: usize,
    local: Vec<usize>,
    out: EvacOutcome,
}

impl<'s, 'a> Worker<'s, 'a> {
    fn new(sh: &'s EvacShared<'a>) -> Self {
        Self {
            sh,
            to_lab: Lab::default(),
            old_lab: Lab::default(),
            next_plab: PLAB_MIN,
            local: Vec::with_capacity(SPILL_HIGH),
            out: EvacOutcome::default(),
        }
    }

    // ----- allocation ---------------------------------------------------

    fn alloc_to(&mut self, size: usize) -> Option<usize> {
        if size >= DIRECT_MIN {
            let mut g = self.sh.to_alloc.lock().unwrap_or_else(|e| e.into_inner());
            let to: &mut Arena = &mut **g;
            let p = to.alloc(size, 8)? as usize;
            to.note_object_start(p - to.base_ptr() as usize);
            return Some(p);
        }
        if let Some(p) = self.to_lab.alloc(size, false) {
            return Some(p);
        }
        self.retire_to_lab();
        let mut g = self.sh.to_alloc.lock().unwrap_or_else(|e| e.into_inner());
        let to: &mut Arena = &mut **g;
        // Shrink the chunk as the arena fills. The Cheney invariant sizes
        // to-space for the survivors exactly, not for the survivors plus
        // `threads` idle tails, so the tails have to become small before the
        // space does.
        let headroom = to.low_bump_headroom();
        let chunk = TO_CHUNK_BYTES
            .min(headroom / (4 * self.sh.threads))
            .max(size)
            & !7;
        let p = match to.alloc(chunk, 8) {
            Some(p) => p as usize,
            None => {
                // No chunk-sized span: take exactly this object, or nothing.
                let p = to.alloc(size, 8)? as usize;
                to.note_object_start(p - to.base_ptr() as usize);
                return Some(p);
            }
        };
        // A chunk start is an object start — the first copy lands there, or
        // the tail filler does — and after the swap this arena is the next
        // cycle's from-space, whose parallel object-start walk is split at
        // exactly these anchors.
        to.note_object_start(p - to.base_ptr() as usize);
        drop(g);
        self.out.to_chunks += 1;
        self.to_lab = Lab {
            start: p,
            cursor: p,
            end: p + chunk,
        };
        self.to_lab.alloc(size, false)
    }

    fn retire_to_lab(&mut self) {
        if self.to_lab.cursor < self.to_lab.end {
            let bytes = self.to_lab.end - self.to_lab.cursor;
            // SAFETY: `[cursor, end)` is this worker's own unused chunk tail
            // inside the mapped to-space arena.
            unsafe { write_dead_filler(self.to_lab.cursor, bytes, false) };
            self.out.to_filler_bytes += bytes;
        }
        self.to_lab = Lab::default();
    }

    fn alloc_old(&mut self, size: usize) -> Option<usize> {
        if size >= DIRECT_MIN {
            let mut g = self.sh.old_alloc.lock().unwrap_or_else(|e| e.into_inner());
            return g.alloc_unzeroed(size, 8).map(|p| p as usize);
        }
        if let Some(p) = self.old_lab.alloc(size, true) {
            return Some(p);
        }
        self.retire_old_lab();
        let mut plab = self.next_plab.max(size);
        if plab - size == 8 {
            // The first allocation must not leave the 8-byte tail `Lab::alloc`
            // refuses, or a fresh buffer would be handed back at once.
            plab += 8;
        }
        let mut g = self.sh.old_alloc.lock().unwrap_or_else(|e| e.into_inner());
        let og: &mut OldGen = &mut **g;
        match og.alloc_unzeroed(plab, 8) {
            Some(p) => {
                drop(g);
                let p = p as usize;
                self.out.plabs += 1;
                self.next_plab = (self.next_plab * 2).min(PLAB_MAX);
                self.old_lab = Lab {
                    start: p,
                    cursor: p,
                    end: p + plab,
                };
                self.old_lab.alloc(size, true)
            }
            // Old gen cannot spare a buffer: try the object on its own, and
            // let the caller fall back to to-space if even that fails.
            None => og.alloc_unzeroed(size, 8).map(|p| p as usize),
        }
    }

    fn retire_old_lab(&mut self) {
        if self.old_lab.cursor < self.old_lab.end {
            let tail = self.old_lab.end - self.old_lab.cursor;
            debug_assert!(
                tail >= HEADER_SIZE && tail % 8 == 0,
                "a promotion buffer tail must be a free-list-sized block (tail={tail})"
            );
            let mut g = self.sh.old_alloc.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: `[cursor, end)` is the never-written tail of a block this
            // worker carved with `alloc_unzeroed`; nothing has been placed in it.
            unsafe { g.release_unused_tail(self.old_lab.cursor as *mut u8, tail) };
        }
        self.old_lab = Lab::default();
    }

    /// Give back a copy that lost the forwarding race.
    fn undo(&mut self, dest: usize, size: usize, landed_old: bool) {
        self.out.lost_races += 1;
        let lab = if landed_old {
            &mut self.old_lab
        } else {
            &mut self.to_lab
        };
        if lab.owns(dest) && lab.cursor == dest + size {
            lab.cursor = dest;
            return;
        }
        // Not the buffer's last allocation (or a direct allocation): leave a
        // dead, parseable object behind. Its size is the copied object's own,
        // so every walker strides it exactly.
        // SAFETY: `[dest, dest + size)` is memory this worker allocated and
        // wrote; no other object overlaps it.
        unsafe { write_dead_filler(dest, size, landed_old) };
    }

    // ----- forwarding ---------------------------------------------------

    /// Forward one from-space candidate. Returns `(address, fresh)`: the
    /// object's post-collection address and whether THIS call copied it (and
    /// so must scan it). A candidate that is not an object start, or fails
    /// the same header screens `forward_object` applies, comes back unchanged
    /// with `fresh == false`.
    fn forward(&mut self, old_ptr: *mut u8) -> (usize, bool) {
        let old = old_ptr as usize;
        if !self.sh.starts.contains(old) {
            // Exact pre-GC membership rejects aligned interior words from
            // conservative roots before anything is written through them.
            return (old, false);
        }
        // Validate the raw tag bytes before reading either as a typed enum
        // (HIB-DCAST-LATEPHASE.1).
        // SAFETY: `old` is a recorded object start in the mapped from-space.
        let kind_tag = unsafe { kind_tag_at(old_ptr) };
        let elem_tag = unsafe { element_type_tag_at(old_ptr) };
        if object_kind_from_tag(kind_tag).is_none() || array_element_type_from_tag(elem_tag).is_none()
        {
            return (old, false);
        }
        let src = old_ptr as *const ObjectHeader;
        // SAFETY: as above; the mark word is read atomically.
        let mut mark = unsafe { (*src).mark_word.load(Ordering::Acquire) };
        loop {
            if ObjectHeader::is_forwarded_mark(mark) {
                let fwd = ObjectHeader::forwarding_target(mark) as usize;
                // BUG-Z: a forwarding address must land in to-space or old gen.
                if fwd == 0 || fwd % 8 != 0 || !(self.sh.in_to(fwd) || self.sh.in_old(fwd)) {
                    return (old, false);
                }
                self.out.tally[4] += 1;
                self.out.map.push((old, fwd));
                return (fwd, false);
            }

            // An owned header snapshot: every scalar through a field-projected
            // read, the mark word from the atomic load above.
            // SAFETY: `src` is a live from-space object header.
            let header: ObjectHeader = unsafe {
                let mut owned = ObjectHeader::new(
                    std::ptr::addr_of!((*src).class_id).read(),
                    (*src).kind(),
                    (*src).element_type(),
                    0,
                    0,
                );
                owned.shape = std::ptr::addr_of!((*src).shape).read();
                owned.mark_word.store(mark, Ordering::Relaxed);
                owned
            };
            let kind_byte = ObjectHeader::kind_tag(mark);
            let is_array = header.kind() == ObjectKind::Array;
            let array_length_too_large = is_array && header.array_length() > i32::MAX as u32;
            let num_slots_too_large = !is_array && header.num_slots() > (1 << 24);
            if kind_byte > 1 || num_slots_too_large || array_length_too_large {
                return (old, false);
            }
            let total_size = gen_object_total_size(&header);
            let from_base = self.sh.young_from.base_ptr() as usize;
            let from_end = from_base + self.sh.young_from.capacity();
            let fits = old >= from_base
                && old
                    .checked_add(total_size)
                    .is_some_and(|end| end <= from_end);
            if total_size < HEADER_SIZE || !fits {
                return (old, false);
            }
            if (fwdguard_enabled() || fwd_resolve_strict())
                && crate::gc::resolve_class_info(header.class_id.as_u32()).is_none()
                && fwd_resolve_strict()
            {
                return (old, false);
            }

            let should_promote =
                self.sh.force_promote_all || header.gc_age() + 1 >= PROMOTION_AGE;
            let dest = if should_promote {
                self.alloc_old(total_size)
                    .or_else(|| self.alloc_to(total_size))
            } else {
                self.alloc_to(total_size)
                    .or_else(|| self.alloc_old(total_size))
            };
            let Some(dest) = dest else {
                // The Cheney invariant makes this unreachable; see the
                // identical abort in `forward_object_impl` for why a copying
                // collector cannot degrade here.
                eprintln!(
                    "FATAL: GC could not relocate a live object — young to-space and old gen \
                     are both full during parallel evacuation (tried {total_size} bytes). \
                     This is an invariant violation: the to-space >= from-space guarantee \
                     in collect_garbage_inner should have made this unreachable."
                );
                std::process::abort();
            };

            // Gated forensics, no-ops unless armed.
            // SAFETY: `old_ptr` is the live source object under STW.
            validate_copy_source_cells(old_ptr, unsafe { &*src }, "cheney-src");
            {
                let w = crate::heap::cell_watch_addr();
                if w != 0 && dest <= w && w.wrapping_sub(dest) < total_size {
                    let src_at = old + (w - dest);
                    // SAFETY: the source spans `total_size` bytes; `src_at` is inside it.
                    let pair = unsafe { std::ptr::read(src_at as *const [u64; 2]) };
                    crate::heap::cell_watch_check(
                        dest,
                        total_size,
                        "cheney-copy",
                        &format!("src=0x{old:x} src[watch]=0x{:016x},0x{:016x}", pair[0], pair[1]),
                    );
                }
            }

            // SAFETY: `[old, old + total_size)` is the source object and
            // `[dest, dest + total_size)` is memory this worker just allocated;
            // the two cannot overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(old_ptr, dest as *mut u8, total_size);
            }
            // SAFETY: `dest` now holds a complete header; replicate the mark
            // word through the atomic so the copy is well-defined.
            let dh = unsafe { &*(dest as *const ObjectHeader) };
            dh.mark_word.store(mark, Ordering::Relaxed);
            let landed_old = self.sh.in_old(dest);
            if landed_old {
                dh.add_gc_flags(GC_FLAG_OLD_GEN);
            } else {
                dh.set_gc_age(dh.gc_age().saturating_add(1));
            }

            // Claim the source. `make_forwarded` keeps the quartet (kind, age,
            // flags) and replaces the lock/hash payload with the destination,
            // which the copy already carries intact.
            let forwarded = ObjectHeader::make_forwarded(mark, dest);
            // SAFETY: `src` is the live source header; the CAS is the only
            // write any worker performs on it.
            match unsafe {
                (*src)
                    .mark_word
                    .compare_exchange(mark, forwarded, Ordering::AcqRel, Ordering::Acquire)
            } {
                Ok(_) => {
                    let slot = if landed_old { 0 } else { 2 };
                    self.out.tally[slot] += total_size as u64;
                    self.out.tally[slot + 1] += 1;
                    self.out.tally[5] += 1;
                    self.out.objects_copied += 1;
                    self.out.map.push((old, dest));
                    return (dest, true);
                }
                Err(cur) => {
                    self.undo(dest, total_size, landed_old);
                    // Another worker got there first (the loop's forwarded arm
                    // resolves it), or — impossible under STW, but handled —
                    // the word changed some other way: retry against it.
                    mark = cur;
                }
            }
        }
    }

    // ----- scanning -----------------------------------------------------

    fn scan(&mut self, addr: usize) {
        // SAFETY: `addr` is a copied or promoted object this drain produced,
        // or one the sequential root phase produced; its header is complete.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        let in_old = self.sh.in_old(addr);
        let cid = header.class_id.as_u32();
        let mut needs_card = false;
        // SAFETY: `addr`/`header` are a valid object; `forward_ref_slots`
        // bounds every slot it visits by the header.
        unsafe {
            forward_ref_slots(addr as *mut u8, header, |ref_ptr| {
                if self.sh.young_from.contains(ref_ptr) {
                    let (n, fresh) = self.forward(ref_ptr);
                    if fresh {
                        self.local.push(n);
                    }
                    // A forwarded ref that stayed young is an old->young edge:
                    // the card is re-marked after `clear_all` by the caller.
                    if in_old && !self.sh.in_old(n) {
                        needs_card = true;
                    }
                    Some(n as *mut u8)
                } else {
                    None
                }
            });
        }
        // HIB-CV-24: a live instance keeps its defining ClassLoader alive.
        if self.sh.loader_pin_on {
            if let Some(loader_old) = cratonvm_types::loader_pin::loader_pin_addr(cid) {
                let lp = loader_old as *mut u8;
                if self.sh.young_from.contains(lp) {
                    let (n, fresh) = self.forward(lp);
                    if fresh {
                        self.local.push(n);
                    }
                }
            }
        }
        if needs_card {
            self.out.deferred_cards.push(addr);
        }
    }

    fn run(&mut self) {
        loop {
            {
                let mut g = self.sh.work.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if g.done {
                        return;
                    }
                    let len = g.stack.len();
                    if len > 0 {
                        let take = len.min(ACQUIRE_CHUNK);
                        self.local.extend(g.stack.drain(len - take..));
                        break;
                    }
                    g.idle += 1;
                    if g.idle == self.sh.threads {
                        // Every worker is out of work and the shared stack is
                        // empty: the closure is complete.
                        g.done = true;
                        self.sh.cv.notify_all();
                        return;
                    }
                    g = self.sh.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    g.idle -= 1;
                }
            }
            while let Some(addr) = self.local.pop() {
                self.scan(addr);
                if self.local.len() >= SPILL_HIGH {
                    let surplus = self.local.len() - SPILL_KEEP;
                    let mut g = self.sh.work.lock().unwrap_or_else(|e| e.into_inner());
                    g.stack.extend(self.local.drain(..surplus));
                    drop(g);
                    self.sh.cv.notify_all();
                }
            }
        }
    }

    fn finish(mut self) -> EvacOutcome {
        self.retire_to_lab();
        self.retire_old_lab();
        std::mem::take(&mut self.out)
    }
}

/// Stamp a dead, walkable object over `[addr, addr + bytes)`.
///
/// Below `HEADER_SIZE` this is the 8-byte GAP sentinel (`GAP_FILLER_CLASS_ID`
/// at +0, the span length at +4) every young walker already recognises; at
/// or above it, the `int[]` filler `Tlab::install_tail_filler` writes, sized
/// so `HEADER_SIZE + array_data_size(len, Int) == bytes`. The body is not
/// zeroed: a walker strides the filler by its header and never reads it, and
/// a conservative candidate landing inside it is rejected by the object-start
/// bitmap, never by the bytes.
///
/// # Safety
/// `[addr, addr + bytes)` must be mapped, 8-aligned, a multiple of 8 long,
/// and owned by the caller. Old-gen spans must be at least `HEADER_SIZE`.
unsafe fn write_dead_filler(addr: usize, bytes: usize, old_gen: bool) {
    debug_assert!(addr % 8 == 0 && bytes >= 8 && bytes % 8 == 0);
    if bytes < HEADER_SIZE {
        debug_assert!(!old_gen, "an old-gen filler below HEADER_SIZE is not walkable");
        std::ptr::write(addr as *mut u32, GAP_FILLER_CLASS_ID.as_u32());
        std::ptr::write((addr + 4) as *mut u32, bytes as u32); // Cast: bytes < 16
        return;
    }
    let data_bytes = bytes - HEADER_SIZE;
    let header = ObjectHeader::new(
        TLAB_FILLER_CLASS_ID,
        ObjectKind::Array,
        ArrayElementType::Int,
        (data_bytes / 4) as u32, // Cast: a filler never exceeds one buffer
        0,
    );
    if old_gen {
        header.add_gc_flags(GC_FLAG_OLD_GEN);
    }
    std::ptr::write(addr as *mut ObjectHeader, header);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_promotion_buffer_never_leaves_an_eight_byte_tail() {
        let mut lab = Lab {
            start: 0,
            cursor: 0,
            end: 64,
        };
        assert_eq!(lab.alloc(40, true), Some(0), "a 24-byte tail is a free block");
        assert_eq!(lab.alloc(16, true), None, "an 8-byte tail is not");
        assert_eq!(lab.alloc(24, true), Some(40), "an exact fit is");
        assert_eq!(lab.alloc(8, true), None);
        // The to-space rule is the plain one: an 8-byte tail becomes a GAP filler.
        let mut to = Lab {
            start: 0,
            cursor: 0,
            end: 64,
        };
        assert_eq!(to.alloc(56, false), Some(0));
        assert!(to.owns(8) && !to.owns(64));
    }

    #[test]
    fn a_dead_filler_is_sized_exactly_and_a_gap_is_a_sentinel() {
        let mut buf = vec![0u64; 32];
        let addr = buf.as_mut_ptr() as usize;
        // SAFETY: `buf` is 8-aligned owned memory of 256 bytes.
        unsafe { write_dead_filler(addr, 128, false) };
        let h = unsafe { &*(addr as *const ObjectHeader) };
        assert_eq!(h.kind(), ObjectKind::Array);
        assert_eq!(gen_object_total_size(h), 128);
        assert_eq!(h.gc_flags() & GC_FLAG_OLD_GEN, 0);
        unsafe { write_dead_filler(addr + 128, 16, true) };
        let h2 = unsafe { &*((addr + 128) as *const ObjectHeader) };
        assert_eq!(gen_object_total_size(h2), 16);
        assert_ne!(h2.gc_flags() & GC_FLAG_OLD_GEN, 0);
        unsafe { write_dead_filler(addr + 144, 8, false) };
        // SAFETY: the two u32 reads are inside `buf`.
        let (cid, len) = unsafe {
            (
                std::ptr::read((addr + 144) as *const u32),
                std::ptr::read((addr + 148) as *const u32),
            )
        };
        assert_eq!(cid, GAP_FILLER_CLASS_ID.as_u32());
        assert_eq!(len, 8);
    }
}

/// Run the drain to completion on `shared.threads` workers (the caller's
/// thread is worker 0) and hand back every worker's outcome.
pub(crate) fn evacuate(shared: &EvacShared<'_>) -> Vec<EvacOutcome> {
    let run_one = || {
        let mut w = Worker::new(shared);
        w.run();
        w.finish()
    };
    if shared.threads <= 1 {
        return vec![run_one()];
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = (1..shared.threads).map(|_| s.spawn(&run_one)).collect();
        let mut outs = Vec::with_capacity(shared.threads);
        outs.push(run_one());
        for h in handles {
            match h.join() {
                Ok(o) => outs.push(o),
                Err(e) => std::panic::resume_unwind(e),
            }
        }
        outs
    })
}
