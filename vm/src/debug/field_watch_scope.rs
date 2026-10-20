// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class-scoped JDWP field watches (interpreter round i4 wave 95, lane L1;
//! `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`, default ON since wave 129 (`=0` off); stage 1 of the field
//! half of
//! `docs/internal/fixed-bugs/interpreter-i10-L1-proposal-exception-and-watch-events-from-compiled-code-DONE-20261224.md`).
//!
//! Only the interpreter's field bytecodes post `FieldAccess` /
//! `FieldModification`, so until now a JDWP watch kept every method of the
//! VM interpreted (`DebuggerGates::interpret_all`) and withdrew every
//! compiled body. HotSpot deoptimizes only the methods that access the
//! watched field. With the switch a watch whose fields this module resolved
//! keeps interpreted only the classes whose constant pools name one of them
//! ([`FieldWatchScope::contains`], asked by `DebuggerGates::concerns_method`
//! for the compile doors and by `interpreter::breakpoint_bars_compiling` for
//! the compiles, splices and binds) and withdraws only those classes'
//! dependents (`interpreter::note_breakpoint_classes_gained`, the
//! breakpoint's scoped withdrawal: their own bodies and every body that
//! inlined one of their methods). Wave 17's prerequisite (a compiled caller's
//! baked direct call into a compiled accessor) is what that withdrawal
//! already serves for breakpoints (wave 38) and class-scoped method events
//! (wave 93).
//!
//! **Which classes.** A class is concerned when one of its constant pool's
//! `Fieldref`s has a watched field's name and descriptor and could resolve
//! to it: its class name is the declaring class's, or any other name unless
//! that name is a JDK class's and the declaring class is not (a JDK class is
//! never a subclass of an application class). Matched without resolution
//! (which could load), so it over-approximates: a class that names a
//! same-named field of an unrelated application class stays interpreted
//! too, which costs speed only.
//!
//! **Degraded scope.** A concerned JDK class (the JIT may expand its methods
//! at a call site without a record the withdrawal reads,
//! `interpreter::jit_may_expand_a_method_of_without_a_record`) or a watched
//! field id that names no stored class's field (a lambda proxy's) degrades
//! the scope: every method runs interpreted, as before.
//!
//! **Classes defined later** are matched by the server's next poll (the
//! store's slots since the last refresh) and, before any of their code runs,
//! at their preparation ([`join`]).
//!
//! **Classes redefined** while a scope is resolved (JDWP `RedefineClasses`,
//! JVMTI `RedefineClasses` / `RetransformClasses`, `java.lang.instrument`,
//! the JFR retransform; all through `NativeContextImpl::redefine_class_with`)
//! are matched against their NEW class file before it is installed
//! ([`join_redefinitions`]; interpreter round i6 wave 116, lane L1): the
//! redefinition withdraws the old compiled dependents itself, and
//! a class that joins is barred from compiling before any of its new code
//! exists. Until wave 116 a class redefined to name a watched field was never
//! matched again (`refresh` scans only new store slots), so its new code
//! could run compiled and miss `FieldAccess` / `FieldModification`.
//!
//! Open: a C JVMTI env's field watch keeps the whole VM interpreted, as
//! before.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// A watched field by name: its declaring class (internal form), its name
/// and its descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatchedField {
    class_name: String,
    name: String,
    descriptor: String,
}

/// The resolved watch: the JDWP field ids it was resolved for (sorted, as
/// `events::EventManager::watched_field_ids` answers them) and their fields.
#[derive(Default)]
struct WatchSpecs {
    ids: Vec<u64>,
    fields: Vec<WatchedField>,
}

