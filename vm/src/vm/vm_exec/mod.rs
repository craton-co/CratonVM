// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Method invocation, dispatch, and the NativeContext adapter.
//!
//! # Error-handling discipline (NEW-7)
//!
//! This module sits between the interpreter and the native-method
//! dispatch surface. Like [`crate::runtime::interpreter`], it must
//! propagate recoverable errors via [`crate::error::VmError`] rather
//! than panicking. The `#![cfg_attr(not(test), deny(...))]` gate
//! below enforces the invariant at compile time for non-test code.
//! See the analogous comment in `interpreter.rs` for the full rationale.

#![cfg_attr(not(test), deny(clippy::panic, clippy::unimplemented, clippy::todo,))]

use crate::classloading::resolution::MethodHandleKind;
use crate::classloading::shadow_layout::ShadowLayoutDiff;
use crate::classloading::ClassId;
use crate::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use crate::memory::heap::{ArrayElementType, ObjectKind};
use crate::native::io::FileDescriptorTable;
use crate::native::registry::{
    FieldMetadata, MethodMetadata, NativeClassAccess, NativeContext, NativeExceptionAccess,
    NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    NativeThreadBlocker, StackTraceEntry,
};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::threading::thread_state::{self, ThreadExecState};
use crate::types::{jlong_bits_as_aligned_object_ptr, ObjectRef, Value};

use super::SharedVm;
use crate::classloading::ClassStore;
use crate::native::registry::NativeCallback;
/// Drop this thread's [`PERMISSIVE_DISPATCH_MEMO`], so the next dispatch of
/// every kind is recorded again. For tests that install a policy after boot.
#[cfg(test)]
fn reset_permissive_dispatch_memo() {
    PERMISSIVE_DISPATCH_MEMO.with(|memo| memo.set((0, 0, 0)));
}

/// Drop this thread's [`GENERAL_DISPATCH_CENSUS_MEMO`], so the next dispatch of
/// every callback is declared again. For the tests that pin the memo's shape.
#[cfg(test)]
fn reset_general_dispatch_census_memo() {
    GENERAL_DISPATCH_CENSUS_MEMO.with(|memo| memo.set((0, 0, 0, 0)));
}

#[cfg(test)]
mod gci_i16dp_deposit_screen_tests;
// ---------------------------------------------------------------------------
// ARCH-2026-08-04 A3 — one mask for every native-funnel diagnostic
// ---------------------------------------------------------------------------
//
// `safe_native_call_impl` is the choke point for essentially every native
// dispatch in the VM, and prior notes put the per-call floor at ~8.4 ns against
// HotSpot's ~1. It had accreted **thirteen** independent diagnostic
// subsystems, each with its own gate function, interleaved with the real work:
// the pin-underflow guard, dispatch tracing, VM-state publication, the native
// ring, the straystack thread-local, EC watch, memwatch, the young scan, the
// linkage dump, stale-objref naming, remap tracing, altrace, and the unpin
// ring.
//
// Every gate was individually cheap — a memoized `OnceLock` read, a lesson
// already learned here when uncached `getenv` probes measured ~7% of a
// HashMapOnly-4M run. But thirteen of them are not cheap: thirteen loads,
// thirteen branches the front end must predict, and a 671-line function body
// whose cold half sits in the middle of the hot one.
//
// This collapses all thirteen into a single `u32`. The common case — every
// diagnostic off, which is every production run — tests one value once before
// the callback and once after, and never touches the cold code.
//
// **The split is by mutability, and it is not cosmetic.** Eleven gates are
// startup-static (memoized env reads) and are computed once. Two are *not*:
// `dispatch_trace::enable()` is called at runtime by the stack-dump watchdog so
// a hang in native code still leaves a breadcrumb, and `native_ring::enable()`
// is likewise toggleable. Baking either into the memoized half would silently
// disable a diagnostic exactly when it is being switched on to chase a hang —
// so they are re-read on every call, which is what they cost today anyway.
mod native_diag {
    // Startup-static bits (memoized env reads).
    pub const BLOCKGC: u32 = 1 << 0;
    pub const VM_STATE: u32 = 1 << 1;
    pub const STRAYSTACK: u32 = 1 << 2;
    pub const EC_WATCH: u32 = 1 << 3;
    pub const MEMWATCH: u32 = 1 << 4;
    pub const YOUNGSCAN: u32 = 1 << 5;
    pub const LINKAGE: u32 = 1 << 6;
    pub const ALTRACE: u32 = 1 << 7;
    pub const UNPIN_RING: u32 = 1 << 8;
    pub const STALE_OBJREF: u32 = 1 << 9;
    pub const REMAP_TRACE: u32 = 1 << 10;
    // Runtime-toggleable bits — re-read every call, never memoized.
    pub const DISPATCH_TRACE: u32 = 1 << 11;
    pub const RING: u32 = 1 << 12;
}

