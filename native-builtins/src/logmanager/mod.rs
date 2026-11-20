// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.H3 — `java.util.logging.LogManager` singleton + `org.jboss.logmanager.LogManager` subclass.
//!
//! KC26 (Quarkus/Keycloak) fails early because
//! `java.util.logging.LogManager.getLogManager()` in our pre-T19.H3 code was
//! registered multiple times with subtly different field counts, and the
//! post-clinit fixup in `vm_util.rs` already allocates a `LogManager.manager`
//! static — but the subsequent Java-side bytecode in `LogManager.<clinit>` /
//! `java.util.logging.LogManager.getLogManager0()` does a
//! `Class.newInstance()` on the class named by the
//! `java.util.logging.manager` system property (Quarkus sets it to
//! `org.jboss.logmanager.LogManager`) and then casts the result to
//! `java.util.logging.LogManager`. When our VM returns a `Class` mirror
//! instead of a fresh instance — e.g. because `newInstance()` was
//! mis-routed or the ctor blew up and left a `Class` on the stack — the
//! checked-cast raises `ClassCastException: java/lang/Class cannot be cast
//! to java/util/logging/LogManager`.
//!
//! This module takes ownership of the whole `LogManager` surface:
//!
//! 1. `LogManager.getLogManager()` (and its `org.jboss.logmanager.LogManager`
//!    subclass equivalent) returns a **per-VM singleton instance
//!    ObjectRef** allocated once and cached in `singleton_cell(vm)`.
//!    Subsequent calls return the same ObjectRef so pointer-identity
//!    comparisons inside Quarkus/JBoss bytecode still observe a stable
//!    manager. Every side table in this module is per VM (`per_vm_table`) and
//!    is forgotten at VM teardown (`forget_vm_logging_state`).
//! 2. `getLogger(String)` — idempotently returns the same `Logger` mirror
//!    for the same name, backed by the `LoggerRegistry`. This complements
//!    the existing `wildfly_core::get_logger` registry by also producing
//!    a heap ObjectRef visible to JDK bytecode.
//! 3. `addLogger(Logger)` — returns `true` on first successful add, `false`
//!    if a logger with the same name is already registered (matches spec).
//!    Rejects names containing `../`, `\\`, `:`, or any ASCII control char
//!    so malicious `loadConfiguration` calls can't traverse the filesystem
//!    via logger-name injection.
//! 4. `readConfiguration()` / `readConfiguration(InputStream)` — both parse
//!    the configuration and apply it (`apply_jul_config_entries`): install
//!    `handlers=` on the root logger and record `<logger>.level` entries.
//!    The no-arg overload follows the JDK's resolution order
//!    (`java.util.logging.config.class`, then `.config.file`, then
//!    `$java.home/conf/logging.properties`).
//! 5. `reset()` — clears the logger registry (leaves the singleton in
//!    place; JDK spec allows the manager instance to be kept while the
//!    logger-name set is flushed).
//! 6. `getLoggerNames()` — returns an `Enumeration<String>` over the
//!    currently-registered logger names, with a snapshot taken at call
//!    time so subsequent registrations don't ConcurrentModify the
//!    enumeration.
//!
//! ## Interaction with the `vm_util.rs` post-clinit fixup
//!
//! `vm_util.rs::post_clinit_fixup` allocates a `LogManager` object and
//! stashes it in the static `LogManager.manager` field. When our
//! `getLogManager()` native runs AFTER that fixup, we read
//! `LogManager.manager` first and honour it; if it's null (fixup ran but
//! allocator failed, or we're in a mode where it didn't run), we fall
//! back to allocating our own singleton via `alloc_object`. This way
//! the DELAYED_HANDLER registered against the fixup-time manager is
//! still reachable from the later native-returned instance.
//!
//! ## Interaction with `wildfly_core::get_logger`
//!
//! `wildfly_core::get_logger(name)` maintains a Rust-side
//! `Arc<LoggerMirror>` registry keyed by name. We delegate to it so
//! tracing redaction + level-override state stays unified across JBoss
//! and JUL code paths.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Mutex, OnceLock,
};

