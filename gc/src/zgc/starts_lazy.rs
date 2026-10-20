// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zr z8/st: the census and materialize half of proposal C
//! (`docs/internal/proposals/gc/zgc-zr7ca-proposal-ir-inline-alloc-fast-path-trims-20261013.md`):
//! a start registry that would need no per-allocation bit inside a live
//! VM-TLAB chunk.
//!
//! # What this module does (and what it does not)
//!
//! Every VM-TLAB object still gets its start bit at allocation; nothing on the
//! allocation path changes and the JIT emitters are untouched. Behind
//! `CRATONVM_ZGC_LAZY_TLAB_STARTS` (default OFF) this module adds the two
//! pieces the bit-less scheme would stand on, and measures the question that
//! decides whether to build the rest:
//!
//! 1. **The chunk header walk** ([`ZgcRealHeap::tlab_walk_step`]): a VM-TLAB
//!    chunk is parseable from its start, because every header carries its size
//!    and no header is ever all-zero (`GC_FLAG_HEADER`), and the chunk is
//!    zeroed at carve. Everything below a buffer's COMMITTED cursor is fully
//!    headed (`Tlab::alloc_initialized` fences before its commit; the JIT's
//!    inline arms write the header before the commit store).
//! 2. **Materialize at the two points a collection needs it**: when a buffer
//!    retires (`Tlab::retire` -> [`crate::tlab::TlabTailSink::note_retired_span`],
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
//! retire and per oracle hit); `census` = walk and count, register nothing;
//! anything else (`1`, `on`, `materialize`) = walk, count, and REGISTER every
//! walked base the registry lacks (`materialized`). With the bit still set at
//! allocation, `unregistered_found` is the whole difference between the two
//! schemes on a real workload: it must read 0 apart from objects whose
//! announce helper returned 0 (a pending exception), which are dead.
//!
//! # What the bit-less scheme still needs (not built here)
//!
//! See `docs/internal/gc/zgc-zr8st-lazy-tlab-starts-remaining-steps-20261013.md`:
//! the owner-thread resolve with a persisted floor, the peer resolve against a
//! committed cursor read from the thread registry's `Tlab` address, atomic
//! inserts on every path that may race a peer's materialization, the
//! concurrent-mark start, and only then the emitter change.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::ZgcRealHeap;
use crate::heap::ObjectHeader;

/// One oracle hit in this many is re-answered by a read-only walk.
pub(crate) const Z_LAZY_CHECK_EVERY: usize = 8;
/// The read-only walk of a census check gives up after this many steps
/// (counted as `q_capped`; the hit is then not compared).
pub(crate) const Z_LAZY_CHECK_STEP_CAP: usize = 1 << 14;
/// Live-chunk records kept at most. A record whose buffer was dropped without
/// a retire (a thread torn down) would otherwise stay; past the cap the
/// oldest is dropped and counted (`chunks_dropped`).
const Z_LAZY_MAX_CHUNKS: usize = 1 << 16;

/// `CRATONVM_ZGC_LAZY_TLAB_STARTS`. See the module doc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ZLazyStartsMode {
    Off = 0,
    Census = 1,
    Materialize = 2,
}

impl ZLazyStartsMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Census,
            2 => Self::Materialize,
            _ => Self::Off,
        }
    }

    /// The token the summary line prints.
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Census => "census",
            Self::Materialize => "materialize",
        }
    }
}

/// Parse a `CRATONVM_ZGC_LAZY_TLAB_STARTS` value.
pub(crate) fn parse_lazy_tlab_starts_mode(raw: Option<&str>) -> ZLazyStartsMode {
    let Some(raw) = raw else {
        return ZLazyStartsMode::Off;
    };
    let v = raw.trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "0" | "off" | "false" | "no" => ZLazyStartsMode::Off,
        "census" => ZLazyStartsMode::Census,
        _ => ZLazyStartsMode::Materialize,
    }
}

