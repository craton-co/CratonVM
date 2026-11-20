// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Unified heap abstraction for the VM.
//!
//! `VmHeap` wraps the available GC implementations (GenerationalHeap, G1Collector,
//! and ZgcRealHeap when enabled) behind a single type, so the VM code doesn't
//! need to be generic or use trait objects.

use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc_gen::concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState};
use cratonvm_gc_g1::g1::{G1Collector, G1CollectorConfig};
use cratonvm_gc_g1::g1_concurrent::ConcurrentMarkController;
use cratonvm_gc_core::gc::GcResult;
use cratonvm_gc_gen::gen_heap::GenerationalHeap;
use cratonvm_gc_core::heap::{ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, HEADER_SIZE};
use cratonvm_gc_gen::old_gen::OldGen;
use cratonvm_gc_core::satb::SatbQueue;
#[cfg(feature = "zgc")]
use cratonvm_gc_zgc::zgc::ZgcRealHeap;
use cratonvm_types::{ClassId, ObjectRef, Value};
use parking_lot::Mutex;
use std::sync::Arc;
#[cfg(test)]
mod gci_i18sw_g1_pool_split_tests;
/// GPU critical-section coordination, at the one dispatcher every
/// collector goes through.
///
/// AUDIT 2026-09-02. `cratonvm_cuda_bridge::critical` — owned tokens,
/// leases, a bounded collector wait, keep-alive roots that survive a
/// move — existed for six weeks with no caller in this crate or the VM.
/// The collectors waited on a bare counter, forever; the VM held that
/// counter from dispatch to writeback, so collection stopped for the
/// length of every kernel; and ZGC, the default collector, did not wait
/// at all, so its compacting slide could run under a device-to-host copy
/// landing in the arena from the completion reaper, a thread the
/// stop-the-world barrier never stops.
///
/// This module is the wiring. Before a cycle:
///
/// 1. wait, bounded by [`cratonvm_cuda_bridge::critical::collector_wait_budget`],
///    for every token that declared
///    [`Relocation::Forbidden`](cratonvm_cuda_bridge::critical::Relocation::Forbidden)
///    — the short windows in which a DMA reads or writes the heap arena in
///    place. A keep-alive-only token, the kind a submission holds for the
///    life of its kernel, is NOT waited for: that is the whole point.
/// 2. if the wait expired, veto relocation for this cycle
///    ([`gpu_relocation_forbidden`]); the collectors take their non-moving
///    path and the diversion is counted.
/// 3. otherwise declare a moving cycle to the registry, so a `Forbidden`
///    acquisition from an unstopped thread blocks until the cycle ends.
/// 4. splice every outstanding token's keep-alive addresses in as roots,
///    so a writeback target that is reachable from nothing else survives
///    and is remapped.
///
/// After the cycle the remapped addresses are written back through
/// [`Registry::remap_keepalive`](cratonvm_cuda_bridge::critical::Registry::remap_keepalive),
/// and a holder reads them out through `CriticalToken::keepalive_addrs`
/// before it writes.
#[cfg(feature = "gpu-offload")]
pub use cratonvm_gc_core::gpu_critical::gpu_coordination;

macro_rules! dispatch {
    ($self:expr, $method:ident ( $($arg:expr),* ) ) => {
        match $self {
            VmHeapInner::Generational(h) => h.$method($($arg),*),
            VmHeapInner::G1(h) => h.$method($($arg),*),
            #[cfg(feature = "zgc")]
            VmHeapInner::Zgc(h) => h.$method($($arg),*),
        }
    };
}
#[cfg(test)]
mod header_if_object_start_tests;

#[cfg(test)]
mod collection_time_tests;

#[cfg(test)]
mod tlab_guard_line_tests;

#[cfg(all(test, feature = "gpu-offload"))]
#[path = "../../../gc-core/src/gpu_coordination_tests.rs"]
mod gpu_coordination_tests;

// ─── Task #56: ConcurrentMarkController wiring into VmHeap ──────────────
//
// Exercises the start/complete coordinator: that g1_start_concurrent_mark
// actually spawns a controller, that g1_signal_marking_complete actually
// joins it, that the SATB barrier is captured during the concurrent
// phase, and that the edge cases (re-entry, defensive no-op, repeated
// cycles) behave as documented.

#[cfg(test)]
mod gc_summary_key_tests;
#[cfg(test)]
mod concurrent_mark_controller_tests;

#[cfg(test)]
mod pin_capability_tests;

/// JIT round 23 wave 1, lane L4 (w17-L2 item 2): `load_ref_field_compact`
/// answers what `get_field` answers for a compact reference field, and
/// declines every other shape.
#[cfg(test)]
mod r23_w1_l4_load_ref_field_compact_tests;

/// [`VmHeap::copy_compact_object_fields`]: the copy is the slot-by-slot copy,
/// on every backend, and a refused copy writes nothing.
/// Filter: `cargo test -j 5 -p cratonvm-gc --lib compact_clone_copy_tests`.
#[cfg(test)]
mod compact_clone_copy_tests;

/// The stack-walker residual-costs page, item 2: a batch holding references
/// (`VmHeap::try_set_compact_fields`) leaves each slot exactly as the scalar
/// `set_field_as` leaves it, on every collector, and declines (writing
/// nothing) where the scalar accessor would box or the plan does not hold.
#[cfg(test)]
mod compact_mixed_batch_tests;

#[cfg(test)]
mod compact_primitive_batch_tests;

#[cfg(test)]
mod gcj2jt2_post_only_completion_tests;

mod compact_copy_plan;
#[allow(unused_imports)]
pub use compact_copy_plan::*;
#[allow(unused_imports)]
use compact_copy_plan::*;
mod as_collector;
#[allow(unused_imports)]
pub use as_collector::*;
#[allow(unused_imports)]
use as_collector::*;
mod soft_ref_policy_free_mb;
#[allow(unused_imports)]
pub use soft_ref_policy_free_mb::*;
#[allow(unused_imports)]
use soft_ref_policy_free_mb::*;
mod render_tlab_guard_line;
#[allow(unused_imports)]
pub use render_tlab_guard_line::*;
#[allow(unused_imports)]
use render_tlab_guard_line::*;

pub mod decision_report;

#[cfg(test)]
mod gch_h4g1_tests;

#[cfg(test)]
mod zgc_isolation_xms_tests;

#[cfg(test)]
mod jr20_w18_gci_ledger_veto_tests;

#[cfg(test)]
mod cross_crate_tests;
