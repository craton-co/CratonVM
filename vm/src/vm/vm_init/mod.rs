/// A `vm_identity` for a unit test that fakes a VM (a row in a per-VM table,
/// no `SharedVm`), drawn from the SAME counter real identities come from.
///
/// gcg g2/k (`common-w25c-proposal-test-vm-identities-from-the-real-counter-DONE-20261015`).
/// Unique against every real VM and every other test by construction, and
/// never handed out again, so no real VM's teardown (`release_vm_native_state`,
/// which forgets its identity's rows in about twenty process tables) can reach
/// a test's rows, and no test needs a private base or a table of bases. It
/// spends one layout domain that no `ClassStore` will own; nothing reads a
/// domain without a store.
#[cfg(test)]
pub(crate) fn test_vm_identity() -> usize {
    vm_identity_for_layout_domain(cratonvm_types::next_layout_domain())
}

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::classloading::resolution::{LambdaCallSite, LinkResolver, ResolutionCache};
use crate::classloading::{ClassId, ClassManager};
use crate::config::VmConfig;
use crate::config::{discover_boot_classpath, discover_ext_classpath};
use crate::error::{MethodCallFailed, MethodCallResult, VmError};
use crate::jit::profile::ProfileStore;
use crate::jit::JitCache;
use crate::memory::heap::ArrayElementType;
use crate::memory::vm_heap::{G1ConfigOverrides, GcBackend, VmHeap};
use crate::native::io::FileDescriptorTable;
use crate::native::register_essential_natives;
use crate::native::register_io_natives;
use crate::native::registry::{NativeMethodRegistry, StackTraceEntry};
use crate::native::{register_builtins, register_collections_natives};
use crate::runtime::lock_order::{LockLevel, OrderedPlMutex, OrderedPlRwLock};
use crate::runtime::stackwalker::BacktraceFrame;
use crate::threading::gc_barrier::GcBarrier;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::threading::monitor::MonitorTable;
use crate::threading::thread_registry::ThreadRegistry;
use crate::types::{ObjectRef, Value};
// `crate::classloading` is a re-export shim that does not forward
// `class_origin`, so the census row type is named through the source crate
// directly rather than by widening that shim (which another agent owns).
use cratonvm_classloading::class_origin::ClassOriginEntry;
use cratonvm_types::compat::CompatibilityMode;
#[cfg(test)]
mod gci_i45_sc_tlab_hand_off_tests;
#[cfg(test)]
mod gci_i43gy_g1_young_sizer_warning_tests;