/// zr z9/rv8: `CRATONVM_ZGC_LAZY_STARTS_DROP_UNPUBLISHED` -- a collection
/// drops the record of every live buffer it did not publish (default ON,
/// `=0` kill). Read once per collection, and only with
/// `CRATONVM_ZGC_LAZY_TLAB_STARTS` on.
pub(crate) fn zgc_lazy_starts_drop_unpublished_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_LAZY_STARTS_DROP_UNPUBLISHED") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "false" | "off" | "no")
        }
        None => true,
    }
}

/// The mode from the environment. Read once per heap (`ZgcCounters`' build).
pub(crate) fn zgc_lazy_tlab_starts_mode() -> ZLazyStartsMode {
    let raw = cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_LAZY_TLAB_STARTS");
    let s = raw.as_ref().map(|r| r.to_string_lossy().into_owned());
    parse_lazy_tlab_starts_mode(s.as_deref())
}

/// The un-materialized span of a live VM-TLAB chunk: `[floor, hi)`, `hi` the
/// chunk end (which identifies the buffer at retire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ZLiveChunk {
    pub(crate) floor: usize,
    pub(crate) hi: usize,
    /// zr z9/rv8: the current pause published this buffer's tail (its floor
    /// moved to the cursor), so the record may outlive the collection. Reset
    /// by [`ZLazyTlabStarts::note_collection`].
    pub(crate) published: bool,
}

/// One step of the chunk walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZTlabStep {
    /// A headed object at `base`; the walk continues at `next`.
    Object { base: usize, next: usize },
    /// A zero word (alignment padding, or past the last object): continue at
    /// the next word.
    Gap(usize),
    /// A header that cannot be parsed, or that runs past the bound.
    Stop,
}

