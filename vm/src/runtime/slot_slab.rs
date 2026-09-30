// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The per-thread slot slab: stage 1 of the contiguous interpreter stack.
//!
//! `docs/known-issues/interpreter/i1-L4-proposal-contiguous-interpreter-stack-20260923.md`.
//! Every interpreted activation used to own four heap buffers — its locals,
//! their kind marks, and the operand stack's slots and kind marks — kept
//! affordable by thread pools, a per-OS-thread spill pool and retired-slot
//! reuse. A frame built by the cached install paths (the fast doors and the
//! general dispatchers, which build 99.96% of all frames on the wave-29 census)
//! now takes ONE window from its [`FrameStack`](crate::runtime::frame::FrameStack)'s
//! slab instead: `max_locals` slots for the locals followed by the (padded)
//! operand stack, each with its parallel kind byte.
//!
//! # The two types
//!
//! * [`SlotVec`] is the storage of one of a frame's four buffers. It is a
//!   `Vec` in pieces (pointer, length, capacity) or a window of slab memory,
//!   and it derefs to a slice either way, so every reader and writer of a
//!   frame's locals and operand stack (the dispatch loop, the GC root scans
//!   and their remap twins, freeze/thaw, the debugger) is unchanged and pays
//!   no branch for the choice: the representation is only consulted when a
//!   buffer is built, recycled or dropped.
//! * [`SlotSlab`] is the memory: a list of chunks that are allocated once and
//!   never move, with a bump mark. A window is `mark += n`; releasing it is
//!   restoring the mark it was taken at.
//!
//! # Address stability
//!
//! A chunk never moves and never shrinks while a window in it is live, so a
//! window's address is stable for its frame's whole life: a raw `*mut Frame`
//! held across a call that pushes frames (`FrameStack::frame_ptr`, a JNI
//! native's `&mut Frame`, the dispatch loop's hoisted frame pointer) reads the
//! same locals before and after. Growth appends a chunk, it never copies one,
//! so there is no second relocation epoch to track beside
//! `FrameStack::reloc_epoch`.
//!
//! # Lifetime
//!
//! The slab is a field of the `FrameStack` whose frames use it, so the frames
//! and their memory travel together (a virtual thread's stack, a stack moved
//! into another `JvmThread`) and are dropped together. A frame that leaves its
//! stack by value (`FrameStack::pop`, the `Vec<Frame>` conversions) is first
//! copied into owned buffers (`Frame::detach_from_slab`), so no window outlives
//! the stack that holds its memory.

use std::mem::ManuallyDrop;
use std::ptr::NonNull;

use crate::types::CompactValue;

/// Marks a [`SlotVec`] as a window: set in its `cap` field.
const WINDOW_BIT: u32 = 1 << 31;

/// Marks a window whose first slot is its caller's first argument slot: the
/// frame's locals overlap its caller's operand stack (stage 2 of the
/// contiguous interpreter stack, argument overlap; interpreter round i1 wave
/// 37, lane L7). Only ever set together with [`WINDOW_BIT`], by
/// [`SlotVec::mark_shared_with_caller`].
const SHARED_BIT: u32 = 1 << 30;

/// The largest capacity an owned [`SlotVec`] records (below both flag bits,
/// so an owned buffer never reads as a window or as a shared one).
const MAX_CAP: usize = (SHARED_BIT - 1) as usize;

/// The bits of `cap` that are flags rather than a size.
const FLAG_BITS: u32 = WINDOW_BIT | SHARED_BIT;