/// One VM's class-scoped field watch (a field of its `DebuggerGates`).
pub(crate) struct FieldWatchScope {
    /// `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`, read once when the VM's gates
    /// are made. Plain data: never written after construction.
    enabled: bool,
    /// A scope is resolved: a class prepared now is matched ([`join`]).
    armed: AtomicBool,
    /// What the scope was resolved for. Written only by [`refresh`] (and
    /// [`FieldWatchScope::clear`]); a leaf lock.
    specs: parking_lot::RwLock<WatchSpecs>,
    /// The concerned classes' ids.
    classes: parking_lot::RwLock<HashSet<u32>>,
    /// Bit `class_id % 64` of every id in [`Self::classes`]: the one load
    /// [`Self::contains`] answers `false` with for most classes.
    class_bits: AtomicU64,
    /// The class store's slot count the last [`refresh`] matched through.
    slots_seen: AtomicUsize,
    /// A concerned class or an unresolvable field takes every method with
    /// it (see the module doc).
    degraded: AtomicBool,
    /// [`Self::degraded`] as the last publication read it, so a join that
    /// degraded the scope where it could not publish is published by the
    /// server's next poll.
    degraded_published: AtomicBool,
    /// Bumped whenever the classes change.
    generation: AtomicU64,
    /// [`Self::generation`] as the last publication read it.
    generation_published: AtomicU64,
}

impl FieldWatchScope {
    /// An empty scope; reads the switch.
    pub(crate) fn new() -> Self {
        Self {
            enabled: cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JDWP_SCOPED_FIELD_WATCHES"),
            armed: AtomicBool::new(false),
            specs: parking_lot::RwLock::new(WatchSpecs::default()),
            classes: parking_lot::RwLock::new(HashSet::new()),
            class_bits: AtomicU64::new(0),
            slots_seen: AtomicUsize::new(0),
            degraded: AtomicBool::new(false),
            degraded_published: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            generation_published: AtomicU64::new(0),
        }
    }

