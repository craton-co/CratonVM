// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-heap state that used to live in process statics (zr z3/lt).
//!
//! # Why this module exists
//!
//! `docs/internal/gc/zgc-zr2rv-process-wide-latches-and-the-watched-set-FIXED-20261012.md`:
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
//! `docs/internal/zgc-round-zr-20261012/z3-lt-report.md` §1. In short:
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
//!   `HEADROOM_BYPASSES_REARM`, `TLAB_FLAG_PUBLISH_FALSE` and
//!   `MARK_CTX_DIRECT` (now a [`ZHeapLocalState`] seed). Still latched,
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
            rewrite_reference_cells_only: AtomicBool::new(runtime_flag_default_on(
                "CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY",
            )),
            par_relocate_balance: AtomicBool::new(runtime_flag_default_on(
                "CRATONVM_ZGC_PAR_RELOCATE_BALANCE",
            )),
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_METRICS_ROW_PER_HEAP")
}

/// The process-wide claim `CRATONVM_ZGC_METRICS_ROW_PER_HEAP=0` restores.
pub(super) static PROCESS_METRICS_ROW_EMITTED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use super::super::*;
    use cratonvm_types::flags::with_thread_overrides;

    fn heap_with(edits: &[(&str, Option<&str>)]) -> ZgcRealHeap {
        with_thread_overrides(edits, || ZgcRealHeap::with_capacity(16 * 1024 * 1024))
    }

    /// The deciding test of the zr2rv page: two heaps in one process, built
    /// under different values of a default-ON switch, each keep their own
    /// answer. With the switch latched in a process `OnceLock` (the old
    /// `zgc_rewrite_reference_cells_only`) whichever heap read first decided
    /// it for both, so one of the two assertions failed.
    #[test]
    fn two_heaps_in_one_process_keep_their_own_switches() {
        let off = heap_with(&[
            ("CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY", Some("0")),
            ("CRATONVM_ZGC_PAR_RELOCATE_BALANCE", Some("0")),
            ("CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE", Some("0")),
            ("CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY", Some("1")),
            ("CRATONVM_ZGC_GEN_YOUNG_RELOCATE", Some("1")),
            ("CRATONVM_ZGC_GEN_OLD_BITMAP", Some("0")),
        ]);
        let on = heap_with(&[
            ("CRATONVM_ZGC_REWRITE_REFERENCE_CELLS_ONLY", Some("1")),
            ("CRATONVM_ZGC_PAR_RELOCATE_BALANCE", Some("1")),
            ("CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE", Some("1")),
            ("CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY", Some("0")),
            ("CRATONVM_ZGC_GEN_YOUNG_RELOCATE", Some("0")),
            ("CRATONVM_ZGC_GEN_OLD_BITMAP", Some("1")),
        ]);
        let l_off = &off.counters.local;
        let l_on = &on.counters.local;
        assert!(!l_off.rewrite_reference_cells_only());
        assert!(l_on.rewrite_reference_cells_only());
        assert!(!l_off.par_relocate_balance());
        assert!(l_on.par_relocate_balance());
        assert!(!l_off.gen_slide_old_live());
        assert!(l_on.gen_slide_old_live());
        assert!(l_off.gen_old_bitmap_verify());
        assert!(!l_on.gen_old_bitmap_verify());
        // The g2 seeds were per-heap fields seeded from a LATCHED reader, so
        // they were per heap only through their test setters.
        assert!(off.counters.gen_young_relocate_on.load(Ordering::Relaxed));
        assert!(!on.counters.gen_young_relocate_on.load(Ordering::Relaxed));
        assert!(!off.counters.gen_old_bits_on.load(Ordering::Relaxed));
        assert!(on.counters.gen_old_bits_on.load(Ordering::Relaxed));
        // zr z4/rv4: rv3's fallback switch landed with a process latch in
        // the same wave; it seeds a per-heap field, so it must not be latched.
        let fb_off = heap_with(&[("CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR", Some("0"))]);
        let fb_on = heap_with(&[("CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR", Some("1"))]);
        assert!(!fb_off
            .counters
            .gen_bitmap_fallback_major
            .load(Ordering::Relaxed));
        assert!(fb_on
            .counters
            .gen_bitmap_fallback_major
            .load(Ordering::Relaxed));
        // The remembered-set summary switch is the table's, per heap.
        let rs_off = heap_with(&[("CRATONVM_ZGC_REMSET_SUMMARY", Some("0"))]);
        let rs_on = heap_with(&[("CRATONVM_ZGC_REMSET_SUMMARY", Some("1"))]);
        assert!(!rs_off.remembered.summary_enabled());
        assert!(rs_on.remembered.summary_enabled());
    }

    /// zr z4/ab (rv4 X2, `zgc-zr4rv4-latched-construction-seeds-FIXED-20261019.md`):
    /// the remaining construction seeds were read through a process
    /// `OnceLock`, so the second heap in a process inherited the first heap's
    /// pause target, concurrent-start window and the rest.
    #[test]
    fn two_heaps_keep_their_own_construction_seeds() {
        let a = heap_with(&[
            ("CRATONVM_ZGC_PAUSE_TARGET_MS", Some("50")),
            ("CRATONVM_ZGC_CONC_START", Some("10")),
            ("CRATONVM_ZGC_MARK_ROOT_FILTER_CONC", Some("1")),
            ("CRATONVM_ZGC_HEADROOM_BYPASSES_REARM", Some("1")),
            ("CRATONVM_ZGC_MARK_CTX_DIRECT", Some("0")),
            ("CRATONVM_ZGC_ALLOC_TRIGGER", Some("20")),
        ]);
        let b = heap_with(&[
            ("CRATONVM_ZGC_PAUSE_TARGET_MS", Some("400")),
            ("CRATONVM_ZGC_CONC_START", Some("auto")),
            ("CRATONVM_ZGC_MARK_ROOT_FILTER_CONC", Some("0")),
            ("CRATONVM_ZGC_HEADROOM_BYPASSES_REARM", Some("0")),
            ("CRATONVM_ZGC_MARK_CTX_DIRECT", Some("1")),
            ("CRATONVM_ZGC_ALLOC_TRIGGER", Some("40")),
        ]);
        assert_eq!(a.pause_target_ms.load(Ordering::Relaxed), 50);
        assert_eq!(b.pause_target_ms.load(Ordering::Relaxed), 400);
        assert!(!a.conc_start_adaptive.load(Ordering::Relaxed));
        assert!(b.conc_start_adaptive.load(Ordering::Relaxed));
        assert_ne!(
            a.conc_start_bytes.load(Ordering::Relaxed),
            b.conc_start_bytes.load(Ordering::Relaxed),
            "10 % vs the adaptive seed (60 %)"
        );
        assert!(a.conc_root_filter.load(Ordering::Relaxed));
        assert!(!b.conc_root_filter.load(Ordering::Relaxed));
        assert!(a.headroom_bypasses_rearm.load(Ordering::Relaxed));
        assert!(!b.headroom_bypasses_rearm.load(Ordering::Relaxed));
        assert!(!a.counters.local.mark_ctx_direct());
        assert!(b.counters.local.mark_ctx_direct());
        // The alloc-trigger seed is floored at `ZGC_ALLOC_TRIGGER_FLOOR`, which
        // a 16 MiB test heap sits under; ask its (now uncached) reader instead.
        let pct = |v: &str| {
            with_thread_overrides(&[("CRATONVM_ZGC_ALLOC_TRIGGER", Some(v))], || {
                zgc_alloc_trigger_percent_explicit()
            })
        };
        assert_eq!((pct("20"), pct("40")), (Some(20), Some(40)));
    }

    /// The zr2rv page's §2 called `gc_quiescence`'s watched-referent set
    /// process-wide. It is a `thread_local!`: a set published on one thread
    /// is invisible to another, so tests on different libtest threads (and
    /// two VMs collecting on different threads) cannot see each other's
    /// publish. This pins that, so a move to a process table is caught.
    #[test]
    fn the_watched_set_is_per_thread_not_per_process() {
        crate::gc_quiescence::set_watched_referents(&[0x1000, 0x2000]);
        let other = std::thread::spawn(crate::gc_quiescence::watched_referents_snapshot)
            .join()
            .expect("snapshot thread");
        assert!(
            other.is_none(),
            "another thread sees no publish of this one"
        );
        assert_eq!(
            crate::gc_quiescence::watched_referents_snapshot().map(|s| s.len()),
            Some(2)
        );
        crate::gc_quiescence::set_watched_referents(&[]);
    }

    /// A latch raised in one heap does not leak into another: the unsizable
    /// count (was the process static `ZGC_UNSIZABLE_OBJECTS`, which a second
    /// VM's `[GC] zgc-integrity:` line read as its own), the targeted-page
    /// counter and the end-of-run metrics claim (was "once per process", so
    /// the second VM's TSV row was never written).
    #[test]
    fn a_latch_in_one_heap_does_not_leak_into_another() {
        let a = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        let b = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        a.counters
            .local
            .unsizable_objects
            .fetch_add(3, Ordering::Relaxed);
        a.counters
            .local
            .targeted_pages_selected
            .fetch_add(2, Ordering::Relaxed);
        assert_eq!(a.unsizable_registered_objects(), 3);
        assert_eq!(b.unsizable_registered_objects(), 0);
        assert_eq!(a.targeted_pages_selected(), 2);
        assert_eq!(b.targeted_pages_selected(), 0);
        with_thread_overrides(&[("CRATONVM_ZGC_METRICS_ROW_PER_HEAP", None)], || {
            assert!(a.claim_end_of_run_metrics(), "the first claim of heap a");
            assert!(!a.claim_end_of_run_metrics(), "one row per heap");
            assert!(
                b.claim_end_of_run_metrics(),
                "heap b's row is not suppressed by heap a's"
            );
        });
    }
}
