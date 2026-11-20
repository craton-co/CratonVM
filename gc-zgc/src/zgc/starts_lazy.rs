// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! zr z8/st: the census half of proposal C
//! (`docs/feature-designs/profile-guided-inlining.md`):
//! a start registry that would need no per-allocation bit inside a live
//! VM-TLAB chunk.
//!
//! # What this module does (and what it does not)
//!
//! Every VM-TLAB object still gets its start bit at allocation; nothing on the
//! allocation path changes and the JIT emitters are untouched. Behind
//! `CRATONVM_ZGC_LAZY_TLAB_STARTS` (default OFF) this module adds the two
//! pieces the bit-less scheme would stand on, and measures the question that
//! decides whether to build the rest. (zt t12/rt: the census refuted the
//! scheme -- 1003-2042 walk steps per query -- and the `materialize` arm that
//! registered walked bases was retired; the census stays as a diagnostic.)
//!
//! 1. **The chunk header walk** ([`ZgcRealHeap::tlab_walk_step`]): a VM-TLAB
//!    chunk is parseable from its start, because every header carries its size
//!    and no header is ever all-zero (`GC_FLAG_HEADER`), and the chunk is
//!    zeroed at carve. Everything below a buffer's COMMITTED cursor is fully
//!    headed (`Tlab::alloc_initialized` fences before its commit; the JIT's
//!    inline arms write the header before the commit store).
//! 2. **Walk at the two points a collection needs it**: when a buffer
//!    retires (`Tlab::retire` -> [`cratonvm_gc_core::tlab::TlabTailSink::note_retired_span`],
//!    on the owner or with the owner stopped, so the span is quiescent), and
//!    when a stop-the-world pause publishes a frozen or blocked peer's tail
//!    (`set_jit_tlab_skip_regions`: the span below the published cursor). The
//!    walked span's floor then moves to that cursor, so a later walk never
//!    re-parses memory a collection may have swept and reissued. A buffer the
//!    pause neither retired nor published loses its record when the
//!    collection sweeps (zr z9/rv8, [`ZLazyTlabStarts::note_collection`]):
//!    below its unknown cursor lie corpses whose headers the sweep no longer
//!    clears (zr z8/zb), and a walk would take them for objects.
//! 3. **The oracle census**: every exact-base hit of `is_object_address`,
//!    `is_heap_addr` and `resolve_interior_for_pin` whose base lies in a live
//!    chunk's un-materialized span `[floor, hi)` is counted, split by whether
//!    the querying thread owns that chunk (`q_own`) or not (`q_peer`). One hit
//!    in [`Z_LAZY_CHECK_EVERY`] is re-answered by a read-only walk from the
//!    chunk's floor up to the address, and a disagreement with the registry is
//!    counted (`q_mismatch`), with the walk's cost (`q_steps`). Those are the
//!    proposal's own refutation rows: frequent peer hits with long walks mean
//!    the walk costs more than the bit.
//!
//! Modes: unset / `0` / `off` = off (no table, no hook, one relaxed load per
//! retire and per oracle hit); anything else (`census`, `1`, `on`) = walk and
//! count, register nothing. With the bit still set at allocation,
//! `unregistered_found` is the whole difference between the two schemes on a
//! real workload: it must read 0 apart from objects whose announce helper
//! returned 0 (a pending exception), which are dead.
//!
//! # What the bit-less scheme still needs (not built here)
//!
//! See `docs/feature-designs/zgc-architecture.md`:
//! the owner-thread resolve with a persisted floor, the peer resolve against a
//! committed cursor read from the thread registry's `Tlab` address, atomic
//! inserts on every path that may race a peer's materialization, the
//! concurrent-mark start, and only then the emitter change.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::ZgcRealHeap;
use cratonvm_gc_core::heap::ObjectHeader;

/// One oracle hit in this many is re-answered by a read-only walk.
pub const Z_LAZY_CHECK_EVERY: usize = 8;
/// The read-only walk of a census check gives up after this many steps
/// (counted as `q_capped`; the hit is then not compared).
pub const Z_LAZY_CHECK_STEP_CAP: usize = 1 << 14;
/// Live-chunk records kept at most. A record whose buffer was dropped without
/// a retire (a thread torn down) would otherwise stay; past the cap the
/// oldest is dropped and counted (`chunks_dropped`).
const Z_LAZY_MAX_CHUNKS: usize = 1 << 16;

/// `CRATONVM_ZGC_LAZY_TLAB_STARTS`. See the module doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ZLazyStartsMode {
    Off = 0,
    Census = 1,
}

