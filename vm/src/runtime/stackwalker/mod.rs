// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.9 — `java.lang.StackWalker` / full stack-trace machinery.
//!
//! This module hosts the helpers used by:
//!
//! * `java.lang.Throwable.fillInStackTrace` / `getStackTrace` (via
//!   `NativeContext::capture_stack_trace`) — frames are populated with
//!   source file, line number, and bytecode index.
//! * `java.lang.StackWalker.walk(Function)` / `forEach(Consumer)` — see
//!   `native-builtins/src/phases_late.rs::p59_sw_walk`, which consumes
//!   `StackTraceEntry.byte_code_index` / `line_number` populated here.
//! * `java.lang.StackWalker.StackFrame.getLineNumber()` /
//!   `getByteCodeIndex()` / `getDeclaringClass()` / `getClassName()` /
//!   `getMethodName()` — see
//!   `native-builtins/src/lang_stackwalker.rs::register_lang_stackwalker`.
//!
//! Line-number lookup walks the method's `LineNumberTable` attribute
//! (JVM spec §4.7.12). The table is a sorted `(start_pc, line_number)`
//! list; for any `bci` the applicable line is the largest `start_pc`
//! that is `<= bci`.
//!
//! Bytecode index (BCI) comes from the frame's `last_instr_pc` (the PC
//! of the last-executed instruction, which is what HotSpot exposes via
//! `StackFrame.getByteCodeIndex()` — note that `pc` itself may already
//! have been advanced past the invoke instruction when the frame sits
//! above the invocation site).
//!
//! ## Capture cost
//!
//! [`capture_full_trace`] is O(depth) and runs on **every** VM-raised throw,
//! including the ones application frameworks use as control flow. Two
//! mechanisms keep that affordable:
//!
//! * a per-signature **method-slot memo** replaces `Class::find_method`'s
//!   linear scan over every declared method (see the memo's own comment
//!   below for its correctness rules — verified on hit, never negative,
//!   retains nothing);
//! * [`capture_frames_no_lines`] + [`resolve_line_numbers_in_place`] let a
//!   caller skip line resolution entirely at capture time and fill it in only
//!   if the trace is ever read. The deferred resolver is fail-closed: it
//!   yields the eager answer or [`LINE_NUMBER_UNKNOWN`], never a wrong line.
//!
//! `StackTraceEntry::method_index` (landed 2026-07-26, `cross-owner-closeout`)
//! is what makes the deferred resolver **exact**: with only
//! `(class_id, method_name, bci)` an overload set is unresolvable, and the
//! members of an overload set have different `LineNumberTable`s. Entries built
//! by [`entry_from_frame`] carry the index; entries built by the lock-free
//! [`capture_frames_no_lines`] do not, and fall back to the (still fail-closed)
//! unambiguous-name rule.
//!
//! See `stackwalk-and-vtable.md` and
//! `cross-owner-closeout.md`.

use std::sync::Arc;

use cratonvm_reader::attribute::{Attribute, LineNumberEntry};
use cratonvm_reader::method::ClassFileMethod;
use parking_lot::RwLock;

use crate::classloading::{Class, ClassId, ClassStore};
use crate::jit::conservative_roots::{ActiveCompiledFrame, InlinedLevel};
use crate::native::registry::StackTraceEntry;
use crate::runtime::frame::Frame;
use crate::runtime::fx_collections::{fx_hashmap, FxHashMap};
#[cfg(test)]
mod r19w5_throws5_fill_call_screen_tests;
#[cfg(test)]
mod door_in_flight_tests;
#[cfg(test)]
mod r23_w3_c2_intrinsic_leaf_tests;
#[cfg(test)]
mod i60_l3_caller_fast_tests;
/// Test-only `ClassStore` fixtures.
///
/// Lives outside `mod tests` because deferred line resolution is *consumed*
/// elsewhere in the crate (`threading::thread_registry::frame_trace_of_resolved`),
/// and that module's tests need a real class to resolve against. Mirrors
/// `classloading::class::tests::make_class`.
#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;

/// Round 13 wave 13 (lane trace3): [`native_standin_frames`] over JDK-25-shaped
/// `Thread` / `Object` bodies.
#[cfg(test)]
mod r13_trace3_standin_tests;

#[cfg(test)]
mod r14w3_trace_thread_run_tests;

#[cfg(test)]
mod r14w3_trace2_thread_run_middle_tests;

/// Round 14 wave 4 (lane trace3): the `join` rows and owner resolution
/// (T4-2), the parked-thread leaf frames (T4-3), and the tightened lock-free
/// `Thread.run` screens.
#[cfg(test)]
mod r14w4_trace3_tests;

#[cfg(test)]
mod r14w5_trace4_tests;

#[cfg(test)]
mod r14w6_trace5_tests;

#[cfg(test)]
mod r14w7_trace6_tests;

/// SBR wave 6 (lane dispatch): the tail captures ([`capture_tail_trace`],
/// [`frame_class_ids_tail`]) are the whole layout's suffix, because the layout
/// is a concatenation -- interpreter frame `i`, its OSR continuation's inlined
/// chain, then the compiled entries pushed at depth `i + 1`. These tests pin
/// that claim on fabricated frames and compiled entries (the live compiled walk
/// itself is exercised by `CRATONVM_CALLER_TAIL=verify` over the regression
/// suite).
#[cfg(test)]
mod tail_layout_tests;

#[cfg(test)]
mod r15w1_trace_callback_native_tests;