/// One of a frame's four slot buffers: an owned `Vec<T>` in pieces, or a
/// window of its stack's [`SlotSlab`].
///
/// 16 bytes against a `Vec`'s 24: `Frame` holds four of these, so the header
/// every call writes shrinks by 32 bytes.
///
/// # Windows are never written through a `Vec`-style operation
///
/// A retired frame (`FrameStack::retire_top`) still holds its window, but the
/// slab has released that memory and the next push at that depth — or at any
/// depth below it — builds its own window there. So nothing may write a
/// window except through the frame that owns it while it is live. The only
/// operations here that could write are [`Self::make_owned`] (which READS the
/// window and writes a new owned buffer) and `DerefMut`, which the frame's
/// accessors use on live frames only. The frame builders that rebuild a
/// retired slot either take a fresh window or turn the old one into an owned
/// buffer first ([`Self::take_vec`]); they never reuse its memory.
pub struct SlotVec<T: Copy> {
    ptr: NonNull<T>,
    len: u32,
    /// The allocation's capacity for an owned buffer; the window's size with
    /// [`WINDOW_BIT`] set for a window.
    cap: u32,
}

// SAFETY: an owned `SlotVec<T>` is a `Vec<T>` in pieces and inherits its
// thread-safety. A window is memory of the `SlotSlab` of the `FrameStack` that
// holds the frame; it is reached only through `&`/`&mut` of that frame, so it
// is shared and sent exactly as the frame is — the same rules a `Vec`-backed
// frame already obeyed (the GC scans a stopped thread's frames from another
// thread through `&Frame`).
unsafe impl<T: Copy + Send> Send for SlotVec<T> {}
// SAFETY: see the `Send` impl; `&SlotVec` only ever hands out `&[T]`.
unsafe impl<T: Copy + Sync> Sync for SlotVec<T> {}

impl<T: Copy> SlotVec<T> {
    /// An empty owned buffer. Allocates nothing.
    #[inline]
    pub const fn new() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
            cap: 0,
        }
    }

    /// Take ownership of `v`'s buffer without copying it.
    #[inline]
    pub fn from_vec(v: Vec<T>) -> Self {
        let v = if v.capacity() > MAX_CAP {
            Self::refit(v)
        } else {
            v
        };
        let mut v = ManuallyDrop::new(v);
        let len = v.len() as u32;
        let cap = v.capacity() as u32;
        // SAFETY: `Vec::as_mut_ptr` is never null (a dangling, aligned pointer
        // for an unallocated `Vec`).
        let ptr = unsafe { NonNull::new_unchecked(v.as_mut_ptr()) };
        Self { ptr, len, cap }
    }

    /// A `Vec` whose capacity does not fit the 31-bit field: copied into one
    /// that does. Unreachable for a frame buffer (`max_locals` and
    /// `max_stack` are `u16`), kept so the conversion is total.
    #[cold]
    #[inline(never)]
    fn refit(v: Vec<T>) -> Vec<T> {
        let mut w = Vec::with_capacity(v.len());
        w.extend_from_slice(&v);
        debug_assert!(
            w.capacity() <= MAX_CAP,
            "a frame buffer of {} slots",
            w.len()
        );
        w
    }

    /// A window of `size` slots at `ptr`, the first `len` of them in use.
    ///
    /// # Safety
    ///
    /// `ptr..ptr + size` must be initialised memory that stays valid, and is
    /// read and written through nothing else, for as long as the returned
    /// value is used — a window `SlotSlab::alloc` handed out, used by the
    /// frame it was taken for while that frame is live.
    #[inline(always)]
    pub unsafe fn window(ptr: NonNull<T>, len: usize, size: usize) -> Self {
        debug_assert!(len <= size && size <= MAX_CAP);
        Self {
            ptr,
            len: len as u32,
            cap: size as u32 | WINDOW_BIT,
        }
    }

    /// Is this a window of slab memory rather than an owned buffer?
    #[inline(always)]
    pub fn is_window(&self) -> bool {
        self.cap & WINDOW_BIT != 0
    }

    /// Is this a window whose first slot is also its caller's first argument
    /// slot (stage 2, argument overlap)? See [`Self::mark_shared_with_caller`].
    #[inline(always)]
    pub fn is_shared_with_caller(&self) -> bool {
        self.cap & SHARED_BIT != 0
    }

    /// Record that this window starts inside the calling frame's operand
    /// stack: `FrameStack::push_cached_compact_overlapping` laid the callee's
    /// locals over the arguments the caller pushed. No-op for an owned
    /// buffer, which can never be shared.
    #[inline(always)]
    pub fn mark_shared_with_caller(&mut self) {
        if self.is_window() {
            self.cap |= SHARED_BIT;
        }
    }

    /// Slots this buffer holds: the allocation's capacity, or the window's
    /// size.
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        (self.cap & !FLAG_BITS) as usize
    }

    /// Slots in use (the same number the slice this derefs to reports).
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// The capacity of an OWNED buffer; 0 for a window (whose memory is the
    /// slab's and must not be rewritten through a retired frame) and for the
    /// empty buffer.
    #[inline(always)]
    pub fn owned_capacity(&self) -> usize {
        if self.is_window() {
            0
        } else {
            self.cap as usize
        }
    }

    /// The buffer's first slot, for a caller that rewrites an owned buffer in
    /// place (see [`Self::owned_capacity`]).
    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr.as_ptr()
    }

    /// Set the number of slots in use.
    ///
    /// # Safety
    ///
    /// `len` is at most the capacity, and the first `len` slots are
    /// initialised.
    #[inline(always)]
    pub unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity());
        self.len = len as u32;
    }

    /// The owned buffer as a `Vec`, leaving an empty buffer behind.
    ///
    /// A window has no allocation to hand over: it yields an empty `Vec`, and
    /// the window is gone from this value (its memory belongs to the slab).
    #[inline]
    pub fn take_vec(&mut self) -> Vec<T> {
        std::mem::take(self).into_vec()
    }

    /// The owned buffer as a `Vec`; an empty `Vec` for a window.
    #[inline]
    pub fn into_vec(self) -> Vec<T> {
        let me = ManuallyDrop::new(self);
        if me.is_window() {
            return Vec::new();
        }
        // SAFETY: an owned `SlotVec` holds exactly the parts of the `Vec<T>`
        // it was built from (`from_vec`), and `ManuallyDrop` keeps `self`'s
        // `Drop` from freeing them a second time.
        unsafe { Vec::from_raw_parts(me.ptr.as_ptr(), me.len as usize, me.cap as usize) }
    }

    /// Copy a window into an owned buffer of the window's length. No-op for
    /// an owned buffer.
    pub fn make_owned(&mut self) {
        if !self.is_window() {
            return;
        }
        let src: &[T] = &**self;
        let mut v = Vec::with_capacity(src.len());
        v.extend_from_slice(src);
        // Dropping the window is a no-op: its memory is the slab's.
        *self = Self::from_vec(v);
    }
}

