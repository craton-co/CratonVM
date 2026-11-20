// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Bytecode interpreter — the heart of the VM.
//!
//! Executes JVM bytecode instructions in a loop, handling:
//! - Local variable loads/stores
//! - Operand stack manipulation
//! - Arithmetic and type conversions
//! - Control flow (branches, returns)
//! - Object creation and method invocation
//! - Exception throw/catch via exception table
//!
//! # Error-handling discipline (NEW-7)
//!
//! This file is on the JVM execution hot path and must NEVER panic in
//! production builds. A stray `unwrap`, `expect`, or `panic!` here would
//! tear down the entire VM for what should be a recoverable `VmError`.
//!
//! The `#![cfg_attr(not(test), deny(...))]` gate below makes clippy refuse
//! to compile this module in a release build when any of the following
//! appear in non-test code:
//!
//! - `.unwrap()` / `.expect()` — use `?` against [`crate::error::VmError`]
//!   or an explicit `match` with a typed error instead.
//! - `panic!()` / `unimplemented!()` / `todo!()` — convert to a
//!   `VmError::Internal` or the appropriate `RuntimeError::*` variant.
//! - `unreachable!()` — if the arm is truly unreachable given JVMS
//!   invariants, annotate it with a `// SAFETY:` comment and use the
//!   `crate::runtime::unreachable_invariant!()` macro which returns a
//!   typed error rather than panicking.
//!
//! Tests inside `#[cfg(test)] mod tests { ... }` are exempt — assertion
//! panics are the standard Rust test failure mechanism and the gate only
//! applies to `not(test)` compilations.

#![cfg_attr(not(test), deny(clippy::panic, clippy::unimplemented, clippy::todo,))]
// T1.8.5 — every `unsafe {}` block in this file now carries a
// `// SAFETY:` comment (27 total after the T1 fourth-pass backfill).
// Promoted from `warn` to `deny` so any new unsafe block without a
// comment fails CI.

use std::sync::Arc;

