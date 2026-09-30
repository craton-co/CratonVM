// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMS §5.3 initiating-loader records, `--jdk-only` (interpreter round i1
//! wave 38, lane L5;
//! `interpreter-L5-an-initiating-loader-record-is-invisible-to-findloadedclass-and-define-FIXED-20261003`).
//!
//! A loader `L` that the VM asked for a name `N` and that answered with a
//! class some other loader defined is an *initiating loader* of that class:
//! `N` is in `L`'s namespace from then on. HotSpot keeps the record in `L`'s
//! dictionary (`SystemDictionary::load_instance_class` → `update_dictionary`;
//! `Class.forName(N, init, L)` makes it too), and two things read it:
//! `L.findLoadedClass(N)` returns the class, and `L` defining `N` itself is a
//! duplicate-definition `LinkageError` (§5.3.5).
//!
//! CratonVM's only record was the capped per-loader memo
//! `ClassRealm::initiating_resolution_cache`, which neither reader consulted
//! and which also holds answers HotSpot never records (a refused name's
//! global fallback). This table is the record: written where HotSpot writes
//! it — the class-resolution door's successful `loadClass` drive
//! (`constants.rs` `drive_defining_loader_load_named`, checked callers only)
//! and `Class.forName` with a loader (`NativeContext::note_initiating_load`)
//! — and read by the `findLoadedClass` natives
//! (`NativeContext::initiated_class_for_loader`) and the define backend
//! (`vm_exec.rs` `define_class_full`, [`define_refusal`]).
//!
//! Scope: user-defined loaders only; names outside the JDK-global namespaces
//! (`is_global_resolution_namespace`: CratonVM routes those globally, and a
//! user loader cannot define a `java/` class at all). `--compatible` records
//! nothing, so every reader answers `None` there.
//!
//! Positive control: `CRATONVM_DBG=access` prints `[ACCESS-DBG]
//! INITIATING-RECORD DENY define …` per refusal, and the exit line
//! `[ACCESS-DBG] LOADER census: …` counts `initiating-findloaded=` and
//! `initiating-define-denied=`. Probe
//! `tools/probes/interp/L5/L5W37InitiatingLoaderRecord.java`.

use crate::classloading::{ClassId, ClassLoaderId};
use crate::runtime::interpreter::constants::is_global_resolution_namespace;
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

/// Whether `name` is a name this table records: not an array (never in a
/// dictionary) and outside the JDK-global namespaces.
#[inline]
fn recordable(name: &str) -> bool {
    !name.starts_with('[') && !is_global_resolution_namespace(name)
}

/// Record that `loader` (user-defined), asked by the VM for `name` (internal
/// form), answered `class_id`. Nothing when the class is `loader`'s own (its
/// definition is its namespace already), outside `--jdk-only`, or for a name
/// this table does not record. The first answer stays, as a dictionary entry
/// does.
pub(crate) fn record_initiating_load(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
    class_id: ClassId,
) {
    if !shared.config.is_jdk_only() || !matches!(loader, ClassLoaderId::UserDefined(_)) {
        return;
    }
    if !recordable(name) {
        return;
    }
    if shared.classes.class_manager.read().get_loader_id(class_id) == Some(loader) {
        return;
    }
    shared
        .classes
        .initiating_records
        .write()
        .entry(loader)
        .or_default()
        .entry(cratonvm_types::intern_arc(name))
        .or_insert(class_id);
}

/// The class `loader` initiated under `name`, if the VM recorded one
/// ([`record_initiating_load`]). `None` outside `--jdk-only`. One read guard.
pub(crate) fn initiated_class(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    if !shared.config.is_jdk_only() {
        return None;
    }
    shared
        .classes
        .initiating_records
        .read()
        .get(&loader)
        .and_then(|m| m.get(name))
        .copied()
}

/// [`initiated_class`] for a `findLoadedClass` native that missed the
/// loader's own definitions: counted in
/// `ClassRealm::initiating_record_find_hits` when it answers.
pub(crate) fn initiated_class_for_find_loaded(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    let found = initiated_class(shared, loader, name)?;
    shared
        .classes
        .initiating_record_find_hits
        .fetch_add(1, Ordering::Relaxed);
    Some(found)
}

