// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The C `jvmtiEnv` a native agent reaches through `GetEnv` (interpreter
//! round i1 wave 12, lane L1; the lifecycle events, `Exception`, per-thread
//! enabling and the class functions in wave 13).
//!
//! Until wave 12 there was no C JVMTI function table at all: `GetEnv` for a
//! JVMTI version answered `JNI_EVERSION` (wave 11), so an `-agentpath:` agent
//! could negotiate nothing and set no breakpoint, although the VM keeps
//! per-VM JVMTI breakpoints and delivers them from the dispatch loop's
//! suspend point (`runtime::jvmti::set_breakpoint_for_vm`, wave 10).
//!
//! `jvmtiEnv` is, like `JNIEnv`, a pointer to a pointer to the function
//! table: [`JvmtiNativeEnv`] starts with that pointer. The table has JDK 25's
//! 156 slots (`jvmtiInterface_1_`, function 1 `reserved1` through 156
//! `SetHeapSamplingInterval`). Since round i2 wave 63 (lane L1) every slot
//! but HotSpot's reserved ones is served, by area: this file (the thread,
//! frame, local, class, method, breakpoint, capability, property and
//! class-search functions, waves 12-61), `agent_functions` (agent threads,
//! interrupt / stop, monitor usage, constant pool, source debug extension,
//! native prefixes, JNI function table, verbose flags, all-virtual-thread
//! suspension, heap sampling interval), `modules` (the module functions),
//! `heap` (tags and the heap walks), `extensions` (HotSpot's three extension
//! functions and three extension events) and `events_more` (the field
//! watches and the events posted outside the dispatch loop: monitor, GC,
//! `ObjectFree`, `VMObjectAlloc`, `SampledObjectAlloc`, `NativeMethodBind`,
//! `ResourceExhausted`, `DataDumpRequest`, virtual threads, `ClassUnload`,
//! compiled code). The history of each slot is on the page
//! `docs/feature-designs/jvmti-delivery-threading.md`.
//!
//! Capabilities follow HotSpot's model (wave 63): the always-available set,
//! the always-solo set (one env at a time), the OnLoad set and the OnLoad-solo
//! set, which only a startup env may acquire, and which it keeps as potential
//! in the live phase (`onload_acquired`); a solo capability another env holds
//! is not potential. The class-hook, retransform, redefine and frame-control
//! capabilities are potential only with their switches on, so an agent never
//! gets a capability whose events never arrive (the D14 rule,
//! once in the older `jvmti/capabilities.rs`, deleted in wave 125).
//!
//! Delivery, always on the thread the event happens on, with a `JNIEnv*`,
//! the thread's `java.lang.Thread` as a local reference (in a local frame
//! popped after the callback) and the thread's JNI context installed, so the
//! callback can call JNI ([`in_event_context`]):
//!
//! * `Breakpoint`, `Exception`, (wave 17) `MethodEntry` / `MethodExit`,
//!   (wave 22) `SingleStep`, (wave 45) `FramePop` and (wave 52)
//!   `ExceptionCatch`
//!   through the VM's `JvmtiEventManager`: the
//!   env registers a Rust-side `runtime::jvmti::JvmtiEnv` on it the first
//!   time it enables one of them, whose closures call the agent's callbacks
//!   (the manager's attached-env loop). The env mirrors its enable state
//!   into that delivery env's own state (wave 14,
//!   [`JvmtiNativeEnv::sync_event`]), which is what the manager's union
//!   flags and delivery read, so a disable or a dispose takes the event off
//!   the VM.
//! * `ClassPrepare` once per class, when the class is linked and prepared
//!   (by its initialization or, wave 17, as a supertype of a class being
//!   linked, which reports the supertype first: `vm::link_class_on_thread`),
//!   before any of its code runs (`debug::class_prepared_on_thread`, behind one per-VM load,
//!   `DebuggerGates::native_class_prepare_armed`; wave 15 moved preparation
//!   ahead of the superclass). A compile door does not link a class early
//!   while the event is enabled (`vm::link_class_without_java`).
//! * `VMInit` once per VM, on the main thread, when the VM enters the live
//!   phase at `SharedVm::set_init_level(3)` ([`enter_live_phase`], wave 14).
//!   The envs a startup agent obtained in `Agent_OnLoad`
//!   ([`collect_startup_envs`]) are bound to the VM by `Vm::new`
//!   ([`bind_startup_envs`]) and are in the start phase until then.
//! * (Round i2 wave 55, with `DebuggerGates::c_jvmti_phases`) the VM is in
//!   the primordial phase from that binding until `System.initPhase1`
//!   returns, with no class event; `VMStart` once per VM, on the main thread,
//!   when it enters the start phase at `SharedVm::set_init_level(2)`
//!   ([`enter_start_phase`]); the dead phase after `VMDeath`.
//! * `VMDeath` when the `Vm` is dropped, followed by the agents'
//!   `Agent_OnUnload` ([`shut_down_agents`]).
//!
//! A `jthread` or `jclass` an agent passes is decoded inside the JNI
//! functions' foreign-thread entry (`jni::ForeignJniEntry`, wave 19;
//! [`thread_id_of`], [`class_of`]), so an agent's own idle attached thread
//! reads no handle while a collection may move its object.
//!
//! An env is never freed: `DisposeEnvironment` clears its breakpoints,
//! unregisters its delivery and marks it disposed (every later call answers
//! `JVMTI_ERROR_INVALID_ENVIRONMENT`), so a callback in flight never reads
//! freed memory. The cost is one small allocation per `GetEnv` for the life
//! of the process.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::{Arc, Weak};