/// gci i46/ts (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item 2, as
/// applied in `069555c4b7`): the `G1YoungGenSizer` warnings follow HotSpot
/// 25's `log_warning(gc, ergo)` on its default `all=warning:stdout` output --
/// an option that turns `gc+ergo` warnings off ON STDOUT silences them, one
/// that configures another output leaves stdout's default in place.
/// `cargo test -j 5 -p cratonvm-vm --lib ts46_`.
/// gci i47w/xl47 (`gci-i45rv-waves-i41-i44-review-findings-FIXED-20261016` item 2's
/// residual): under `-Xlog` the sizer warnings are written by the installed
/// logger with its stdout decorators (`unified_logging`'s
/// `gci_i47w_xl47_the_default_stdout_line_takes_stdouts_decorators` pins the
/// line), right after the logger is installed and before the GC startup
/// lines; without `-Xlog`, in HotSpot's default shape as before.
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i47w_xl47_`.
#[cfg(test)]
mod gci_i47w_xl47_sizer_warning_decorator_tests;

#[cfg(test)]
mod gci_i46ts_g1_sizer_warning_selection_tests;
#[cfg(test)]
mod jdk_only_app_class_path_tests;
/// nio21 wave 21 L5: the launcher and the embedded boot run the shared
/// real-JDK sequence in the same order (source-shape pins: the sequence needs a
/// JDK image to run).
#[cfg(test)]
mod w21_l5_shared_boot_sequence_tests;

#[cfg(test)]
mod w18_l5_init_phase1_tail_tests;
#[cfg(test)]
mod typed_bootstrap_phase_tests;
#[cfg(test)]
mod throwable_trace_rekey_tests;
#[cfg(test)]
mod gci_i5g_g1o_soft_lru_tests;

#[cfg(test)]
mod wait_site_tests;
// ---------------------------------------------------------------------------
// WP1.3 — VM bootstrap init-level state machine
// ---------------------------------------------------------------------------

/// WP1.3 — Re-exports of the process-wide init-level registry that
/// lives in [`cratonvm_native_api::init_level`].  The native-api crate
/// hosts the shared state so both the `vm` crate (which has a
/// `SharedVm`) and the `native-builtins` crate (which implements
/// `jdk/internal/misc/VM.initLevel()`) reach the same monotonic
/// counter without a crate-level cycle.
///
/// * [`set_init_level`] advances the level.  Wakes every thread
///   blocked in [`await_init_level`].
/// * [`get_init_level`] reads the current level with `Acquire` ordering.
/// * [`await_init_level`] blocks until the level reaches the target.
pub use cratonvm_native_api::init_level::{await_init_level, get_init_level, set_init_level};
// ---------------------------------------------------------------------------
// Lock-order-checked accessors for SharedVm's hot locks.
//
// The authoritative hierarchy lives in `crate::runtime::lock_order` (its
// `LockLevel` enum IS the source of truth; there is no separate
// docs/feature-designs/jit-local-exception-handlers.md). Two of the locks below enforce their own level because
// the field itself is now an ordered wrapper:
//
//   * `class_manager`  -- `OrderedPlRwLock` at L10
//   * `ref_processor`  -- `OrderedPlMutex`  at L7
//
// For the rest the field is still a raw `parking_lot` lock, so the accessor
// pairs the original guard with a `LevelScope` that records the level in
// `lock_order`'s per-thread tracker for the lifetime of the guard.
//
// IMPORTANT: an accessor for a lock that is *already* an ordered wrapper must
// NOT also take a `LevelScope` -- that would record the level twice on the same
// thread and trip the same-level assertion. Those accessors are plain aliases
// for the field's own `lock()` / `read()` / `write()`.
//
// Levels are taken straight from `LockLevel`; there is no remapping.
//
// TODO(orchestrator): extend the wiring to the remaining hot locks
// (`jit_cache`, `native_libraries`, `upcall_table`, `jni_global_refs`,
// `class_init_waiters`, `class_loading_locks`, `deopt_log`,
// `jit_skip_set`, `cleaner_thread.pending_actions`,
// and the `ProfileStore` L4.a/L4.b sub-hierarchy in `jit/src/profile.rs`).

mod ranked_locks {
    //! Thin adapter over [`crate::runtime::lock_order`] for the `_ranked`
    //! accessors.
    //!
    //! This module used to carry its *own* copy of the per-thread held-level
    //! bit-set, mirroring the one in `lock_order`. That was a false-negative
    //! factory: a level recorded here was invisible to `lock_order`'s tracker
    //! and vice versa, so an L6 monitor (an `OrderedMutex`, tracked by
    //! `lock_order`) held across a `class_manager_read_ranked()` call (tracked
    //! here) was never reported. There is now exactly one tracker, and it lives
    //! in `lock_order`; enforcement therefore also honours the release-build
    //! `CRATONVM_LOCK_ORDER_CHECK` opt-in instead of being debug-only.
    use crate::runtime::lock_order::{enter_level, LevelScope, LockLevel};

    /// RAII rank scope. Alias for [`LevelScope`], kept because `RankScope` is
    /// part of this crate's public surface.
    pub type RankScope = LevelScope;

    /// Enter a rank scope at `level`.
    ///
    /// # Panics
    ///
    /// When lock-order enforcement is active (always in debug builds; in
    /// release when `CRATONVM_LOCK_ORDER_CHECK` is set), panics if the calling
    /// thread already holds a lock at an equal or *lower* level.
    #[inline]
    pub(super) fn enter(level: LockLevel) -> RankScope {
        enter_level(level)
    }

    // ----- Per-lock level mapping -----------------------------------
    //
    // `class_manager` (L10) and `ref_processor` (L7) are deliberately absent:
    // their fields are ordered wrappers that record their own level.

    pub(super) const MONITORS: LockLevel = LockLevel::Monitors; // L6
    pub(super) const THREAD_REGISTRY: LockLevel = LockLevel::ThreadRegistry; // L5
    pub(super) const FLIGHT_RECORDER: LockLevel = LockLevel::FlightRecorder; // L4
    pub(super) const NATIVE_MEMORY: LockLevel = LockLevel::NativeMemory; // L2
}

pub use ranked_locks::RankScope;
#[cfg(test)]
mod zt14x11_illegal_native_access_taken_at_boot_tests;

#[cfg(test)]
mod zt12df4_illegal_native_access_property_tests;
#[cfg(test)]
mod i136_l1b_release_version_date_tests;
#[cfg(test)]
mod zt11pl3_metaspace_tally_tests;

#[cfg(test)]
mod zt1ob_metaspace_figures_tests;
#[cfg(test)]
mod zs9ob6_class_store_cut_tests;
/// gci i32/mf: the HotSpot-shaped metaspace model's pieces
/// (`gci-i31hp-metaspace-figure-is-not-hotspots-metaspace-used-FIXED-20261016`).
#[cfg(test)]
mod gci_i32_mf_tests;

/// gci i33/ms (`gci-i33ms-metaspace-spaces-and-limit-FIXED-20261016`): the class
/// space, committed granules, reservations and the `-XX:MaxMetaspaceSize`
/// check of the HotSpot metaspace model.
#[cfg(test)]
mod gci_i33_ms_tests;
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod zgc_relocation_gate_tests;
#[cfg(test)]
mod tests;

// ═══════════════════════════════════════════════════════════════════════════
// F30 (2026-08-13) — the registrar call graph, as a gate rather than folklore
//
// Four separate lanes in one session each rediscovered, by hand, that a
// `register_*` pass is reachable in fewer modes than its name suggests:
//
//   1. `register_p67_string_template` and the phase-64/67 registrars are
//      reached only from `register_synthetic_overrides`, which is
//      `#[cfg(feature = "synthetic-jdk")]` AND has exactly one caller,
//      `register_builtins` — i.e. only the synthetic-MODE arm above. A doc
//      comment had already claimed those rows survive `--real-jdk` because
//      their `NativeKind` is `Bridge`: true about categories, false about
//      that registrar.
//   2. `register_pe_panama` likewise has one call site
//      (`native-builtins/src/lib.rs`, inside `register_synthetic_overrides`),
//      so real-JDK mode and `--real-jdk` never registered panama's layouts at
//      all. The two shipping modes ran different `structLayout`
//      implementations and the synthetic-mode tests exercised the one
//      `--real-jdk` does not run.
//   3. In synthetic mode `register_builtins` runs essentials and THEN the
//      synthetic overrides, so a triple registered in both places resolves to
//      the second; the real-JDK arms run essentials only, so the same triple
//      resolves to the first. Two guards for one rule were live in different
//      modes, silently drifting.
//   4. `register_io_natives` runs AFTER `register_essential_natives_with_shims`
//      in both real-JDK arms, so `native-io`'s bodies shadow `native-builtins`'
//      aliasing implementations — a lane measured a clearance against a body
//      that never runs.
//
// `register()` is last-write-wins and `NativeKind` is ambient, so ORDER and
// ARM MEMBERSHIP are semantics, not style. The tests below read this file out
// of the working tree and go red when either changes. A comment cannot do
// that; that a comment is not a gate is this session's most repeated lesson.
//
// Census record:
// `docs/feature-designs/native-dispatch-architecture.md`.
//
// Deliberately NOT covered here: the inline `native_methods.register(...)`
// rows. Two of them (`ArrayList.toArray`, `AbstractCollection.toArray`; a third,
// `CopyOnWriteArrayList.addIfAbsent`, was deleted in interpreter round i3 wave
// 73 as a dead synthetic-layout copy) exist only in the feature-OFF arm and their
// bodies call helper `fn`s declared inside that arm, so unifying them is a
// code move rather than a call move. Recorded in the census instead.
#[cfg(test)]
mod registrar_call_graph_witness;