use cratonvm_native_api::{
    NativeContext, NativeHandle, NativeHandleScope, NativeKind, NativeMethodRegistry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use crate::try_alloc_concurrent_synthetic;
/// Test-only helper: wipe the singleton + logger registry so tests
/// don't see state bleed between parallel threads.
/// VM identity the crate's own tests run under.
///
/// The side tables below are per-VM (see `per_vm_table`), keyed by
/// `NativeContext::vm_identity`. These tests drive mock contexts, which take
/// the trait's default identity of 0, so that is the scope their assertions
/// and `reset_state_for_tests` must look at.
#[cfg(test)]
const TEST_VM: usize = 0;

#[cfg(test)]
pub fn reset_state_for_tests() {
    if let Ok(mut g) = singleton_cell(TEST_VM).lock() {
        *g = None;
    }
    if let Ok(mut r) = logger_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_logger_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_root_handler_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut h) = logger_handlers(TEST_VM).lock() {
        h.clear();
    }
    // The explicit-level table was NOT reset here, so one test's
    // `setLevel("com.example.App", WARNING)` was still the threshold every
    // LATER test's `com.example.App.*` logger inherited — order-dependent
    // results in the one table whose whole purpose is to be consulted by name.
    if let Ok(mut l) = logger_explicit_levels(TEST_VM).lock() {
        l.clear();
    }
    // Both level tables and the resolved-initializer cache, for the same
    // reason: they are process-wide and name-keyed, so anything left in them
    // is the next test's inherited threshold or the next test's provider.
    if let Ok(mut l) = logger_minimum_levels(TEST_VM).lock() {
        l.clear();
    }
    if let Ok(mut c) = jboss_initializer_cell(TEST_VM).lock() {
        *c = None;
    }
    if let Ok(mut l) = config_listeners(TEST_VM).lock() {
        l.clear();
    }
    if let Ok(mut m) = log_record_messages(TEST_VM).lock() {
        m.clear();
    }
    if let Ok(mut p) = parsed_log_properties(TEST_VM).lock() {
        p.clear();
    }
    if let Ok(mut g) = jboss_log_context_singleton(TEST_VM).lock() {
        *g = None;
    }
    if let Ok(mut r) = jboss_logger_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut m) = attachments(TEST_VM).lock() {
        m.clear();
    }
    // gci i41/rv: the whole row, so the console-sink switch latch is re-read
    // under the next test's overrides.
    if let Ok(mut d) = primordial_config_done(TEST_VM).lock() {
        *d = JulVmLatch::default();
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;

/// gc-common w13-b (`common-w12b-jul-config-state-is-process-global`): the JUL
/// configuration, its config-file latch and the `LogRecord` payloads are per
/// VM, and a VM's logging rows are forgotten at its teardown. Every test runs
/// under its own VM identities (`0xB13_*`), so none needs `tests::test_lock`.
#[cfg(test)]
mod w13b_per_vm_logging_state_tests;

/// gc-common w14-d (`common-w13b-jul-identity-keyed-rows-outlive-their-keys-FIXED-20260923`):
/// the Tomcat JULI registries are keyed by the context loader's address and
/// swept with it, and `removeHandler` on the JULI root takes the handler out of
/// the root registry. Every test runs under its own VM identity (`0xD14_02*`).
#[cfg(test)]
mod w14d_juli_registry_tests;

/// Interpreter round i3 wave 79, lane L1
/// (`i74-orch-compatible-anonymous-logger-parent-config-npe`): which loggers
/// count as minted, so the `Logger.getLevel` / `setLevel` natives keep a minted
/// logger's level in `LOGGER_FIELD_LEVEL` although its `config` is now a real
/// `ConfigurationData`.
#[cfg(test)]
mod i79_minted_logger_tests;

/// gci i24/lk (`gci-i24lk-jul-name-keyed-rows-pin-webapp-loaders-FIXED-20261017`):
/// the name-keyed `addHandler` rows belong to the Logger that filed them.
#[cfg(test)]
mod i24lk_named_handler_row_tests;

/// gci i25/jl: JUL delivery as HotSpot's `Logger.log(LogRecord)` (the filter's
/// answer honoured with the handlers' record, every level of the parent chain,
/// the level gate, a null message still published), `removeHandler`'s own row,
/// and the weak named-logger registry (`CRATONVM_JUL_WEAK_LOGGER_REGISTRY`).
#[cfg(test)]
mod i25jl_jul_delivery_tests;

mod per_vm_table;
#[allow(unused_imports)]
pub use per_vm_table::*;
#[allow(unused_imports)]
use per_vm_table::*;
mod native_jboss_logger_log_level;
#[allow(unused_imports)]
pub use native_jboss_logger_log_level::*;
#[allow(unused_imports)]
use native_jboss_logger_log_level::*;
mod register_jboss_backend_do_log_stand;
#[allow(unused_imports)]
pub use register_jboss_backend_do_log_stand::*;
#[allow(unused_imports)]
use register_jboss_backend_do_log_stand::*;
