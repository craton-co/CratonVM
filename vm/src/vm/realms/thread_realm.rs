// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java threads: registry, monitors, virtual-thread scheduling and the main ThreadGroup.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.threads.<field>`.

use crate::threading::monitor::MonitorTable;
use crate::threading::thread_registry::ThreadRegistry;
use crate::types::{ObjectRef, Value};
use crate::vm::vm_init::{MainThreadGroupInit, ThrowableStackTrace};
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::{Arc, OnceLock, Weak};

/// Java threads: registry, monitors, virtual-thread scheduling and the main ThreadGroup.
pub struct ThreadRealm {
    /// Captured Throwable backtraces, shared by every Java thread in this VM.
    ///
    /// Java exposes a Throwable's stack trace independently of the thread that
    /// filled it in. Keeping this in `SharedVm` also lets the registry survive
    /// the producer thread's teardown. Entries are non-owning and swept after
    /// each collection, so completed exception-heavy workloads do not retain
    /// stale frame vectors indefinitely.
    pub(crate) throwable_stacks: RwLock<FxHashMap<i32, ThrowableStackTrace>>,

    /// Monitor table for JVM intrinsic locks (monitorenter/monitorexit).
    pub monitors: MonitorTable,

    /// Cached "main" `java.lang.ThreadGroup` object used by every
    /// VM-created `Thread` as `holder.group`.  Lazily created the first
    /// time `NativeContextImpl::current_thread_object` builds a Thread
    /// in real-JDK mode — the ThreadGroup constructor invokes bytecode,
    /// so it can't run during `SharedVm::new`.
    pub main_thread_group: RwLock<Option<ObjectRef>>,

    /// Claim/wait coordination for the lazy `main_thread_group` build
    /// (`NativeContextImpl::get_or_create_main_thread_group`,
    /// `vm/src/vm/vm_exec.rs`). Mirrors `class_init_waiters`'s JVMS §5.5
    /// claim/wait/notify shape: exactly one thread transitions
    /// `Idle -> InProgress` and performs the (allocating, bytecode-running)
    /// build; every other concurrent caller blocks on the `InProgress`
    /// waiter's condvar instead of redundantly building its own
    /// `ThreadGroup` pair. Fixes a confirmed TOCTOU race (see
    /// CRATONVM-SPRING-GENUINE-BUGLIST, "5.8
    /// follow-up #4") where several `cratonvm-aio-dispatch-N` threads
    /// lazily building their first `Thread` mirror at the same instant
    /// could each observe `main_thread_group == None` and independently
    /// race through the whole unguarded build, corrupting whichever
    /// build's objects lost the final write.
    pub main_thread_group_init: parking_lot::Mutex<MainThreadGroupInit>,

    /// Registry of all JVM threads (main + spawned).
    pub thread_registry: ThreadRegistry,

    /// Virtual thread scheduler — bounds concurrent virtual thread execution
    /// to a carrier-thread pool (JEP 444, Java 21).
    pub virtual_scheduler: Arc<crate::threading::VirtualThreadScheduler>,

    /// Continuation scheduler and heap-resident virtual-thread execution
    /// states. Unlike `virtual_scheduler`'s legacy permit semaphore, this
    /// manager owns a bounded set of carrier OS threads.
    pub virtual_thread_manager: Arc<crate::threading::VirtualThreadManager>,
}