impl<T: Copy> Default for SlotVec<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> Drop for SlotVec<T> {
    #[inline]
    fn drop(&mut self) {
        if self.is_window() || self.cap == 0 {
            return;
        }
        // SAFETY: an owned buffer with a non-zero capacity is the `Vec<T>` it
        // was built from; this is its only release.
        unsafe {
            drop(Vec::from_raw_parts(
                self.ptr.as_ptr(),
                self.len as usize,
                self.cap as usize,
            ));
        }
    }
}

impl<T: Copy> std::ops::Deref for SlotVec<T> {
    type Target = [T];
    #[inline(always)]
    fn deref(&self) -> &[T] {
        // SAFETY: `ptr..ptr + len` is initialised: the `Vec` prefix it was
        // built from, or the in-use prefix of a window its frame laid down.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl<T: Copy> std::ops::DerefMut for SlotVec<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as for `deref`; `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len as usize) }
    }
}

impl<T: Copy + std::fmt::Debug> std::fmt::Debug for SlotVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Slots in a thread's first chunk (9 KiB with the kind bytes). A thread that
/// never builds a windowed frame allocates nothing.
const SLAB_FIRST_CHUNK: usize = 1024;

/// Each later chunk doubles, up to this many slots (576 KiB with the kind
/// bytes). A window larger than this gets a chunk of its own size.
const SLAB_MAX_CHUNK: usize = 1 << 16;