#[cfg(test)]
mod r16w9_native_store_satb_tests;
#[cfg(test)]
mod prx23_load_class_chain_tests;

#[cfg(test)]
mod r23_w2e_invoke_virtual_alloc_tests;
#[cfg(test)]
mod j27_served_chain_site_tests;
#[cfg(test)]
mod i65_l3_settled_door_key_tests;

#[cfg(test)]
mod i64_l3_reflective_door_tests;

#[cfg(test)]
mod i63_l3_door_in_flight_tests;
/// Round 16 wave 9 (lane satb), proposal C3W3-4: the pin balance of one
/// native callback, measured when the callback returns and BEFORE the funnel
/// truncates its pins (`docs/feature-designs/jit-ir-optimizations.md`).
///
/// `floor` is the pin-stack length the native was entered with (the
/// funnel's argument pins included), `exit_len` the length it left.
#[cfg(any(debug_assertions, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativePinBalance {
    /// The native released every pin it took.
    Balanced,
    /// The native threw and left `n` pins: the funnel's documented
    /// exception handoff (a live throwable pinned above the argument roots).
    ExceptionHandoff(usize),
    /// A normal return left `n` pins behind. The funnel truncates them, so
    /// nothing fails -- the objects only live until the native returns. This
    /// is the `?`-skipped-unpin family `r15w3-compat2` item 2 fixed by hand.
    Leaked(usize),
    /// The native truncated `n` pins it did not take: its caller's argument
    /// roots (or the caller's own pins) are gone while the caller still reads
    /// them through their indices. Always a bug.
    Underflow(usize),
}

#[cfg(any(debug_assertions, test))]
fn native_pin_balance(floor: usize, exit_len: usize, threw: bool) -> NativePinBalance {
    if exit_len < floor {
        NativePinBalance::Underflow(floor - exit_len)
    } else if exit_len == floor {
        NativePinBalance::Balanced
    } else if threw {
        NativePinBalance::ExceptionHandoff(exit_len - floor)
    } else {
        NativePinBalance::Leaked(exit_len - floor)
    }
}

#[cfg(test)]
mod r16w9_native_pin_balance_tests;

/// JIT round 17 wave 2 (lane funnel): `safe_native_call_jit_lean` against the
/// funnel it trims.
#[cfg(test)]
mod r17w2_funnel_lean_tests;
/// Interpreter round i2 wave 62, lane L6: which natives a frame-trace pause
/// may list (`NX_LISTABLE`).
#[cfg(test)]
mod i62_l6_excusable_native_tests;
#[cfg(test)]
thread_local! {
    /// Unit tests' restartable callback (one per test thread), beside the
    /// production list in `cratonvm_native_builtins::oom_restartable`.
    static TEST_OOM_RESTARTABLE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}



#[cfg(test)]
mod native_slot_write_back_tests;
#[cfg(test)]
mod synchronized_method_guard_tests;

// ---------------------------------------------------------------------------
// Field name в†’ slot index resolution
// ---------------------------------------------------------------------------

/// Resolve a field name to its absolute slot index by walking the class hierarchy.
/// Searches the given class first, then its superclass chain.
///
/// Field slot indices are: parent fields at 0..first_field_index, then own
/// instance fields sequentially from first_field_index.
/// Test helper re-exporting `resolve_field_index_in_hierarchy` so unit
/// tests in sibling modules (e.g. `vm_init`) can resolve field slots
/// without duplicating the traversal logic.
#[cfg(test)]
pub(crate) fn resolve_field_index_for_test(
    class_id: ClassId,
    field_name: &str,
    store: &ClassStore,
) -> Option<usize> {
    resolve_field_index_in_hierarchy(class_id, field_name, store)
}