    /// Is the switch on for this VM?
    #[inline]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Is a scope resolved, so a class prepared now must be matched?
    #[inline(always)]
    pub(crate) fn armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }

    /// Must class `class_id`'s methods run interpreted for a watch in force?
    /// One load for a class outside the bits; the set is empty whenever no
    /// scope is resolved.
    #[inline]
    pub(crate) fn contains(&self, class_id: u32) -> bool {
        let bit = 1u64 << (class_id % 64);
        if self.class_bits.load(Ordering::Acquire) & bit == 0 {
            return false;
        }
        self.classes.read().contains(&class_id)
    }

    /// Does this scope serve the watches in force, whose field ids are
    /// `wanted` (`events::EventManager::watched_field_ids`)? Resolved for
    /// exactly those ids and not degraded. Records what the publication
    /// read ([`Self::degraded_published`]). Asked by `publish_debugger_gates`
    /// under the debug-state lock (the specs' lock is a leaf).
    pub(crate) fn serves(&self, wanted: &[u64]) -> bool {
        let degraded = self.degraded.load(Ordering::Acquire);
        self.degraded_published.store(degraded, Ordering::Release);
        self.enabled
            && !wanted.is_empty()
            && !degraded
            && self.armed()
            && self.specs.read().ids == wanted
    }

    /// Did the concerned classes change since the last publication asked?
    pub(crate) fn take_grew(&self) -> bool {
        let now = self.generation.load(Ordering::Acquire);
        self.generation_published.swap(now, Ordering::AcqRel) != now
    }

    /// Add `class_ids` (`replace`: they are the whole set, plus any id at or
    /// past `carry_from` already there, which a [`join`] may have added while
    /// the caller resolved). Answers the ids that were not in the set.
    fn store_classes(&self, class_ids: &[u32], replace: bool, carry_from: usize) -> Vec<u32> {
        let mut set = self.classes.write();
        let gained: Vec<u32> = class_ids
            .iter()
            .copied()
            .filter(|id| !set.contains(id))
            .collect();
        if replace {
            // Widening: a class id to a store slot index.
            set.retain(|&id| id as usize >= carry_from);
        }
        set.extend(class_ids.iter().copied());
        let bits = set
            .iter()
            .fold(0u64, |bits, &id| bits | (1u64 << (id % 64)));
        self.class_bits.store(bits, Ordering::Release);
        drop(set);
        // Interpreter round i4 wave 96 (lane L1): only a class that joined is
        // growth. Bumping on every call made each server poll that saw a new
        // class (concerned or not) ask every compiled loop to exit
        // (`publish_debugger_gates`' `concerned_grew`).
        if !gained.is_empty() {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        gained
    }

    /// Forget the scope: no watch is in force.
    fn clear(&self) {
        self.armed.store(false, Ordering::Release);
        *self.specs.write() = WatchSpecs::default();
        self.classes.write().clear();
        self.class_bits.store(0, Ordering::Release);
        self.degraded.store(false, Ordering::Release);
        self.slots_seen.store(0, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// Is `name` (internal form) a JDK class's?
fn is_jdk_class(name: &str) -> bool {
    crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record(name)
}

/// Does a `Fieldref` naming class `ref_class`, field `name` and `descriptor`
/// possibly resolve to `watched`? Name and descriptor must match; the class
/// is the declaring one, or any other unless a JDK class names an
/// application class's field (see the module doc).
fn field_ref_may_name(ref_class: &str, name: &str, descriptor: &str, watched: &WatchedField) -> bool {
    name == watched.name
        && descriptor == watched.descriptor
        && (ref_class == watched.class_name
            || !(is_jdk_class(ref_class) && !is_jdk_class(&watched.class_name)))
}

/// Does `pool` hold a `Fieldref` that may name one of `watched`?
fn pool_names_a_watched_field(
    pool: &cratonvm_reader::constant_pool::ConstantPool,
    watched: &[WatchedField],
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    (1..pool.len()).any(|index| {
        let Ok(index) = u16::try_from(index) else {
            return false;
        };
        let Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) = pool.get(index)
        else {
            return false;
        };
        let (Some(ref_class), Some((name, descriptor))) = (
            pool.get_class_name(*class_index),
            pool.get_name_and_type(*name_and_type_index),
        ) else {
            return false;
        };
        watched
            .iter()
            .any(|w| field_ref_may_name(ref_class, name, descriptor, w))
    })
}

/// The fields `ids` name, or `None` when one names no stored class's field.
fn resolve_watched_fields(
    cm: &crate::classloading::ClassManager,
    ids: &[u64],
) -> Option<Vec<WatchedField>> {
    ids.iter()
        .map(|&id| {
            let (class_id, index) = super::field_of_id(id);
            let class = cm.class_store.get(class_id)?;
            let field = class.fields.get(index)?;
            Some(WatchedField {
                class_name: class.name.to_string(),
                name: field.name.to_string(),
                descriptor: field.descriptor.to_string(),
            })
        })
        .collect()
}

/// Resolve the JDWP field watches in force to the classes they concern; the
/// field-watch twin of `refresh_method_event_scope`, called beside it by the
/// JDWP server (holding no lock) at each poll, before it publishes after a
/// command, and at detach. Nothing with the switch off.
///
/// When the watched fields changed every class of the store is matched and
/// the result replaces the set; otherwise only the classes defined since the
/// last call are. The specs are published BEFORE the store's slot count is
/// read, so a class defined after that read joins at its preparation
/// ([`join`]). On the edge where a class joins, its dependents are withdrawn
/// (`interpreter::note_breakpoint_classes_gained`).
///
/// Answers whether the scope changed (its fields, its classes or its
/// degradation), so a caller no publication follows publishes the gates.
pub(crate) fn refresh(shared: &crate::vm::SharedVm) -> bool {
    let scope = &shared.debug.debugger_gates.field_watch_scope;
    if !scope.enabled {
        return false;
    }
    let wanted = shared.debug.debug_state.lock().events.watched_field_ids();
    if wanted.is_empty() {
        if scope.armed() || !scope.specs.read().ids.is_empty() {
            scope.clear();
            shared
                .debug
                .debugger_gates
                .generation
                .fetch_add(1, Ordering::Release);
            return true;
        }
        return false;
    }
    let changed = scope.specs.read().ids != wanted;
    if changed {
        // Resolved under the class manager's read lock, which is released
        // before the specs' write lock is taken: the specs' locks are never
        // waited for while the class manager is held (lock order: specs,
        // then the class manager; see `join`).
        let fields = {
            let cm = shared.classes.class_manager.read();
            resolve_watched_fields(&cm, &wanted)
        };
        let mut specs = scope.specs.write();
        specs.ids = wanted.clone();
        specs.fields = fields.clone().unwrap_or_default();
        // The degradation restarts with the new fields, under the specs'
        // write lock, so a join's verdict (stored under their read lock) is
        // never undone; an unresolvable field degrades.
        scope.degraded.store(fields.is_none(), Ordering::Release);
    }
    scope.armed.store(true, Ordering::SeqCst);
    let fields = scope.specs.read().fields.clone();
    let (matched, degraded, slots) = {
        let cm = shared.classes.class_manager.read();
        let from = if changed {
            0
        } else {
            scope.slots_seen.load(Ordering::Acquire)
        };
        let slots = cm.class_store.slot_count();
        if !changed && slots <= from {
            return scope.degraded.load(Ordering::Acquire)
                != scope.degraded_published.load(Ordering::Acquire);
        }
        let mut degraded = false;
        let mut matched: Vec<(u32, String)> = Vec::new();
        if !fields.is_empty() {
            for slot in from..slots {
                let Ok(raw) = u32::try_from(slot) else {
                    break;
                };
                let Some(class) = cm.class_store.get(crate::classloading::ClassId::new(raw)) else {
                    continue;
                };
                if pool_names_a_watched_field(&class.constant_pool, &fields) {
                    degraded |= is_jdk_class(&class.name);
                    matched.push((raw, class.name.to_string()));
                }
            }
        }
        (matched, degraded, slots)
    };
    let ids: Vec<u32> = matched.iter().map(|&(id, _)| id).collect();
    let gained = scope.store_classes(&ids, changed, slots);
    scope.slots_seen.store(slots, Ordering::Release);
    if degraded {
        scope.degraded.store(true, Ordering::Release);
    }
    shared
        .debug
        .debugger_gates
        .generation
        .fetch_add(1, Ordering::Release);
    let degraded = scope.degraded.load(Ordering::Acquire);
    if crate::runtime::env_cache::dbg_jitc() && (changed || !gained.is_empty()) {
        eprintln!(
            "[cratonvm-jitc] field watch scope: fields={} classes={} gained={} degraded={degraded}",
            wanted.len(),
            scope.classes.read().len(),
            gained.len(),
        );
    }
    let moved = changed
        || !gained.is_empty()
        || degraded != scope.degraded_published.load(Ordering::Acquire);
    if degraded || gained.is_empty() {
        // Degraded: the publication that follows withdraws every body.
        return moved;
    }
    let classes: Vec<(crate::classloading::ClassId, Option<String>)> = matched
        .into_iter()
        .filter(|(id, _)| gained.contains(id))
        .map(|(id, name)| (crate::classloading::ClassId::new(id), Some(name)))
        .collect();
    crate::runtime::interpreter::note_breakpoint_classes_gained(shared, &classes);
    moved
}

/// Class `class_id` of this VM was just prepared while a field watch scope
/// is resolved ([`FieldWatchScope::armed`]): if its constant pool names a
/// watched field, it joins the scope before any of its code runs. A JDK
/// class degrades the scope and republishes the gates (not on the server or
/// a thread running its heap work, which may hold the debug-state lock: the
/// server's next poll publishes). Called from `class_prepared_on_thread`
/// with no lock held.
pub(crate) fn join(shared: &crate::vm::SharedVm, class_id: crate::classloading::ClassId) {
    let scope = &shared.debug.debugger_gates.field_watch_scope;
    if scope.contains(class_id.as_u32()) {
        return;
    }
    let (ids, fields) = {
        let specs = scope.specs.read();
        (specs.ids.clone(), specs.fields.clone())
    };
    if fields.is_empty() {
        return;
    }
    let degrades = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.class_store.get(class_id) else {
            return;
        };
        if !pool_names_a_watched_field(&class.constant_pool, &fields) {
            return;
        }
        is_jdk_class(&class.name)
    };
    let fresh_degradation = {
        // Stored only while the fields matched above are still the resolved
        // ones, holding the specs' read lock across the store (no class
        // manager lock is held here): a refresh that resolved other fields
        // meanwhile walks the store, this class included, and is never
        // undone by this join.
        let specs = scope.specs.read();
        if specs.ids != ids {
            return;
        }
        // `carry_from` past every slot: nothing is replaced.
        let _ = scope.store_classes(&[class_id.as_u32()], false, usize::MAX);
        shared
            .debug
            .debugger_gates
            .generation
            .fetch_add(1, Ordering::Release);
        degrades && !scope.degraded.swap(true, Ordering::AcqRel)
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] field watch scope: class id={} joined at its preparation",
            class_id.as_u32(),
        );
    }
    if fresh_degradation && !super::debugger_hooks_suppressed() {
        let ds = shared.debug.debug_state.lock();
        super::publish_debugger_gates(shared, &ds);
    }
}

