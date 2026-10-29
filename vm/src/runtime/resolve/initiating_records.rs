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
//! Scope: user-defined loaders only; application names and `java/` names.
//! Java-name resolution is observable even though user definition is forbidden;
//! its dictionary entries outlive eviction of the capped memo (i47). Since
//! interpreter round i6 wave 133 (lane L5), the other JDK-global names too
//! (`javax/`, `jdk/`, `sun/`, `com/sun/`) under
//! `CRATONVM_GLOBAL_NAME_INITIATING_RECORD` (default on; `=0` off), so
//! `findLoadedClass` answers a class the loader answered for such a name; the
//! define refusal still reads the first two kinds only.
//! `--compatible` records
//! nothing, so every reader answers `None` there -- unless
//! `CRATONVM_COMPAT_INITIATING_RECORDS` turns the records on in that mode too
//! (`ClassRealm::initiating_records_on`; interpreter round i1 wave 48, lane
//! L5), which every gate below reads.
//!
//! Positive control: `CRATONVM_DBG=access` prints `[ACCESS-DBG]
//! INITIATING-RECORD DENY define …` per refusal, and the exit line
//! `[ACCESS-DBG] LOADER census: …` counts `initiating-findloaded=` and
//! `initiating-define-denied=`. Probe
//! `tools/probes/interp/L5/L5W37InitiatingLoaderRecord.java`.

use crate::classloading::{ClassId, ClassLoaderId};
use crate::runtime::interpreter::constants::is_global_resolution_namespace;
use crate::vm::SharedVm;
use rustc_hash::FxHashSet;
use std::sync::atomic::Ordering;

/// Whether `name` is a name this table records: not an array (never in a
/// dictionary), either `java/` or outside the other global namespaces.
#[inline]
fn recordable(name: &str) -> bool {
    !name.starts_with('[') && (name.starts_with("java/") || !is_global_resolution_namespace(name))
}

/// A JDK-global name other than `java/` (`javax/`, `jdk/`, `sun/`,
/// `com/sun/`, ...): the names [`recordable`] leaves out, which only
/// [`record_global_name_refusal`] (under `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD`)
/// and, since wave 133, [`record_initiating_load`] (under
/// `CRATONVM_GLOBAL_NAME_INITIATING_RECORD`, default on) write to the table.
/// Not an array.
#[inline]
fn jdk_global_name(name: &str) -> bool {
    !name.starts_with('[') && !name.starts_with("java/") && is_global_resolution_namespace(name)
}

/// `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD` (`VmConfig::global_name_refusal_record`,
/// default off; interpreter round i6 wave 125, lane L5,
/// `i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`):
/// `constants.rs` `drive_loader_for_global_name` asked the user loader
/// `loader` for the JDK-global name `name` and its checked door answered
/// `BaseMiss` -- the loader's chain reaches the application loader, the flat
/// store stood in for its parent, and the global route answers the built-in
/// class `class_id`. HotSpot's parent chain answers that class and the loader
/// becomes an initiating loader of it (a dictionary row), so it is filed in
/// the loader's initiating RECORD, which `findLoadedClass` reads, instead of
/// the capped memo. [`loader_initiated_answer`] reads it behind a memo miss
/// under the same switch. A new row bumps the resolution epoch, as a memo
/// write does. Nothing outside the records' modes, for a built-in loader, or
/// for a name that is not [`jdk_global_name`]. The first answer stays.
pub(crate) fn record_global_name_refusal(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
    class_id: ClassId,
) {
    if !shared.classes.initiating_records_on
        || !matches!(loader, ClassLoaderId::UserDefined(_))
        || !jdk_global_name(name)
    {
        return;
    }
    let inserted = {
        let mut records = shared.classes.initiating_records.write();
        let entries = records.entry(loader).or_default();
        let key = cratonvm_types::intern_arc(name);
        if entries.contains_key(key.as_ref()) {
            false
        } else {
            entries.insert(key, class_id);
            true
        }
    };
    // A user loader's row: the slot's global counter stays (t1 wave 10,
    // lane L71; `constants::bump_resolution_epoch_for_loader_row_in`).
    if inserted {
        crate::runtime::interpreter::bump_resolution_epoch_for_loader_row_in(shared);
    }
}

