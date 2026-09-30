// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMS §5.3 / §5.4.3: what a VM-initiated class resolution does with the
//! initiating loader's own throwable, and with another thread's outcome for
//! the same resolution.
//!
//! * **A loader that throws** (interpreter round i1 wave 38, lane L5). When the
//!   VM asks a user-defined loader for a name and its `loadClass` throws,
//!   HotSpot's resolution fails with that throwable: a
//!   `ClassNotFoundException` becomes `NoClassDefFoundError` (message: the
//!   internal name; cause: the loader's exception), anything else propagates
//!   as it is (`SystemDictionary::load_instance_class` returns the pending
//!   exception, `resolve_or_fail` wraps only a CNFE). CratonVM's door
//!   (`constants.rs` `drive_defining_loader_load_named`) treated every failed
//!   `loadClass` as "not answered" and fell back to the global store, which
//!   may define the name in the application loader: a loader that refused a
//!   name on purpose was overridden, and a real error was replaced by a
//!   success. Under `--jdk-only`, for a loader whose `loadClass` is its own
//!   bytecode (`classloader_real::loader_load_class_is_bytecode`), the throw
//!   is now the resolution's ([`loader_throw_as_resolution_error`]). Probe
//!   `tools/probes/interp/L5/L5W37LoaderThrowPropagates.java`. Since wave 40
//!   a loader whose `loadClass` is CratonVM's base-delegation native too, for
//!   every throwable that is not a `ClassNotFoundException`, and for a CNFE
//!   where that native consults no flat store before the loader's `findClass`
//!   (`classloader_real::base_load_class_miss_is_the_loaders`); its other
//!   CNFEs keep the fallback, since those misses are not all faithful yet.
//!   Probe `tools/probes/interp/L5/L5W40BaseLoaderThrow.java`.
//! * **A racing resolution** (`i25-L5-a-racing-resolution-…`, fixes 1 and 2).
//!   HotSpot publishes one outcome per constant-pool entry: a failure is
//!   saved only while the entry is unresolved (a loser takes the winner's
//!   class), and a success that meets an entry already in error throws the
//!   recorded error. Counted by [`note_resolution_race`]; probe
//!   `tools/probes/interp/L5/L5W37RacingEntryOutcome.java`.
//!
//! Positive control: `CRATONVM_DBG=access` prints `[ACCESS-DBG] LOADER-THROW
//! PROPAGATE …` and `[ACCESS-DBG] RESOLUTION-RACE …` per event and, at exit,
//! `[ACCESS-DBG] LOADER census: …` ([`report_loader_census_at_exit`]).
//! `--compatible` is never reached: every caller tests the mode first.

use crate::classloading::ClassId;
use crate::error::MethodCallFailed;
use crate::threading::jvm_thread::JvmThread;
use crate::types::ObjectRef;
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