use cratonvm_native_api::{
    NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::constant_pool::ConstantPoolEntry;
use cratonvm_reader::instruction::Instruction;
use tracing::trace;

use crate::classloading::resolution::{
    CachedBytecodeMethod, CachedInvokeTarget as GenericCachedInvokeTarget, MethodHandleKind,
    RedefineGate, ResolvedField, ResolvedMethod,
};
use crate::classloading::{find_field_recursive, ClassId};
use crate::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use crate::jit::profile::MethodKey;
use crate::memory::gc::{update_all_roots, update_value_ref};
use crate::memory::heap::ArrayElementType;
use crate::memory::roots::collect_roots;
use crate::runtime::exceptions::convert_class_not_found;
use crate::runtime::frame::Frame;
use crate::runtime::redefine_state::{
    class_was_redefined, hierarchy_fingerprint_in, named_class_was_redefined,
    native_shadow_suppressed_in,
};
use crate::threading::gc_barrier::LoopPollWord;
use crate::threading::jvm_thread::JvmThread;
use crate::types::{CompactTag, CompactValue, ObjectRef, Value};
use crate::vm::{
    coerce_native_return_value, coerce_value_for_return, coerce_value_for_return_validated,
    create_java_string, create_java_string_from_units, ensure_class_initialized_shared, ensure_system_stdin_object,
    get_static_shared, invoke_on_class_shared, invoke_or_native, invoke_shared, read_java_string,
    set_static_shared, SharedVm,
};

#[cfg(test)]
mod gcg_g3rw_bind_tests;

#[cfg(test)]
mod gcg_g6rs_soft_timestamp_tests;
#[cfg(test)]
mod i40_l2_dispatch_ceiling_tests;
#[cfg(test)]
mod i133_l4_reentry_stack_guard_tests;
#[cfg(test)]
mod i1w9_l5_exec_ceiling_tests;
/// Interpreter round i3 wave 76, lane L4: the `execute` method hint is taken
/// once, and only by the class it was left for.
#[cfg(test)]
mod i76_l4_execute_method_hint_tests;

pub(crate) mod entry_record;
/// Interpreter round i6 wave 115, lane L7: the method-index memo's key
/// separates classes and the two halves of the method's identity.
#[cfg(test)]
mod i115_l7_execute_method_index_key_tests;
/// Interpreter round i3 wave 77, lane L4: the shared view names the same
/// text as the copies, shares only when on, and the empty table is shared.
#[cfg(test)]
mod i77_l4_execute_method_view_tests;
pub(crate) use exception_dispatch::withdraw_compiled_constructor_at_entry;
#[cfg(test)]
mod continuation_return_boundary_tests;
#[cfg(test)]
mod mon11_i_plus_b_tests;


/// JIT round 16 (lane coldpath): the OSR-entry tier-up credit
/// (`osr_entry_tierup_armed`).
#[cfg(test)]
mod r16_coldpath_osr_entry_tierup_tests;

#[cfg(test)]
mod osr_jvmti_gate_tests;

/// JIT round 21 wave 13 (review rev-jit, item 1 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the OSR
/// offer asks `finalizer_bars_compiling` like every other compile door, after
/// the per-method agent screen and before the method-wide budget, and retires
/// the loop header for the activation when it refuses (an unrecorded refusal
/// would be re-asked on every back edge past the threshold).
#[cfg(test)]
mod r21w13_osr_finalizer_ctor_gate_tests;
#[cfg(test)]
mod gcj1cm_loop_top_handshake_poll_tests;
/// The dispatch loop's gate words, built once per entry from `(gate, bit)`
/// pairs: the union of the bits whose gate is set, as one OR expression per
/// pair (interpreter round i3 wave 83, lane L7, host frame rework).
///
/// Waves 77 and 82 built the words with `static_ldc_gate_word` / `gate_byte`,
/// a fold over an ARRAY of `(bool, bit)` pairs. The array is a stack
/// temporary of `execute_frame_from_index` (48 bytes for the twelve
/// `diag_gates` pairs, 52 for `static_ldc_gates`); unless LLVM unrolls the
/// fold and then scalarises the array, it keeps an alloca of that size in
/// the loop's frame. The wave-82 packing measured +16 bytes where it should
/// have saved about 11, and the two new arrays were the packing's only new
/// stack temporaries. This expansion has no array: one `bool` select and
/// one `or` per pair, nothing in memory. The functions stay as the tests'
/// oracle.
macro_rules! gate_word {
    ($t:ty; $(($on:expr, $bit:expr $(,)?)),* $(,)?) => {{
        let mut word: $t = 0;
        $(
            word |= if $on { $bit } else { 0 };
        )*
        word
    }};
}

/// Interpreter round i4 wave 84, lane L7: the parked error is one pointer in
/// the loop's frame, and it comes back exactly as parked.
#[cfg(test)]
mod i84_l7_pending_runtime_error_tests;
// ---------------------------------------------------------------------------
// Per-opcode dispatch
// ---------------------------------------------------------------------------
//
// `execute_instruction`, one arm per JVM opcode: `interpreter/opcodes.rs`.

// ---------------------------------------------------------------------------
// Helper: lambda proxy type-check for checkcast/instanceof
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/typecheck.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod typecheck;
pub use typecheck::*;
// ---------------------------------------------------------------------------
// Helper: LDC / LDC_W (load constant from pool)
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/constants.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
pub(crate) mod constants;
pub use constants::*;
// ---------------------------------------------------------------------------
// Helper: Field resolution
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/field_access.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
//
// C2 P0 — the module declaration itself is `pub(crate)` (it was private) so
// that `crate::runtime::resolve::MemberResolver` can name the field-resolution
// core it delegates to. Nothing inside became more visible: the glob re-export
// still caps each item at its own declared visibility, and only the one item
// `resolve` calls was widened. Call `runtime::resolve`, not this module —
// `runtime::resolve::guard` enforces it.
pub(crate) mod field_access;
pub use field_access::*;
mod field_fast;
mod invoke_fast;
// Blocks of `execute` moved out verbatim (the split-execute proposal, stage 1).
mod execute_entry;
// The interpreter's resolved constant pool: per-thread, lock-free site caches
// for field and method constant-pool references. `pub` so `vm-cli` can print
// the `CRATONVM_DBG=field-site` tally at exit.
pub mod field_phases;
pub mod invoke_phases;
pub mod site_cache;
pub use site_cache::{
    CastSiteCache, ClassSiteCache, FastFieldSite, FastFieldSiteCache, FieldSiteCache,
    IfaceSelectSiteCache, InterpSiteTables, MethodSiteCache, MethodSiteInfo, ResolvedNewSite,
};
// ---------------------------------------------------------------------------
// Helper: Method invocation
// ---------------------------------------------------------------------------
//
// Moved to `interpreter/invoke.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
//
// C2 P0 — `pub(crate)` for the same reason as `field_access` above: it is the
// method-resolution core that `crate::runtime::resolve::MemberResolver`
// delegates to. Only `resolve_method_metadata` was widened.
pub(crate) mod invoke;
pub(crate) mod sig_poly_site;
pub use invoke::*;
pub(crate) mod gc_and_alloc;
// gen r4/plumbing: the per-collection ordinal / JFR / `-Xlog:gc` accounting
// every GC door in `gc_and_alloc` calls into.
mod gc_events;
// gen r4w6/concsvc6: the generational concurrent-cycle service thread (opt-in
// `CRATONVM_GEN_CONC_SERVICE_THREAD`), started by `Vm::new`.
pub(crate) mod gen_conc_service;
pub use gc_and_alloc::*;
mod opcodes;
pub use opcodes::*;
// Structured locking (JVMS §2.11.10): the per-frame record of the monitors a
// frame's own `monitorenter`s hold (`Frame::held_monitors`) and the rules the
// interpreter enforces from it (interpreter round i1 wave 23, lane L7).
mod deopt_resume;
pub(crate) mod held_monitors;
pub use deopt_resume::*;
mod exception_dispatch;
pub use exception_dispatch::*;
mod jvmti_events;
pub use jvmti_events::*;
mod dispatch_virtual;
pub use dispatch_virtual::*;
mod dispatch_static;
pub use dispatch_static::*;
pub(crate) mod lambda;
pub use lambda::*;
mod native_override;
pub use native_override::*;
// Frames still running a body a class redefinition replaced (interpreter
// round i1 wave 19, lane L3).
pub(crate) mod jit_bridge;
// JIT round 21 lane reflect4 (REFLECT4-21-1): the bytecode-only doors' entry
// into a door-invisible compiled body of a reviewed `Intrinsic`'s own JDK body.
pub(crate) mod bytecode_only_body;
pub(crate) mod obsolete_frames;
pub use jit_bridge::*;
// JEP 358 for an NPE raised inside COMPILED code: rebuilds the full message
// from the trapping method's own bytecode, so a hot row equals its cold row.
pub(crate) mod jit_npe_message;
// j27 wave 9 (lane W9EXC): a compiled caller's null `MethodHandle` at
// `invokeExact` / `invoke` raises from the invoker's `Invokers` check, as HotSpot's compiled code does.
pub(crate) mod jit_npe_mh_invoker;
// j27 wave 5 (lane JVMTIFRAME): the C JVMTI table's reads of a compiled
// activation's / an OSR'd frame's locals from their home slots
// (`CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`, default OFF).
#[cfg(feature = "experimental-debug")]
pub(crate) mod compiled_frame_locals;
// The frame-level half of the osr-02 exit differential: back-edge arrivals and
// OSR-exit resumed frames, in one format, under one flag. Inert unless
// `CRATONVM_DBG_OSR_FRAME_TRACE` names a class substring.
mod osr_frame_trace;
// j27 wave 5 (lane OSRRETIRE): the OSR offer retires a loop header whose
// operand stack is live (`CRATONVM_JIT_OSR_LIVE_STACK_RETIRE`).
mod osr_live_stack;
// gci i11/jd: `CRATONVM_DBG_BASE_BODY_ENTRY`, the frame-entry witness for a
// virtual call that ran a superclass body its receiver overrides. Inert unset.
pub(crate) mod gci_i11jd_witness;
/// Interpreter round i6 wave 115, lane L8: the shared `ifnull` test pops
/// exactly one slot and answers [`ref_operand_is_null`] for every shape that
/// is not an `Object`-tagged slot.
#[cfg(test)]
mod i115_l8_null_test_tests;
/// Interpreter round i4 wave 92, lane L1: the pieces of the compiled throw
/// site and the compiled catch prediction that need no compiled code.
#[cfg(test)]
mod i92_l1_compiled_exception_site_tests;

/// Pack `(gate, bit)` pairs into the loop's `static_ldc_gates` word: the
/// union of the bits whose gate is set. Runs once per `execute_frame_from_index`
/// entry.
#[cfg(test)]
fn static_ldc_gate_word<const N: usize>(gates: [(bool, u16); N]) -> u16 {
    gates
        .iter()
        .fold(0, |word, &(on, bit)| if on { word | bit } else { word })
}


/// Pack `(gate, bit)` pairs into one of the loop's byte-wide gate words
/// (`door_gates`, `top_gates`). Runs once per `execute_frame_from_index`
/// entry.
#[cfg(test)]
fn gate_byte<const N: usize>(gates: [(bool, u8); N]) -> u8 {
    gates
        .iter()
        .fold(0, |word, &(on, bit)| if on { word | bit } else { word })
}

/// Interpreter round i3 wave 82, lane L7: the packed diagnostic, door and
/// loop-top words answer each gate exactly as the bool it replaces, and no
/// two gates of a word share a bit.
#[cfg(test)]
mod i82_l7_gate_word_tests;

/// Interpreter round i3 wave 77, lane L7: the packed gate word answers each
/// gate exactly as the bool it replaces, and no two gates share a bit.
#[cfg(test)]
mod i77_l7_static_ldc_gate_tests;
/// Interpreter round i4 wave 91, lane L7: [`iconst_arith_group`] answers each
/// operation as its own arm would, and declines every other shape.
#[cfg(test)]
mod i91_l7_iconst_arith_tests;
/// Interpreter round i5 wave 106, lane L7: [`i2l_long_tail`] decodes the
/// tails javac emits after `i2l` for `sum += <int expression>`.
#[cfg(test)]
mod i106_l7_i2l_long_tail_tests;
/// Interpreter round i5 wave 102, lane L7: [`double_arith_group`] decodes
/// each form javac emits for a double expression over locals and constants,
/// and [`double_arith_apply`] answers as the unfused arms.
#[cfg(test)]
mod i102_l7_double_arith_tests;

/// Interpreter round i5 wave 101, lane L7: [`long_arith_group`] decodes each
/// form javac emits for a long expression over locals and constants, and
/// [`long_arith_apply`] answers as the unfused arms.
#[cfg(test)]
mod i101_l7_long_arith_tests;
#[cfg(test)]
mod i63_l7_fused_ldc_bound_tests;

#[cfg(test)]
mod i64_l7_ldc_inline_and_getstatic_bound_tests;

#[cfg(test)]
mod i65_l7_getstatic_and_ldc2w_inline_tests;

#[cfg(test)]
mod i66_l7_putstatic_inline_tests;

#[cfg(test)]
mod i67_l7_fused_static_increment_tests;
#[cfg(test)]
mod w10f_multianewarray_plan_tests;


#[cfg(test)]
mod r15w5_classsite2_multianewarray_tests;
/// G13-1 (2026-08-17) — the Path B interface-substitution table, and the
/// property that makes an ARRAY receiver a refusal rather than a guess.
///
/// These live inline rather than in `interpreter/tests.rs` because this lane
/// owns exactly one file. They are pure-function tests: nothing here builds a
/// VM, so they run in milliseconds and cannot flake.
#[cfg(test)]
mod g13_array_receiver_tests;

/// Interpreter round i1, lane L1: the raw-bytecode fast path, run end to end.
///
/// Each test hands a synthetic frame to `execute_prebuilt_frame`, so the bytes
/// go through the real dispatch loop — preamble, fast-path `match`, fusions,
/// return arm — exactly as a Java method would. Every sequence below is
/// covered by fast-path arms only, so no class metadata is needed.
#[cfg(test)]
mod l1_dispatch_tests;

/// Interpreter round i1 wave 11, lane L1: the JVMTI `Exception` event, posted
/// by the unwinder in both feature states (it had no producer).
#[cfg(test)]
mod i1w11_l1_jvmti_exception_tests;

/// Interpreter round i1 wave 21, lane L2: the compiled catch doors post
/// `Exception` at the compiled throw site and `ExceptionCatch` at the handler
/// (`report_exception_caught_by_compiled_door`), and the compiled local
/// handler declines while this VM may have an exception client
/// (`exception_events_may_be_armed`). Proposal
/// `docs/feature-designs/real-jdk-mode.md`.
#[cfg(test)]
mod i1w21_l2_compiled_catch_event_tests;

/// Interpreter round i1 wave 10, lane L1: JDWP field watches and exception
/// events, reported by the field bytecodes' hook and the unwinder's.
/// Interpreter round i2 wave 58, lane L6 (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`).
#[cfg(test)]
mod i58_l6_native_excuse_tests;

#[cfg(all(test, feature = "experimental-debug"))]
mod i1w10_l1_debugger_event_tests;

#[cfg(test)]
mod wave1_adoption_tests;

#[cfg(test)]
mod tests;

// JIT round 22 wave 6b (lane loops): HotSpot 25's NaN-operand table for the
// arithmetic ops (`docs/feature-designs/real-jdk-mode.md`;
// `FpNaN2` cold row: HotSpot's INTERPRETER; C2 answers value1's NaN for an
// add / mul of two NaNs, so only the interpreter follows these cells). Operands
// (row-major, value1 x value2): +NaN 7ff8.., -NaN(0x123), sNaN(1), 1.5; each
// cell is add, sub, mul, div, rem raw bits.
#[cfg(test)]
mod r22_loops_nan_payload_tests;

/// JIT round 21 (lane osrreentry): the early OSR entry's tier-up credit
/// (`CRATONVM_JIT_OSR_EARLY_REENTRY_CREDIT`) and the loop-work retry crossing
/// (`CRATONVM_JIT_LOOP_WORK_RETRY_CROSSING`);
/// `docs/feature-designs/jit-osr-exit-and-recompile.md`.
#[cfg(test)]
mod jr21_osrreentry_tests;

#[allow(unused_imports)]
pub(in crate::runtime::interpreter) use super::exceptions;

mod cached_invoke_target;
#[allow(unused_imports)]
pub use cached_invoke_target::*;
#[allow(unused_imports)]
use cached_invoke_target::*;
mod execute;
#[allow(unused_imports)]
pub use execute::*;
#[allow(unused_imports)]
use execute::*;
mod resume_continuation;
#[allow(unused_imports)]
pub use resume_continuation::*;
#[allow(unused_imports)]
use resume_continuation::*;
mod execute_frame_from_index_loop;
#[allow(unused_imports)]
pub use execute_frame_from_index_loop::*;
#[allow(unused_imports)]
use execute_frame_from_index_loop::*;
mod resolve_array_class_loader_aware;
#[allow(unused_imports)]
pub use resolve_array_class_loader_aware::*;
#[allow(unused_imports)]
use resolve_array_class_loader_aware::*;
mod park_for_debugger_under;
#[allow(unused_imports)]
pub use park_for_debugger_under::*;
#[allow(unused_imports)]
use park_for_debugger_under::*;
mod double_arith_apply;
#[allow(unused_imports)]
pub use double_arith_apply::*;
#[allow(unused_imports)]
use double_arith_apply::*;