/// `CRATONVM_DBG=access` census of `drive_loader_for_global_name`'s refusals
/// by the checked door's reason (`ClassRealm::global_name_record_census`
/// `[1]` base-miss, `[2]` degrade; the exit line's
/// `global-name-refusal-memo=<base-miss>/<degrade>`). Interpreter round i6
/// wave 125, lane L5. Cold: a refusal.
#[cold]
#[inline(never)]
pub(crate) fn note_global_name_refusal(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
    base_miss: bool,
    filed: &str,
) {
    let slot = if base_miss { 1 } else { 2 };
    let n = shared.classes.global_name_record_census[slot]
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    eprintln!(
        "[ACCESS-DBG] GLOBAL-NAME REFUSAL #{n} {} for {loader}: {} -> {filed}",
        name.replace('/', "."),
        if base_miss { "base-miss" } else { "degrade" }
    );
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
    if !shared.classes.initiating_records_on || !matches!(loader, ClassLoaderId::UserDefined(_)) {
        return;
    }
    // `CRATONVM_GLOBAL_NAME_INITIATING_RECORD` (default ON, `=0` off;
    // interpreter round i6 wave 133, lane L5): a JDK-global name other than
    // `java/` is recorded too. HotSpot's `update_dictionary` files every name
    // a loader answered for the VM; without it a `javax/` answer was kept in
    // the capped memo only, which `findLoadedClass` does not read
    // (`L5W125GlobalNameRefusalRecord`: `after-first find=null`).
    let global = !recordable(name)
        && shared.config.global_name_initiating_record
        && jdk_global_name(name);
    if !recordable(name) && !global {
        return;
    }
    // A row already written costs one read guard (the transparent-loader
    // writer in `constants.rs` runs once per site-cache miss).
    if shared
        .classes
        .initiating_records
        .read()
        .get(&loader)
        .is_some_and(|entries| entries.contains_key(name))
    {
        return;
    }
    if shared.classes.class_manager.read().get_loader_id(class_id) == Some(loader) {
        return;
    }
    let inserted = {
        let mut records = shared.classes.initiating_records.write();
        let entries = records.entry(loader).or_default();
        let key = cratonvm_types::intern_arc(name);
        if entries.contains_key(key.as_ref()) {
            false
        } else {
            entries.insert(key, class_id);
            true
        }
    };
    // When the record is what the loader-namespaced lookups read
    // ([`loader_initiated_answer`]), a new row changes what a loader answers
    // without touching `loaded_classes`, as a memo write does
    // (`constants.rs` `cache_loader_initiated`): the resolved-field site
    // caches must see it. Outside the switch the memo write beside every
    // drive's record bumps it already.
    //
    // A `java/` row is read by `loader_initiated_answer` in EVERY mode with
    // records on (it asks `initiated_class` before the memo), so it changes
    // the loader's answer with neither switch: `--compatible` with
    // `CRATONVM_COMPAT_INITIATING_RECORDS` and no `RecordBehindMemo` row
    // wrote such a row (`Class.forName(javaName, init, loader)`, which writes
    // no memo) without moving the epoch, and the static door's owner checks
    // (`dispatch_static::cached_static_owner_stale`'s memo, the invoke
    // cache's owner stamp) key on that epoch (interpreter round i4 wave 84,
    // lane L7). One bump per new row; rows are written once.
    // `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` (wave 123, lane L5): the record is
    // read behind a memo miss there too.
    // A JDK-global row (wave 133, above) is read behind a memo miss under
    // `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD` (`loader_initiated_answer`), so it
    // moves the epoch there as well.
    if inserted
        && (shared.classes.record_replaces_memo
            || shared.classes.record_behind_memo
            || shared.config.loader_dictionary_one_door
            || name.starts_with("java/")
            || (global && shared.config.global_name_refusal_record))
    {
        // A user loader's row (t1 wave 10, lane L71): read by loader-namespaced
        // resolutions only, so the slot's global counter stays.
        crate::runtime::interpreter::bump_resolution_epoch_for_loader_row_in(shared);
    }
}

/// What `loader` (user-defined) answers for `name` from what it was already
/// asked, without a `loadClass` call: the loader-namespaced lookups' shared
/// reader (`constants.rs` `lookup_loader_initiated`,
/// `class_resolved_without_loading`, the JDK-global-name record,
/// `field_access.rs` `owner_in_referencing_namespace`).
///
/// The capped memo (`ClassRealm::initiating_resolution_cache`) -- except
/// under `--jdk-only` with `CRATONVM_RECORD_REPLACES_MEMO`
/// (`ClassRealm::record_replaces_memo`), where a name this table records
/// ([`recordable`]) is answered from the initiating RECORD alone: the memo
/// also holds answers HotSpot never records (a refused name's global
/// fallback, `invoke.rs` `resolve_owner_unknown_to_caller`), and an evicted
/// memo row re-drove the loader. A JDK-global name keeps the memo (the record
/// does not hold such names). Interpreter round i1 wave 49, lane L5
/// (`i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`).
///
/// Census (`CRATONVM_DBG=access` only, cold): a memo row with no record
/// behind it -- an answer the switch stops serving, or, with the switch on,
/// stopped serving -- is counted in `ClassRealm::initiating_memo_only_answers`
/// and printed as `initiating-memo-only=` in the exit `LOADER census` line.
#[inline]
pub(crate) fn loader_initiated_answer(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    if name.starts_with("java/") {
        if let Some(id) = initiated_class(shared, loader, name) {
            return Some(id);
        }
    }
    let replaced = shared.classes.record_replaces_memo && recordable(name);
    if replaced && !cratonvm_types::flags().loader.dbg_access {
        return initiated_class(shared, loader, name);
    }
    let memo = shared
        .classes
        .initiating_resolution_cache
        .read()
        .get(&loader)
        .and_then(|m| m.get(name))
        .copied();
    if memo.is_none() && shared.classes.record_behind_memo && recordable(name) {
        return record_behind_memo(shared, loader, name);
    }
    // `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` (default off; interpreter round i6
    // wave 123, lane L5, i46-L5 / i120-L7): a memo miss falls to the loader's
    // dictionary's initiating half, so a name the loader initiated without a
    // memo row -- `Class.forName(name, init, loader)`, which writes the record
    // only, or a row the 4096-row cap evicted -- is not asked of the loader
    // again (HotSpot's `resolve_or_fail` finds the dictionary entry). Records
    // are `--jdk-only` (or `CRATONVM_COMPAT_INITIATING_RECORDS`): elsewhere
    // `initiated_class` is `None` and nothing changes. Probe
    // `L5W123ForNameRecordAnswersResolution`.
    if memo.is_none() && shared.config.loader_dictionary_one_door && recordable(name) {
        return initiated_class(shared, loader, name);
    }
    // `CRATONVM_GLOBAL_NAME_REFUSAL_RECORD` (default off; wave 125, lane L5):
    // a JDK-global name's refusal is filed in the record, not the memo
    // ([`record_global_name_refusal`]), so a memo miss reads it there.
    if memo.is_none() && shared.config.global_name_refusal_record && jdk_global_name(name) {
        if let Some(id) = initiated_class(shared, loader, name) {
            return Some(id);
        }
    }
    if !cratonvm_types::flags().loader.dbg_access {
        return memo;
    }
    memo_census(shared, loader, name, memo, replaced)
}

