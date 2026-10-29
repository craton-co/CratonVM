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
    // The row-off route dropped the throw for the global fallback (wave 51,
    // lane L5: the alignment census).
    shared
        .classes
        .note_alignment_divergence(cratonvm_types::HotSpotAlignment::LoaderThrows);
    if cratonvm_types::flags().loader.dbg_access {
        let (loader_class, referencing) = {
            let cm = shared.classes.class_manager.read();
            let loader_class = cratonvm_native_builtins::classloader::defining_loader_for(
                &shared.loader_identity,
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
    shared
        .classes
        .note_alignment_divergence(cratonvm_types::HotSpotAlignment::LoaderThrows);
    if cratonvm_types::flags().loader.dbg_access {
        let (loader_class, referencing) = {
            let cm = shared.classes.class_manager.read();
            let loader_class = cratonvm_native_builtins::classloader::defining_loader_for(
                &shared.loader_identity,
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
/// `[ACCESS-DBG] LOADER census: mode=jdk-only loader-throws-propagated=0 resolution-race-adoptions=0 reentry-circularities=0 reentry-parallel-reasks=0 loader-nulls-propagated=0 initiating-findloaded=0 initiating-define-denied=0 initiating-memo-only=0 caller-drives-skipped=0 nested-jar-walk-skipped=0 nested-jar-walk-asked=0 nested-jar-bytes-served=0 nested-jar-bytes-body=0 jar-loader-bytes-served=0 jar-loader-bytes-body=0 nested-jar-rows-built=0 nested-jar-rows-rebuilt=0 nested-jar-rows-refused=0 nested-jar-refusals-retried=0 nested-jar-refusals-kept=0 nested-jar-class-path-lent=0 nested-jar-outer-handle-reused=0 jar-loader-class-path-lent=0 jar-loader-class-path-direct=0 jar-loader-class-path-direct-fallback=0 nested-jar-pooled-stat-skipped=0`
///
/// The `nested-jar-rows-*` / `nested-jar-refusals-*` counters (interpreter
/// round i2 wave 69, lane L5; `cratonvm_native_api::NestedJarRowEvent`) are
/// the nested-jar rows' own events, summed over the walk index and both
/// bytes indexes: a miss that built a row, a changed row rebuilt, a build
/// (or rebuild, or retry) that refused or a changed row turned into a
/// refusal, a stamped refusal retried, a refused row answered without a
/// build; `nested-jar-class-path-lent` counts rows built from the
/// application class path's own parse (`CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`),
/// `nested-jar-outer-handle-reused` directory-row reads through a kept
/// handle (`CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`), and
/// `jar-loader-class-path-lent` jar-loader bytes rows built from the class
/// path's own archive (`CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`; wave 70);
/// `jar-loader-class-path-direct` jar-loader entries served straight from
/// the class path's own entry with no row, and
/// `jar-loader-class-path-direct-fallback` asks the class path did not hold
/// unchanged, which went to the rows (`CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`;
/// interpreter round i3 wave 71); `nested-jar-pooled-stat-skipped` nested
/// bytes-row hits that left the file check to the pooled read
/// (`CRATONVM_NESTED_JAR_POOLED_STAT`; wave 71).
/// Informational.
///
/// `nested-jar-walk-skipped` / `nested-jar-walk-asked` are informational: the
/// `URLClassPath$Loader` walk intrinsic's two answers
/// (`CRATONVM_NESTED_JAR_WALK_INDEX`; interpreter round i2 wave 54, lane L5).
/// `nested-jar-bytes-served` / `nested-jar-bytes-body` likewise: the
/// `Resource.getBytes` intrinsic's two answers for the generic loader's
/// `jar:nested:` resources (`CRATONVM_NESTED_JAR_BYTES_INDEX`; wave 63, L5).
/// `jar-loader-bytes-served` / `jar-loader-bytes-body` likewise for
/// `URLClassPath$JarLoader` resources (`CRATONVM_JAR_LOADER_BYTES_INDEX`;
/// interpreter round i2 wave 64, L5).
///
/// `caller-drives-skipped` is informational too: the base `loadClass`
/// native's flat-store misses that no longer drive the calling frame's loader
/// (`ClassRealm::load_class_caller_drives_skipped`; interpreter round i2 wave
/// 53, lane L5).
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
/// Bound on `ClassRealm::caller_drive_census` rows.
const CALLER_DRIVE_CENSUS_CAP: usize = 2048;

/// Tally one caller-loader drive of a native's by-name lookup at `site`
/// (`ClassRealm::caller_drive_census`; `CRATONVM_DBG=access`, interpreter round
/// i6 wave 116, lane L5). The caller has checked the flag.
pub(crate) fn note_caller_drive(shared: &SharedVm, site: String, answered: bool) {
    let mut rows = shared.classes.caller_drive_census.lock();
    let key = if rows.len() < CALLER_DRIVE_CENSUS_CAP || rows.contains_key(&site) {
        site
    } else {
        "<rows over cap>".to_string()
    };
    let row = rows.entry(key).or_insert([0, 0]);
    row[0] += 1;
    if answered {
        row[1] += 1;
    }
}

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
        "[ACCESS-DBG] LOADER census: mode={mode} loader-throws-propagated={} resolution-race-adoptions={} reentry-circularities={} reentry-parallel-reasks={} loader-nulls-propagated={} initiating-findloaded={} initiating-define-denied={} initiating-memo-only={} initiating-memo-only-global={} global-name-refusal-memo={}/{} caller-drives-skipped={} nested-jar-walk-skipped={} nested-jar-walk-asked={} nested-jar-bytes-served={} nested-jar-bytes-body={} jar-loader-bytes-served={} jar-loader-bytes-body={} nested-jar-rows-built={} nested-jar-rows-rebuilt={} nested-jar-rows-refused={} nested-jar-refusals-retried={} nested-jar-refusals-kept={} nested-jar-class-path-lent={} nested-jar-outer-handle-reused={} jar-loader-class-path-lent={} jar-loader-class-path-direct={} jar-loader-class-path-direct-fallback={} nested-jar-pooled-stat-skipped={}",
        shared.classes.loader_throws_propagated.load(Ordering::Relaxed),
        shared.classes.resolution_race_adoptions.load(Ordering::Relaxed),
        shared.classes.loader_reentry_circularities.load(Ordering::Relaxed),
        shared.classes.loader_reentry_reasks.load(Ordering::Relaxed),
        shared.classes.loader_nulls_propagated.load(Ordering::Relaxed),
        shared.classes.initiating_record_find_hits.load(Ordering::Relaxed),
        shared.classes.initiating_record_define_refusals.load(Ordering::Relaxed),
        shared.classes.initiating_memo_only_answers.load(Ordering::Relaxed),
        shared.classes.global_name_record_census[0].load(Ordering::Relaxed),
        shared.classes.global_name_record_census[1].load(Ordering::Relaxed),
        shared.classes.global_name_record_census[2].load(Ordering::Relaxed),
        shared.classes.load_class_caller_drives_skipped.load(Ordering::Relaxed),
        shared.classes.nested_jar_walk_skipped.load(Ordering::Relaxed),
        shared.classes.nested_jar_walk_asked.load(Ordering::Relaxed),
        shared.classes.nested_jar_bytes_served.load(Ordering::Relaxed),
        shared.classes.nested_jar_bytes_body.load(Ordering::Relaxed),
        shared.classes.jar_loader_bytes_served.load(Ordering::Relaxed),
        shared.classes.jar_loader_bytes_body.load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[0].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[1].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[2].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[3].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[4].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[5].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[6].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[7].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[8].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[9].load(Ordering::Relaxed),
        shared.classes.nested_jar_row_events[10].load(Ordering::Relaxed),
    );
    // Caller-loader drives by site (interpreter round i6 wave 116, lane L5;
    // `i53-L5-proposal-ctx-load-class-takes-an-explicit-initiating-loader`),
    // most driven first.
    {
        let rows = shared.classes.caller_drive_census.lock();
        let mut sorted: Vec<(&String, &[u64; 2])> = rows.iter().collect();
        sorted.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(a.0.cmp(b.0)));
        let total: u64 = sorted.iter().map(|(_, r)| r[0]).sum();
        eprintln!(
            "[ACCESS-DBG] CALLER-DRIVE census: mode={mode} drives={total} sites={}",
            sorted.len()
        );
        for (site, row) in sorted {
            eprintln!(
                "[ACCESS-DBG] CALLER-DRIVE {site} drives={} answered={}",
                row[0], row[1]
            );
        }
    }
    // The user-loader requests by kind and door (interpreter round i6 wave
    // 122, lane L5; `i39-L5-proposal-one-loader-request-door-with-an-explicit-kind`):
    // the `lookup` rows are CratonVM's own drives, the retirement list.
    {
        let rows = shared.classes.loader_request_census.lock();
        let mut sorted: Vec<(&String, &[u64; 2])> = rows.iter().collect();
        sorted.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(a.0.cmp(b.0)));
        let lookup: u64 = sorted
            .iter()
            .filter(|(k, _)| k.starts_with("lookup "))
            .map(|(_, r)| r[0])
            .sum();
        eprintln!(
            "[ACCESS-DBG] LOADER-REQUEST census: mode={mode} lookup-requests={lookup} doors={}",
            sorted.len()
        );
        for (door, row) in sorted {
            eprintln!(
                "[ACCESS-DBG] LOADER-REQUEST {door} requests={} dictionary-answered={}",
                row[0], row[1]
            );
        }
    }
    // How often each HotSpot-alignment row answered other than its row-off
    // route would have (`cratonvm_types::AlignmentCensus`; interpreter round
    // i1 wave 51, lane L5). `<row>=?`: no site counts that row; `/off`: the
    // row is not in force in this VM.
    eprintln!(
        "[ACCESS-DBG] ALIGNMENT census: {}",
        shared.classes.alignment_census.census_line(
            shared.config.compatibility_mode,
            shared.classes.hotspot_aligned
        )
    );
    // zt t22/df10 (define page §9c): the built-in `findLoadedClass` rule
    // (`ClassRealm::builtin_find_loaded_initiated_only`). `withheld` counts the
    // ancestor's classes it answered null for (each one a `BuiltinClassLoader`
    // delegation in Java that the ancestor answer skipped), `by-row` /
    // `by-supertype` the ones it served, `rows-filed` the dictionary rows the
    // resolution door filed for the application and platform loaders.
    // Informational: the boot-cost census of the rule.
    eprintln!(
        "[ACCESS-DBG] BUILTIN-INITIATED census: on={} withheld={} by-row={} by-supertype={} rows-filed={}",
        shared.classes.builtin_find_loaded_initiated_only,
        shared.classes.builtin_find_loaded_withheld.load(Ordering::Relaxed),
        shared.classes.builtin_find_loaded_served_by_row.load(Ordering::Relaxed),
        shared
            .classes
            .builtin_find_loaded_served_by_supertype
            .load(Ordering::Relaxed),
        shared.classes.builtin_initiated_rows_filed.load(Ordering::Relaxed),
    );
}