/// gc-common w8-b: `NativeContext::try_alloc_object` against the real heap.
/// A refusal needs a heap filled to the edge, which no fixture builds; the
/// refusal and reclaim ladder are covered on the mock
/// (`native-builtins` `w8b_reclaiming_alloc_tests`) and by the probes.
#[cfg(test)]
mod w8b_try_alloc_object_tests;

/// gci i48/zl2: `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` covers every collector
/// (it was ZGC only): default ON on Generational, G1 and ZGC, `=0` kills on
/// all three.
#[cfg(test)]
mod gci_i48_zl2_native_doors_collect_tests;
#[cfg(all(test, target_arch = "x86_64"))]
mod nio21_w6_misaligned_cas_tests;
#[cfg(test)]
mod gcd_d6f_carrier_stack_tests;
#[cfg(test)]
mod gc11_hidden_run_index_tests;

#[cfg(test)]
mod r14w4_trace3_served_index_tests;
/// JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`): the
/// cross-thread reader takes the target's block state BEFORE the pause that
/// makes it publish and hands it to the parked-leaf reader, and the excused
/// publication lists a census native's stand-in chain rather than its bare
/// head (source pins: the race needs a second thread inside a park's
/// running phase, which `R25LockAttribution.java`
/// covers on a built VM).
#[cfg(test)]
mod r25_parked_leaf_frames_tests;
#[cfg(test)]
mod i81_l6_thread_dump_tests;
/// nio21 wave 5 L1: `consume_thread_interrupt` (page
/// `r21-interrupt-field-and-flag-can-split-on-a-consume-race-FIXED-20261009.md`).
#[cfg(test)]
mod nio21_interrupt_consume_tests;

#[cfg(test)]
mod nio21_synchronizer_census_tests;
/// zs s7/nd6: a test observer called just before `prune_layer_module_pins`
/// frees dead handles (the order rv4 residual 1 is about).
#[cfg(test)]
pub(crate) mod zs7nd6_prune_observer;
#[cfg(test)]
mod mcs21_unloaded_native_library_tests;
#[cfg(test)]
mod referent_read_tests;
mod dispatch_tally {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    fn enabled() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            None::<std::ffi::OsString>.is_some()
        })
    }

    fn counts() -> &'static Mutex<HashMap<String, u64>> {
        static COUNTS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
        COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn record(site: &str, class_name: &str, method_name: &str, descriptor: &str) {
        if !enabled() {
            return;
        }
        {
            let mut guard = counts().lock().unwrap_or_else(|p| p.into_inner());
            *guard
                .entry(format!("{site} {class_name}.{method_name}{descriptor}"))
                .or_insert(0) += 1;
        }
        let n = TOTAL_SO_FAR.fetch_add(1, Ordering::Relaxed) + 1;
        if n % (1 << 20) == 0 {
            dump(n);
        }
    }

    /// Print the tally at exit, from `interp_census::report_at_exit`.
    ///
    /// `record` only dumped every 2^20 rows, so a workload that generates
    /// fewer than a million dispatches -- `HibfixComposeProbe2` at 40 000
    /// chains generates ~100 000 -- armed the instrument and printed nothing.
    /// An instrument that cannot report at the size its own page measures is
    /// an instrument armed where nobody reads it.
    pub(super) fn dump_at_exit() {
        if !enabled() {
            return;
        }
        dump(TOTAL_SO_FAR.load(Ordering::Relaxed));
    }

    /// Every row recorded so far, for [`dump_at_exit`]. `record`'s own
    /// periodic dump reads the same counter.
    pub(super) static TOTAL_SO_FAR: AtomicU64 = AtomicU64::new(0);

    pub(super) fn dump(total: u64) {
        let guard = counts().lock().unwrap_or_else(|p| p.into_inner());
        let mut rows: Vec<(u64, String)> = guard.iter().map(|(k, v)| (*v, k.clone())).collect();
        drop(guard);
        rows.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        eprintln!("[dispatch-tally] total={total} distinct={}", rows.len());
        // t1 lane H (i13-L5 stage 1): the per-label sums over EVERY row, not
        // only the top 25, so the arm shares (`invoke_or_native.arm.*`) the
        // memo's decision rule needs can be read off one dump.
        let mut sites: Vec<(&str, u64)> = Vec::new();
        for (count, key) in &rows {
            let label = key.split(' ').next().unwrap_or("");
            match sites.iter_mut().find(|(l, _)| *l == label) {
                Some((_, sum)) => *sum += *count,
                None => sites.push((label, *count)),
            }
        }
        sites.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        for (label, sum) in &sites {
            eprintln!("[dispatch-tally] site {sum:>12}  {label}");
        }
        for (count, key) in rows.into_iter().take(25) {
            eprintln!("[dispatch-tally] {count:>12}  {key}");
        }
    }
}

