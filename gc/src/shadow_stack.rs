// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-thread **shadow stack** of live object references for precise,
//! *rewritable* GC roots inside JIT-compiled code.
//!
//! # Why this exists
//!
//! A moving (Cheney) young-gen collection must update every root that points
//! at a relocated object. For interpreter frames the VM tracks exact oop
//! locations, so `update_all_roots` rewrites them. JIT-compiled frames are the
//! problem: the only root information the GC otherwise has is the
//! **conservative** native-stack scan (`conservative_roots::scan_active_jit_frames`),
//! which can *mark* (a value that looks like a heap pointer is treated as a
//! root, over-retaining at worst) but can **not rewrite** — a stack word that
//! merely *looks* like a pointer might be an `i64`, and rewriting it would
//! corrupt mutator data. Because of that the collector is forced onto a
//! non-moving young sweep whenever a JIT frame is live, which cannot drain a
//! large long-lived young set (the bintrees18 throughput wall).
//!
//! The precise-oop-map approach (RBP-chain walk + per-safepoint bytecode-PC
//! matching) is fragile and, critically, **misses operand-stack oops held in
//! callee-saved registers** across a safepoint: e.g. `make()` does
//! `aload_1 (n); … invokestatic make; putfield l` — `n` is live on the operand
//! stack across the recursive call but lives in a register, so the frame-slot
//! oop map never lists it and the move leaves the register stale.
//!
//! # The mechanism
//!
//! The shadow stack is a flat, thread-local array of **oop values**. JIT code,
//! immediately before any GC-capable call, *pushes* every live oop (locals AND
//! operand-stack entries) onto it, and immediately after the call *reloads*
//! each value from its slot (the GC may have rewritten it) and pops. Because
//! every slot is, by construction, exactly one object reference:
//!   * the GC **marks** each slot value precisely (no false positives), and
//!   * the GC **rewrites** each slot in place when its object moved — safe,
//!     because the slot is known to be an oop.
//!
//! No RBP-chain walk, no per-PC oop-map matching, no register-invisibility:
//! the live set is materialised explicitly at runtime. After the call the JIT
//! reloads from the (updated) slots, so a relocated object's new address flows
//! back into the registers the compiled code keeps using.
//!
//! # JIT contract (layout)
//!
//! `top` is at byte offset 0 so the push fast path is a `[reg+0]` load/store
//! (1 byte shorter ModR/M), mirroring [`crate::tlab::Tlab`]. `end` follows at
//! offset 8 for the overflow guard. The VM embeds a `ShadowStack` in each
//! `JvmThread` and exposes `JvmThread base + SHADOW_OFFSET + TOP_OFFSET` to the
//! codegen.

/// Default capacity (in 8-byte slots) of a thread's shadow stack.
///
/// This bounds the total number of live oops across *all* simultaneously
/// active JIT frames on one thread. Java stack depth is itself bounded
/// (StackOverflowError) long before a realistic method nest pushes anywhere
/// near this many oops, so 256K slots (2 MiB) is a generous ceiling that never
/// reallocates (keeping `base`/`end` stable for the JIT). Threads that never
/// run JIT code keep an empty (unallocated) shadow stack.
pub const DEFAULT_SHADOW_SLOTS: usize = 256 * 1024;

/// A per-thread stack of live object references, maintained by JIT-compiled
/// code around GC-capable call sites and scanned/rewritten by the collector.
///
/// Layout is `#[repr(C)]` with `top` first — see the module docs for the JIT
/// contract. The backing storage is a heap `Box<[usize]>` whose address is
/// stable for the lifetime of the thread, so `base`/`end` (and any live `top`)
/// remain valid even if the `ShadowStack` struct itself is moved.
#[repr(C)]
pub struct ShadowStack {
    /// Next free slot address (exclusive high-water of pushed oops).
    /// The GC scans `[base, top)`. JIT contract: must remain at byte offset 0.
    pub top: usize,
    /// One-past-the-last usable slot address (overflow guard).
    /// JIT contract: must remain at byte offset 8.
    pub end: usize,
    /// First slot address (inclusive). Used to bound the GC scan and to reset.
    pub base: usize,
    /// Owns the backing storage. Never read directly through this field by the
    /// JIT or GC — they use `base`/`top`/`end` raw addresses — but keeping the
    /// `Box` here ties the allocation's lifetime to the thread.
    _buf: Box<[usize]>,
}

