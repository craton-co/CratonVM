// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! System, Runtime, ProcessBuilder, and Thread native method implementations.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, platform_lib_name, try_alloc_concurrent_synthetic};
// ---------------------------------------------------------------------------
// Process-global singletons for `System.getenv()` (no-arg) and
// `System.getProperties()`.
//
// HotSpot returns the SAME object on every call:
//   - `System.getenv()` в†’ the cached unmodifiable
//     `ProcessEnvironment.theUnmodifiableEnvironment` Map, and
//   - `System.getProperties()` в†’ the `System.props` singleton `Properties`.
// so `System.getenv() == System.getenv()` and
// `System.getProperties() == System.getProperties()` hold (Spring's
// `StandardEnvironmentTests.getSystemEnvironment` / `.getSystemProperties`
// assert this via `isSameAs`). CratonVM allocated a fresh object on every call,
// so identity failed (SC-env-classreading RC-A).
//
// Cache the built object the first time and return it thereafter. The cached
// `ObjectRef`s live ONLY in these process-global mutexes (a Rust side-table,
// invisible to the field/stack/static root scans), so they must be reported as
// GC roots and remapped after a moving collection вЂ” exactly like the singleton
// class loaders (`classloader::gc_scan_loader_singleton_roots`). The matching
// hooks are `gc_scan_system_singleton_roots` (wired into `roots.rs`) and
// `gc_update_system_singleton_refs` (wired into `gc.rs`); `reset_system_singletons`
// clears them when a new VM is created (mirrors `reset_loader_singletons`).
// ---------------------------------------------------------------------------
use cratonvm_native_api::vm_scoped::VmScoped;
/// zt t7/cl3: [`system_stream_report`] answers one line per stream, and says
/// "null" for a `System` it cannot read (a mock has no statics) instead of
/// skipping it: a report that went quiet would read as "nothing wrong".
#[cfg(test)]
mod zt7cl3_system_stream_report_tests;
#[cfg(test)]
mod r23w10_supertype_ask_order_tests;
#[cfg(test)]
mod zt15x12_nameless_duplicate_text_tests;
#[cfg(test)]
mod zt10x7_define0_adoption_tests;

#[cfg(test)]
mod zt9pl_perf_attach_tests;

// ---------------------------------------------------------------------------
// Tests for T2.2.21 Thread.sleep(long, int) argument validation
// ---------------------------------------------------------------------------
#[cfg(test)]
mod t2_tests;

// ---------------------------------------------------------------------------
// Tests for T19.N2 вЂ” Thread.sleep0(J)V
// ---------------------------------------------------------------------------
#[cfg(test)]
mod t19_n2_thread_sleep0_tests;

#[cfg(test)]
mod t14_tests;

#[cfg(test)]
mod t15_tests;

// ---------------------------------------------------------------------------
// SecurityManager.checkExec gating for process spawn (HIGH security)
// ---------------------------------------------------------------------------
//
// These tests verify the contract documented on `check_exec_or_throw`:
//
//   1. With no SecurityManager installed, the gate is a no-op and the
//      spawn proceeds (existing behaviour, preserves backwards compat).
//   2. With a SecurityManager that denies any exec, the gate propagates
//      `SecurityException` and `std::process::Command` is NEVER touched.
//   3. With a SecurityManager that allows only specific paths, only the
//      allowed paths reach the spawn syscall.
//
// We exercise the integration through `native_runtime_exec_string` and
// `native-io`'s `native_process_builder_start` -- the two Java-visible spawn
// entry points -- so any future refactor that bypasses `check_exec_or_throw`
// regresses these tests. `ProcessBuilder.start` reaches the gate only through
// the hook `install_spawn_policy_hook` installs, which is why that test calls
// it explicitly.
//
// MockNativeContext.invoke_virtual returns whatever's pre-armed in
// `invoke_virtual_result` (taken once), defaulting to `Ok(None)` вЂ”
// matching JDK's "no exception thrown == allowed" semantics. This lets us
// simulate both deny (pre-arm an Err) and allow (default).
/// `Runtime.exec` cmdarray validation, measured against HotSpot 25 in
/// probes/ProcSurfaceProbe.java (T10/T11).
#[cfg(test)]
mod exec_cmdarray_tests;