#[cfg(test)]
mod nio21_w21_static_settle_on_throw_tests;
/// zt t27/jd1: the owner load a compiled `invokestatic` owes before it decides
/// native vs bytecode (`static_owner_load_pending`), and the doors that pay it.
#[cfg(test)]
mod zt27_jd1_static_owner_load_tests;
/// The by-name static entry and its shared owner-initialization helpers
/// (interpreter round i1 wave 13, lane L5;
/// `interpreter-L5-native-callback-static-native-skips-class-init-FIXED-20260925.md`).
#[cfg(test)]
mod i13_l5_static_native_init_tests;

/// The by-name static tails initialize the class that DECLARES an inherited
/// static, never the symbolic owner it is named through (JVMS §5.5).
/// Interpreter round i1 wave 14, lane L5;
/// `interpreter-L5-by-name-static-tails-initialize-the-symbolic-owner-FIXED-20260925.md`,
/// probe `JitInheritedStaticInit.java`.
#[cfg(test)]
mod i14_l5_static_tail_tests;

/// The by-name dispatch's argument rooting (interpreter round i1 wave 15,
/// lane L5; `interpreter-L5-by-name-dispatch-pins-and-copies-arguments-twice-FIXED-20260925.md`):
/// a copy is made only when a pinned reference moved, and the dispatch guard
/// refreshes by argument position with no per-call handle vector.
#[cfg(test)]
mod i15_l5_by_name_rooting_tests;
#[cfg(all(test, feature = "management"))]
mod gci_i16_nt_non_heap_threshold_tests;

#[cfg(all(test, feature = "management"))]
mod gci_i18_dk_non_heap_sample_tests;
#[cfg(test)]
mod gci_i42_mx_tests;
/// Per-phase timing for [`annotation_proxy_dispatch_impl`], armed by
/// `CRATONVM_DBG=ann-proxy-prof`.
///
/// Exists because reading one annotation attribute measures ~10.5 us on this VM
/// against ~10 ns on HotSpot — 660-1000x, and 7x worse than any neighbouring
/// reflection accessor in the same probe (`ReflProbe2`). Spring's annotation
/// machinery reads attributes constantly, so this is the largest single
/// per-operation gap found on that surface. Reading the source offers three
/// candidate terms — the `runtime_var_os` flag read on the hot path, the
/// per-element `read_java_string` materialisation used only to compare a name,
/// and the dispatch into this function at all — and cannot rank them.
///
/// `walk` is the element-accessor tail; `total - walk` is the entry plus the
/// match. `flagread` and `namecmp` are sub-phases *inside* `walk`, so
/// `walk - flagread - namecmp` is the rest of the loop.
pub(crate) mod ann_proxy_prof {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;

    pub(crate) static CALLS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static CALLS_WALK: AtomicU64 = AtomicU64::new(0);
    pub(crate) static TOTAL_NS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static WALK_NS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static FLAGREAD_NS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static NAMECMP_NS: AtomicU64 = AtomicU64::new(0);

    pub(crate) const REPORT_EVERY: u64 = 100_000;

