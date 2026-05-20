//! Threading infrastructure for the JVM.
//!
//! This module provides per-thread execution state (`JvmThread`), thread
//! identity (`ThreadId`), and monitor/synchronization support.

pub mod gc_barrier;
pub mod jmm;
pub mod jvm_thread;
pub mod monitor;
pub mod thread_registry;
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

pub use gc_barrier::GcBarrier;
pub use jvm_thread::{JvmThread, ParkState, ThreadId, ThreadKind};
pub use monitor::MonitorTable;
pub use thread_registry::ThreadRegistry;
pub use virtual_scheduler::VirtualThreadScheduler;
pub use virtual_threads::{
    Continuation, ContinuationScope, ContinuationState, ForkJoinScheduler, FrozenFrame, PinReason,
    SchedulerStatsSnapshot, ThreadBuilder, ThreadBuilderKind, VirtualThread,
    VirtualThreadManager, VirtualThreadState,
};
pub use event_loop::{
    EventLoop, EventLoopAffinity, EventLoopError, EventLoopId, EventLoopManager,
    EventLoopTask, event_loop_manager, current_event_loop,
};