/// The mark a frame carries when no `FrameStack` recorded one (a frame built
/// by a constructor and not yet pushed). Releasing to it changes nothing,
/// because [`SlotSlab::release_to`] only moves the mark down.
pub(crate) const SLAB_MARK_UNKNOWN: u64 = u64::MAX;

/// One chunk: two parallel arrays of `len` slots, leaked `Box<[_]>`s so no
/// unique-ownership claim covers the memory windows point into.
struct SlabChunk {
    vals: NonNull<CompactValue>,
    kinds: NonNull<u8>,
    len: usize,
}

impl SlabChunk {
    fn new(len: usize) -> Self {
        let vals: Box<[CompactValue]> = vec![CompactValue::zero(); len].into_boxed_slice();
        let kinds: Box<[u8]> = vec![0u8; len].into_boxed_slice();
        // SAFETY: `Box::into_raw` never returns null.
        let (vals, kinds) = unsafe {
            (
                NonNull::new_unchecked(Box::into_raw(vals) as *mut CompactValue),
                NonNull::new_unchecked(Box::into_raw(kinds) as *mut u8),
            )
        };
        Self { vals, kinds, len }
    }
}

impl Drop for SlabChunk {
    fn drop(&mut self) {
        // SAFETY: the two pointers are the `Box<[_]>`s `new` leaked, each of
        // `len` elements; this is their only release.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.vals.as_ptr(),
                self.len,
            )));
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.kinds.as_ptr(),
                self.len,
            )));
        }
    }
}

/// A window `SlotSlab::alloc` handed out.
#[derive(Clone, Copy)]
pub(crate) struct SlabWindow {
    /// The window's first value slot.
    pub(crate) vals: NonNull<CompactValue>,
    /// The kind byte of that slot; the window's kind bytes are parallel.
    pub(crate) kinds: NonNull<u8>,
    /// The slab's mark before this window: releasing to it frees the window
    /// and everything allocated after it.
    pub(crate) mark: u64,
}

/// The slot memory of one `FrameStack`'s windowed frames.
///
/// A mark is `(chunk index << 32) | offset of the first free slot`, so marks
/// grow with allocation order and a frame's mark is the slab's state just
/// before its window. Frames are pushed and popped LIFO, so popping to depth
/// `d` releases to the mark of the frame that was at `d`.
pub struct SlotSlab {
    mark: u64,
    /// The chunk the three cached fields below describe; `u32::MAX` for none.
    cur: u32,
    cur_len: u32,
    cur_vals: *mut CompactValue,
    cur_kinds: *mut u8,
    /// Never shrinks while the slab lives, and a chunk's memory never moves
    /// (growing this `Vec` moves only the chunk headers).
    chunks: Vec<SlabChunk>,
}

// SAFETY: the raw pointers are into `chunks`, which this value owns; the slab
// is only touched through `&mut` of its `FrameStack`.
unsafe impl Send for SlotSlab {}
// SAFETY: `&SlotSlab` exposes nothing but the mark.
unsafe impl Sync for SlotSlab {}

impl SlotSlab {
    /// An empty slab. Allocates nothing until the first window.
    pub const fn new() -> Self {
        Self {
            mark: 0,
            cur: u32::MAX,
            cur_len: 0,
            cur_vals: std::ptr::null_mut(),
            cur_kinds: std::ptr::null_mut(),
            chunks: Vec::new(),
        }
    }

    /// The current mark: a frame pushed now records it.
    #[inline(always)]
    pub fn mark(&self) -> u64 {
        self.mark
    }

    /// Release every window taken at or after `mark`.
    ///
    /// Only ever moves the mark DOWN: a mark above the current one (a frame's
    /// `SLAB_MARK_UNKNOWN`, or a frame that was not pushed through this
    /// stack) releases nothing, so a frame the stack did not mark can leak a
    /// window until a lower pop but can never hand a live one out twice.
    #[inline(always)]
    pub fn release_to(&mut self, mark: u64) {
        self.mark = self.mark.min(mark);
    }

    /// Release every window (the stack is empty).
    #[inline(always)]
    pub fn release_all(&mut self) {
        self.mark = 0;
    }

