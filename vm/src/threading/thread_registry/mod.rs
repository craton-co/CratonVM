// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Registry for tracking all JVM threads.
//!
//! The `ThreadRegistry` keeps track of spawned threads, their Java Thread
//! objects, join handles, and liveness status. It also hands out unique
//! `ThreadId` values.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::threading::jvm_thread::{GcBlockState, ParkState, ThreadId};
use crate::types::ObjectRef;

// ---------------------------------------------------------------------------
// CRATONVM_DBG_THREADREG_PERF — cumulative cost instrumentation for the
// full-registry-walk functions called on every GC / STW pause
// (`collect_all_root_snapshots`, `alive_count_and_os_tids`,
// `alive_count_blocked_and_os_tids`). Investigating a CratonVM-specific
// per-alive-thread VM overhead gap (Cluster B,
// http-client-connector-teardown-hang-crash-FIXED.md):
// real HotSpot finishes a test class that briefly accumulates ~750 mostly-
// idle threads in 6s; CratonVM takes 25-300+s for the identical thread
// count. This measures which of these O(N) walkers actually dominates
// wall-clock, rather than guessing from a stack dump. Zero cost when unset
// (one load of a latched `OnceLock` per call to check the gate; gcj j1/cm:
// the registry size is now asked only when the gate is on -- it was a
// `threads.read()` per call, inside the STW census's `inner` hold).
mod threadreg_perf {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::Instant;

    fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .map(|v| {
                    let t = v.trim();
                    !t.is_empty() && t != "0" && !t.eq_ignore_ascii_case("false")
                })
                .unwrap_or(false)
        })
    }

    pub struct Counters {
        calls: AtomicU64,
        nanos: AtomicU64,
        entries: AtomicU64,
        name: &'static str,
        started_report: AtomicBool,
    }

    impl Counters {
        pub const fn new(name: &'static str) -> Self {
            Self {
                calls: AtomicU64::new(0),
                nanos: AtomicU64::new(0),
                entries: AtomicU64::new(0),
                name,
                started_report: AtomicBool::new(false),
            }
        }

        /// Time a closure, accumulating (call count, nanos, entries scanned).
        /// `entries` is the registry size AT THIS CALL (not a running total) —
        /// used to report an average N alongside cumulative cost, and asked
        /// (`entries()`) only when the gate is on. Prints a
        /// running summary every 50 calls so a long test run shows progress
        /// without waiting for process exit (which this diagnostic-only VM
        /// mostly doesn't reach cleanly on a hang anyway).
        #[inline]
        pub fn time<R>(&self, entries: impl FnOnce() -> usize, f: impl FnOnce() -> R) -> R {
            if !enabled() {
                return f();
            }
            let entries = entries();
            let start = Instant::now();
            let r = f();
            let elapsed = start.elapsed().as_nanos() as u64;
            let calls = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            let nanos = self.nanos.fetch_add(elapsed, Ordering::Relaxed) + elapsed;
            let total_entries =
                self.entries.fetch_add(entries as u64, Ordering::Relaxed) + entries as u64;
            if calls == 1 || calls % 50 == 0 {
                self.started_report.store(true, Ordering::Relaxed);
                eprintln!(
                    "[threadreg-perf] {} calls={} total_ms={:.1} avg_us={:.1} avg_registry_size={:.0} last_registry_size={}",
                    self.name,
                    calls,
                    nanos as f64 / 1_000_000.0,
                    (nanos as f64 / 1000.0) / calls as f64,
                    total_entries as f64 / calls as f64,
                    entries,
                );
            }
            r
        }
    }
}

#[cfg(test)]
mod tests;

/// The inline thin-lock's half of the lock-stack contract (round 11 wave 6):
/// compiled code writes [`JmxLockStack`] through the raw
/// `cratonvm_jit_api::JIT_LOCK_STACK_*` offsets, so these tests write it the
/// same way and require every reader to agree.
#[cfg(test)]
mod jit_inline_lock_stack_tests;

/// Round 12 wave 1 (lane lock): `Thread.getState()` of a thread inside a
/// contended monitor entry that has not entered the GC-blocked region -- the
/// opt-in running park, `CRATONVM_MONITOR_LAZY_PARK_US` -- is BLOCKED, and it
/// is RUNNABLE again once the entry completes.
#[cfg(test)]
mod r12_lock_block_state_tests;

/// Interpreter round i2 wave 66, lane L1 (`CRATONVM_DBG_UNRECORDED_WAIT_KINDS`):
/// the census of blocking regions that open with no recorded wait kind.
#[cfg(test)]
mod i66_unrecorded_wait_census_tests;

/// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
/// a stand-in region's published waiting monitor.
#[cfg(test)]
mod i68_region_waiting_monitor_tests;

