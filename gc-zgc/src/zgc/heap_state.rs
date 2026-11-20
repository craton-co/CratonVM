// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-heap state that used to live in process statics (zr z3/lt).
//!
//! # Why this module exists
//!
//! `docs/feature-designs/zgc-architecture.md`:
//! several ZGC switches were read once per PROCESS (a `static OnceLock<bool>`
//! in the reader), and several per-heap counters and one-shot latches were
//! `static` atomics. With two VMs in one process (`libcratonvm`,
//! `cratonvm-embed`, the `vm` crate's own test binary) the first heap to touch
//! one decided it for every other heap: a second VM could not run a different
//! value of `CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY`, its
//! `[GC] zgc-integrity: unsizable_registered_objects=` line counted the first
//! VM's refusals, its `targeted_pages=` counted the first VM's targets, and
//! its end-of-run metrics row was never written because the first VM had
//! already claimed the process-wide latch.
//!
//! Everything here is seeded once per heap, in `ZgcRealHeap::with_capacity`,
//! from an UNCACHED read of the switch, so a heap built under
//! `cratonvm_types::flags::with_thread_overrides` gets its own answer. The
//! cost is one environment lookup per switch per heap construction.
//!
//! # The inventory (zr z3/lt, 2026-10-12)
//!
//! The classification of every process-wide `static`, `OnceLock` and
//! `thread_local!` in `gc/src/zgc.rs`, `gc/src/zgc/*`, `gc/src/vm_heap.rs`
//! and the ZGC arms of `vm/src/runtime/interpreter/gc_and_alloc.rs` is in
//! `docs/feature-designs/zgc-architecture.md` §1. In short:
//!
//! * **(a) pure environment-switch caches** -- the ~60 `static ON/CACHED:
//!   OnceLock<bool|usize>` readers. Benign: the value is fixed for the run and
//!   is the same for every VM in a process unless the embedder changes the
//!   environment between VMs, which `-XX:` layering does not do. The ones a
//!   heap SEEDS at construction are uncached (their latch decided the seed
//!   for every later heap); the ones read on a hot path stay latched. zr
//!   z4/ab finished the construction seeds rv4 listed
//!   (`zgc-zr4rv4-latched-construction-seeds-FIXED-20261019.md`): the pause target,
//!   the alloc trigger, `CONC_START`, `MARK_ROOT_FILTER_CONC`,
//!   `HEADROOM_BYPASSES_REARM`, `TLAB_FLAG_PUBLISH_FALSE` (retired in zt
//!   t12/rt) and `MARK_CTX_DIRECT` (now a [`ZHeapLocalState`] seed). Still latched,
//!   because they are also read per cycle: `CRATONVM_ZGC_NO_JIT_READ_BOUNDS`
//!   (`set_barrier_color`, `collect_garbage`) and `CRATONVM_ZGC_TRIGGER_SHADOW`
//!   (`record_metrics_for_cycle`).
//! * **(b) per-heap state** -- moved into [`ZHeapLocalState`] (below), into
//!   `ZgcCounters`' existing per-heap seeds, into
//!   `remembered::ZRememberedSetTable` and into `mark::ZMarkShared`.
//! * **(c) genuine process resources** -- the GPU critical-section registry,
//!   the barrier fallback stats, the mark-buffer registries and the
//!   thread-locals keyed by registry id (which already tell two heaps apart).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The per-heap half of what used to be process statics. Owned by
/// `ZgcCounters::local`; see the module header.
#[derive(Debug)]
pub(super) struct ZHeapLocalState {
    /// `CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY` (zr z1/rl, default ON).
    pub(super) rewrite_reference_cells_only: AtomicBool,
    /// `CRATONVM_ZGC_PAR_RELOCATE_BALANCE` (zr z1/rl, default ON).
    pub(super) par_relocate_balance: AtomicBool,
    /// `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY` (zr z1/gn, diagnostic, default OFF).
    pub(super) gen_old_bitmap_verify: AtomicBool,
    /// `CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE` (zr z1/gn, default ON).
    pub(super) gen_slide_old_live: AtomicBool,
    /// Registered objects this heap's sweep refused to size (was the process
    /// static `ZGC_UNSIZABLE_OBJECTS`). See `ZgcRealHeap::alloc_size`.
    pub(super) unsizable_objects: AtomicUsize,
    /// One-shot latch for the warning that goes with the count above (was
    /// `ZGC_UNSIZABLE_WARNED`).
    pub(super) unsizable_warned: AtomicBool,
    /// Pages admitted to a relocation set because they fell in a window an
    /// allocation failure named (was `forwarding::TARGETED_PAGES_SELECTED`).
    pub(super) targeted_pages_selected: AtomicUsize,
    /// Power-of-two rate limiter of `warn_alloc_failed_once` (was a
    /// function-local `static FAILURES`).
    pub(super) alloc_failure_warnings: AtomicUsize,
    /// `frag_report_once`'s two-report budget (was a function-local
    /// `static REPORTS`).
    pub(super) frag_reports: AtomicUsize,
    /// `emit_end_of_run_metrics`' claim (was a function-local
    /// `static EMITTED`, "once per process" -- while the TSV contract it
    /// guards is one row per VM).
    pub(super) end_of_run_metrics_emitted: AtomicBool,
    /// `CRATONVM_ZGC_MARK_CTX_DIRECT` (default ON), read per mark cycle by
    /// `mark_context_arc`. zr z4/ab (rv4 X2): seeded here instead of a
    /// process latch.
    pub(super) mark_ctx_direct: AtomicBool,
}