#[cfg(all(test, unix))]
mod open_file_limit_tests;

#[cfg(test)]
mod test_vm_identity_tests;

#[cfg(test)]
mod gch_h1jp_lazy_oome_tests;





#[cfg(test)]
mod infallible_embedding_retirement_tests;

#[cfg(test)]
mod gci_i47w_oz47_class_load_flag_tests;

/// Interpreter round i6 wave 122, lane L1 (i61-L1 stage 3): the VM's own
/// property list is `VM_PROPERTIES`' keys in order, its fixed values are
/// `VM_IDENTITY`'s, and `SharedVm::new`'s map answers each list value (no
/// `-D` given).
#[cfg(test)]
mod i61_l1_vm_property_list_tests;

#[cfg(test)]
mod r21w14_finalizable_define_listener_tests;

/// j27 (`docs/feature-designs/real-jdk-mode.md`):
/// the `--compatible` fast-boot arm of `boot_init_phase1` skips the
/// `initPhase1` native, so it must wire the system streams itself, or every
/// write through the JDK's `PrintStream` bytecode is discarded.
#[cfg(test)]
mod j27_boot_level_and_java_lang_invoke_tests;

#[allow(unused_imports)]
pub(in crate::vm::vm_init) use super::*;

mod missing_native_entry;
#[allow(unused_imports)]
pub use missing_native_entry::*;
#[allow(unused_imports)]
use missing_native_entry::*;
mod real_jdk_to_array_typed;
#[allow(unused_imports)]
pub use real_jdk_to_array_typed::*;
#[allow(unused_imports)]
use real_jdk_to_array_typed::*;
mod zgc_relocation_permitted;
#[allow(unused_imports)]
pub use zgc_relocation_permitted::*;
#[allow(unused_imports)]
use zgc_relocation_permitted::*;
mod wait_site;
#[allow(unused_imports)]
pub use wait_site::*;
#[allow(unused_imports)]
use wait_site::*;
mod class_store_cut_with;
#[allow(unused_imports)]
pub use class_store_cut_with::*;
#[allow(unused_imports)]
use class_store_cut_with::*;