impl ZLazyStartsMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Census,
            _ => Self::Off,
        }
    }

    /// The token the summary line prints.
    pub fn token(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Census => "census",
        }
    }
}

/// Parse a `CRATONVM_ZGC_LAZY_TLAB_STARTS` value.
pub fn parse_lazy_tlab_starts_mode(raw: Option<&str>) -> ZLazyStartsMode {
    let Some(raw) = raw else {
        return ZLazyStartsMode::Off;
    };
    let v = raw.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "0" | "off" | "false" | "no" => ZLazyStartsMode::Off,
        // zt t12/rt: the `materialize` arm is retired; every on word is the
        // census.
        _ => ZLazyStartsMode::Census,
    }
}

/// zr z9/rv8: `CRATONVM_ZGC_LAZY_STARTS_DROP_UNPUBLISHED` -- a collection
/// drops the record of every live buffer it did not publish (default ON,
/// `=0` kill). Read once per collection, and only with
/// `CRATONVM_ZGC_LAZY_TLAB_STARTS` on.
pub fn zgc_lazy_starts_drop_unpublished_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "false" | "off" | "no")
        }
        None => true,
    }
}

/// The mode from the environment. Read once per heap (`ZgcCounters`' build).
pub fn zgc_lazy_tlab_starts_mode() -> ZLazyStartsMode {
    let raw = None::<std::ffi::OsString>;
    let s = raw.as_ref().map(|r| r.to_string_lossy().into_owned());
    parse_lazy_tlab_starts_mode(s.as_deref())
}

/// The un-materialized span of a live VM-TLAB chunk: `[floor, hi)`, `hi` the
/// chunk end (which identifies the buffer at retire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZLiveChunk {
    pub floor: usize,
    pub hi: usize,
    /// zr z9/rv8: the current pause published this buffer's tail (its floor
    /// moved to the cursor), so the record may outlive the collection. Reset
    /// by [`ZLazyTlabStarts::note_collection`].
    pub published: bool,
}

/// One step of the chunk walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZTlabStep {
    /// A headed object at `base`; the walk continues at `next`.
    Object { base: usize, next: usize },
    /// A zero word (alignment padding, or past the last object): continue at
    /// the next word.
    Gap(usize),
    /// A header that cannot be parsed, or that runs past the bound.
    Stop,
}

/// The table and counters. Lives in `ZgcCounters` (one per heap).
pub struct ZLazyTlabStarts {
    mode: AtomicU8,
    chunks: Mutex<Vec<ZLiveChunk>>,
    pub retire_walks: AtomicUsize,
    pub stw_walks: AtomicUsize,
    pub objects_walked: AtomicUsize,
    pub already_registered: AtomicUsize,
    pub unregistered_found: AtomicUsize,
    pub walk_stops: AtomicUsize,
    pub chunks_dropped: AtomicUsize,
    /// zr z9/rv8: records a collection dropped because their buffer was
    /// neither retired nor published before it (`note_collection`).
    pub chunks_unpublished_dropped: AtomicUsize,
    pub q_own: AtomicUsize,
    pub q_peer: AtomicUsize,
    pub q_checked: AtomicUsize,
    pub q_mismatch: AtomicUsize,
    pub q_steps: AtomicUsize,
    pub q_capped: AtomicUsize,
}

/// The summary line's figures (`[GC] zgc-lazy-starts:`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZLazyStartsReport {
    pub mode: &'static str,
    pub chunks_live: usize,
    pub retire_walks: usize,
    pub stw_walks: usize,
    pub objects_walked: usize,
    pub already_registered: usize,
    pub unregistered_found: usize,
    pub walk_stops: usize,
    pub chunks_dropped: usize,
    pub chunks_unpublished_dropped: usize,
    pub q_own: usize,
    pub q_peer: usize,
    pub q_checked: usize,
    pub q_mismatch: usize,
    pub q_steps: usize,
    pub q_capped: usize,
}