    /// Take a window of `n` slots (values and kind bytes). Its memory is
    /// initialised but holds whatever the last window there left; the caller
    /// writes what its readers need.
    #[inline(always)]
    pub(crate) fn alloc(&mut self, n: usize) -> SlabWindow {
        let mark = self.mark;
        if (mark >> 32) as u32 == self.cur {
            let off = mark as u32 as u64;
            if off + n as u64 <= u64::from(self.cur_len) {
                self.mark = mark + n as u64;
                // SAFETY: `off + n <= cur_len`, so the window lies inside chunk
                // `cur`, whose pointers these are; neither is null.
                return unsafe {
                    SlabWindow {
                        vals: NonNull::new_unchecked(self.cur_vals.add(off as usize)),
                        kinds: NonNull::new_unchecked(self.cur_kinds.add(off as usize)),
                        mark,
                    }
                };
            }
        }
        self.alloc_slow(n)
    }

    /// Take a window of `n` slots that STARTS at the address `start`, inside
    /// the window of the frame on top of the stack (stage 2 of the contiguous
    /// interpreter stack, argument overlap; interpreter round i1 wave 37,
    /// lane L7).
    ///
    /// `start` is the address of the top frame's first free operand-stack
    /// slot -- where the caller's arguments lie, already popped -- and `end`
    /// is one past the top frame's window. The callee's locals then begin on
    /// the arguments, and nothing has to be copied for them.
    ///
    /// # When it refuses
    ///
    /// `None`, with nothing changed, unless all of these hold:
    ///
    /// * the mark is in the cached chunk, and `start < end <= ` the mark's
    ///   address, with `start` at or past the chunk's first slot. `start` is
    ///   then strictly inside the top frame's window, and chunks are disjoint
    ///   allocations, so that window is in THIS chunk (an address comparison
    ///   across chunks alone would prove nothing: chunk addresses are not
    ///   ordered); and the top frame's window is the last live one in it. Everything from `start` up is then
    ///   dead memory: the top frame's own free stack slots, the free stack
    ///   slots of the frames below it that this frame already overlapped, and
    ///   released windows. Every live slot of every frame below lies below
    ///   `start` -- a frame's callee starts at the frame's live stack top
    ///   (overlap) or at the mark, which is at or past the end of every live
    ///   window (a plain window) -- so no live slot is handed out twice.
    /// * `start` is slot-aligned, and the window fits the chunk.
    ///
    /// # The mark
    ///
    /// The returned window's `mark` is the slab's mark BEFORE this call, as
    /// for [`Self::alloc`], so releasing to it (the callee's pop) restores
    /// exactly the state the caller had. The slab's own mark moves up to the
    /// window's end if the window reaches past it; it never moves down here,
    /// because the region between the window's end and the old mark may be a
    /// frame below's free stack slots, which that frame needs back when it is
    /// the top again.
    #[inline(always)]
    pub(crate) fn alloc_overlapping(
        &mut self,
        start: usize,
        end: usize,
        n: usize,
    ) -> Option<SlabWindow> {
        let mark = self.mark;
        if (mark >> 32) as u32 != self.cur {
            return None;
        }
        const SLOT: usize = std::mem::size_of::<CompactValue>();
        let base = self.cur_vals as usize;
        let mark_off = mark as u32 as usize;
        let mark_addr = base.wrapping_add(mark_off * SLOT);
        if start < base || start >= end || end > mark_addr {
            return None;
        }
        let diff = start - base;
        if diff % SLOT != 0 {
            return None;
        }
        let off = diff / SLOT;
        let new_end = off.checked_add(n)?;
        if new_end > self.cur_len as usize {
            return None;
        }
        if new_end > mark_off {
            // Same chunk: only the offset half of the mark changes.
            self.mark = (mark & !u64::from(u32::MAX)) | new_end as u64;
        }
        // SAFETY: `off + n <= cur_len`, so the window lies inside chunk `cur`,
        // whose pointers these are; neither is null.
        Some(unsafe {
            SlabWindow {
                vals: NonNull::new_unchecked(self.cur_vals.add(off)),
                kinds: NonNull::new_unchecked(self.cur_kinds.add(off)),
                mark,
            }
        })
    }

