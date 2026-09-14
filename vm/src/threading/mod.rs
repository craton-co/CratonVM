// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Threading infrastructure for the JVM.
//!
//! This module provides per-thread execution state (`JvmThread`), thread
//! identity (`ThreadId`), and monitor/synchronization support.

pub mod gc_barrier;
pub mod jmm;
pub mod jvm_thread;
pub mod monitor;
pub mod thread_registry;
// P1 — explicit runtime transition states. A checked state machine over the
// scattered flags/counters that encode a thread's execution state today; a
// SHADOW record in this pass (the existing mechanism stays authoritative).
// See `docs/threading/thread-transition-states.md`.
pub mod thread_state;
pub mod varhandle;
pub mod virtual_scheduler;
pub mod virtual_threads;
// T19.6 — Vert.x / Netty NioEventLoop affinity scheduler.
pub mod event_loop;
// WP4.7 — `StampedLock` + `ReentrantReadWriteLock` integration shim
// (state actually lives in `cratonvm-native-builtins::stamped_lock`).
pub mod stamped;
// WP4.3 / WP4.5 — ForkJoinPool common-pool singleton + ScheduledExecutorService
// task registry. Native overrides in `native-builtins/{phases_early,phases_late,
// concurrent_extras}.rs` wire submission paths into these modules so the
// real JDK 25 `ForkJoinPool` / `ScheduledThreadPoolExecutor` classes have
// somewhere to land queued work.
pub mod forkjoin;
pub mod scheduled;

pub use event_loop::{
    current_event_loop, event_loop_manager, EventLoop, EventLoopAffinity, EventLoopError,
    EventLoopId, EventLoopManager, EventLoopTask,
};
pub use gc_barrier::GcBarrier;
pub use jvm_thread::{JvmThread, ParkState, ThreadId, ThreadKind};
pub use monitor::MonitorTable;
pub use thread_registry::ThreadRegistry;
pub use thread_state::{thread_state_census, RelocationRule, ThreadExecState, ThreadStateCensus};
pub use virtual_scheduler::VirtualThreadScheduler;
pub use virtual_threads::{
    Continuation, ContinuationScope, ContinuationState, ForkJoinScheduler, FrozenFrame, PinReason,
    SchedulerStatsSnapshot, ThreadBuilder, ThreadBuilderKind, VirtualThread, VirtualThreadManager,
    VirtualThreadState,
};