/// What a redefinition's new class file does to the scope
/// ([`redefinition_verdict`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedefinitionVerdict {
    /// Its constant pool names no watched field: nothing.
    Stays,
    /// It may name one: the class joins; `degrades` when it is a JDK class
    /// (or the file could not be read, so nothing is known).
    Joins { degrades: bool },
}

/// The verdict for a new class file read as `parsed` (`None`: it did not
/// parse; the install refuses such a file, but the scope does not rely on
/// that). Pure, for the unit tests.
fn redefinition_verdict(
    parsed: Option<(&str, &cratonvm_reader::constant_pool::ConstantPool)>,
    fields: &[WatchedField],
) -> RedefinitionVerdict {
    match parsed {
        None => RedefinitionVerdict::Joins { degrades: true },
        Some((name, pool)) if pool_names_a_watched_field(pool, fields) => {
            RedefinitionVerdict::Joins {
                degrades: is_jdk_class(name),
            }
        }
        Some(_) => RedefinitionVerdict::Stays,
    }
}

/// Classes `classes` (id and NEW class file) of this VM are about to be
/// redefined while a field watch scope is resolved (interpreter round i6
/// wave 116, lane L1; the switch triage's "unsound for redefinition"): each
/// whose new constant pool may name a watched field joins the scope now,
/// before the install, so no compile door admits its new code (the doors ask
/// [`FieldWatchScope::contains`] directly) and the redefinition's own
/// ahead-of-install withdrawal has already sent its old compiled dependents
/// to the interpreter. A JDK class, or a file that does not parse, degrades
/// the scope (every method interpreted) and republishes the gates, as
/// [`join`] does -- except on the JDWP server or a thread running its work,
/// where the server republishes after the command. Called from
/// `NativeContextImpl::redefine_class_with` with no lock held. One
/// load when no scope is resolved.
pub(crate) fn join_redefinitions(
    shared: &crate::vm::SharedVm,
    classes: &[(crate::classloading::ClassId, &[u8])],
) {
    let scope = &shared.debug.debugger_gates.field_watch_scope;
    if !scope.enabled || !scope.armed() {
        return;
    }
    let (ids, fields) = {
        let specs = scope.specs.read();
        (specs.ids.clone(), specs.fields.clone())
    };
    if fields.is_empty() {
        return;
    }
    let mut joining: Vec<u32> = Vec::new();
    let mut degrades = false;
    for &(class_id, bytes) in classes {
        let parsed = cratonvm_reader::read_class(bytes).ok();
        let verdict = redefinition_verdict(
            parsed
                .as_ref()
                .map(|file| (&*file.this_class, &file.constant_pool)),
            &fields,
        );
        if let RedefinitionVerdict::Joins { degrades: d } = verdict {
            degrades |= d;
            joining.push(class_id.as_u32());
        }
    }
    if joining.is_empty() {
        return;
    }
    let fresh_degradation = {
        // As in `join`: stored only while the fields matched are still the
        // resolved ones (a refresh that resolved others walks the whole
        // store after this install, the new constant pools included).
        let specs = scope.specs.read();
        if specs.ids != ids {
            return;
        }
        let _ = scope.store_classes(&joining, false, usize::MAX);
        shared
            .debug
            .debugger_gates
            .generation
            .fetch_add(1, Ordering::Release);
        degrades && !scope.degraded.swap(true, Ordering::AcqRel)
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] field watch scope: {} redefined class(es) joined before the install \
             degraded={degrades}",
            joining.len(),
        );
    }
    if fresh_degradation && !super::debugger_hooks_suppressed() {
        let ds = shared.debug.debug_state.lock();
        super::publish_debugger_gates(shared, &ds);
    }
}