/// The resolution error for `exc`, which the `loadClass` of the loader that
/// defined `referencing_class_id` threw when the VM asked it for `name`
/// (internal form): `NoClassDefFoundError: <error_name>` caused by `exc` when
/// `exc` is a `ClassNotFoundException`, else `exc` itself. `error_name` is
/// the name being resolved: `name` itself, or the array class whose element
/// `name` is (HotSpot's message names the array; interpreter round i1 wave
/// 41, lane L5, probe `L5W41ArrayComponentLoaderThrow`). Counted in
/// `ClassRealm::loader_throws_propagated` and traced under
/// `CRATONVM_DBG=access`. `--jdk-only` callers only; cold.
///
/// `exc` must be live on entry (no allocation since the loader returned); it
/// is pinned across the `NoClassDefFoundError`'s allocation.
#[cold]
#[inline(never)]
pub(crate) fn loader_throw_as_resolution_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    error_name: &str,
    exc: ObjectRef,
) -> MethodCallFailed {
    let is_cnfe = is_class_not_found(shared, exc);
    let exc_name = shared
        .classes
        .class_manager
        .read()
        .get_class(shared.mem.heap.class_id_of(exc))
        .map(|c| c.name.replace('/', "."))
        .unwrap_or_default();
    let n = shared
        .classes
        .loader_throws_propagated
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if cratonvm_types::flags().loader.dbg_access {
        let (loader_class, referencing) = {
            let cm = shared.classes.class_manager.read();
            let loader_class = cratonvm_native_builtins::classloader::defining_loader_for(
                shared.vm_identity,
                referencing_class_id.as_u32(),
            )
            .and_then(|loader| cm.get_class(shared.mem.heap.class_id_of(loader)))
            .map(|c| c.name.replace('/', "."))
            .unwrap_or_default();
            let referencing = cm
                .get_class(referencing_class_id)
                .map(|c| c.name.replace('/', "."))
                .unwrap_or_default();
            (loader_class, referencing)
        };
        let loaded_elsewhere = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(name)
            .is_some();
        eprintln!(
            "[ACCESS-DBG] LOADER-THROW PROPAGATE #{n} {exc_name} from {loader_class}.loadClass(\"{}\") resolving it for {referencing}{}",
            name.replace('/', "."),
            if loaded_elsewhere {
                " (a class of that name is loaded elsewhere: the old fallback would have answered)"
            } else {
                ""
            }
        );
    }
    if !is_cnfe {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let ncdfe = crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/NoClassDefFoundError",
        Some(error_name),
    );
    let exc = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    match ncdfe {
        Ok(ncdfe) => {
            // No allocation between the read-back and the store.
            crate::runtime::exceptions::set_cause_by_name(shared, ncdfe, exc);
            MethodCallFailed::ExceptionThrown(ncdfe)
        }
        // The error could not be built (heap exhausted): that failure is the
        // resolution's, as any allocation failure while resolving is.
        Err(e) => e,
    }
}

/// Whether `exc` is a `java.lang.ClassNotFoundException` (or a subclass).
/// Reads only; allocates nothing.
pub(crate) fn is_class_not_found(shared: &SharedVm, exc: ObjectRef) -> bool {
    let cm = shared.classes.class_manager.read();
    let exc_class = shared.mem.heap.class_id_of(exc);
    cm.get_loaded_class_id("java/lang/ClassNotFoundException")
        .is_some_and(|cnfe| exc_class == cnfe || cm.is_subclass_of(exc_class, cnfe))
}

/// The resolution error when the `loadClass` of the loader that defined
/// `referencing_class_id` RETURNED `null` for `name` (internal form), or
/// (`wrong_name`) a class of another name: `NoClassDefFoundError: <name>`
/// with no cause, as HotSpot's `resolve_or_fail` raises it for a load that
/// produced no class and left no pending exception (JVMS §5.3.2). The class
/// opcodes record it against the entry like any `LinkageError`. Counted in
/// `ClassRealm::loader_nulls_propagated` and traced under
/// `CRATONVM_DBG=access`. `--jdk-only`, a loader whose `loadClass` is its own
/// bytecode; cold. Interpreter round i1 wave 39, lane L5; probe
/// `tools/probes/interp/L5/L5W39LoaderReturnsNull.java`.
#[cold]
#[inline(never)]
pub(crate) fn loader_null_as_resolution_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    error_name: &str,
    wrong_name: bool,
) -> MethodCallFailed {
    let n = shared
        .classes
        .loader_nulls_propagated
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if cratonvm_types::flags().loader.dbg_access {
        let (loader_class, referencing) = {
            let cm = shared.classes.class_manager.read();
            let loader_class = cratonvm_native_builtins::classloader::defining_loader_for(
                shared.vm_identity,
                referencing_class_id.as_u32(),
            )
            .and_then(|loader| cm.get_class(shared.mem.heap.class_id_of(loader)))
            .map(|c| c.name.replace('/', "."))
            .unwrap_or_default();
            let referencing = cm
                .get_class(referencing_class_id)
                .map(|c| c.name.replace('/', "."))
                .unwrap_or_default();
            (loader_class, referencing)
        };
        eprintln!(
            "[ACCESS-DBG] LOADER-NULL PROPAGATE #{n} {}.loadClass(\"{}\") returned {} resolving it for {referencing}",
            loader_class,
            name.replace('/', "."),
            if wrong_name {
                "a class of another name"
            } else {
                "null"
            }
        );
    }
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/NoClassDefFoundError",
        Some(error_name),
    ) {
        Ok(ncdfe) => MethodCallFailed::ExceptionThrown(ncdfe),
        // The error could not be built (heap exhausted): that failure is the
        // resolution's, as any allocation failure while resolving is.
        Err(e) => e,
    }
}