/// [`loader_initiated_answer`] for a name the memo has no row for, under
/// `--compatible` with the `RecordBehindMemo` row
/// (`ClassRealm::record_behind_memo`; interpreter round i1 wave 52, lane
/// L5): the initiating record's answer, if any. Every such answer is one the
/// row-off route did not have (the memo evicted the row past its 4096-row
/// cap, or never held it: a `Class.forName` record), so it re-asked the
/// loader: counted in the alignment census. Cold: a memo miss.
#[cold]
#[inline(never)]
fn record_behind_memo(shared: &SharedVm, loader: ClassLoaderId, name: &str) -> Option<ClassId> {
    let record = initiated_class(shared, loader, name)?;
    shared
        .classes
        .note_alignment_divergence(cratonvm_types::HotSpotAlignment::RecordBehindMemo);
    Some(record)
}

/// [`loader_initiated_answer`] under `CRATONVM_DBG=access`: counts and
/// traces a memo row with no record behind it, then answers as the switch
/// says.
#[cold]
#[inline(never)]
fn memo_census(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
    memo: Option<ClassId>,
    replaced: bool,
) -> Option<ClassId> {
    let record = if recordable(name) {
        initiated_class(shared, loader, name)
    } else {
        None
    };
    if let Some(memo_id) = memo {
        if recordable(name) && shared.classes.initiating_records_on && record.is_none() {
            let n = shared
                .classes
                .initiating_memo_only_answers
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            eprintln!(
                "[ACCESS-DBG] INITIATING-MEMO-ONLY #{n} {} for {loader}: memo {memo_id:?}, no record ({})",
                name.replace('/', "."),
                if replaced { "not served" } else { "served" }
            );
        }
        // The JDK-global rows the count above cannot see (it counts
        // `recordable` names only): `initiating-memo-only-global=` (wave 125,
        // lane L5). Their only VM writer is `drive_loader_for_global_name`'s
        // refusal memo.
        if jdk_global_name(name)
            && shared.classes.initiating_records_on
            && initiated_class(shared, loader, name).is_none()
        {
            let n = shared.classes.global_name_record_census[0]
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            eprintln!(
                "[ACCESS-DBG] INITIATING-MEMO-ONLY-GLOBAL #{n} {} for {loader}: memo {memo_id:?}, no record (served)",
                name.replace('/', ".")
            );
        }
    }
    if replaced {
        record
    } else {
        memo
    }
}

/// The class `loader` initiated under `name`, if the VM recorded one
/// ([`record_initiating_load`]). `None` outside `--jdk-only`. One read guard.
pub(crate) fn initiated_class(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    if !shared.classes.initiating_records_on {
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

/// The ONE door to a user loader's dictionary by name (interpreter round i6
/// wave 121, lane L5; `i46-L5-proposal-one-resolved-class-entry-table-every-door-writes`
/// stage 2 with `i39-L5-proposal-one-loader-request-door-with-an-explicit-kind`):
/// what HotSpot's `SystemDictionary` holds for `loader` under `name` -- the
/// class the loader DEFINED under it, else the class it INITIATED under it
/// (its initiating record, written by every successful drive). The readers
/// that asked the two records one after the other each made the same two
/// reads with small differences (a JDK-global name's cold full-store scan in
/// one, not in another); they ask this instead under
/// `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` (`VmConfig::loader_dictionary_one_door`,
/// default off), and `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` asks it whenever
/// it is on. A JDK-global name ([`is_global_resolution_namespace`]) is looked up
/// under the loader's exact key (one hash probe), never by the defined-class
/// scan, as `lookup_loader_defined_exact` does. Two statements: the class
/// manager's read guard is dropped before the record's own lock is taken.
/// `None` for a built-in loader (its dictionary is the flat store's). Since
/// wave 133 the initiating half holds the JDK-global names the loader answered
/// too (`CRATONVM_GLOBAL_NAME_INITIATING_RECORD`, [`record_initiating_load`]).
///
/// The per-ENTRY half of the same table, `(referencing class, cp index) ->
/// class`, is `ResolutionCache::class_entries` (lane L7, i1-L2 stage 1;
/// widened by `CRATONVM_CLASS_ENTRY_RECORD_ALL`): an entry that resolved once
/// answers there before any of these doors is reached.
pub(crate) fn loader_dictionary(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    if !matches!(loader, ClassLoaderId::UserDefined(_)) {
        return None;
    }
    let defined = {
        let cm = shared.classes.class_manager.read();
        if is_global_resolution_namespace(name) {
            cm.loaded_class_under_exact_key(name, loader)
        } else {
            cm.class_defined_by_loader_exact(name, loader)
        }
    };
    defined.or_else(|| initiated_class(shared, loader, name))
}

/// The kind of a VM-initiated request of a user loader
/// (`i39-L5-proposal-one-loader-request-door-with-an-explicit-kind`):
/// `Resolution` (JVMS §5.4.3: HotSpot's outcome in every column), `Lookup`
/// (CratonVM's own questions -- an array pre-pass, a stub-avoidance probe, a
/// receiver fallback, a lambda dispatch override, the miss rescue -- which
/// never record and never propagate, and are counted so each can be
/// retired). Interpreter round i6 wave 122, lane L5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoaderRequestKind {
    Resolution,
    Lookup,
}

impl LoaderRequestKind {
    fn label(self) -> &'static str {
        match self {
            LoaderRequestKind::Resolution => "resolution",
            LoaderRequestKind::Lookup => "lookup",
        }
    }
}

