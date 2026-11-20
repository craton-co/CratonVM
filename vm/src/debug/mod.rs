// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP (Java Debug Wire Protocol) foundation.
//!
//! This module implements the wire protocol that IDE debuggers (IntelliJ,
//! Eclipse, VS Code) use to attach to a running JVM.  It provides:
//!
//! - **Transport** — TCP listener with JDWP handshake (`transport.rs`)
//! - **Protocol** — Packet serialization/deserialization (`protocol.rs`)
//! - **Commands** — Handlers for the standard JDWP command sets (`commands.rs`)
//! - **Inspect** — The commands that read or write the heap (`inspect.rs`)
//! - **Events** — Breakpoints, thread events, VM lifecycle (`events.rs`)
//! - **IDs** — Wire-level ID management (`ids.rs`)

pub mod commands;
pub(crate) mod early_return;
pub mod events;
pub(crate) mod field_watch_scope;
pub mod ids;
pub(crate) mod jit_expansion_set;
pub mod inspect;
pub(crate) mod pop_frames;
pub mod protocol;
pub mod transport;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use commands::{FieldInfo, MethodInfo};
use cratonvm_native_api::NativeHeapAccess as _;
use events::EventManager;
use ids::IdManager;
use ids::ThreadId;
use ids::{ObjectExport, ObjectTable};
use protocol::JdwpPacket;
use transport::JdwpConnection;
/// The JDWP `ThreadStatus` of a registered thread (wave 11), mapped as
/// HotSpot's back end maps JVMTI thread state (`map2jdwpThreadStatus`): a
/// dead thread is `ZOMBIE`; a thread blocked on monitor entry `MONITOR`; one
/// in `Thread.sleep` `SLEEPING`; any other wait, park or join, timed or not,
/// `WAIT`; everything else `RUNNING`. The blocking kind is the one
/// `Thread.getState()` reads (`ThreadRegistry::java_block_state`: 1 waiting,
/// 2 blocked, 3 timed). A timed block is a sleep when the frames the thread
/// deposited on entering it (`ThreadRegistry::frame_trace_of`) end in
/// `java.lang.Thread.sleep*`: the blocking region does not say which native
/// entered it.
#[cfg(test)]
fn jdwp_thread_status(
    registry: &crate::threading::thread_registry::ThreadRegistry,
    tid: crate::threading::ThreadId,
) -> u32 {
    jdwp_thread_status_with(registry, tid, &|_| false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------



#[cfg(test)]
mod gci_i11ja_vm_death_hold_tests;

/// Interpreter round i4 wave 93, lane L1: the class-scoped method event set
/// of `DebuggerGates` (`CRATONVM_JDWP_SCOPED_METHOD_EVENTS`).
#[cfg(test)]
mod i93_l1_method_event_scope_tests;

/// Interpreter round i5 wave 105, lane L1: the JDWP server loop's wake
/// (`CRATONVM_JDWP_PROMPT_EVENTS`, [`ServerInput::Wake`]).
#[cfg(test)]
mod i105_l1_server_wake_tests;

/// Interpreter round i7 wave 140, lane L1: `-agentlib:jdwp`'s `allow=`, as
/// HotSpot 25.0.3's socket transport parses and applies it (MEASURED,
/// `tools/probes/interp/L1/L1W140JdwpAllow.java`).
#[cfg(test)]
mod i140_l1_jdwp_allow_tests;

mod local_value;
#[allow(unused_imports)]
pub use local_value::*;
#[allow(unused_imports)]
use local_value::*;
mod c_class_hooks_switch;
#[allow(unused_imports)]
pub use c_class_hooks_switch::*;
#[allow(unused_imports)]
use c_class_hooks_switch::*;
mod ensure_startup_array_classes;
#[allow(unused_imports)]
pub use ensure_startup_array_classes::*;
#[allow(unused_imports)]
use ensure_startup_array_classes::*;