/// nio21 wave 5 L1 (`CRATONVM_INTERRUPT_LOCK`): the striped interrupt locks.
#[cfg(test)]
mod nio21_interrupt_lock_tests;

/// Interpreter round i3 wave 74, lane L6: the thread-start window of page
/// `i73-L6-unpark-in-the-thread-start-window-is-lost-20261106`.
#[cfg(test)]
mod i74_start_window_tests;

#[cfg(test)]
mod sync619_published_compiled_hold_tests;

/// gci i12/vp: the parked-continuation fold filter
/// (`CRATONVM_GC_VT_PARKED_FOLD_FILTER`) and its census. Filter:
/// `cargo test -p cratonvm-vm gci_i12vp`.

/// JIT round 21 lane refs, REFS21-6: `park_credited_os_tids` credits only a
/// blocked, armed park whose deposit scanned its compiled chain.
#[cfg(test)]
mod refs21_park_credit_tests;

#[cfg(test)]
mod gci_i12vp_tests;

/// gci i13/vg: the split root gather (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`),
/// the gather half of
/// `gci-i12vp-parked-continuations-folded-by-every-young-pause-FIXED-20261016`.
/// Filter: `cargo test -p cratonvm-vm gci_i13vg`.
#[cfg(test)]
mod gci_i13vg_tests;

/// gci i14/ve: the mirror-keyed gather (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`),
/// item 6 of `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`.
/// Filter: `cargo test -p cratonvm-vm gci_i14ve`.
#[cfg(test)]
mod gci_i14ve_tests;

/// gci i15/vh: the G1 half of the mirror-keyed gather
/// (`CRATONVM_GC_VT_PARKED_MIRROR_KEYED`), item 6 of
/// `gci-i11vt-virtual-thread-continuation-roots-review-findings-FIXED-20261016`.
/// Filter: `cargo test -p cratonvm-vm gci_i15vh`.
#[cfg(test)]
mod gci_i15vh_tests;

/// gci i16/mr: the G1 remark closes every `metadata_pin` row over the
/// completed mark on the default path
/// (`gci-i15vh-g1-marking-scan-rows-filed-while-the-marker-runs-FIXED-20261017`).
/// Filter: `cargo test -j 5 -p cratonvm-vm gci_i16mr`.
#[cfg(test)]
mod gci_i16mr_tests;

/// gci i17/hw (`gci-i15orch-g1-parked-virtual-thread-holder-graph-reads-wrong-on-the-default-path-RETIRED-UNFIXED-20261018`):
/// the parked-continuation chain (`fold_one_blocked_chain` per pause, one
/// lookup per frame slot at the remount's `check_post_block_gc`) keeps a
/// frame-held address on ITS object across the G1 shapes the page asks about:
/// a mixed pause moving the Old holder, the evacuation-failure drain's
/// composed map (a pass-2 copy lands on a pass-1 key, the recycled
/// address another frame object vacated), and a later move of both.
#[cfg(test)]
mod gci_i17hw_tests;

/// gci i18/hw: a virtual thread's continuation deposits precisely on G1
/// (`continuation_deposit_is_precise`, `CRATONVM_G1_VT_PRECISE_DEPOSIT`).
#[cfg(test)]
mod gci_i18hw_tests;

/// gci i19/vd: the precise G1 continuation deposit covers exactly the
/// continuations that never entered compiled code
/// (`continuation_is_interpreted_only`,
/// `GcBlockState::continuation_entered_compiled`).
#[cfg(test)]
mod gci_i19vd_tests;

#[cfg(test)]
mod w95_l6_continuation_one_publish_tests;

#[cfg(test)]
mod gci_i20vh_tests;

#[cfg(test)]
mod ki3_covered_chain_tests;

#[cfg(test)]
mod gci_i21pr_tests;

#[cfg(test)]
mod i133_l7_attach_order_tests;

/// [`NATIVE_SLOTS_DUPLICATE_CAPTURES`], for a test. Test-only since JIT round
/// 22 wave 7 (lane review): no census line ever read it through this getter.
#[cfg(test)]
pub(crate) fn native_slot_duplicate_captures() -> u64 {
    NATIVE_SLOTS_DUPLICATE_CAPTURES.load(Ordering::Relaxed)
}

mod handshake_request;
#[allow(unused_imports)]
pub use handshake_request::*;
#[allow(unused_imports)]
use handshake_request::*;
mod deposit;
#[allow(unused_imports)]
pub use deposit::*;
#[allow(unused_imports)]
use deposit::*;
mod unrecorded_root_guard_deposits;
#[allow(unused_imports)]
pub use unrecorded_root_guard_deposits::*;
#[allow(unused_imports)]
use unrecorded_root_guard_deposits::*;