/// The matching rules (interpreter round i4 wave 95, lane L1). The scope's
/// VM-level behaviour is pinned by the probe
/// `tools/probes/interp/L1/L1W95JdiScopedFieldWatch.java`.
#[cfg(test)]
mod tests {
    use super::*;

    fn watched(class_name: &str, name: &str, descriptor: &str) -> WatchedField {
        WatchedField {
            class_name: class_name.to_string(),
            name: name.to_string(),
            descriptor: descriptor.to_string(),
        }
    }

    #[test]
    fn a_reference_through_the_declaring_class_or_an_application_class_may_name_the_field() {
        let w = watched("app/Counter", "count", "I");
        assert!(field_ref_may_name("app/Counter", "count", "I", &w));
        assert!(field_ref_may_name("app/SubCounter", "count", "I", &w), "a subclass may");
        assert!(!field_ref_may_name("app/Counter", "count", "J", &w), "another descriptor");
        assert!(!field_ref_may_name("app/Counter", "total", "I", &w), "another name");
    }

    #[test]
    fn a_jdk_class_reference_never_names_an_application_field() {
        let w = watched("app/Counter", "count", "I");
        assert!(!field_ref_may_name("java/lang/StringBuilder", "count", "I", &w));
        let jdk = watched("java/util/ArrayList", "size", "I");
        assert!(field_ref_may_name("java/util/ArrayList", "size", "I", &jdk));
        assert!(field_ref_may_name("app/MyList", "size", "I", &jdk), "an app subclass may");
        assert!(field_ref_may_name("java/util/Other", "size", "I", &jdk), "JDK to JDK may");
    }