/// The ONE prologue of every VM-initiated user-loader request door
/// (`i39-L5`, interpreter round i6 wave 122, lane L5): the request of `kind`
/// made by `door` for `name` on behalf of `referencing_class_id`.
///
/// * `CRATONVM_LOADER_REQUEST_DOOR` (`VmConfig::loader_request_door`, default
///   off): a `Lookup` request is answered from the loader's dictionary
///   ([`loader_dictionary`]) when it holds the name, without a `loadClass`
///   call -- the rule `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` applies to
///   the checked doors, given to the CratonVM-only drives too (HotSpot asks a
///   loader once per name, from any door). `Resolution` requests keep their
///   own path (the checked doors and the dictionary switch).
/// * `CRATONVM_DBG=access`: every request is tallied by `(kind, door)` with
///   how many the dictionary answered (`ClassRealm::loader_request_census`,
///   printed as `LOADER-REQUEST` rows of the exit loader census): the
///   `Lookup` rows are the retirement list the page asks for.
///
/// `Some(id)`: answered here (the caller returns it); `None`: drive as before.
pub(crate) fn loader_request_prologue(
    shared: &SharedVm,
    kind: LoaderRequestKind,
    door: &'static str,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    let census = cratonvm_types::flags().loader.dbg_access;
    let answer_from_dictionary = shared.config.loader_request_door
        && kind == LoaderRequestKind::Lookup;
    if !census && !answer_from_dictionary {
        return None;
    }
    let answer = if answer_from_dictionary {
        let loader = shared
            .classes
            .class_manager
            .read()
            .get_loader_id(referencing_class_id);
        loader.and_then(|loader| loader_dictionary(shared, loader, name))
    } else {
        None
    };
    if census {
        let key = format!("{} {door}", kind.label());
        let mut rows = shared.classes.loader_request_census.lock();
        let row = rows.entry(key).or_insert([0, 0]);
        row[0] += 1;
        if answer.is_some() {
            row[1] += 1;
        }
    }
    answer
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
    // Without records `findLoadedClass` answered null (wave 51, lane L5: the
    // alignment census).
    shared
        .classes
        .note_alignment_divergence(cratonvm_types::HotSpotAlignment::InitiatingRecords);
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
///
/// zt df4 §9c (`CRATONVM_DEFINE_BUILTIN_INITIATED_REFUSAL`, default ON, `=0`
/// kill): the application and platform loaders too, from their dictionary
/// rows (`ClassRealm::jdk_names_initiated`): a define of a name whose
/// ANCESTOR's class the loader initiated (the platform loader's class of
/// that name for the application loader, a boot class for either) is
/// HotSpot's duplicate-definition `LinkageError`
/// ([`builtin_define_refusal`]).
pub(crate) fn define_refusal(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
) -> Option<ClassId> {
    if matches!(loader, ClassLoaderId::Application | ClassLoaderId::Extension) {
        return builtin_define_refusal(shared, loader, name);
    }
    if !shared.classes.initiating_records_on || !matches!(loader, ClassLoaderId::UserDefined(_)) {
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
    shared
        .classes
        .note_alignment_divergence(cratonvm_types::HotSpotAlignment::InitiatingRecords);
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!(
            "[ACCESS-DBG] INITIATING-RECORD DENY #{n} define {} by {loader}: the loader initiated it as {initiated:?}",
            name.replace('/', ".")
        );
    }
    Some(initiated)
}

/// zt df4 §9c: [`define_refusal`] for the built-in `loader` (application or
/// platform): the class of `name` an ancestor defined (the platform loader's
/// for the application loader, then the bootstrap loader's) when the loader's
/// dictionary row holds it. One class-manager read and one row read; `None`
/// without the rows (`ClassRealm::builtin_find_loaded_initiated_only` off),
/// under the kill, for a hidden or array name.
fn builtin_define_refusal(shared: &SharedVm, loader: ClassLoaderId, name: &str) -> Option<ClassId> {
    if !shared.classes.builtin_find_loaded_initiated_only
        || name.is_empty()
        || name.starts_with('[')
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_DEFINE_BUILTIN_INITIATED_REFUSAL",
        )
    {
        return None;
    }
    let ancestors: &[ClassLoaderId] = if loader == ClassLoaderId::Application {
        &[ClassLoaderId::Extension, ClassLoaderId::Bootstrap]
    } else {
        &[ClassLoaderId::Bootstrap]
    };
    let candidates: Vec<ClassId> = {
        let cm = shared.classes.class_manager.read_recursive();
        ancestors
            .iter()
            .filter_map(|&anc| cm.class_defined_by_loader_exact(name, anc))
            .collect()
    };
    let initiated = {
        let rows = shared.classes.jdk_names_initiated.read();
        let row = rows.get(&loader)?;
        candidates.into_iter().find(|cid| row.contains(cid))
    }?;
    shared
        .classes
        .initiating_record_define_refusals
        .fetch_add(1, Ordering::Relaxed);
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!(
            "[ACCESS-DBG] BUILTIN-INITIATED DENY define {} by {loader}: the loader initiated it as {initiated:?}",
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
/// (`interpreter-L5-getinitiatedclasses-lists-a-jdk-class-an-unresolved-constant-names-FIXED-20261011`).
pub(crate) fn record_jdk_name_initiated(
    shared: &SharedVm,
    loader: ClassLoaderId,
    resolved: ClassId,
) {
    if loader == ClassLoaderId::Bootstrap {
        return;
    }
    let leaf = {
        // Recursive: the reflective and launcher writers below may be reached
        // from a native that holds a class-manager read guard.
        let cm = shared.classes.class_manager.read_recursive();
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
/// `ClassRealm::jdk_names_initiated_armed`. With the resolved class-entry
/// table on (`ClassRealm::resolved_entry_table`), the door writes that
/// table instead ([`note_resolved_class_entry`]).
#[inline]
pub(crate) fn note_class_resolution(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    resolved: ClassId,
) {
    if shared.classes.resolved_entry_table {
        note_resolved_class_entry(shared, referencing_class_id, resolved);
        return;
    }
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

// ---------------------------------------------------------------------------
// The resolved class-entry table (interpreter round i1 wave 47, lane L5;
// `i46-L5-proposal-one-resolved-class-entry-table-every-door-writes`, first
// stage) and the initiating records of the non-constant-pool resolutions.
// ---------------------------------------------------------------------------

/// The resolved class-entry table's cross-loader row: the class
/// `referencing` resolved one of its class entries to `resolved`. HotSpot
/// files the class in `referencing`'s loader's dictionary (JVMS §5.3: that
/// loader is an initiating loader of it), which
/// `Instrumentation.getInitiatedClasses` lists.
///
/// Written on a door's miss only -- the `CONSTANT_Class` door's success
/// ([`note_class_resolution`]) and a method reference's first resolution
/// (the method metadata core, [`note_member_owner_resolved`]) -- never on a
/// site-cache hit. A pair already written costs one read guard, and every
/// pair is written once, the loader's own classes too (the reader drops
/// those: its definition is its namespace already), so no answer is
/// classified twice; a class of the bootstrap loader is written once as a
/// `None` row and records nothing (its loader lists no initiated class). An
/// array records its element class too. Nothing while the switch
/// (`ClassRealm::resolved_entry_table`) is off.
pub(crate) fn note_resolved_class_entry(
    shared: &SharedVm,
    referencing: ClassId,
    resolved: ClassId,
) {
    if !shared.classes.resolved_entry_table || referencing == resolved {
        return;
    }
    match shared
        .classes
        .resolved_class_entries
        .read()
        .get(&referencing)
    {
        Some(None) => return,
        Some(Some(set)) if set.contains(&resolved) => return,
        _ => {}
    }
    note_resolved_class_entry_miss(shared, referencing, resolved);
}

/// [`note_resolved_class_entry`]'s write, out of line.
#[cold]
#[inline(never)]
fn note_resolved_class_entry_miss(shared: &SharedVm, referencing: ClassId, resolved: ClassId) {
    // zt t22/df10: the built-in loader's dictionary row this resolution makes
    // (filed below, after the entry table's guard is dropped).
    let mut builtin_row: Option<(ClassLoaderId, ClassId)> = None;
    let row = {
        let cm = shared.classes.class_manager.read_recursive();
        let Some(tag) = cm.get_loader_id(referencing) else {
            return;
        };
        // zt df4 §9c: a boot-namespace class of a platform or tool module of
        // the runtime image is its Java-visible loader's (HotSpot's platform
        // or application loader defines it), so its resolutions are that
        // loader's rows.
        let loader = match cm.get_class(referencing) {
            Some(class) if tag == ClassLoaderId::Bootstrap => {
                java_visible_builtin_loader(shared, &cm, class)
            }
            _ => tag,
        };
        if loader == ClassLoaderId::Bootstrap {
            None
        } else {
            let Some(mut class) = cm.get_class(resolved) else {
                return;
            };
            while class.name.starts_with('[') {
                let Some(component) = class
                    .array_info
                    .as_ref()
                    .and_then(|info| cm.get_class(info.component_class_id))
                else {
                    break;
                };
                class = component;
            }
            if shared.classes.builtin_find_loaded_initiated_only
                && builtin_dictionary_row(loader, class, java_visible_builtin_loader(shared, &cm, class))
            {
                builtin_row = Some((loader, class.id));
            }
            Some(class.id)
        }
    };
    {
        let mut entries = shared.classes.resolved_class_entries.write();
        let slot = entries
            .entry(referencing)
            .or_insert_with(|| row.map(|_| FxHashSet::default()));
        if let (Some(set), Some(leaf)) = (slot.as_mut(), row) {
            set.insert(resolved);
            set.insert(leaf);
        }
    }
    if let Some((loader, leaf)) = builtin_row {
        file_builtin_initiated_row(shared, loader, leaf);
    }
}

/// zt df4 §9c: the loader that defines `class` as Java sees it
/// (`Class.getClassLoader`): its tag, except a boot-namespace class of a
/// platform or tool module of the runtime image, which HotSpot's platform or
/// application loader defines (`ModuleRegistry::image_module_builtin_loader`;
/// `ClassRealm::builtin_image_class_rows`).
#[inline]
fn java_visible_builtin_loader(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
) -> ClassLoaderId {
    if class.loader_id != ClassLoaderId::Bootstrap || !shared.classes.builtin_image_class_rows {
        return class.loader_id;
    }
    class
        .module_name
        .as_deref()
        .and_then(|m| cm.module_registry.image_module_builtin_loader(m))
        .unwrap_or(ClassLoaderId::Bootstrap)
}

/// zt t22/df10 (define page §9c): does a class of the built-in `loader`
/// (application or platform) resolving `class` (an array's element) make a
/// row in that loader's dictionary that its `findLoadedClass` must answer?
/// `visible` is `class`'s Java-visible defining loader
/// ([`java_visible_builtin_loader`]; zt df4: a `java.sql` class is the
/// platform loader's own, not an ancestor's).
/// HotSpot's `SystemDictionary::load_instance_class` files the class the
/// loader answered in the loader's dictionary when the loader is not its
/// defining loader; for a built-in loader that is a class an ANCESTOR
/// defined: the bootstrap loader's, or the platform loader's for the
/// application loader (the rule [`record_jdk_name_initiated`] keeps). Its own
/// classes are answered as its own already; a hidden class, a primitive
/// element and an array are in no dictionary.
#[inline]
fn builtin_dictionary_row(
    loader: ClassLoaderId,
    class: &crate::classloading::Class,
    visible: ClassLoaderId,
) -> bool {
    if !matches!(
        loader,
        ClassLoaderId::Application | ClassLoaderId::Extension
    ) || class.hidden
        || class.name.len() == 1
        || class.name.starts_with('[')
    {
        return false;
    }
    match visible {
        ClassLoaderId::Bootstrap => true,
        ClassLoaderId::Extension => loader == ClassLoaderId::Application,
        _ => false,
    }
}

/// zt t22/df10: file `leaf` in the built-in `loader`'s row of
/// `ClassRealm::jdk_names_initiated` (the record
/// `Instrumentation.getInitiatedClasses` lists and the built-in
/// `findLoadedClass` rule reads, [`builtin_loader_initiated`]). A row already
/// there costs one read guard; a new one is counted in
/// `ClassRealm::builtin_initiated_rows_filed`.
fn file_builtin_initiated_row(shared: &SharedVm, loader: ClassLoaderId, leaf: ClassId) {
    if shared
        .classes
        .jdk_names_initiated
        .read()
        .get(&loader)
        .is_some_and(|set| set.contains(&leaf))
    {
        return;
    }
    let inserted = shared
        .classes
        .jdk_names_initiated
        .write()
        .entry(loader)
        .or_default()
        .insert(leaf);
    if inserted {
        shared
            .classes
            .builtin_initiated_rows_filed
            .fetch_add(1, Ordering::Relaxed);
        // zt pl4 item 1: a new row is the VM resolving a name the built-in
        // loader did not hold: HotSpot's upcall to its `loadClass`, counted in
        // `sun.cls.appClassLoadCount` (this VM answers it without Java, so
        // there is no time to add).
        if let Some(counters) = crate::runtime::perf_data_file::class_trace(shared) {
            counters.class_load_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// zt t22/df10 (define page §9c): whether the built-in `loader` (application
/// or platform) has `class_id` in its recorded dictionary rows
/// (`ClassRealm::jdk_names_initiated`: the resolution door's rows
/// [`file_builtin_initiated_row`], `Class.forName` with the loader, a
/// reflective listing's signature types, the launcher's `main` lookup). One
/// read guard.
pub(crate) fn builtin_loader_initiated(
    shared: &SharedVm,
    loader: ClassLoaderId,
    class_id: ClassId,
) -> bool {
    shared
        .classes
        .jdk_names_initiated
        .read()
        .get(&loader)
        .is_some_and(|set| set.contains(&class_id))
}

/// A method reference of `referencing` resolved its owner to `owner` (the
/// class the name denotes in `referencing`'s namespace): the entry table's
/// row for the reference's class entry ([`note_resolved_class_entry`]). The
/// invoke doors resolve owners by name or by receiver without the
/// `CONSTANT_Class` door, so this is the write that sees them. One field
/// read while the switch is off.
#[inline]
pub(crate) fn note_member_owner_resolved(shared: &SharedVm, referencing: ClassId, owner: ClassId) {
    if shared.classes.resolved_entry_table {
        note_resolved_class_entry(shared, referencing, owner);
    }
}

/// The classes the resolved class-entry table records for the classes in
/// `referencing` (one loader's classes), copied out under one read guard.
/// The loader's own classes and arrays are among them; the caller lists only
/// the other loaders' classes.
pub(crate) fn resolved_entries_of(shared: &SharedVm, referencing: &[ClassId]) -> Vec<ClassId> {
    let entries = shared.classes.resolved_class_entries.read();
    if entries.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for class_id in referencing {
        if let Some(Some(set)) = entries.get(class_id) {
            out.extend(set.iter().copied());
        }
    }
    out
}

/// Whether [`record_jdk_name_initiated`]'s writers run (one relaxed load).
#[inline]
fn jdk_names_armed(shared: &SharedVm) -> bool {
    shared
        .classes
        .jdk_names_initiated_armed
        .load(Ordering::Relaxed)
}

/// Record, for `loader`, the JDK class each of `names` (internal names; an
/// array descriptor counts by its element) denotes in `loader`'s namespace,
/// when it is loaded: a signature type the VM resolved through `loader`
/// outside a constant-pool entry. A name not loaded is skipped (HotSpot
/// would load it; CratonVM's reflection loads such a type lazily).
fn record_named_jdk_classes<'a>(
    shared: &SharedVm,
    loader: ClassLoaderId,
    names: impl IntoIterator<Item = &'a str>,
) {
    if loader == ClassLoaderId::Bootstrap {
        return;
    }
    let found: Vec<ClassId> = {
        let cm = shared.classes.class_manager.read_recursive();
        names
            .into_iter()
            .filter_map(|name| {
                let leaf = name.trim_start_matches('[');
                let leaf = leaf
                    .strip_prefix('L')
                    .and_then(|rest| rest.strip_suffix(';'))
                    .unwrap_or(leaf);
                (leaf.len() > 1).then_some(leaf)
            })
            .filter_map(|leaf| cm.find_class_by_name_for_loader(leaf, loader))
            .collect()
    };
    for class_id in found {
        record_jdk_name_initiated(shared, loader, class_id);
    }
}

// Not recorded: a call site's or a `CONSTANT_MethodType`'s type. HotSpot's
// `SystemDictionary::find_method_handle_type` looks a type visible to every
// loader (a public java.base class) up with the null loader, so the caller's
// loader initiates no JDK class there (measured, JDK 25:
// `L5W47InitiatedResolutionPaths`, `indy-type=false`).

/// Which members a reflective listing built (`Class.getDeclaredMethods0`,
/// `getDeclaredConstructors0`, `getDeclaredFields0`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReflectedMembers {
    Methods,
    Constructors,
    Fields,
}

/// A reflective listing of `class_id`'s members resolved their signature
/// types through the class's loader: HotSpot's `Reflection::new_method` /
/// `new_constructor` / `new_field` resolve the parameter, return, exception
/// and field types with the holder's loader, which files each in that
/// loader's dictionary. `public_only` is the natives' flag. One relaxed load
/// while the record is not armed.
pub(crate) fn note_reflected_members(
    shared: &SharedVm,
    class_id: ClassId,
    members: ReflectedMembers,
    public_only: bool,
) {
    if !jdk_names_armed(shared) {
        return;
    }
    let (loader, names): (ClassLoaderId, Vec<std::sync::Arc<str>>) = {
        let cm = shared.classes.class_manager.read_recursive();
        let Some(class) = cm.get_class(class_id) else {
            return;
        };
        let mut names: Vec<std::sync::Arc<str>> = Vec::new();
        match members {
            ReflectedMembers::Fields => {
                for field in &class.fields {
                    if public_only && field.access_flags.bits() & 0x0001 == 0 {
                        continue;
                    }
                    names.push(std::sync::Arc::clone(&field.descriptor));
                }
            }
            ReflectedMembers::Methods | ReflectedMembers::Constructors => {
                for method in &class.methods {
                    let is_constructor = &*method.name == "<init>";
                    if (members == ReflectedMembers::Constructors) != is_constructor
                        || &*method.name == "<clinit>"
                    {
                        continue;
                    }
                    if public_only && method.access_flags.bits() & 0x0001 == 0 {
                        continue;
                    }
                    names.extend(
                        crate::runtime::interpreter::constants::descriptor_class_names(
                            &method.descriptor,
                        )
                        .into_iter()
                        .map(std::sync::Arc::from),
                    );
                    for attribute in &method.attributes {
                        if let Some(cratonvm_reader::attribute::Attribute::Exceptions {
                            exception_indices,
                        }) = attribute.as_decoded()
                        {
                            names.extend(
                                exception_indices
                                    .iter()
                                    .filter_map(|&index| class.constant_pool.get_class_name(index))
                                    .map(std::sync::Arc::from),
                            );
                        }
                    }
                }
            }
        }
        (class.loader_id, names)
    };
    record_named_jdk_classes(shared, loader, names.iter().map(|name| &**name));
}

/// The launcher's lookup of the main class's `main` method, which the VM
/// makes without running `sun.launcher.LauncherHelper`: JDK 25's
/// `MethodFinder.findMainMethod` asks `Class.findMethod` for a public
/// `main(String[])`, then any `main(String[])`, then any `main()`; each is
/// `getMethodsRecursive`, which lists the declared (public, or all) methods
/// of the class and, when none matches, of its superclasses and interfaces,
/// resolving every listed method's signature types through that class's
/// loader. That is how the application loader has initiated
/// `java.lang.String` before `main` runs (measured, JDK 25: a public
/// method's parameter, return and `throws` types are listed, a
/// package-private one's are not, for a class with a public `main`). Called
/// by the launcher before `main`; one relaxed load while the record is not
/// armed.
pub(crate) fn note_launcher_main_lookup(shared: &SharedVm, main_class_id: ClassId) {
    if !jdk_names_armed(shared) {
        return;
    }
    // `isValidMainMethod`: returns `void` and is not private.
    let valid = |access: u16, descriptor: &str| descriptor.ends_with(")V") && access & 0x0002 == 0;
    const WITH_ARGS: &str = "([Ljava/lang/String;)";
    let found = launcher_find_main(shared, main_class_id, true, WITH_ARGS, true, 0)
        .or_else(|| launcher_find_main(shared, main_class_id, false, WITH_ARGS, true, 0));
    if !found.is_some_and(|(access, descriptor)| valid(access, &descriptor)) {
        let _ = launcher_find_main(shared, main_class_id, false, "()", true, 0);
    }
}

/// One `Class.getMethodsRecursive("main", parameters, include_static,
/// public_only)` of [`note_launcher_main_lookup`]: records the listed
/// methods' signature types, and answers the first match's access flags and
/// descriptor.
fn launcher_find_main(
    shared: &SharedVm,
    class_id: ClassId,
    public_only: bool,
    parameters: &str,
    include_static: bool,
    depth: u32,
) -> Option<(u16, String)> {
    if depth > 64 {
        return None;
    }
    note_reflected_members(shared, class_id, ReflectedMembers::Methods, public_only);
    let (found, superclass, interfaces) = {
        let cm = shared.classes.class_manager.read_recursive();
        let class = cm.get_class(class_id)?;
        let found = class
            .methods
            .iter()
            .find(|method| {
                let access = method.access_flags.bits();
                &*method.name == "main"
                    && method.descriptor.starts_with(parameters)
                    && (!public_only || access & 0x0001 != 0)
                    && (include_static || access & 0x0008 == 0)
            })
            .map(|method| (method.access_flags.bits(), method.descriptor.to_string()));
        (found, class.superclass, class.interfaces.clone())
    };
    if found.is_some() {
        return found;
    }
    let mut result = superclass.and_then(|superclass| {
        launcher_find_main(
            shared,
            superclass,
            public_only,
            parameters,
            include_static,
            depth + 1,
        )
    });
    for interface in interfaces {
        let from_interface =
            launcher_find_main(shared, interface, public_only, parameters, false, depth + 1);
        result = result.or(from_interface);
    }
    result
}

/// An agent's start-up as JPLIS and `sun.instrument.InstrumentationImpl`
/// make it, which the VM makes without them: `loadClassAndStartAgent` finds
/// `premain` / `agentmain` by `getDeclaredMethod`, which lists every
/// declared method of the agent class and resolves its signature types
/// through the class's loader; and the JPLIS library's JNI `FindClass` of
/// `sun/instrument/InstrumentationImpl` and `java/lang/InternalError` runs
/// with no Java frame, so through the system class loader (measured, JDK 25:
/// the application loader lists both, and the agent class's private
/// methods' types). One relaxed load while the record is not armed.
pub(crate) fn note_agent_start(shared: &SharedVm, agent_class_id: Option<ClassId>) {
    if !jdk_names_armed(shared) {
        return;
    }
    if let Some(agent_class_id) = agent_class_id {
        note_reflected_members(shared, agent_class_id, ReflectedMembers::Methods, false);
    }
    record_named_jdk_classes(
        shared,
        ClassLoaderId::Application,
        [
            "sun/instrument/InstrumentationImpl",
            "java/lang/InternalError",
        ],
    );
}

/// A JNI `FindClass` with no Java frame on the thread (a native agent's
/// VM-start callback, a host thread that attached and called it directly)
/// found `class_id`: HotSpot's `jni_FindClass` resolves such a name through
/// the system class loader, which so initiates it
/// (`Instrumentation.getInitiatedClasses`). `note_agent_start` covers JPLIS's
/// own two such lookups; this is every other one (interpreter round i1 wave
/// 51, lane L5; the i47-L5 `getInitiatedClasses` page, item 5). One relaxed
/// load while the record is not armed.
pub(crate) fn note_jni_find_class_without_caller(shared: &SharedVm, class_id: ClassId) {
    if !jdk_names_armed(shared) {
        return;
    }
    record_jdk_name_initiated(shared, ClassLoaderId::Application, class_id);
}