/// The table and counters. Lives in `ZgcCounters` (one per heap).
pub(crate) struct ZLazyTlabStarts {
    mode: AtomicU8,
    chunks: Mutex<Vec<ZLiveChunk>>,
    pub(crate) retire_walks: AtomicUsize,
    pub(crate) stw_walks: AtomicUsize,
    pub(crate) objects_walked: AtomicUsize,
    pub(crate) already_registered: AtomicUsize,
    pub(crate) unregistered_found: AtomicUsize,
    pub(crate) materialized: AtomicUsize,
    pub(crate) walk_stops: AtomicUsize,
    pub(crate) chunks_dropped: AtomicUsize,
    /// zr z9/rv8: records a collection dropped because their buffer was
    /// neither retired nor published before it (`note_collection`).
    pub(crate) chunks_unpublished_dropped: AtomicUsize,
    pub(crate) q_own: AtomicUsize,
    pub(crate) q_peer: AtomicUsize,
    pub(crate) q_checked: AtomicUsize,
    pub(crate) q_mismatch: AtomicUsize,
    pub(crate) q_steps: AtomicUsize,
    pub(crate) q_capped: AtomicUsize,
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
    pub materialized: usize,
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
    pub(crate) fn new(mode: ZLazyStartsMode) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            chunks: Mutex::new(Vec::new()),
            retire_walks: AtomicUsize::new(0),
            stw_walks: AtomicUsize::new(0),
            objects_walked: AtomicUsize::new(0),
            already_registered: AtomicUsize::new(0),
            unregistered_found: AtomicUsize::new(0),
            materialized: AtomicUsize::new(0),
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
    pub(crate) fn mode(&self) -> ZLazyStartsMode {
        ZLazyStartsMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    #[inline]
    pub(crate) fn on(&self) -> bool {
        self.mode.load(Ordering::Relaxed) != ZLazyStartsMode::Off as u8
    }

    /// Tests: switch the mode on a built heap.
    #[cfg(test)]
    pub(crate) fn set_mode_for_test(&self, mode: ZLazyStartsMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    /// A chunk `[lo, hi)` was carved for a VM thread's buffer. Any older
    /// record overlapping it is stale (its memory was reissued) and goes.
    pub(crate) fn note_carve(&self, lo: usize, hi: usize) {
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
    pub(crate) fn take_retiring(
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
    pub(crate) fn advance_for_published(&self, s: usize, e: usize) -> Option<(usize, usize)> {
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
    /// the corpses as objects (and `materialize` would register them). Its
    /// cursor is unknown here, so the record goes (counted); a published one
    /// keeps its record with the floor already at its cursor. `keep_all`
    /// (`CRATONVM_ZGC_LAZY_STARTS_DROP_UNPUBLISHED=0`) restores the z8
    /// behaviour for an A/B.
    pub(crate) fn note_collection(&self, keep_all: bool) {
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
    pub(crate) fn chunk_holding(&self, addr: usize) -> Option<ZLiveChunk> {
        self.chunks
            .lock()
            .iter()
            .copied()
            .find(|c| addr >= c.floor && addr < c.hi)
    }

    pub(crate) fn live_chunks(&self) -> usize {
        self.chunks.lock().len()
    }

    pub(crate) fn report(&self) -> ZLazyStartsReport {
        let r = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        ZLazyStartsReport {
            mode: self.mode().token(),
            chunks_live: self.live_chunks(),
            retire_walks: r(&self.retire_walks),
            stw_walks: r(&self.stw_walks),
            objects_walked: r(&self.objects_walked),
            already_registered: r(&self.already_registered),
            unregistered_found: r(&self.unregistered_found),
            materialized: r(&self.materialized),
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
    pub(crate) unsafe fn tlab_walk_step(cur: usize, top: usize) -> ZTlabStep {
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
    pub(crate) unsafe fn walk_tlab_span(lo: usize, top: usize, mut f: impl FnMut(usize)) -> bool {
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
    pub(crate) unsafe fn tlab_walk_finds(
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

    /// Walk `[lo, top)` of a quiescent chunk: count every base, and register
    /// the ones the registry lacks in `materialize` mode. Returns the bases
    /// walked.
    pub(crate) fn lazy_materialize_span(&self, lo: usize, top: usize) -> usize {
        let lz = &self.counters.lazy_starts;
        let mode = lz.mode();
        if mode == ZLazyStartsMode::Off || top <= lo || lo < self.arena_base || top > self.arena_end
        {
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
                    if mode == ZLazyStartsMode::Materialize {
                        self.registry.insert(base);
                    }
                }
            })
        };
        lz.objects_walked.fetch_add(walked, Ordering::Relaxed);
        lz.already_registered.fetch_add(known, Ordering::Relaxed);
        lz.unregistered_found.fetch_add(missing, Ordering::Relaxed);
        if mode == ZLazyStartsMode::Materialize {
            lz.materialized.fetch_add(missing, Ordering::Relaxed);
        }
        if !complete {
            lz.walk_stops.fetch_add(1, Ordering::Relaxed);
        }
        walked
    }

    /// A VM buffer `[start, end)` retires with its cursor at `cursor`
    /// (`TlabTailSink::note_retired_span`).
    pub(crate) fn lazy_note_retired_span(&self, start: usize, cursor: usize, end: usize) {
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
    pub(crate) fn lazy_note_published_tails(&self, regions: &[(usize, usize)]) {
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
    pub(crate) fn lazy_note_collection(&self) {
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
    pub(crate) fn lazy_register_retired_span_listener(&self) {
        if self.counters.lazy_starts.on() {
            crate::tlab::register_retired_span_listener();
        }
    }

    /// The oracle census: `base` was just answered as a registered object.
    #[inline]
    pub(crate) fn lazy_note_query(&self, base: usize) {
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
mod zr8st_lazy_starts_tests {
    use super::*;
    use crate::heap::{ArrayElementType, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE};
    use cratonvm_types::ClassId;

    #[test]
    fn the_mode_parses_like_its_doc() {
        assert_eq!(parse_lazy_tlab_starts_mode(None), ZLazyStartsMode::Off);
        for off in ["", "0", "off", "FALSE", " no "] {
            assert_eq!(
                parse_lazy_tlab_starts_mode(Some(off)),
                ZLazyStartsMode::Off,
                "{off:?}"
            );
        }
        assert_eq!(
            parse_lazy_tlab_starts_mode(Some("census")),
            ZLazyStartsMode::Census
        );
        for on in ["1", "on", "materialize", "yes"] {
            assert_eq!(
                parse_lazy_tlab_starts_mode(Some(on)),
                ZLazyStartsMode::Materialize,
                "{on:?}"
            );
        }
    }

    /// A tiny deterministic generator (no `rand` dependency in this crate).
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    /// Fill a VM-TLAB chunk the way the VM does (header, then
    /// `note_tlab_object`, then the commit) with a random mix of legacy
    /// instances and primitive / reference arrays, some at 16-byte alignment.
    /// Returns the buffer and every base handed out.
    struct Filled {
        tlab: crate::tlab::Tlab,
        bases: Vec<usize>,
        lo: usize,
        end: usize,
    }

    impl Filled {
        fn cursor(&self) -> usize {
            self.lo + self.tlab.consumed_bytes()
        }
    }

    fn fill_chunk(heap: &ZgcRealHeap, seed: u64) -> Filled {
        let (ptr, size) = heap.refill_tlab(64 * 1024).expect("a VM-TLAB chunk");
        // SAFETY: the chunk was just carved, zeroed, for this buffer.
        let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
        let mut rng = XorShift(seed | 1);
        let mut bases = Vec::new();
        loop {
            let pick = rng.next() % 4;
            let (header, total) = match pick {
                0 | 1 => {
                    let slots = (rng.next() % 6) as u32;
                    let h = ObjectHeader::new(
                        ClassId::new(7),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        slots,
                    );
                    (h, HEADER_SIZE + slots as usize * crate::heap::SLOT_SIZE)
                }
                2 => {
                    let len = (rng.next() % 40) as u32;
                    let h = ObjectHeader::new(
                        ClassId::new(0),
                        ObjectKind::Array,
                        ArrayElementType::Byte,
                        len,
                        len,
                    );
                    let data =
                        crate::heap::array_data_size_checked(len as usize, ArrayElementType::Byte)
                            .unwrap();
                    (h, ARRAY_DATA_OFFSET + data)
                }
                _ => {
                    let len = (rng.next() % 12) as u32;
                    let h = ObjectHeader::new(
                        ClassId::new(0),
                        ObjectKind::Array,
                        ArrayElementType::Int,
                        len,
                        len,
                    );
                    let data =
                        crate::heap::array_data_size_checked(len as usize, ArrayElementType::Int)
                            .unwrap();
                    (h, ARRAY_DATA_OFFSET + data)
                }
            };
            let align = if rng.next() % 5 == 0 { 16 } else { 8 };
            let header = std::cell::Cell::new(Some(header));
            let got = tlab.alloc_initialized(total, align, |p| {
                // SAFETY: `p` is this buffer's fresh, unpublished reservation.
                unsafe { std::ptr::write(p as *mut ObjectHeader, header.take().unwrap()) };
                heap.note_tlab_object(p, total);
            });
            match got {
                Some(p) => bases.push(p as usize),
                None => break,
            }
        }
        Filled {
            tlab,
            bases,
            lo: ptr as usize,
            end: (ptr as usize + size) & !7,
        }
    }

    fn bases_in(heap: &ZgcRealHeap, lo: usize, hi: usize) -> Vec<usize> {
        (lo..hi)
            .step_by(8)
            .filter(|&a| heap.registry.contains(a))
            .collect()
    }

    #[test]
    fn the_walk_finds_exactly_the_registered_bases_over_random_chunks() {
        for seed in [1u64, 2, 3, 0xDEAD_BEEF, 0x1234_5678_9ABC] {
            let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
            let f = fill_chunk(&heap, seed);
            let (lo, top, bases) = (f.lo, f.cursor(), f.bases.clone());
            assert!(bases.len() > 100, "seed {seed}: the chunk holds objects");
            assert_eq!(
                bases_in(&heap, lo, top),
                bases,
                "seed {seed}: the bit regime"
            );
            let mut walked = Vec::new();
            // SAFETY: the buffer is this thread's, quiescent, below its cursor.
            let complete = unsafe { ZgcRealHeap::walk_tlab_span(lo, top, |b| walked.push(b)) };
            assert!(complete, "seed {seed}: the walk parses the whole span");
            assert_eq!(
                walked, bases,
                "seed {seed}: the walk agrees with the registry"
            );
            // Every registered base is found from the floor, and no address
            // between two bases is.
            for (i, &b) in bases.iter().enumerate() {
                // SAFETY: as above.
                let (found, _) =
                    unsafe { ZgcRealHeap::tlab_walk_finds(lo, b, usize::MAX) }.unwrap();
                assert!(found, "seed {seed}: base {i}");
                if b + 8 < top && !bases.contains(&(b + 8)) {
                    // SAFETY: as above (a `false` is accepted).
                    let (found, _) =
                        unsafe { ZgcRealHeap::tlab_walk_finds(lo, b + 8, usize::MAX) }.unwrap();
                    assert!(!found, "seed {seed}: an interior word is not a base");
                }
            }
        }
    }

    /// The oracle answers the same with and without the per-allocation bit:
    /// clear every start bit of the chunk, retire through the real
    /// `Tlab::retire` path in `materialize` mode, and `is_object_address`
    /// answers every word of the chunk exactly as before.
    #[test]
    fn materialize_on_retire_restores_every_oracle_answer() {
        crate::tlab::register_retired_span_listener();
        for seed in [11u64, 12, 0xC0FFEE] {
            let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
            heap.counters
                .lazy_starts
                .set_mode_for_test(ZLazyStartsMode::Materialize);
            let mut f = fill_chunk(&heap, seed);
            let (lo, top, bases) = (f.lo, f.cursor(), f.bases.clone());
            let before: Vec<bool> = (lo..top)
                .step_by(8)
                .map(|a| heap.is_object_address(a).is_some())
                .collect();
            for &b in &bases {
                heap.registry.remove(b);
            }
            assert!(bases_in(&heap, lo, top).is_empty(), "the bits are gone");
            f.tlab.retire();
            let after: Vec<bool> = (lo..top)
                .step_by(8)
                .map(|a| heap.is_object_address(a).is_some())
                .collect();
            assert_eq!(before, after, "seed {seed}: identical oracle answers");
            let r = heap.lazy_tlab_starts_report();
            assert_eq!(r.retire_walks, 1);
            assert_eq!(r.objects_walked, bases.len());
            assert_eq!(r.materialized, bases.len());
            assert_eq!(r.unregistered_found, bases.len());
            assert_eq!(r.walk_stops, 0);
            assert_eq!(r.chunks_live, 0, "the retired chunk's record is gone");
        }
    }

    /// `census` walks and counts but registers nothing; `off` never walks.
    #[test]
    fn census_counts_without_registering_and_off_is_inert() {
        crate::tlab::register_retired_span_listener();
        for mode in [ZLazyStartsMode::Census, ZLazyStartsMode::Off] {
            let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
            heap.counters.lazy_starts.set_mode_for_test(mode);
            let mut f = fill_chunk(&heap, 99);
            let bases = f.bases.clone();
            let dropped = bases[bases.len() / 2];
            heap.registry.remove(dropped);
            f.tlab.retire();
            let r = heap.lazy_tlab_starts_report();
            assert!(
                !heap.registry.contains(dropped),
                "{mode:?}: nothing registered"
            );
            if mode == ZLazyStartsMode::Census {
                assert_eq!(r.objects_walked, bases.len());
                assert_eq!(r.already_registered, bases.len() - 1);
                assert_eq!(r.unregistered_found, 1, "the positive control");
                assert_eq!(r.materialized, 0);
            } else {
                assert_eq!(
                    r,
                    ZLazyStartsReport {
                        mode: "off",
                        ..Default::default()
                    }
                );
            }
        }
    }

    /// A published tail walks the span below its cursor once and moves the
    /// floor, so the retire after it walks only what came later.
    #[test]
    fn a_published_tail_moves_the_floor() {
        let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
        heap.counters
            .lazy_starts
            .set_mode_for_test(ZLazyStartsMode::Census);
        let f = fill_chunk(&heap, 5);
        let bases = f.bases.clone();
        let s = f.cursor();
        let e = f.end;
        heap.lazy_note_published_tails(&[(s, e)]);
        let r = heap.lazy_tlab_starts_report();
        assert_eq!((r.stw_walks, r.objects_walked), (1, bases.len()));
        heap.lazy_note_published_tails(&[(s, e)]);
        assert_eq!(
            heap.lazy_tlab_starts_report().objects_walked,
            bases.len(),
            "nothing new"
        );
        heap.lazy_note_retired_span(f.lo, s, e);
        let r = heap.lazy_tlab_starts_report();
        assert_eq!((r.retire_walks, r.objects_walked), (1, bases.len()));
        assert_eq!(r.chunks_live, 0);
    }

    /// zr z9/rv8: a collection between carve and retire frees the chunk's dead
    /// objects, and since zr z8/zb (`CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS`
    /// default ON) the sweep leaves their headers intact. A retire walk from
    /// the carve-time floor then parses those corpses as objects and, in
    /// `materialize` mode, registers them -- a dead base back in the registry
    /// over free-listed memory (pinned, traced, swept a second time). The
    /// collection must drop the record of every buffer it did not publish.
    #[test]
    fn a_collection_drops_unpublished_records_so_no_corpse_is_registered() {
        use crate::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
        struct NoMonitors;
        impl MonitorCleanup for NoMonitors {
            fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
        }
        for kill in [false, true] {
            let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
            heap.counters
                .lazy_starts
                .set_mode_for_test(ZLazyStartsMode::Materialize);
            let f = fill_chunk(&heap, 31);
            let (lo, cursor, end, bases) = (f.lo, f.cursor(), f.end, f.bases.clone());
            assert!(bases.len() > 100);
            // The buffer stays live (not retired, not published) across the
            // collection; nothing roots its objects, so all of them die.
            std::mem::forget(f.tlab);
            let edits: [(&str, Option<&str>); 1] = [(
                "CRATONVM_ZGC_LAZY_STARTS_DROP_UNPUBLISHED",
                kill.then_some("0"),
            )];
            cratonvm_types::flags::with_thread_overrides(&edits, || {
                // SAFETY: single-threaded unit test.
                let stw = unsafe { StopTheWorldToken::new() };
                let mut roots: Vec<cratonvm_types::ObjectRef> = Vec::new();
                heap.collect_garbage(&stw, &mut roots, &NoMonitors);
            });
            assert!(
                bases.iter().all(|&b| !heap.registry.contains(b)),
                "the sweep unregistered the dead"
            );
            heap.lazy_note_retired_span(lo, cursor, end);
            let resurrected = bases.iter().filter(|&&b| heap.registry.contains(b)).count();
            eprintln!(
                "zr9rv8 kill={kill}: bases={} resurrected={resurrected} dead_headers_skipped={}",
                bases.len(),
                heap.sweep_dead_headers_skipped()
            );
            if kill {
                // Positive control: the pre-fix behaviour (the record survives
                // the collection) registers corpses when the headers survive.
                if heap.sweep_dead_headers_skipped() > 0 {
                    assert!(resurrected > 0, "kill arm: the walk re-registers corpses");
                }
            } else {
                assert_eq!(resurrected, 0, "no dead base is re-registered");
                let r = heap.lazy_tlab_starts_report();
                assert_eq!(
                    r.retire_walks, 0,
                    "the record was dropped at the collection"
                );
                assert!(r.chunks_unpublished_dropped >= 1);
            }
        }
    }

    /// The oracle census: a hit inside this thread's live chunk counts as
    /// `q_own`, its check walk agrees with the registry, and a hit outside
    /// every live chunk is not counted.
    #[test]
    fn the_query_census_classifies_and_checks_hits() {
        let heap = ZgcRealHeap::new_shared(8 * 1024 * 1024);
        heap.counters
            .lazy_starts
            .set_mode_for_test(ZLazyStartsMode::Census);
        let f = fill_chunk(&heap, 21);
        let bases = f.bases.clone();
        for &b in &bases {
            assert!(heap.is_object_address(b).is_some());
        }
        let r = heap.lazy_tlab_starts_report();
        assert_eq!(r.q_own + r.q_peer, bases.len());
        assert_eq!(r.q_mismatch, 0);
        assert!(
            r.q_checked > 0,
            "one hit in {Z_LAZY_CHECK_EVERY} is checked"
        );
        let outside = crate::collector::GarbageCollector::alloc_object(&*heap, ClassId::new(7), 1);
        assert!(heap.is_object_address(outside.as_ptr() as usize).is_some());
        let r2 = heap.lazy_tlab_starts_report();
        assert_eq!(
            r2.q_own + r2.q_peer,
            bases.len(),
            "a non-TLAB object is not counted"
        );
    }

    /// The cross-thread window: a peer queries bases the owner publishes
    /// (header, start bit, commit, then a Release store of the base) while the
    /// owner keeps bumping. Every read-only walk from the floor finds the
    /// published base, whatever the owner is writing above it.
    #[test]
    fn a_peer_walk_finds_every_published_base_while_the_owner_bumps() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let heap = ZgcRealHeap::new_shared(16 * 1024 * 1024);
        let published = Arc::new(AtomicUsize::new(0));
        let floor = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let owner = {
            let heap = Arc::clone(&heap);
            let published = Arc::clone(&published);
            let floor = Arc::clone(&floor);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let (ptr, size) = heap.refill_tlab(256 * 1024).expect("a VM-TLAB chunk");
                // SAFETY: freshly carved and zeroed for this buffer.
                let mut tlab = unsafe { crate::tlab::Tlab::new(ptr, size) };
                floor.store(ptr as usize, Ordering::Release);
                let mut rng = XorShift(77);
                let mut n = 0usize;
                loop {
                    let len = (rng.next() % 6) as u32;
                    let data =
                        crate::heap::array_data_size_checked(len as usize, ArrayElementType::Long)
                            .unwrap();
                    let total = ARRAY_DATA_OFFSET + data;
                    let got = tlab.alloc_initialized(total, 8, |p| {
                        let h = ObjectHeader::new(
                            ClassId::new(0),
                            ObjectKind::Array,
                            ArrayElementType::Long,
                            len,
                            len,
                        );
                        // SAFETY: fresh, unpublished reservation.
                        unsafe { std::ptr::write(p as *mut ObjectHeader, h) };
                        heap.note_tlab_object(p, total);
                    });
                    let Some(p) = got else { break };
                    n += 1;
                    if n % 3 == 0 {
                        published.store(p as usize, Ordering::Release);
                    }
                }
                done.store(true, Ordering::Release);
                n
            })
        };
        let mut checked = 0usize;
        while !done.load(Ordering::Acquire) || checked == 0 {
            let p = published.load(Ordering::Acquire);
            let lo = floor.load(Ordering::Acquire);
            if p == 0 || lo == 0 {
                std::hint::spin_loop();
                // `p` may have been read before the owner's last publish and
                // `done`: re-read it after seeing `done`, or a loaded host
                // ends the loop having checked nothing.
                if done.load(Ordering::Acquire) && published.load(Ordering::Acquire) == 0 {
                    break;
                }
                continue;
            }
            assert!(heap.registry.contains(p), "a published base is registered");
            // SAFETY: `p` is a published, registered base of the owner's
            // live chunk starting at `lo`.
            let (found, _) = unsafe { ZgcRealHeap::tlab_walk_finds(lo, p, usize::MAX) }.unwrap();
            assert!(found, "the peer walk finds the published base 0x{p:x}");
            checked += 1;
        }
        let n = owner.join().unwrap();
        assert!(n > 50, "the owner allocated ({n})");
        assert!(checked > 0, "the peer checked at least one base");
    }
}
