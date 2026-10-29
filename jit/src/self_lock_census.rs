// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 (lane selflock19): the per-VM census of compiled
//! `synchronized` methods (self-locking bodies) and of the monitor holds the
//! exceptional-frame drop sites release.
//!
//! Two proposals share one table:
//!
//! * **SYNCDOOR618-2** (`jit-r18-syncdoor6-proposals.md`): why a
//!   method-entry single-pass compile of an `ACC_SYNCHRONIZED` method did or
//!   did not get a SELF-LOCKING body, by cause. The method-level refusals are
//!   the causes of `lib.rs` `self_lock_admission_refusal`; the body-check
//!   refusals are the bail sites `x64/driver.rs` records when it refuses the
//!   finished body (`self-lock-open-exit`, `self-lock-handler-exits`,
//!   `self-lock-blocks-shape`, and since JIT round 19 wave 3
//!   `self-lock-handler-mode-exits`; since j27 wave 4
//!   `self-lock-handler-shared-exits`). A refused method keeps the wrapped entry, so
//!   every compiled caller's call of it pays the helper door (SD17-5's
//!   population).
//! * **SDB17-2** (`jit-backlog-triage-r18-CLOSED-20261012.md`): the monitor-leak
//!   census of the exceptional hand-over. `drop-released-*` counts the holds
//!   the VM's drop sites released for a frame they took and will not resume
//!   (`deopt_resume.rs` `CompiledLocksOfAStash::release_for_a_dropped_frame`);
//!   `drop-unreleased-frames` counts frames that named a compiled hold and
//!   were dropped WITHOUT a release (a `keep` frame of a live OSR activation,
//!   a frame below a door's entry floor). On a battery the second row is the
//!   leak signal: it must stay 0 unless a live interpreter frame accounts for
//!   the hold.
//!
//! Per VM: the table lives in the VM's [`crate::JitVerdictRegistry`], so it
//! is not a process static (`process_global_statics_ratchet`). Counted
//! unconditionally (one relaxed add on a compile or on a cold drop path,
//! never per call); printed once at exit on the `CRATONVM_DBG=jit-method-stats`
//! dump (`tiered::dump_method_stats_to_stderr`), and only when a row is
//! non-zero.

use std::sync::atomic::{AtomicU64, Ordering};