/// JVMS §5.3.5 at define: `loader` (user-defined) defining `name` after it
/// initiated `name` as another loader's class is refused. `Some(initiated)`
/// — the recorded class — when refused; the caller builds HotSpot's message
/// (`classloader_real::initiated_duplicate_definition_message`) and fails the
/// define. Counted in `ClassRealm::initiating_record_define_refusals` and
/// traced under `CRATONVM_DBG=access`. `None` outside `--jdk-only`, for a
/// built-in loader, and while the loader has no record (one read guard; an
/// unnamed define or an array name takes none).
pub(crate) fn define_refusal(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    if !shared.config.is_jdk_only() || !matches!(loader, ClassLoaderId::UserDefined(_)) {
        return None;
    }
    if name.is_empty() || !recordable(name) {
        return None;
    }
    let initiated = {
        let records = shared.classes.initiating_records.read();
        if records.is_empty() {
            return None;
        }
        records.get(&loader).and_then(|m| m.get(name)).copied()
    }?;
    let n = shared
        .classes
        .initiating_record_define_refusals
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!(
            "[ACCESS-DBG] INITIATING-RECORD DENY #{n} define {} by {loader}: the loader initiated it as {initiated:?}",
            name.replace('/', ".")
        );
    }
    Some(initiated)
}

/// Record that a class of `loader` (not the bootstrap loader) resolved to
/// `resolved`, when that is a JDK class -- defined by the bootstrap loader,
/// or by the platform loader for any loader but itself; an array records its
/// element class -- for `Instrumentation.getInitiatedClasses`
/// (`ClassRealm::jdk_names_initiated`; HotSpot files the class in the
/// resolving loader's dictionary). The caller has read
/// `ClassRealm::jdk_names_initiated_armed`. A class already recorded costs
/// one read guard. Interpreter round i1 wave 46, lane L5
/// (`i44-L5-getinitiatedclasses-lists-a-jdk-class-an-unresolved-constant-names`).
pub(crate) fn record_jdk_name_initiated(shared: &SharedVm, loader: ClassLoaderId, resolved: ClassId) {
    if loader == ClassLoaderId::Bootstrap {
        return;
    }
    let leaf = {
        let cm = shared.classes.class_manager.read();
        let Some(mut class) = cm.get_class(resolved) else {
            return;
        };
        while class.name.starts_with('[') {
            let Some(component) = class
                .array_info
                .as_ref()
                .and_then(|info| cm.get_class(info.component_class_id))
            else {
                return;
            };
            class = component;
        }
        // A primitive element (`[I`) is every loader's; a hidden class is in
        // no dictionary.
        if class.name.len() == 1 || class.hidden {
            return;
        }
        let jdk = match class.loader_id {
            ClassLoaderId::Bootstrap => true,
            ClassLoaderId::Extension => loader != ClassLoaderId::Extension,
            _ => false,
        };
        if !jdk {
            return;
        }
        class.id
    };
    if shared
        .classes
        .jdk_names_initiated
        .read()
        .get(&loader)
        .is_some_and(|set| set.contains(&leaf))
    {
        return;
    }
    shared
        .classes
        .jdk_names_initiated
        .write()
        .entry(loader)
        .or_default()
        .insert(leaf);
}

/// [`record_jdk_name_initiated`] for a `CONSTANT_Class` resolution by the
/// class `referencing_class_id` (the resolution door's success,
/// `constants.rs` `resolve_class_loader_aware`), after one relaxed load of
/// `ClassRealm::jdk_names_initiated_armed`.
#[inline]
pub(crate) fn note_class_resolution(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    resolved: ClassId,
) {
    if !shared
        .classes
        .jdk_names_initiated_armed
        .load(Ordering::Relaxed)
    {
        return;
    }
    let loader = shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing_class_id);
    if let Some(loader) = loader {
        record_jdk_name_initiated(shared, loader, resolved);
    }
}