    /// A pool `[_, Utf8 class, Class #1, Utf8 name, Utf8 desc, NameAndType,
    /// Fieldref]`: one `Fieldref` to `class.name:desc`.
    fn pool_with_fieldref(
        class: &str,
        name: &str,
        desc: &str,
    ) -> cratonvm_reader::constant_pool::ConstantPool {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};
        ConstantPool::new(vec![
            E::Tombstone,
            E::Utf8(class.into()),
            E::ClassReference { name_index: 1 },
            E::Utf8(name.into()),
            E::Utf8(desc.into()),
            E::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            },
            E::FieldReference {
                class_index: 2,
                name_and_type_index: 5,
            },
        ])
    }

    /// Interpreter round i6 wave 116, lane L1: a redefinition's new class
    /// file joins the scope when it names a watched field (a JDK class's
    /// degrades it), stays out otherwise, and an unreadable file joins and
    /// degrades (nothing is known about it).
    #[test]
    fn a_redefinition_that_names_a_watched_field_joins_before_its_install() {
        let fields = [watched("app/Counter", "count", "I")];
        let names = pool_with_fieldref("app/Counter", "count", "I");
        assert_eq!(
            redefinition_verdict(Some(("app/Bumper", &names)), &fields),
            RedefinitionVerdict::Joins { degrades: false }
        );
        assert_eq!(
            redefinition_verdict(Some(("java/util/Bumper", &names)), &fields),
            RedefinitionVerdict::Joins { degrades: true },
            "a JDK class that names it degrades"
        );
        let other = pool_with_fieldref("app/Counter", "total", "I");
        assert_eq!(
            redefinition_verdict(Some(("app/Bumper", &other)), &fields),
            RedefinitionVerdict::Stays
        );
        assert_eq!(
            redefinition_verdict(None, &fields),
            RedefinitionVerdict::Joins { degrades: true }
        );
    }

    #[test]
    fn the_scope_starts_empty_and_holds_what_it_stores() {
        let scope = FieldWatchScope::new();
        assert!(!scope.armed());
        assert!(!scope.contains(70));
        assert_eq!(scope.store_classes(&[70], false, usize::MAX), vec![70]);
        assert!(scope.contains(70));
        assert!(!scope.contains(70 + 64), "same bit, not in the set");
        assert!(scope.take_grew());
        assert!(!scope.take_grew(), "nothing changed since");
        assert!(scope.store_classes(&[70], false, usize::MAX).is_empty());
        assert!(!scope.take_grew(), "storing a class already there is no growth");
        scope.clear();
        assert!(!scope.contains(70));
        assert!(!scope.serves(&[1]), "never armed");
    }
}