impl ShadowStack {
    /// Byte offset of the `top` field. Read by the JIT-emitted inline push.
    pub const TOP_OFFSET: usize = 0;
    /// Byte offset of the `end` field. Read by the JIT-emitted overflow guard
    /// that both backends emit ahead of every push (`x64::emit_shadow_push`,
    /// `ir_lower::emit_shadow_push`): a push whose slots would not all fit
    /// below `end` stores nothing, leaves `top` alone, and marks its saved-base
    /// slot with bit 0 so the paired reload skips restoring homes from slots
    /// that were never written. Bailing degrades that safepoint's root
    /// publication — the runtime coverage verifier then rejects the moving-young
    /// proof — but it is bounded; storing past `end` was not.
    pub const END_OFFSET: usize = 8;
    /// Byte offset of the `base` field.
    ///
    /// Part of the same `#[repr(C)]` contract as `TOP_OFFSET`/`END_OFFSET` and
    /// asserted by `layout_offsets_match_jit_contract`. Not read by codegen —
    /// it exists so the moving-young coverage verifier
    /// (`conservative_roots::moving_young_unpublished_frame_oop_present`) can
    /// recover a thread's published shadow window `[base, top)` from a live
    /// compiled frame alone: that frame caches its `*mut JvmThread` in
    /// `[rbp - CompiledMethod::shadow_thread_slot_off]` and the `ShadowStack`
    /// sits at `thread + CompiledMethod::shadow_off_in_thread`. Without `base`
    /// the verifier could find `top` but not the start of the scan range.
    pub const BASE_OFFSET: usize = 16;
}

// SAFETY: a `ShadowStack` is exclusive to its owning thread; the raw addresses
// in `base`/`top`/`end` point into `_buf`, which is `Send`. No concurrent
// access occurs except at a stop-the-world safepoint, where the collecting
// thread reads another thread's stack only after that thread has parked.
unsafe impl Send for ShadowStack {}

impl Default for ShadowStack {
    fn default() -> Self {
        Self::empty()
    }
}

impl ShadowStack {
    /// An unallocated shadow stack (no backing storage). Pushing is impossible
    /// until [`Self::ensure_allocated`] runs; scans/remaps are no-ops.
    pub fn empty() -> Self {
        Self {
            top: 0,
            end: 0,
            base: 0,
            _buf: Vec::new().into_boxed_slice(),
        }
    }

    /// Allocate the backing buffer if this stack is still empty. Idempotent.
    /// Called lazily the first time a thread is about to enter JIT code with
    /// the shadow-stack mechanism enabled.
    pub fn ensure_allocated(&mut self) {
        if self.base != 0 {
            return;
        }
        let mut buf = vec![0usize; DEFAULT_SHADOW_SLOTS].into_boxed_slice();
        let base = buf.as_mut_ptr() as usize;
        self.base = base;
        self.top = base;
        self.end = base + DEFAULT_SHADOW_SLOTS * std::mem::size_of::<usize>();
        self._buf = buf;
    }

    /// True if no backing storage has been allocated.
    #[inline]
    pub fn is_unallocated(&self) -> bool {
        self.base == 0
    }

    /// Number of oops currently pushed.
    #[inline]
    pub fn depth(&self) -> usize {
        if self.base == 0 || self.top < self.base {
            return 0;
        }
        (self.top - self.base) / std::mem::size_of::<usize>()
    }

    /// Reset to empty (drop all pushed oops). Used as a defensive heal if a
    /// thread unwinds out of JIT code abnormally (e.g. exception) without the
    /// codegen having popped — see the entry/exit watermark in the VM.
    #[inline]
    pub fn reset(&mut self) {
        if self.base != 0 {
            self.top = self.base;
        }
    }

    /// Set the high-water mark (used to restore a saved watermark on JIT exit).
    /// Clamped into `[base, end]` so a corrupt value can never widen the scan
    /// range beyond the backing buffer.
    #[inline]
    pub fn set_top(&mut self, top: usize) {
        if self.base == 0 {
            return;
        }
        let clamped = top.clamp(self.base, self.end);
        self.top = clamped & !(std::mem::size_of::<usize>() - 1);
    }

    /// Push one oop value. Returns `false` (and drops the value) on overflow —
    /// the JIT fast path does *not* call this (it bumps inline); this is the
    /// helper/testing entry point.
    #[inline]
    pub fn push(&mut self, oop: usize) -> bool {
        if self.base == 0 || self.top >= self.end {
            return false;
        }
        // SAFETY: `top` is in `[base, end)` and 8-aligned by construction.
        unsafe { (self.top as *mut usize).write(oop) };
        self.top += std::mem::size_of::<usize>();
        true
    }