    pub(crate) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            false
        })
    }

    #[inline]
    pub(crate) fn add(counter: &AtomicU64, ns: u64) {
        counter.fetch_add(ns, Ordering::Relaxed);
    }

    pub(crate) fn report() {
        let calls = CALLS.load(Ordering::Relaxed).max(1);
        let walks = CALLS_WALK.load(Ordering::Relaxed).max(1);
        eprintln!(
            "[ANN-PROXY-PROF] calls={calls} walks={walks} | total={}ns/call \
             walk={}ns/walk (flagread={} namecmp={} rest={})",
            TOTAL_NS.load(Ordering::Relaxed) / calls,
            WALK_NS.load(Ordering::Relaxed) / walks,
            FLAGREAD_NS.load(Ordering::Relaxed) / walks,
            NAMECMP_NS.load(Ordering::Relaxed) / walks,
            WALK_NS
                .load(Ordering::Relaxed)
                .saturating_sub(FLAGREAD_NS.load(Ordering::Relaxed))
                .saturating_sub(NAMECMP_NS.load(Ordering::Relaxed))
                / walks,
        );
    }
}

#[cfg(test)]
mod r12_compat2_mode_gate_tests;
#[cfg(test)]
mod gci_i22oo_native_mirror_reclaim_tests;

#[cfg(test)]
mod gch_h2fj_proxy_mirror_tests;
#[cfg(test)]
mod gci_i12mf_tests;

#[cfg(test)]
mod gci_i11cf_tests;

#[cfg(test)]
mod gci_i10cr_tests;

#[cfg(test)]
mod gci_i9cs_jni_site_memo_tests;

#[cfg(test)]
mod gci_i13jm_jni_site_miss_stamp_tests;

#[cfg(test)]
mod gci_i8dm_jni_dispatch_memo_tests;
#[cfg(test)]
mod name_override_memo_tests;
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Where the native funnel's fixed per-call cost actually goes.
///
/// `native-call-funnel-per-call-floor-item2-20260805.md`
/// measured the funnel at ~180-330 ns for zero arguments and then said so
/// itself: *"Nobody has profiled it; this document asserts where the time is,
/// not which line."* This module is the answer to that. It drives
/// [`safe_native_call`] with a `Ok(None)` callback against a bare
/// [`SharedVm`], then times each component of the funnel body **on its own**,
/// so the breakdown is measured rather than reasoned about.
///
/// It is `#[ignore]`d because it is a measurement, not an assertion — a
/// timing threshold here would be a flake on a shared build host. Run it:
///
/// ```text
/// cargo test --release -p cratonvm-vm --lib funnel -- --ignored --nocapture
/// ```
///
/// Multi-pass by construction: a rung that has not gone flat is not a
/// measurement (the same lesson the source document records under
/// "Corrections").
#[cfg(test)]
mod native_funnel_profile;

#[cfg(test)]
mod native_diag_tests;



// ---------------------------------------------------------------------------
// JDK-ONLY-WAVE2 — the real-protected-stub allow-list is ONE predicate, and
// this is the check that keeps it one.
//
// The P0 row *Native-first dispatch and hard-coded overrides* in
// `docs/feature-designs/real-jdk-mode.md` describes two hand-maintained copies of
// that allow-list — an inline `matches!` here marked `COPY 1 OF 2` with 11
// classes, and `real_protected_stub_class` with 10 — and asks that they be
// "reconciled, not merged". **That premise expired on 2026-08-04.** There is
// one predicate (`native_override.rs :: real_protected_stub_class_common`,
// TWELVE entries), `java/util/StringJoiner` is protected on both paths, no
// `COPY 1 OF 2` marker exists anywhere in the tree, and the symbol does not
// live in the file the row names. See
// `docs/feature-designs/real-jdk-mode.md`
// and `G86-1`.
//
// What survived the reconciliation is a COMMENT above the call site saying "Do
// NOT re-inline a copy here". A premise in a comment is not a compile-time
// link, and re-inlining a copy is precisely how the divergence arose the first
// time — a warm/cold split in which a native's yield-to-real-bytecode verdict
// depended on how many times its call site had executed. So the module below
// asserts the property the comment asks for, against the working tree, on
// every `cargo test`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod real_protected_stub_single_predicate_witness;