impl ZLazyTlabStarts {
    pub fn new(mode: ZLazyStartsMode) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            chunks: Mutex::new(Vec::new()),
            retire_walks: AtomicUsize::new(0),
            stw_walks: AtomicUsize::new(0),
            objects_walked: AtomicUsize::new(0),
            already_registered: AtomicUsize::new(0),
            unregistered_found: AtomicUsize::new(0),
            walk_stops: AtomicUsize::new(0),
            chunks_dropped: AtomicUsize::new(0),
            chunks_unpublished_dropped: AtomicUsize::new(0),
            q_own: AtomicUsize::new(0),
            q_peer: AtomicUsize::new(0),
            q_checked: AtomicUsize::new(0),
            q_mismatch: AtomicUsize::new(0),
            q_steps: AtomicUsize::new(0),
            q_capped: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub fn mode(&self) -> ZLazyStartsMode {
        ZLazyStartsMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn on(&self) -> bool {
        self.mode.load(Ordering::Relaxed) != ZLazyStartsMode::Off as u8
    }

    /// Tests: switch the mode on a built heap.
    #[cfg(test)]
    pub fn set_mode_for_test(&self, mode: ZLazyStartsMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    /// A chunk `[lo, hi)` was carved for a VM thread's buffer. Any older
    /// record overlapping it is stale (its memory was reissued) and goes.
    pub fn note_carve(&self, lo: usize, hi: usize) {
        if hi <= lo {
            return;
        }
        let mut chunks = self.chunks.lock();
        chunks.retain(|c| c.hi <= lo || c.floor >= hi);
        if chunks.len() >= Z_LAZY_MAX_CHUNKS {
            chunks.remove(0);
            self.chunks_dropped.fetch_add(1, Ordering::Relaxed);
        }
        chunks.push(ZLiveChunk {
            floor: lo,
            hi,
            published: false,
        });
    }

    /// The record of the buffer ending at `end` whose floor lies in
    /// `[start, cursor]`, removed: the buffer is retiring.
    pub fn take_retiring(
        &self,
        start: usize,
        cursor: usize,
        end: usize,
    ) -> Option<ZLiveChunk> {
        let mut chunks = self.chunks.lock();
        let i = chunks
            .iter()
            .position(|c| c.hi == end && c.floor >= start && c.floor <= cursor)?;
        Some(chunks.swap_remove(i))
    }

    /// The span a published tail `[s, e)` leaves to walk, `[floor, s)`, with
    /// the record's floor moved to `s`. `None` when no record names the tail.
    pub fn advance_for_published(&self, s: usize, e: usize) -> Option<(usize, usize)> {
        let mut chunks = self.chunks.lock();
        let c = chunks.iter_mut().find(|c| c.hi == e && c.floor <= s)?;
        let span = (c.floor, s);
        c.floor = s;
        c.published = true;
        Some(span)
    }

    /// zr z9/rv8: a collection is about to sweep. A record whose buffer this
    /// pause did not publish (and that has not retired) names a span
    /// `[floor, cursor)` the collection may free, and since zr z8/zb the sweep
    /// leaves a dead object's header intact, so a later retire walk would parse
    /// the corpses as objects (and count them as unregistered bases). Its
    /// cursor is unknown here, so the record goes (counted); a published one
    /// keeps its record with the floor already at its cursor. `keep_all`
    /// (`CRATONVM_ZGC_LAZY_STARTS_DROP_UNPUBLISHED=0`) restores the z8
    /// behaviour for an A/B.
    pub fn note_collection(&self, keep_all: bool) {
        let mut chunks = self.chunks.lock();
        if !keep_all {
            let before = chunks.len();
            chunks.retain(|c| c.published);
            let gone = before - chunks.len();
            if gone != 0 {
                self.chunks_unpublished_dropped
                    .fetch_add(gone, Ordering::Relaxed);
            }
        }
        for c in chunks.iter_mut() {
            c.published = false;
        }
    }

    /// The live record whose un-materialized span holds `addr`.
    pub fn chunk_holding(&self, addr: usize) -> Option<ZLiveChunk> {
        self.chunks
            .lock()
            .iter()
            .copied()
            .find(|c| addr >= c.floor && addr < c.hi)
    }

    pub fn live_chunks(&self) -> usize {
        self.chunks.lock().len()
    }

    pub fn report(&self) -> ZLazyStartsReport {
        let r = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        ZLazyStartsReport {
            mode: self.mode().token(),
            chunks_live: self.live_chunks(),
            retire_walks: r(&self.retire_walks),
            stw_walks: r(&self.stw_walks),
            objects_walked: r(&self.objects_walked),
            already_registered: r(&self.already_registered),
            unregistered_found: r(&self.unregistered_found),
            walk_stops: r(&self.walk_stops),
            chunks_dropped: r(&self.chunks_dropped),
            chunks_unpublished_dropped: r(&self.chunks_unpublished_dropped),
            q_own: r(&self.q_own),
            q_peer: r(&self.q_peer),
            q_checked: r(&self.q_checked),
            q_mismatch: r(&self.q_mismatch),
            q_steps: r(&self.q_steps),
            q_capped: r(&self.q_capped),
        }
    }
}

impl ZgcRealHeap {
    /// One step of the chunk walk at `cur`, with no object allowed to end
    /// past `top`.
    ///
    /// A zero word is padding or unused space (no header is all-zero:
    /// `GC_FLAG_HEADER`). A non-zero word must be a header carrying
    /// `GC_FLAG_HEADER` that [`Self::alloc_size`] sizes, ending at or below
    /// `top`; anything else stops the walk -- a walk never guesses.
    ///
    /// # Safety
    /// `[cur, top)` lies inside one VM-TLAB chunk of this heap's arena, and
    /// every object in it below `top` is fully headed (a committed cursor, or
    /// a registered base the caller walks up to).
    pub unsafe fn tlab_walk_step(cur: usize, top: usize) -> ZTlabStep {
        if cur & 7 != 0 || cur.checked_add(8).map_or(true, |e| e > top) {
            return ZTlabStep::Stop;
        }
        // SAFETY: `cur` is 8-aligned and `[cur, cur + 8)` is inside the chunk.
        let word = unsafe { std::ptr::read_volatile(cur as *const u64) };
        if word == 0 {
            return ZTlabStep::Gap(cur + 8);
        }
        // SAFETY: a fully headed object starts here (caller's contract); the
        // chunk's memory past `top` is arena memory, mapped.
        let header = unsafe { &*(cur as *const ObjectHeader) };
        if header.gc_flags() & cratonvm_types::GC_FLAG_HEADER == 0 {
            return ZTlabStep::Stop;
        }
        let Some(size) = Self::alloc_size(header).filter(|&s| s >= 8) else {
            return ZTlabStep::Stop;
        };
        match cur.checked_add(size) {
            Some(end) if end <= top => ZTlabStep::Object {
                base: cur,
                next: (end + 7) & !7,
            },
            _ => ZTlabStep::Stop,
        }
    }

    /// Every object base in `[lo, top)` in address order, through `f`;
    /// `false` when the walk stopped on an unparseable header before `top`.
    ///
    /// # Safety
    /// As [`Self::tlab_walk_step`] for the whole span.
    pub unsafe fn walk_tlab_span(lo: usize, top: usize, mut f: impl FnMut(usize)) -> bool {
        let mut cur = lo;
        while cur < top {
            // SAFETY: forwarded from the caller.
            match unsafe { Self::tlab_walk_step(cur, top) } {
                ZTlabStep::Object { base, next } => {
                    f(base);
                    cur = next;
                }
                ZTlabStep::Gap(next) => cur = next,
                ZTlabStep::Stop => return false,
            }
        }
        true
    }

    /// Would a walk from `floor` find `addr` as an object base? Read-only;
    /// `None` when it took more than `cap` steps. Objects below a registered
    /// base of the same chunk were headed before it (one writer, program
    /// order, x86-64 TSO), so the walk is sound for a registered `addr`
    /// whatever the owner is doing above it.
    ///
    /// # Safety
    /// `[floor, addr]` lies inside one live VM-TLAB chunk of this heap, and
    /// `addr` is a registered base or the caller accepts a `false`.
    pub unsafe fn tlab_walk_finds(
        floor: usize,
        addr: usize,
        cap: usize,
    ) -> Option<(bool, usize)> {
        let mut cur = floor;
        let mut steps = 0usize;
        loop {
            if cur == addr {
                return Some((true, steps));
            }
            if cur > addr {
                return Some((false, steps));
            }
            if steps >= cap {
                return None;
            }
            steps += 1;
            // SAFETY: forwarded; every object below `addr` ends at or below it.
            match unsafe { Self::tlab_walk_step(cur, addr) } {
                ZTlabStep::Object { next, .. } => cur = next,
                ZTlabStep::Gap(next) => cur = next,
                ZTlabStep::Stop => return Some((false, steps)),
            }
        }
    }

    /// Walk `[lo, top)` of a quiescent chunk: count every base, and the ones
    /// the registry lacks. Registers nothing (zt t12/rt retired the
    /// `materialize` arm). Returns the bases walked.
    pub fn lazy_materialize_span(&self, lo: usize, top: usize) -> usize {
        let lz = &self.counters.lazy_starts;
        if !lz.on() || top <= lo || lo < self.arena_base || top > self.arena_end {
            return 0;
        }
        let mut walked = 0usize;
        let mut known = 0usize;
        let mut missing = 0usize;
        // SAFETY: `[lo, top)` is the consumed span of a VM-TLAB chunk this heap
        // carved (a record of the table), below a committed cursor, and its
        // owner is retiring it or stopped.
        let complete = unsafe {
            Self::walk_tlab_span(lo, top, |base| {
                walked += 1;
                if self.registry.contains(base) {
                    known += 1;
                } else {
                    missing += 1;
                }
            })
        };
        lz.objects_walked.fetch_add(walked, Ordering::Relaxed);
        lz.already_registered.fetch_add(known, Ordering::Relaxed);
        lz.unregistered_found.fetch_add(missing, Ordering::Relaxed);
        if !complete {
            lz.walk_stops.fetch_add(1, Ordering::Relaxed);
        }
        walked
    }

    /// A VM buffer `[start, end)` retires with its cursor at `cursor`
    /// (`TlabTailSink::note_retired_span`).
    pub fn lazy_note_retired_span(&self, start: usize, cursor: usize, end: usize) {
        let lz = &self.counters.lazy_starts;
        if !lz.on() || end <= start || start < self.arena_base || end > self.arena_end {
            return;
        }
        let Some(chunk) = lz.take_retiring(start, cursor, end) else {
            return;
        };
        lz.retire_walks.fetch_add(1, Ordering::Relaxed);
        self.lazy_materialize_span(chunk.floor, cursor);
    }

    /// A stop-the-world pause publishes the reserved tails `regions` of
    /// frozen or blocked peers: walk each one's span below its cursor.
    pub fn lazy_note_published_tails(&self, regions: &[(usize, usize)]) {
        let lz = &self.counters.lazy_starts;
        if !lz.on() {
            return;
        }
        for &(s, e) in regions {
            if let Some((lo, top)) = lz.advance_for_published(s, e) {
                lz.stw_walks.fetch_add(1, Ordering::Relaxed);
                self.lazy_materialize_span(lo, top);
            }
        }
    }

    /// zr z9/rv8: called by `collect_garbage` before its sweep (after the VM
    /// published this pause's tails); see [`ZLazyTlabStarts::note_collection`].
    /// One relaxed load when the switch is off.
    pub fn lazy_note_collection(&self) {
        let lz = &self.counters.lazy_starts;
        if !lz.on() {
            return;
        }
        lz.note_collection(!zgc_lazy_starts_drop_unpublished_enabled());
    }

    /// Ask `Tlab::retire` for consumed spans, when the switch is on (called
    /// once by `new_shared_with_initial`). Lives here so the collector file's
    /// submodule-adoption census does not count a reference to the crate's
    /// `tlab` module as one to `zgc::tlab`.
    pub fn lazy_register_retired_span_listener(&self) {
        if self.counters.lazy_starts.on() {
            cratonvm_gc_core::tlab::register_retired_span_listener();
        }
    }

    /// The oracle census: `base` was just answered as a registered object.
    #[inline]
    pub fn lazy_note_query(&self, base: usize) {
        if self.counters.lazy_starts.on() {
            self.lazy_note_query_slow(base);
        }
    }

    #[inline(never)]
    fn lazy_note_query_slow(&self, base: usize) {
        let lz = &self.counters.lazy_starts;
        let Some(chunk) = lz.chunk_holding(base) else {
            return;
        };
        let own = super::vm_tlab::owned_vm_tlab_chunk_end_at(base) == Some(chunk.hi);
        let n = if own {
            lz.q_own.fetch_add(1, Ordering::Relaxed)
        } else {
            lz.q_peer.fetch_add(1, Ordering::Relaxed)
        };
        if n % Z_LAZY_CHECK_EVERY != 0 {
            return;
        }
        // SAFETY: `base` is a registered base inside the live chunk's
        // un-materialized span `[floor, hi)`.
        match unsafe { Self::tlab_walk_finds(chunk.floor, base, Z_LAZY_CHECK_STEP_CAP) } {
            Some((found, steps)) => {
                lz.q_checked.fetch_add(1, Ordering::Relaxed);
                lz.q_steps.fetch_add(steps, Ordering::Relaxed);
                if !found {
                    lz.q_mismatch.fetch_add(1, Ordering::Relaxed);
                }
            }
            None => {
                lz.q_capped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// `CRATONVM_ZGC_LAZY_TLAB_STARTS`'s figures, for `[GC] zgc-lazy-starts:`.
    pub fn lazy_tlab_starts_report(&self) -> ZLazyStartsReport {
        self.counters.lazy_starts.report()
    }
}

#[cfg(test)]
mod zr8st_lazy_starts_tests;