    /// [`Self::alloc`] when the window does not fit the cached chunk: the
    /// first window, a mark in another chunk, or the chunk is full.
    #[cold]
    #[inline(never)]
    fn alloc_slow(&mut self, n: usize) -> SlabWindow {
        let mark = self.mark;
        let c = (mark >> 32) as usize;
        let off = mark as u32 as usize;
        // Where the window goes: after the mark in its own chunk if it fits,
        // else at the start of the next chunk. Every live window is below the
        // mark, so the chunks after `c` hold none.
        let (idx, start) = match self.chunks.get(c) {
            Some(chunk) if off + n <= chunk.len => (c, off),
            Some(_) => (c + 1, 0),
            None => (c, 0),
        };
        let fits = self
            .chunks
            .get(idx)
            .is_some_and(|chunk| start + n <= chunk.len);
        if !fits {
            let prev = if idx == 0 {
                0
            } else {
                self.chunks.get(idx - 1).map_or(0, |chunk| chunk.len)
            };
            let size = if prev == 0 {
                SLAB_FIRST_CHUNK
            } else {
                prev.saturating_mul(2).min(SLAB_MAX_CHUNK)
            };
            let chunk = SlabChunk::new(size.max(n));
            if idx < self.chunks.len() {
                // A spare chunk too small for this window. No live window is
                // in it (it is past the mark); a RETIRED frame's window may
                // still point into it, and such a pointer is never read.
                self.chunks[idx] = chunk;
            } else {
                // `idx <= chunks.len()`: a mark only ever names a chunk that
                // exists, or chunk 0 of an empty slab.
                self.chunks.push(chunk);
            }
        }
        let chunk = &self.chunks[idx];
        self.cur = idx as u32;
        self.cur_len = chunk.len as u32;
        self.cur_vals = chunk.vals.as_ptr();
        self.cur_kinds = chunk.kinds.as_ptr();
        self.mark = ((idx as u64) << 32) | (start + n) as u64;
        // SAFETY: `start + n <= chunk.len` (checked above, or the chunk was
        // just built at least `n` long with `start == 0` — a new chunk is only
        // built for `idx == c + 1` or an absent chunk, both with `start == 0`,
        // or for `idx == c` when it did not exist).
        unsafe {
            SlabWindow {
                vals: NonNull::new_unchecked(chunk.vals.as_ptr().add(start)),
                kinds: NonNull::new_unchecked(chunk.kinds.as_ptr().add(start)),
                mark,
            }
        }
    }
}

impl Default for SlotSlab {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(w: SlabWindow, n: usize, tag: u64) {
        for i in 0..n {
            // SAFETY: `w` is a live window of `n` slots.
            unsafe {
                w.vals
                    .as_ptr()
                    .add(i)
                    .write(CompactValue::from_bits(tag + i as u64));
                w.kinds
                    .as_ptr()
                    .add(i)
                    .write((tag as u8).wrapping_add(i as u8));
            }
        }
    }

    fn holds(w: SlabWindow, n: usize, tag: u64) -> bool {
        (0..n).all(|i| {
            // SAFETY: `w` is a live window of `n` slots.
            unsafe {
                w.vals.as_ptr().add(i).read().raw_bits() == tag + i as u64
                    && w.kinds.as_ptr().add(i).read() == (tag as u8).wrapping_add(i as u8)
            }
        })
    }

    #[test]
    fn a_released_window_is_the_next_one_handed_out() {
        let mut slab = SlotSlab::new();
        let a = slab.alloc(5);
        let b = slab.alloc(7);
        assert_eq!(a.mark, 0);
        assert_eq!(b.mark, 5, "windows are contiguous");
        // SAFETY: both windows are in chunk 0.
        assert_eq!(unsafe { a.vals.as_ptr().add(5) }, b.vals.as_ptr());
        slab.release_to(b.mark);
        let b2 = slab.alloc(3);
        assert_eq!(b2.vals, b.vals, "the released window is reused");
        slab.release_to(a.mark);
        assert_eq!(slab.mark(), 0);
        assert_eq!(slab.alloc(1).vals, a.vals);
    }