#[cfg(test)]
mod nio21_w6_hw_ref_array_cas_tests;

#[cfg(test)]
mod nio21_w5_hw_array_cas_tests;
#[cfg(test)]
mod r23_w6_armed_ref_cas_route_tests;
/// Interpreter round i1 wave 16, lane L3: `NativeContextImpl::invoke_virtual`
/// on a lambda proxy hands `REF_invokeSpecial` to the interpreter's
/// dispatcher (stage 1 of
/// `docs/feature-designs/native-dispatch-architecture.md`),
/// and `invoke_or_native_impl` carries a `ByNameKind`.
#[cfg(test)]
mod i16_l3_native_lambda_tests;

/// Interpreter round i1 wave 17, lane L4: `NativeContextImpl::invoke_virtual`
/// on a lambda proxy hands EVERY implementation kind to the interpreter's
/// dispatcher (`native_callback_lambda_via_interpreter`), and the native
/// per-kind copy is gone
/// (`docs/feature-designs/native-dispatch-architecture.md`).
#[cfg(test)]
mod i17_l4_native_lambda_tests;

/// SBR round 2, lane `thread`: the `holder.threadStatus` words the VM writes
/// decode, through the JDK's own `VM.toThreadState` rule, to the state
/// `GcBlockState::java_state` names.
#[cfg(test)]
mod sbr2_thread_status_tests;

/// `VmConfig::static_owner_supers_initialized`: an inherited static called
/// through an uninitialized owner answers "nothing to initialize" from one
/// superclass walk when its declaring class is initialized, and every other
/// shape keeps the resolution's answer. Interpreter round i3 wave 72, lane L3
/// (`i71-L3-proposal-an-inherited-static-owner-answers-from-its-superclasses-bits-20261104.md`).
#[cfg(test)]
mod i72_l3_inherited_static_owner_tests;
#[cfg(test)]
mod gci_i6jd_path_tostring_redirect_tests;

#[cfg(test)]
mod gci_i18sw_cas_lock_path_satb_tests;
#[cfg(test)]
mod zr11nd_native_oom_replay_tests;

#[cfg(test)]
mod zs2nd2_forced_refusal_replay_tests;

/// zs s5/nd5 (ZGC round zs, wave s5): a user loader's Module lives with its
/// loader on ZGC (`layer_module_loader_owned`).
#[cfg(test)]
mod zs5nd5_tests;

/// zs s9/nd8 (ZGC round zs, wave s9): `Class.getModule()` of a user
/// loader's generated proxy class.
#[cfg(test)]
mod zs9nd8_tests;

/// zs s7/nd6 (ZGC round zs, wave s7): the embedder boot latch and the
/// remaining native-door residuals.
#[cfg(test)]
mod zs7nd6_tests;

/// zs s3/nd3 (ZGC round zs, wave s3): typed reflection listings and the
/// forced-refusal controls of the conversions this lane made.
#[cfg(test)]
mod zs3nd3_tests;

/// Interpreter round i4 wave 95, lane L7 (`CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH`,
/// `class_id_memo_epoch`): with the switch on, a definition of an unrelated
/// class leaves the class-id memos' stamp alone, and an in-place provenance
/// write (a stub promoted to its real bytes) still retires a transient
/// descriptor answer. Each test sets the switch on its own VM explicitly.
#[cfg(test)]
mod i95_l7_class_memo_narrow_epoch_tests;


#[cfg(test)]
mod infallible_thread_mirror_retirement_tests;



#[cfg(test)]
mod gci_i47w_jw1_windows_park_at_once_tests;

#[cfg(test)]
mod gci_i47w_oz47_thread_start_class_load_attribution_tests;

/// JIT round 22 wave 4b, lane sync (`CRATONVM_HW_FLOAT_CAS`): the `float` /
/// `double` hardware-CAS kinds address the right cell, and are never published
/// to the compiled `Unsafe` read-modify-write prefixes.
#[cfg(test)]
mod r22_sync_hw_float_cas_tests;

#[cfg(test)]
mod zt23zb8_refers_to_no_keepalive_tests;

#[cfg(test)]
mod r25_w6f_parked_frame_sample_tests;