/// A row of [`SelfLockCensus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum SelfLockRow {
    /// A method-entry single-pass compile of an `ACC_SYNCHRONIZED` method
    /// (every row up to [`Self::RefusedMemo`] partitions these).
    EntryCompiles,
    /// The method-level admission accepted it (`self_lock_admits_method`).
    Admitted,
    /// `static synchronized` with no mirror slot handed to the compile yet.
    RefusedStaticNoSlot,
    /// `static synchronized` with `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC=0`.
    RefusedStaticSwitchOff,
    /// `<init>` / `<clinit>`.
    RefusedInit,
    /// An exception table with `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS=0`.
    RefusedTableHandlersOff,
    /// An exception table whose ranges cover a `*return`.
    RefusedTableCoversReturn,
    /// `CRATONVM_JIT_SELF_LOCKING_SYNC=0`.
    RefusedSwitchOff,
    /// Code the strict decoder cannot walk.
    RefusedUndecodable,
    /// `monitorenter` / `monitorexit` the blocks rule does not admit (blocks
    /// switch off, a `static` method without its sub-switch, unbalanced).
    RefusedMonitorBytecode,
    /// `jsr` / `ret`.
    RefusedJsrRet,
    /// `invokedynamic` with `CRATONVM_JIT_SELF_LOCKING_SYNC_INDY=0`.
    RefusedIndy,
    /// A store to local 0 of an instance method.
    RefusedStoreLocal0,
    /// A loop with `CRATONVM_JIT_SELF_LOCKING_SYNC_LOOPS=0`.
    RefusedLoop,
    /// A loop that indexes an array without the array-loop hand-over.
    RefusedArrayLoop,
    /// Admitted, but this VM's backend already refused its body (the
    /// refusal memo), or the backend request dropped it.
    RefusedMemo,
    /// The backend published a self-locking body.
    BodyPublished,
    /// The body check refused a table-free body (open exits).
    BodyRefusedOpenExits,
    /// The body check refused a body with a table (an exit not offered to the
    /// in-frame handlers, or one that does not hand the hold over).
    BodyRefusedHandlerExits,
    /// JIT round 19 wave 3 (lane syncdoor3, SYNCDOOR3-1): a body with a table
    /// refused ONLY for mode exits that hand the hold over, which
    /// `CRATONVM_JIT_SELF_LOCK_HANDLERS_MODE_EXITS` admits (counted instead of
    /// [`Self::BodyRefusedHandlerExits`]).
    BodyRefusedHandlerModeExits,
    /// j27 wave 4 (lane MONROWS, SDB17-3): a body with a table refused under
    /// the exceptional hand-over ONLY for protected shared sentinel exits /
    /// reason-3 throws its local-handler sites offer first, the population a
    /// hand-over twin for those exits would serve (counted instead of
    /// [`Self::BodyRefusedHandlerExits`]).
    BodyRefusedHandlerSharedExits,
    /// The body check refused a body with `synchronized` blocks whose shape is
    /// not described, or with the blocks / hand-over switches off.
    BodyRefusedBlocksShape,
    /// Refused up front: an `invokedynamic` row lowers to the reason-8 trap.
    BodyRefusedIndyTrap,
    /// Refused by any other backend bail (or published wrapped).
    BodyRefusedOther,
    /// Exceptional frames a VM drop site took and released holds for.
    DropReleasedFrames,
    /// The single releases those drops made.
    DropReleasedHolds,
    /// Frames naming a compiled hold that a drop site dropped WITHOUT a
    /// release (`keep`, below the entry floor). The leak signal.
    DropUnreleasedFrames,
    /// JIT round 19 proposal wave 6, lane sync6 (SELFLOCK19-3): inflated
    /// monitors a terminating Java thread still owned, which the VM's
    /// thread-death sweep force-released (`MonitorTable::
    /// release_monitors_held_by_except`; its own `Thread` mirror aside). Java
    /// code cannot leave a monitor held at thread exit, so any count is a
    /// leak from SOME path, compiled or not. Folded in at exit
    /// (`deopt_resume::report_inlined_chain_census_at_exit`).
    MonitorsHeldAtThreadExit,
    /// JIT round 22 wave 8, lane sync (the positive-control page
    /// `r22-w7-selflock-census-positive-control-unreached-FIXED-20261020.md`): the
    /// chain from a hand-over to a drop, one row per link, so one run tells
    /// where it stops. Link 1, compile time: a published self-locking body
    /// whose precise exceptional exits HAND the method monitor over (it has
    /// an exception table and `crate::self_lock_exc_handover_enabled`).
    ExcHandoverBodies,
    /// Link 2: an exceptional frame naming a compiled hold reached a VM sink
    /// that adopts or releases the handed method monitor
    /// (`CompiledLocksOfAStash::pin_compiled_locks_of_method`).
    HoldFramesTaken,
    /// Link 3: a compiled catch found stashed exceptional frames above its
    /// floor (`helpers.rs` `drop_orphaned_exceptional_frames_at_compiled_catch`),
    /// whatever the drop gate says.
    CatchStashAboveFloor,
    /// Link 4: frames the orphan-drop walk took there (each then goes to
    /// `release_for_a_dropped_frame`, which counts the drop rows above).
    OrphanDropFrames,
    /// JIT round 22 wave 9, lane sync: which VM sink took a frame that HANDS a
    /// method monitor over. The door drain (`exception_dispatch.rs`
    /// `route_jit_signal_exception`): the compiled method returned to an
    /// interpreter / helper door with the exception pending, so the frame never
    /// waited beneath a compiled catch.
    DoorDrainHandovers,
    /// The callee-handler claim (`exception_dispatch.rs`
    /// `precise_handler_frame_for`): a dispatch helper found a handler of the
    /// throwing method itself.
    CalleeClaimHandovers,
}

/// Number of rows.
const ROWS: usize = SelfLockRow::CalleeClaimHandovers as usize + 1;

/// The printed name of each row, in [`SelfLockRow`] order.
const ROW_NAMES: [&str; ROWS] = [
    "entry-compiles",
    "admitted",
    "ref-static-no-slot",
    "ref-static-switch-off",
    "ref-init",
    "ref-table-handlers-off",
    "ref-table-covers-return",
    "ref-switch-off",
    "ref-undecodable",
    "ref-monitor-bytecode",
    "ref-jsr-ret",
    "ref-indy",
    "ref-store-local0",
    "ref-loop",
    "ref-array-loop",
    "ref-memo",
    "body-published",
    "body-ref-open-exits",
    "body-ref-handler-exits",
    "body-ref-handler-mode-exits",
    "body-ref-handler-shared-exits",
    "body-ref-blocks-shape",
    "body-ref-indy-trap",
    "body-ref-other",
    "drop-released-frames",
    "drop-released-holds",
    "drop-unreleased-frames",
    "monitors-held-at-thread-exit",
    "exc-handover-bodies",
    "hold-frames-taken",
    "catch-stash-above-floor",
    "orphan-drop-frames",
    "door-drain-handovers",
    "callee-claim-handovers",
];

/// The census table of one VM. See the module doc.
pub struct SelfLockCensus {
    rows: [AtomicU64; ROWS],
}

