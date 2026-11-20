// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r5w5/old9 (2026-09-27) .. gcg g6/og (2026-10-02).
//!
//! This file held the O(live) in-place sweep's old-gen half
//! (`OldGen::close_live_set_by_oracle`, `OldGen::sweep_dead_runs_around`,
//! `LiveClosure`, `LiveSweepOutcome`, `LiveSweepRefusal`, the
//! `note_live_sweep_*` counters). The sweep itself (`CRATONVM_GC_OLD_LIVE_SWEEP`)
//! was deleted by gcg g5/og -- it lost to the walked collection on every
//! measured shape (`docs/feature-designs/generative-gc-architecture.md`)
//! -- and gcg g6/og deleted this now-unused API. What remains is the debug
//! walk-gap plant, which the walked collection's gap recovery still uses.

use super::OldGen;

impl OldGen {
    /// DEBUG ONLY — make a WALK GAP on purpose
    /// (`CRATONVM_DBG_OLD_PLANT_WALK_BREAK`, see
    /// `GenerationalHeap::after_old_gen_collection`): allocate one small block
    /// nobody references and give it a header no walk can size (an invalid
    /// kind tag), so every later walk of its allocated region stops there and
    /// reports the rest of the region as a gap — the heap anomaly
    /// (`SCAN_REGION_BREAK_HITS` / `WALK_DESYNC_HITS`) the walk-gap
    /// recoveries exist for, which nothing else can produce on demand
    /// (`gengc-r4w5-oldcompact5-concurrent-remark-refuses-its-sweep-over-a-walk-gap`).
    ///
    /// Harmless to live data by construction: the block is fresh, zeroed and
    /// unreferenced, so no object and no reference is behind its header; the
    /// LIVE objects the gap hides are never freed (the walked sweep's gap
    /// recovery keeps gap bytes, the compactors refuse a walk that does not
    /// cover `used()`). No walked collection ever frees the block (no walk can
    /// size it). Planted at most once per generation. Returns the block's
    /// address, or `None` (already planted, or no room).
    pub fn plant_walk_break(&mut self) -> Option<usize> {
        if self.sizing.walk_breaks_planted > 0 {
            return None;
        }
        let p = self.alloc(64, 8)?;
        // SAFETY: `p` is a fresh, zeroed, 8-aligned block of 64 bytes owned
        // by nobody; the kind-tag byte lies inside its first header.
        unsafe { std::ptr::write(p.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };
        self.sizing.walk_breaks_planted = 1;
        Some(p as usize)
    }
}

#[cfg(test)]
mod tests;