#[cfg(test)]
mod w9_1_held_read_staleness_tests;

#[cfg(test)]
mod w11_1_held_read_shape_tests;

/// JIT round 22 wave 1, lane runtime (page rt22): a native that resolves an
/// array descriptor against "the calling class" must see compiled activations,
/// which push no interpreter `Frame`.
#[cfg(test)]
mod rt22_array_resolution_caller_tests;


/// Boundary forwarding tests for `NativeContextImpl` accessors
/// (`bug-h2-testmultithread-mvstore-writer-object-identity-FIXED-20261113.md`).
///
/// An unpinned `ObjectRef` held in a native local across a GC-capable call is
/// not in the GC root set, so a moving collection leaves the local pointing at
/// the vacated from-space address. Every `NativeContextImpl` boundary method
/// that accepts an `ObjectRef` or `Value` must resolve it through
/// `load_and_forward` / `forward_boundary_value` before consulting headers or
/// payload bytes.
#[cfg(test)]
mod h2_mvid_boundary_forwarding_tests;

#[allow(unused_imports)]
pub(in crate::vm::vm_exec) use super::*;

mod method;
#[allow(unused_imports)]
pub use method::*;
#[allow(unused_imports)]
use method::*;
mod report_pad_access;
#[allow(unused_imports)]
pub use report_pad_access::*;
#[allow(unused_imports)]
use report_pad_access::*;
mod diagnose_sync_on_value_based_class;
#[allow(unused_imports)]
pub use diagnose_sync_on_value_based_class::*;
#[allow(unused_imports)]
use diagnose_sync_on_value_based_class::*;
mod thread_interrupted_field_slot_of;
#[allow(unused_imports)]
pub use thread_interrupted_field_slot_of::*;
#[allow(unused_imports)]
use thread_interrupted_field_slot_of::*;
mod clear_breakpoints_of_redefined_class;
#[allow(unused_imports)]
pub use clear_breakpoints_of_redefined_class::*;
#[allow(unused_imports)]
use clear_breakpoints_of_redefined_class::*;
mod load_class;
#[allow(unused_imports)]
pub use load_class::*;
#[allow(unused_imports)]
use load_class::*;
mod native_new_initializes_class_first;
#[allow(unused_imports)]
pub use native_new_initializes_class_first::*;
#[allow(unused_imports)]
use native_new_initializes_class_first::*;
mod new_object;
#[allow(unused_imports)]
pub use new_object::*;
#[allow(unused_imports)]
use new_object::*;
mod gen_referent_read_keep_alive;
#[allow(unused_imports)]
pub use gen_referent_read_keep_alive::*;
#[allow(unused_imports)]
use gen_referent_read_keep_alive::*;
mod append_native_standin_frames;
#[allow(unused_imports)]
pub use append_native_standin_frames::*;
#[allow(unused_imports)]
use append_native_standin_frames::*;
mod unloaded_native_libraries;
#[allow(unused_imports)]
pub use unloaded_native_libraries::*;
#[allow(unused_imports)]
use unloaded_native_libraries::*;
mod extract_annotations_from_attributes;
#[allow(unused_imports)]
pub use extract_annotations_from_attributes::*;
#[allow(unused_imports)]
use extract_annotations_from_attributes::*;
mod invoke_static_settling_with_first;
#[allow(unused_imports)]
pub use invoke_static_settling_with_first::*;
#[allow(unused_imports)]
use invoke_static_settling_with_first::*;
mod invoke_special_shared;
#[allow(unused_imports)]
pub use invoke_special_shared::*;
#[allow(unused_imports)]
use invoke_special_shared::*;
mod proxy_resolve_declaring_method;
#[allow(unused_imports)]
pub use proxy_resolve_declaring_method::*;
#[allow(unused_imports)]
use proxy_resolve_declaring_method::*;
mod invoke_on_class_shared_inner;
#[allow(unused_imports)]
pub use invoke_on_class_shared_inner::*;
#[allow(unused_imports)]
use invoke_on_class_shared_inner::*;
mod jni_table_native_call;
#[allow(unused_imports)]
pub use jni_table_native_call::*;
#[allow(unused_imports)]
use jni_table_native_call::*;