// Written out: `Default` is derived for arrays of at most 32 elements, and the
// table has more rows than that since JIT round 22 wave 9.
impl Default for SelfLockCensus {
    fn default() -> Self {
        Self { rows: std::array::from_fn(|_| AtomicU64::new(0)) }
    }
}

impl SelfLockCensus {
    /// Count one event of `row`.
    pub fn note(&self, row: SelfLockRow) {
        self.note_n(row, 1);
    }

    /// Count `n` events of `row`.
    pub fn note_n(&self, row: SelfLockRow, n: u64) {
        if n != 0 {
            self.rows[row as usize].fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The count of `row`.
    pub fn get(&self, row: SelfLockRow) -> u64 {
        self.rows[row as usize].load(Ordering::Relaxed)
    }

    /// `(name, count)` for every row, zeroes included.
    pub fn rows(&self) -> Vec<(&'static str, u64)> {
        ROW_NAMES
            .iter()
            .zip(self.rows.iter())
            .map(|(n, c)| (*n, c.load(Ordering::Relaxed)))
            .collect()
    }

    /// The `[cratonvm] self-lock census:` exit line, or `None` when every row
    /// is zero (no synchronized method was compiled and no drop site saw a
    /// hold). Rows are relaxed samples.
    pub fn line(&self) -> Option<String> {
        let rows = self.rows();
        if rows.iter().all(|(_, v)| *v == 0) {
            return None;
        }
        let body = rows
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        Some(format!("[cratonvm] self-lock census: {body}"))
    }

    /// The body-check row for the bail site `x64/driver.rs` recorded when it
    /// refused a self-locking body (`None`: no site recorded).
    pub fn body_refusal_row(site: Option<&str>) -> SelfLockRow {
        match site {
            Some("self-lock-open-exit") => SelfLockRow::BodyRefusedOpenExits,
            Some("self-lock-handler-exits") => SelfLockRow::BodyRefusedHandlerExits,
            Some("self-lock-handler-mode-exits") => SelfLockRow::BodyRefusedHandlerModeExits,
            Some("self-lock-handler-shared-exits") => SelfLockRow::BodyRefusedHandlerSharedExits,
            Some("self-lock-blocks-shape") => SelfLockRow::BodyRefusedBlocksShape,
            _ => SelfLockRow::BodyRefusedOther,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SelfLockCensus, SelfLockRow, ROWS, ROW_NAMES};

    #[test]
    fn every_row_has_one_name_and_the_last_row_closes_the_table() {
        assert_eq!(ROW_NAMES.len(), ROWS);
        let mut names = ROW_NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ROWS, "row names are unique");
    }

    #[test]
    fn an_untouched_census_prints_nothing_and_a_count_prints_every_row() {
        let census = SelfLockCensus::default();
        assert!(census.line().is_none());
        census.note(SelfLockRow::EntryCompiles);
        census.note_n(SelfLockRow::DropReleasedHolds, 2);
        census.note_n(SelfLockRow::DropUnreleasedFrames, 0);
        assert_eq!(census.get(SelfLockRow::DropReleasedHolds), 2);
        let line = census.line().expect("a non-zero row prints");
        assert!(line.starts_with("[cratonvm] self-lock census: entry-compiles=1 "));
        assert!(line.contains(" drop-released-holds=2 "));
        assert!(line.contains(" drop-unreleased-frames=0 "));
        assert!(line.contains(" monitors-held-at-thread-exit=0 "));
        // JIT round 22 wave 8: the chain rows follow, in link order.
        assert!(line.ends_with(
            " exc-handover-bodies=0 hold-frames-taken=0 catch-stash-above-floor=0 orphan-drop-frames=0 door-drain-handovers=0 callee-claim-handovers=0"
        ));
        assert_eq!(census.rows().len(), ROWS);
    }

    #[test]
    fn the_driver_bail_sites_map_to_their_rows() {
        let row = SelfLockCensus::body_refusal_row;
        assert_eq!(
            row(Some("self-lock-open-exit")),
            SelfLockRow::BodyRefusedOpenExits
        );
        assert_eq!(
            row(Some("self-lock-handler-exits")),
            SelfLockRow::BodyRefusedHandlerExits
        );
        assert_eq!(
            row(Some("self-lock-blocks-shape")),
            SelfLockRow::BodyRefusedBlocksShape
        );
        assert_eq!(
            row(Some("self-lock-handler-mode-exits")),
            SelfLockRow::BodyRefusedHandlerModeExits
        );
        assert_eq!(
            row(Some("self-lock-handler-shared-exits")),
            SelfLockRow::BodyRefusedHandlerSharedExits
        );
        assert_eq!(
            row(Some("implicit-null-unbound")),
            SelfLockRow::BodyRefusedOther
        );
        assert_eq!(row(None), SelfLockRow::BodyRefusedOther);
    }
}