#[cfg(test)]
mod checkexec_security_tests;

#[cfg(test)]
mod runtime_version_parse_tests;

// ---------------------------------------------------------------------------
// P3-C — `defineClass0/1/2` must not flatten a typed linkage error
// (W7-31-enable-preview-wiring.md, Falsifier 3)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod define_class_error_typing_tests;

/// G11-1 (2026-08-17) — the shutdown-hook registration contract.
///
/// Everything here is a unit test of the REFUSAL LADDER and the exact text of
/// its four messages, because that is the part of W7-92's runner that can be
/// decided without a live VM. What these tests deliberately do NOT prove:
///
/// * that a hook ever RUNS. `run_shutdown_hooks` needs `invoke_virtual`, a
///   thread registry and a real `Thread.start()`; only `RShutdownHooks`
///   against a built binary can settle that.
/// * the `Hook already running` arm. `test_utils`' mock answers
///   `thread_is_alive == false` unconditionally, so that arm is reachable in
///   the VM and not from here; it is pinned by the constant test instead.
///
/// The registry is per VM since gc-common w16-e (`SHUTDOWN_STATE`). The tests
/// that touch it use their own VM identity (`hook_ctx`), so no other test in
/// the binary shares their row; they still take `hook_registry_guard()`
/// because they share that one row with each other.
#[cfg(test)]
mod shutdown_hook_contract_tests;

// ---------------------------------------------------------------------------
// Monotonic clock origins (`System.nanoTime`, `Perf.highResCounter`)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod monotonic_clock_tests;

// ---------------------------------------------------------------------------
// gc-common w16-e: per-VM `Runtime.version()` singleton handle
// ---------------------------------------------------------------------------
#[cfg(test)]
mod w16e_runtime_version_root_tests;

// ---------------------------------------------------------------------------
// gci i1/wr: the logging pre-warm arming is per VM
// ---------------------------------------------------------------------------
#[cfg(test)]
mod gci_wr_logging_prewarm_arming_tests;
/// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
/// a served join's region publishes the joined thread for the `wait` loop
/// kinds only.
#[cfg(test)]
mod i68_join_region_tests;

/// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 2): with `CRATONVM_EXIT_HOOK_GC_SAFE` the exit natives run the
/// pre-exit hook inside ONE blocking region on their own context; off, they
/// open none (the default path, unchanged).
#[cfg(test)]
mod gci_i11ja_exit_hook_region_tests;

/// gci i3/jd: `defineClass2` of a DIRECT buffer reads `address + offset`,
/// where `offset` is the caller's `b.position()` (JDK 25
/// `ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain)`), as
/// HotSpot's native does; it used to add the position a second time
/// (`docs/feature-designs/native-dispatch-architecture.md`).
#[cfg(test)]
mod gci_i3jd_define_class2_direct_tests;

/// gci i12/td: `jdk.internal.vm.ThreadSnapshot.create` (the JDK 25 thread
/// dumper's per-thread snapshot).
#[cfg(test)]
mod gci_i12td_thread_snapshot_tests;

/// JIT round 21 wave 26, lane nesting (`CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE`).
#[cfg(test)]
mod r21w26_supertype_miss_cause_tests;

mod shutdown_state;
#[allow(unused_imports)]
pub use shutdown_state::*;
#[allow(unused_imports)]
use shutdown_state::*;
mod runtime_version_publish;
#[allow(unused_imports)]
pub use runtime_version_publish::*;
#[allow(unused_imports)]
use runtime_version_publish::*;
mod note_supertype_initiation;
#[allow(unused_imports)]
pub use note_supertype_initiation::*;
#[allow(unused_imports)]
use note_supertype_initiation::*;