    #[test]
    fn releasing_to_a_higher_or_unknown_mark_releases_nothing() {
        let mut slab = SlotSlab::new();
        let a = slab.alloc(4);
        let _b = slab.alloc(4);
        let before = slab.mark();
        slab.release_to(SLAB_MARK_UNKNOWN);
        assert_eq!(slab.mark(), before);
        slab.release_to(before + 100);
        assert_eq!(slab.mark(), before);
        slab.release_to(a.mark);
        assert_eq!(slab.mark(), a.mark);
    }

    #[test]
    fn growth_appends_chunks_and_never_moves_a_live_window() {
        // Deep enough to cross several chunks (1024, 2048, 4096, ... slots).
        let mut slab = SlotSlab::new();
        let mut live = Vec::new();
        for depth in 0..3000u64 {
            let n = 1 + (depth as usize % 29);
            let w = slab.alloc(n);
            fill(w, n, depth * 1000);
            live.push((w, n, depth * 1000));
        }
        assert!(
            slab.chunks.len() > 2,
            "the test must cross chunk boundaries"
        );
        for &(w, n, tag) in &live {
            assert!(
                holds(w, n, tag),
                "window at mark {:#x} was disturbed",
                w.mark
            );
        }
        // Pop half the stack and push it again: the lower half is untouched and
        // at the same addresses.
        let half = live.len() / 2;
        slab.release_to(live[half].0.mark);
        for depth in 0..(live.len() - half) as u64 {
            let n = 3;
            let w = slab.alloc(n);
            fill(w, n, 7_000_000 + depth);
        }
        for &(w, n, tag) in &live[..half] {
            assert!(holds(w, n, tag), "a live window changed after a re-push");
        }
    }

    #[test]
    fn a_window_larger_than_any_chunk_gets_one_of_its_own() {
        let mut slab = SlotSlab::new();
        let small = slab.alloc(10);
        fill(small, 10, 1);
        let n = SLAB_MAX_CHUNK + 17;
        let big = slab.alloc(n);
        fill(big, n, 50);
        assert!(holds(small, 10, 1));
        assert!(holds(big, n, 50));
        slab.release_to(big.mark);
        assert_eq!(slab.mark(), small.mark + 10);
        // A spare chunk too small for a later window is replaced, not grown in
        // place: the window below the mark is still intact.
        let again = slab.alloc(n + 1);
        fill(again, n + 1, 90);
        assert!(holds(small, 10, 1));
    }

    /// Stage 2 (argument overlap): a window that starts inside the top
    /// window, the marks it records and restores, and every refusal.
    #[test]
    fn an_overlapping_window_starts_where_asked_and_its_release_restores_the_mark() {
        const SLOT: usize = std::mem::size_of::<CompactValue>();
        let mut slab = SlotSlab::new();
        assert!(
            slab.alloc_overlapping(8, 64, 1).is_none(),
            "no chunk yet: nothing to overlap"
        );
        let caller = slab.alloc(10);
        let base = caller.vals.as_ptr() as usize;
        let end = base + 10 * SLOT;
        let mark = slab.mark();
        assert_eq!(mark, 10);

        // Starts inside the caller, ends past it: the mark moves up, and the
        // window records the mark before it.
        let w = slab.alloc_overlapping(base + 6 * SLOT, end, 8).expect("fits");
        assert_eq!(w.vals.as_ptr() as usize, base + 6 * SLOT);
        // SAFETY: both windows are in chunk 0.
        assert_eq!(w.kinds.as_ptr(), unsafe { caller.kinds.as_ptr().add(6) });
        assert_eq!(w.mark, mark);
        assert_eq!(slab.mark(), 14);

        // Nested and ending below the mark: the mark never moves down.
        let inner = slab
            .alloc_overlapping(base + 8 * SLOT, base + 14 * SLOT, 2)
            .expect("fits");
        assert_eq!(inner.mark, 14);
        assert_eq!(slab.mark(), 14);
        slab.release_to(inner.mark);
        assert_eq!(slab.mark(), 14);
        slab.release_to(w.mark);
        assert_eq!(slab.mark(), mark, "the callee's pop restores the caller's mark");

        // Refusals, each leaving the mark alone: below the chunk; the top
        // window reaching past the mark; no free slot in the top window; not
        // slot-aligned; not fitting the chunk.
        let refused = [
            (base - SLOT, end, 1),
            (base, end + SLOT, 1),
            (end, end, 1),
            (base + 1, end, 1),
            (base, end, SLAB_FIRST_CHUNK + 1),
        ];
        for (start, top_end, n) in refused {
            assert!(slab.alloc_overlapping(start, top_end, n).is_none());
            assert_eq!(slab.mark(), mark);
        }
        // A plain window after all that lands right after the caller.
        assert_eq!(slab.alloc(1).vals.as_ptr() as usize, end);
    }