/// Count (and, under `CRATONVM_DBG=access`, trace) a racing resolution that
/// took another thread's published outcome: `what` names the arm
/// (`"refusal adopts the published class"` / `"success meets a recorded
/// failure"`). Cold; `--jdk-only` callers only.
#[cold]
#[inline(never)]
pub(crate) fn note_resolution_race(
    shared: &SharedVm,
    what: &'static str,
    referencing_class_id: ClassId,
    detail: &dyn std::fmt::Display,
) {
    let n = shared
        .classes
        .resolution_race_adoptions
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if cratonvm_types::flags().loader.dbg_access {
        let referencing = shared
            .classes
            .class_manager
            .read()
            .get_class(referencing_class_id)
            .map(|c| c.name.replace('/', "."))
            .unwrap_or_default();
        eprintln!("[ACCESS-DBG] RESOLUTION-RACE #{n} {what}: {detail} in {referencing}");
    }
}

/// The `CRATONVM_DBG=access` exit line of this module's two counters and the
/// initiating-record ones (`runtime::resolve::initiating_records`), once per
/// VM, zeros included:
///
/// `[ACCESS-DBG] LOADER census: mode=jdk-only loader-throws-propagated=0 resolution-race-adoptions=0 reentry-circularities=0 reentry-parallel-reasks=0 loader-nulls-propagated=0 initiating-findloaded=0 initiating-define-denied=0`
///
/// `reentry-parallel-reasks` is informational like `initiating-findloaded`
/// (HotSpot re-asks a parallel-capable loader too), but a non-zero count on a
/// real workload is unusual: read its `LOADER-REENTRY-PARALLEL` rows.
///
/// A real workload (the suite, Spring Boot, Tomcat) must read
/// `loader-throws-propagated=0`, `loader-nulls-propagated=0`, `reentry-circularities=0` and
/// `initiating-define-denied=0`, or each `LOADER-THROW PROPAGATE` /
/// `LOADER-REENTRY` / `INITIATING-RECORD DENY` row names a failure
/// HotSpot raises too (or a CratonVM loader-native miss, or a drive HotSpot
/// would not make, to fix before the row is trusted).
/// `initiating-findloaded` is informational (HotSpot answers those too).
/// Called beside the member-access census
/// (`cratonvm_vm::report_member_access_census_at_exit`).
pub(crate) fn report_loader_census_at_exit(shared: &SharedVm) {
    if !cratonvm_types::flags().loader.dbg_access
        || shared
            .classes
            .loader_census_reported
            .swap(true, Ordering::AcqRel)
    {
        return;
    }
    let mode = if shared.config.is_jdk_only() {
        "jdk-only"
    } else {
        "compatible"
    };
    eprintln!(
        "[ACCESS-DBG] LOADER census: mode={mode} loader-throws-propagated={} resolution-race-adoptions={} reentry-circularities={} reentry-parallel-reasks={} loader-nulls-propagated={} initiating-findloaded={} initiating-define-denied={}",
        shared.classes.loader_throws_propagated.load(Ordering::Relaxed),
        shared.classes.resolution_race_adoptions.load(Ordering::Relaxed),
        shared.classes.loader_reentry_circularities.load(Ordering::Relaxed),
        shared.classes.loader_reentry_reasks.load(Ordering::Relaxed),
        shared.classes.loader_nulls_propagated.load(Ordering::Relaxed),
        shared.classes.initiating_record_find_hits.load(Ordering::Relaxed),
        shared.classes.initiating_record_define_refusals.load(Ordering::Relaxed),
    );
}