    /// Invoke `f` with every currently-pushed oop value (the scan range
    /// `[base, top)`). Used by the marking root collector. No-op when empty.
    #[inline]
    pub fn for_each_value(&self, mut f: impl FnMut(usize)) {
        if self.base == 0 {
            return;
        }
        let mut p = self.base;
        while p < self.top {
            // SAFETY: `p` walks 8-aligned slots within the live `[base, top)`
            // range of the backing buffer; each holds a pushed oop value.
            let v = unsafe { (p as *const usize).read() };
            f(v);
            p += std::mem::size_of::<usize>();
        }
    }

    /// After a moving collection, rewrite every pushed slot whose value names a
    /// relocated object (`pointer_map[old] = new`). This is the precise,
    /// *rewritable* root update that conservative scanning cannot do — every
    /// slot here is known to be an oop, so the rewrite is unconditionally safe.
    /// Returns the number of slots rewritten (for diagnostics).
    pub fn remap(&self, pointer_map: &cratonvm_types::PointerMap) -> usize {
        if self.base == 0 || pointer_map.is_empty() {
            return 0;
        }
        let mut rewritten = 0usize;
        let mut p = self.base;
        while p < self.top {
            // SAFETY: see `for_each_value`.
            let old = unsafe { (p as *const usize).read() };
            if let Some(&new) = pointer_map.get(&old) {
                // SAFETY: same slot, rewriting a known oop in place.
                unsafe { (p as *mut usize).write(new) };
                rewritten += 1;
            }
            p += std::mem::size_of::<usize>();
        }
        rewritten
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_noop() {
        let s = ShadowStack::empty();
        assert!(s.is_unallocated());
        assert_eq!(s.depth(), 0);
        let mut seen = 0;
        s.for_each_value(|_| seen += 1);
        assert_eq!(seen, 0);
        assert_eq!(s.remap(&cratonvm_types::PointerMap::default()), 0);
    }

    #[test]
    fn layout_offsets_match_jit_contract() {
        let s = ShadowStack::empty();
        let base = &s as *const ShadowStack as usize;
        assert_eq!(
            &s.top as *const usize as usize - base,
            ShadowStack::TOP_OFFSET
        );
        assert_eq!(
            &s.end as *const usize as usize - base,
            ShadowStack::END_OFFSET
        );
        assert_eq!(
            &s.base as *const usize as usize - base,
            ShadowStack::BASE_OFFSET,
            "the moving-young coverage verifier recovers the published shadow \
             window [base, top) from a live compiled frame using these offsets",
        );
    }

    /// The verifier compares a compiled frame's spill words against the values
    /// in `[base, top)`. That range is what `for_each_value` walks, so the two
    /// must agree exactly — a `top` above the pushed data would let the
    /// verifier accept an unpublished oop because a stale slot happened to
    /// hold it.
    #[test]
    fn published_window_matches_the_scanned_range() {
        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(s.push(0x1000));
        assert!(s.push(0x2000));

        let mut scanned = Vec::new();
        s.for_each_value(|v| scanned.push(v));
        let window: Vec<usize> = (0..s.depth())
            .map(|i| unsafe { ((s.base + i * 8) as *const usize).read() })
            .collect();
        assert_eq!(scanned, window);
        assert_eq!(s.top, s.base + scanned.len() * 8);
    }

    #[test]
    fn push_scan_remap_roundtrip() {
        let mut s = ShadowStack::empty();
        s.ensure_allocated();
        assert!(!s.is_unallocated());
        assert!(s.push(0x1000));
        assert!(s.push(0x2000));
        assert!(s.push(0x3000));
        assert_eq!(s.depth(), 3);

        let mut collected = Vec::new();
        s.for_each_value(|v| collected.push(v));
        assert_eq!(collected, vec![0x1000, 0x2000, 0x3000]);

        // Relocate 0x2000 -> 0x9000.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x2000usize, 0x9000usize);
        assert_eq!(s.remap(&map), 1);

        let mut after = Vec::new();
        s.for_each_value(|v| after.push(v));
        assert_eq!(after, vec![0x1000, 0x9000, 0x3000]);

        // Pop semantics via set_top.
        s.set_top(s.base + 8);
        assert_eq!(s.depth(), 1);
        s.reset();
        assert_eq!(s.depth(), 0);
    }
}
