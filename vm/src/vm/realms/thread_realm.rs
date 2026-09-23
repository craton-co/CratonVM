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
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::{Arc, OnceLock, Weak};

/// Sentinel for a Throwable field slot that has not yet been resolved in this
/// VM. Field layouts are VM state: a process-global slot cache silently binds
/// a second VM to the first VM's bootstrap layout.
pub(crate) const UNRESOLVED_THROWABLE_FIELD_INDEX: usize = usize::MAX;
/// Sentinel for a StackTraceElement class id that has not yet been resolved in
/// this VM. Class id zero is valid (`java/lang/Object`), so it cannot be the
/// sentinel.
pub(crate) const UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID: u32 = u32::MAX;

/// Independent registry locks for retained Throwable traces. This is a power
/// of two so a captured throwable selects its shard with a cheap hash mask.
pub(crate) const THROWABLE_STACK_SHARDS: usize = 32;

/// Hot Throwable metadata scoped to one VM. Values are immutable once
/// resolved, so atomics avoid turning the constructor path into lock traffic.
pub(crate) struct ThrowableLayoutCache {
    pub(crate) detail_message: AtomicUsize,
    pub(crate) cause: AtomicUsize,
    pub(crate) suppressed_exceptions: AtomicUsize,
    pub(crate) backtrace: AtomicUsize,
    pub(crate) depth: AtomicUsize,
    pub(crate) stack_trace: AtomicUsize,
    pub(crate) stack_trace_element_class_id: AtomicU32,
    pub(crate) throwable_class_id: AtomicU32,
    pub(crate) suppressed_sentinel_static_index: AtomicUsize,
}

impl ThrowableLayoutCache {
    pub(crate) fn new() -> Self {
        Self {
            detail_message: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            cause: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            suppressed_exceptions: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            backtrace: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            depth: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            stack_trace: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
            stack_trace_element_class_id: AtomicU32::new(UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID),
            throwable_class_id: AtomicU32::new(UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID),
            suppressed_sentinel_static_index: AtomicUsize::new(UNRESOLVED_THROWABLE_FIELD_INDEX),
        }
    }

    pub(crate) fn field_slot(&self, field_name: &str) -> Option<&AtomicUsize> {
        match field_name {
            "detailMessage" => Some(&self.detail_message),
            "cause" => Some(&self.cause),
            "suppressedExceptions" => Some(&self.suppressed_exceptions),
            "backtrace" => Some(&self.backtrace),
            "depth" => Some(&self.depth),
            "stackTrace" => Some(&self.stack_trace),
            _ => None,
        }
    }
}

/// Java threads: registry, monitors, virtual-thread scheduling and the main ThreadGroup.
pub struct ThreadRealm {
    /// Captured Throwable backtraces, shared by every Java thread in this VM.
    ///
    /// Java exposes a Throwable's stack trace independently of the thread that
    /// filled it in. Keeping this in `SharedVm` also lets the registry survive
    /// the producer thread's teardown. Entries are non-owning and swept after
    /// each collection, so completed exception-heavy workloads do not retain
    /// stale frame vectors indefinitely.
    pub(crate) throwable_stacks: Vec<RwLock<FxHashMap<usize, ThrowableStackTrace>>>,

    /// Per-VM slots used by Throwable and StackTraceElement native helpers.
    pub(crate) throwable_layout_cache: ThrowableLayoutCache,

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

#[cfg(test)]
mod tests {
    use super::{ThrowableLayoutCache, UNRESOLVED_THROWABLE_FIELD_INDEX};
    use std::sync::atomic::Ordering;

    #[test]
    fn throwable_layout_slots_are_not_shared_between_vms() {
        let first = ThrowableLayoutCache::new();
        let second = ThrowableLayoutCache::new();
        first
            .detail_message
            .store(7, Ordering::Relaxed);

        assert_eq!(first.detail_message.load(Ordering::Relaxed), 7);
        assert_eq!(
            second.detail_message.load(Ordering::Relaxed),
            UNRESOLVED_THROWABLE_FIELD_INDEX,
            "a second VM must not inherit the first VM's Throwable layout"
        );
    }
}