impl ZHeapLocalState {
    /// Seed every switch from an uncached read (see the module header).
    pub(super) fn from_env() -> Self {
        use cratonvm_types::flags::runtime_flag_default_on;
        Self {
            rewrite_reference_cells_only: AtomicBool::new(true),
            par_relocate_balance: AtomicBool::new(true),
            gen_old_bitmap_verify: AtomicBool::new(super::zgc_flag_default(
                "CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY",
                false,
            )),
            gen_slide_old_live: AtomicBool::new(super::zgc_flag_default(
                "CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE",
                true,
            )),
            unsizable_objects: AtomicUsize::new(0),
            unsizable_warned: AtomicBool::new(false),
            targeted_pages_selected: AtomicUsize::new(0),
            alloc_failure_warnings: AtomicUsize::new(0),
            frag_reports: AtomicUsize::new(0),
            end_of_run_metrics_emitted: AtomicBool::new(false),
            mark_ctx_direct: AtomicBool::new(super::zgc_mark_ctx_direct()),
        }
    }

    #[inline]
    pub(super) fn mark_ctx_direct(&self) -> bool {
        self.mark_ctx_direct.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn rewrite_reference_cells_only(&self) -> bool {
        self.rewrite_reference_cells_only.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn par_relocate_balance(&self) -> bool {
        self.par_relocate_balance.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn gen_old_bitmap_verify(&self) -> bool {
        self.gen_old_bitmap_verify.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn gen_slide_old_live(&self) -> bool {
        self.gen_slide_old_live.load(Ordering::Relaxed)
    }
}

/// `CRATONVM_ZGC_METRICS_ROW_PER_HEAP` (zr z3/lt): **default ON** -- the
/// end-of-run metrics summary and TSV row are claimed once per HEAP, and the
/// `Drop` backstop emits only for a heap that ran at least one collection (a
/// sizing probe or a replaced init-time heap has no row to write, and with a
/// per-heap claim it would otherwise write an empty one). `=0` restores the
/// process-wide claim (the first heap to emit, or to be dropped, suppresses
/// every later heap's row).
pub(super) fn zgc_metrics_row_per_heap() -> bool {
    true
}

/// The process-wide claim `CRATONVM_ZGC_METRICS_ROW_PER_HEAP=0` restores.
pub(super) static PROCESS_METRICS_ROW_EMITTED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
mod tests;