/// Round 15 wave 2 (lane trace2): the served built-in loader's miss frames
/// (`served_loader_miss_frames`) and the parked `LockSupport` / `Unsafe.park`
/// rows (TR5-5).
#[cfg(test)]
mod r15w2_trace2_tests;

/// Round 15 wave 3 (lane trace3; TR2W2-5): a served bare `URLClassLoader`'s
/// miss gets `ClassLoader.loadClass(String)`, the base `loadClass(String,
/// boolean)` at its `findClass` call and `URLClassLoader.findClass` at its
/// plain `ClassNotFoundException`.
#[cfg(test)]
mod r15w3_trace3_tests;

/// Round 15 wave 5 (lane trace5;
/// `r15w3-trace3-url-loader-find-class-middle-frames-FIXED-20260930.md`): the
/// `ClassLoader.loadClass` frames between a caller of the served base
/// `loadClass` and the `findClass` bytecode it ran, the `forName` shape, the
/// `URLClassLoader` subclass hint, and the direct-`findClass` screen.
#[cfg(test)]
mod r15w5_trace5_tests;

#[cfg(test)]
mod i48_l3_standin_table_tests;

/// JIT round 16 (lane throwpath, TP16-1): the scan-free label helpers answer
/// exactly what the `rsplit_once` forms they replaced answered.
#[cfg(test)]
mod r16_throwpath_label_split_tests;

/// JIT round 16 wave 7 (lane throwpath, TP16-6): the capture memo answers only
/// the capture it was made from, and the OSR dedupe's no-name fast path keeps
/// the walk's frames as they came.
#[cfg(test)]
mod r16_throwpath_capture_memo_tests;

/// JIT round 16 wave 9 (lane throwpath, TP16-9): the single-pass capture
/// screens answer exactly what the three per-frame screens answer.
#[cfg(test)]
mod r16_throwpath_capture_screens_tests;

/// JIT round 16 wave 8 (lane trace): the compiled indy bridge's synthetic
/// frame (`<jit-indy>.bridge` / `.concat`) is in no trace.
#[cfg(test)]
mod r16_trace_indy_bridge_tests;

/// Interpreter round i1 wave 50, lane L3: the lambda proxy frame's bci and
/// the stand-in insertion merge.
#[cfg(test)]
mod i50_l3_lambda_proxy_frame_tests;

#[cfg(test)]
mod i52_l3_member_lookup_tests;

/// Interpreter round i2 wave 53, lane L3: [`served_raise_frames`], the frames
/// of a raise the native-call funnel marked.
#[cfg(test)]
mod i53_l3_served_raise_tests;

/// JIT round 16 wave 11 (lane throw2): TP16-10's per-site stand-in memo and
/// TP16-12's trim `is_a` memo answer only for exactly the key they were made
/// from.
#[cfg(test)]
mod r16_throw2_capture_memo_tests;

/// JIT round 16 wave 13 (lane wmte): the JDK frames of a `MethodHandle`
/// door's `WrongMethodTypeException` (`mh_wmte_raise_frames`), on
/// JDK-25-shaped bodies with HotSpot's lines.
#[cfg(test)]
mod r16_wmte_frames_tests;

/// JIT round 16 wave 15 (lane throwfill): the walk-scoped capture door is the
/// capture.
#[cfg(test)]
mod r16w15_throwfill_capture_door_tests;

#[cfg(test)]
mod r17w1_throwfill_fused_capture_tests;

#[cfg(test)]
mod r17w2_walk_bounded_capture_tests;

#[cfg(test)]
mod r18w10_throws_fused_refusal_tests;

#[cfg(test)]
mod r19w1_throwfill_fused_rewritten_tests;

/// nio21 wave 15 L4: the throw path's per-thread memos share instead of
/// copying, and answer only for what they were made from.
#[cfg(test)]
mod nio21w15_l4_throw_memo_tests;

#[cfg(test)]
mod r19_throws19_plain_cap_build_tests;

#[cfg(test)]
mod r19_throws2_shared_chain_tests;

/// JIT round 22 wave 1, lane runtime (flip review R1 / R3): the linked
/// reflective call's row lookup and its fail-closed answer.
#[cfg(test)]
mod r22_runtime_linked_reflective_adapter_tests;

/// j27 wave 5 (lane REFLECT; page
/// `docs/feature-designs/class-resolution-and-caching.md`):
/// [`class_nesting_native_raised`] through [`native_leaf_frame`].
#[cfg(test)]
mod j27_class_nesting_native_leaf_tests;

/// j27 wave 5, lane STACKHUNT
/// (`j27-line-table-duplicate-start-pc-picks-the-last-row-FIXED-20261029.md`): the
/// line of a bci is HotSpot's `Method::line_number_from_bci`, which every
/// capture path (interpreter, compiled, inlined, deferred) reads through
/// `line_number_for_bci_in_method`.
#[cfg(test)]
mod j27_stackhunt_line_rule_tests;

mod method_slot_memo;
#[allow(unused_imports)]
pub use method_slot_memo::*;
#[allow(unused_imports)]
use method_slot_memo::*;
mod frame_class_ids_tail;
#[allow(unused_imports)]
pub use frame_class_ids_tail::*;
#[allow(unused_imports)]
use frame_class_ids_tail::*;
mod door_key_memo;
#[allow(unused_imports)]
pub use door_key_memo::*;
#[allow(unused_imports)]
use door_key_memo::*;
mod is_member_lookup_throwable;
#[allow(unused_imports)]
pub use is_member_lookup_throwable::*;
#[allow(unused_imports)]
use is_member_lookup_throwable::*;