use crate::classloading::ClassId;
use crate::native::jni::{self, JInt, JNIEnv, JObject};
use crate::runtime::interpreter::BlockedRow;
use crate::runtime::jvmti::{self as rt, EventCallbacks, JvmtiEventKind};
use crate::runtime::lambda_proxy::{LambdaProxyFacts, LambdaProxyMethod};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use cratonvm_native_api::{NativeClassAccess as _, NativeHeapAccess as _, NativeSystemAccess as _};

// Interpreter round i2 wave 63, lane L1: the slots and events the table
// still answered with `JVMTI_ERROR_NOT_AVAILABLE` (the i51-L1 page's last
// items), by area.
mod agent_functions;
pub(crate) use agent_functions::{
    all_native_method_prefixes, run_agent_thread_body, take_pending_agent_body, AgentThreadBody,
};
mod events_more;
pub(crate) use events_more::{
    note_unloaded_classes, post_data_dump_request, post_field_watch, post_gc, post_monitor_event,
    post_native_method_bind, post_resource_exhausted, post_rust_native_method_bind,
    post_virtual_thread, post_vm_object_alloc, sample_allocation, FieldEvent, MonitorEvent,
    RustBindTracking, VirtualThreadEvent,
};
// j27 wave 5 (lane JVMTIFRAME): a compiled / OSR'd frame's locals from its
// home slots (`CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`, default OFF).
mod compiled_locals;
mod extensions;
mod heap;
// Interpreter round i5 wave 103 (lane L1): the JDWP heap walks
// (`debug::inspect`, `canGetInstanceInfo`).
// Wave 104: `debugger_heap_scan` (`ClassesBySignature` of an array).
pub(crate) use heap::{
    debugger_heap_query, debugger_heap_scan, DebuggerHeapAnswer, DebuggerHeapQuery,
};
mod modules;
/// `trace` (outermost first) with the rows of `natives` (outermost first,
/// `(interpreter depth at the call, class, key, JIT entry-chain length at the
/// call)`, `JvmThread::jni_native_frames`) spliced in (wave 42): row `k` goes
/// right before `trace[positions[k]]`, or after every row for a position at
/// or past the end (`stackwalker::capture_trace_with_anchor_positions`: the
/// first frame the native's call put on the stack, an interpreter frame or,
/// since wave 44, a compiled activation its own upcall entered). Positions
/// never decrease; natives at one position keep their order, the outer one
/// first.
#[cfg(test)]
fn splice_native_rows<'a>(
    trace: &'a [crate::native::registry::StackTraceEntry],
    positions: &[usize],
    natives: &[(usize, ClassId, u64, u32)],
) -> Vec<FrameRow<'a>> {
    splice_rows(trace, positions, natives, &[])
}

/// Interpreter round i7 wave 138, lane L1: the proxy rows a step counts.
#[cfg(test)]
mod i138_l1_proxy_rows_below_tests;
// ---------------------------------------------------------------------------
// What agents call at startup (interpreter round i2 wave 61, lane L1)
// ---------------------------------------------------------------------------
//
// The VM system properties (130-132), the class search additions (149 /
// 151), `GetObjectHashCode` (58), `GetObjectSize` (154) and `GetTimerInfo`
// (138), which answered `JVMTI_ERROR_NOT_AVAILABLE` until wave 61
// (`docs/feature-designs/jvmti-delivery-threading.md`).
// MEASURED on HotSpot 25.0.3 with the Rust port of the shim
// (`tools/probes/interp/L1/L1W61JvmtiAgentStartup.java`). No switch: each
// slot answered 98.

// The keys of HotSpot's VM property list (`Arguments::system_properties()`,
// its readable entries) besides the `-D` ones, which `SetSystemProperty` may
// write in the OnLoad phase, and which `-D` ones are internal: one table,
// `cratonvm_types::vm_properties` (interpreter round i4 wave 95, lane L1;
// stage 1 of
// `docs/feature-designs/real-jdk-mode.md`;
// private constants of this file until then). `user.dir`, `os.name`,
// `java.version` and the rest of `System.getProperties()` come from the class
// library, not the VM, and answer `JVMTI_ERROR_NOT_AVAILABLE`.
use cratonvm_types::vm_properties::{
    is_internal_property as internal_property, is_writeable_vm_property, vm_property_keys,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod gc8_window_rows_tests;

#[cfg(test)]
mod gci_i11ja_event_thread_tests;

#[cfg(test)]
mod gci_i16sn_native_transition_tests;

#[cfg(test)]
mod gci_i47w_oz47_raw_monitor_lock_tests;

#[cfg(test)]
mod i134_l1_blocked_entry_frame_control_tests;

mod has_cap;
#[allow(unused_imports)]
pub use has_cap::*;
#[allow(unused_imports)]
use has_cap::*;
mod jit_chain_len;
#[allow(unused_imports)]
pub use jit_chain_len::*;
#[allow(unused_imports)]
use jit_chain_len::*;
mod read_capabilities;
#[allow(unused_imports)]
pub use read_capabilities::*;
#[allow(unused_imports)]
use read_capabilities::*;