    #[test]
    fn a_shared_window_keeps_its_size_and_an_owned_buffer_cannot_be_shared() {
        let mut slab = SlotSlab::new();
        let w = slab.alloc(6);
        // SAFETY: `w` is live for the whole test.
        let mut s = unsafe { SlotVec::window(w.kinds, 4, 6) };
        assert!(!s.is_shared_with_caller());
        s.mark_shared_with_caller();
        assert!(s.is_shared_with_caller() && s.is_window());
        assert_eq!(s.capacity(), 6);
        assert_eq!(s.len(), 4);
        assert_eq!(s.owned_capacity(), 0);
        assert!(s.take_vec().is_empty(), "a shared window is still slab memory");

        let mut o = SlotVec::from_vec(vec![1u8, 2, 3]);
        o.mark_shared_with_caller();
        assert!(!o.is_shared_with_caller() && !o.is_window());
        assert_eq!(o.into_vec(), vec![1, 2, 3]);
    }

    #[test]
    fn an_owned_slot_vec_round_trips_its_vec_with_its_capacity() {
        let mut v: Vec<u8> = Vec::with_capacity(40);
        v.extend_from_slice(&[1, 2, 3]);
        let cap = v.capacity();
        let ptr = v.as_ptr();
        let s = SlotVec::from_vec(v);
        assert!(!s.is_window());
        assert_eq!(s.len(), 3);
        assert_eq!(s.capacity(), cap);
        assert_eq!(&*s, &[1, 2, 3]);
        let back = s.into_vec();
        assert_eq!(back.as_ptr(), ptr, "no copy");
        assert_eq!(back.capacity(), cap);
        assert_eq!(back, vec![1, 2, 3]);
    }

    #[test]
    fn a_window_hands_over_no_vec_and_copies_when_made_owned() {
        let mut slab = SlotSlab::new();
        let w = slab.alloc(4);
        fill(w, 4, 11);
        // SAFETY: `w` is live for the whole test.
        let mut s = unsafe { SlotVec::window(w.kinds, 4, 4) };
        assert!(s.is_window());
        assert_eq!(s.capacity(), 4);
        s[2] = 99;
        // SAFETY: as above.
        assert_eq!(
            unsafe { w.kinds.as_ptr().add(2).read() },
            99,
            "writes go to the slab"
        );
        let mut t = unsafe { SlotVec::<u8>::window(w.kinds, 4, 4) };
        assert!(
            t.take_vec().is_empty(),
            "a window has no allocation to hand over"
        );
        assert!(!t.is_window());
        s.make_owned();
        assert!(!s.is_window());
        assert_eq!(s[2], 99);
        s[2] = 5;
        // SAFETY: as above.
        assert_eq!(
            unsafe { w.kinds.as_ptr().add(2).read() },
            99,
            "an owned copy no longer aliases the slab"
        );
    }
}
