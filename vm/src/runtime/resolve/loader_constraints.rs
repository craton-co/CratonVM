// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMS §5.3.4 loader constraints at member resolution, the VM half.
//!
//! When a class of loader `L1` resolves a field or method declared by a class
//! of loader `L2`, every class name `N` in the member's descriptor must mean
//! the same class to both loaders (`N^L1 = N^L2`). HotSpot checks it in
//! `LinkResolver::check_method_loader_constraints` /
//! `check_field_loader_constraints`, and when a side has not loaded `N` yet it
//! RECORDS the constraint, which `SystemDictionary::check_constraints` then
//! enforces when that side loads `N` (a definition, or an initiating load that
//! a loader answers with another loader's class).
//!
//! * Wave 31 (orchestrator): both sides already see a class for `N` and the
//!   two differ — refused at the resolution (`--jdk-only`), counted
//!   (`--compatible`).
//! * Wave 37 (lane L5, this module): one side (or neither) has loaded `N` —
//!   recorded in the per-VM table (`ClassManager::loader_constraint_table`,
//!   `cratonvm_classloading::loader_constraints`) under `--jdk-only`, checked
//!   at define (`ClassManager`) and at an initiating load
//!   ([`check_initiating_load`]), with HotSpot's messages. Probe
//!   `tools/probes/interp/L5/L5W37LoaderConstraintPending.java`.
//!
//! * Wave 42 (lane L5): the JDK-global names a user loader may define
//!   (`javax/`, `jdk/`, `sun/`, `com/sun/`) are recorded too, with each user
//!   loader's view read from its own answers ([`constraint_view`]); a
//!   transparent loader's global-route answer is checked
//!   (`constants.rs` `resolve_class_loader_aware`). Probe
//!   `tools/probes/interp/L5/L5W42GlobalNameConstraint.java`.
//!
//! Every entry point runs on a resolution MISS only (the site caches keep the
//! answer), or on a successful user-loader `loadClass` driven by the VM.
//! `--compatible` records nothing, so its define path pays one `is_empty` test.

use crate::classloading::{ClassId, ClassLoaderId, ClassManager, ClassStore};
use crate::error::{LinkageError, MethodCallFailed};
use crate::runtime::interpreter::constants::{
    descriptor_class_names, is_global_resolution_namespace,
};
use crate::runtime::interpreter::field_access::owner_in_referencing_namespace;
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use cratonvm_classloading::loader_constraints::LoaderLabel;

/// What a member resolution (or, `"override"`, a link-time override check,
/// [`check_override_constraints_at_link`]) found against JVMS §5.3.4, handed
/// from the resolver (which holds the class-manager read guard) to [`settle`] (which
/// must not: it reads loader objects).
#[derive(Debug)]
pub(crate) struct MemberConstraintFinding {
    /// `"method"`, `"field"`, `"override"` or `"itable"`.
    what: &'static str,
    accessor_id: ClassId,
    declaring_id: ClassId,
    member: String,
    descriptor: String,
    /// A name both loaders already see, as two different classes: the
    /// violation (wave 31's case).
    violated: Option<String>,
    /// Names one side (or neither) has loaded: constraints to record
    /// (`--jdk-only`).
    pending: Vec<String>,
    /// `"itable"` only: the class being linked, and the selected method's
    /// holder kind, which HotSpot's itable message names.
    itable: Option<ItableSite>,
}

/// The parts of HotSpot's itable message a [`MemberConstraintFinding`] does
/// not otherwise carry. The accessor is the selected method's holder and the
/// declaring class the superinterface, but the class being linked can be a
/// third class (a subclass of the holder, or an implementor of a default).
#[derive(Debug)]
struct ItableSite {
    /// External (dotted) name of the class being linked.
    linked_name: String,
    /// `"class"` or `"interface"`: the selected method's holder.
    selected_kind: &'static str,
}

/// A descriptor name a constraint is worth recording for. `java/` is left
/// out: a user loader cannot define a class under it, so every loader that
/// sees such a name sees the one class the built-in loaders defined and no
/// constraint on it can ever be violated. The other JDK-global namespaces
/// (`javax/`, `jdk/`, `sun/`, `com/sun/`) are recorded since interpreter round
/// i1 wave 42 (lane L5), through [`constraint_view`], which reads a user
/// loader's view of such a name from what the loader itself answered and never
/// from the global route's guess (the reason they were left out before: a
/// false `LinkageError` is worse than a missed one).
#[inline]
fn constrainable(name: &str) -> bool {
    !name.starts_with("java/")
}

/// A JDK-global name a user-defined loader may define a class under
/// (`constants.rs` `is_user_definable_global_name`, which is private to the
/// interpreter): every [`is_global_resolution_namespace`] name but `java/`.
#[inline]
fn user_definable_global_name(name: &str) -> bool {
    !name.starts_with("java/") && is_global_resolution_namespace(name)
}

/// The class `name` means to `class_id`'s defining loader, for JVMS §5.3.4,
/// known without loading.
///
/// For every name but a JDK-global one seen from a user-defined loader's
/// class, [`owner_in_referencing_namespace`] (the view the member-access
/// checks judge by), unchanged. For a `javax/`, `jdk/`, `sun/` or `com/sun/`
/// name seen from a user-defined loader's class, that view is
/// `find_class_by_name_for_class`'s GUESS ("the loader's own namespace, then
/// the built-in chain"): the resolver routes such a name globally for a loader
/// it judges transparent, so what the loader would answer is not known until it
/// is asked. Recording that guess would pin a class the loader may never answer
/// (a child-first loader defines its own copy on request), and the define that
/// follows would be refused where HotSpot refuses nothing. So here only what
/// the loader itself has answered counts: the class itself or a direct
/// supertype of that name (resolved through the loader when the class was
/// linked), the class the loader defined under the name, then its initiating
/// memo — the same record the class-resolution door answers such a name from
/// (`constants.rs` `loader_own_record_of_global_name`). Anything else is "not
/// loaded yet": the constraint is recorded, and the loader's later answer is
/// checked (at its define, at a VM-driven `loadClass`, or at the global route's
/// answer for a transparent loader, `constants.rs` `resolve_class_loader_aware`).
/// Interpreter round i1 wave 42, lane L5; probe
/// `tools/probes/interp/L5/L5W42GlobalNameConstraint.java`.
fn constraint_view(
    shared: &SharedVm,
    cm: &ClassManager,
    class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    // `--compatible` records nothing and only counts a both-loaded violation
    // (census); its view is left as it was.
    if !user_definable_global_name(name) || !shared.config.is_jdk_only() {
        return owner_in_referencing_namespace(shared, cm, class_id, name);
    }
    let class = cm.get_class(class_id)?;
    let loader @ ClassLoaderId::UserDefined(_) = class.loader_id else {
        return owner_in_referencing_namespace(shared, cm, class_id, name);
    };
    if &*class.name == name {
        return Some(class_id);
    }
    class
        .superclass
        .into_iter()
        .chain(class.interfaces.iter().copied())
        .find(|&sup| cm.get_class(sup).is_some_and(|c| &*c.name == name))
        .or_else(|| cm.loaded_class_under_exact_key(name, loader))
        .or_else(|| {
            shared
                .classes
                .initiating_resolution_cache
                .read()
                .get(&loader)
                .and_then(|m| m.get(name))
                .copied()
        })
}

/// JVMS §5.3.4 for `accessor_id` resolving `member` (with `descriptor`) of
/// `declaring_id`: `None` when both have one loader, or every descriptor name
/// already means one class to both (or, under `--compatible`, when nothing is
/// violated). Each side's view is [`owner_in_referencing_namespace`], the view
/// the member-access checks judge by; nothing is loaded. Under the caller's
/// class-manager read guard.
pub(crate) fn member_constraint_finding(
    shared: &SharedVm,
    cm: &ClassManager,
    what: &'static str,
    accessor_id: ClassId,
    declaring_id: ClassId,
    member: &str,
    descriptor: &str,
) -> Option<MemberConstraintFinding> {
    let accessor_loader = cm.get_loader_id(accessor_id)?;
    let declaring_loader = cm.get_loader_id(declaring_id)?;
    if accessor_loader == declaring_loader {
        return None;
    }
    let record = shared.config.is_jdk_only();
    let mut violated = None;
    let mut pending = Vec::new();
    for name in descriptor_class_names(descriptor) {
        let seen_by_accessor = constraint_view(shared, cm, accessor_id, name);
        let seen_by_declaring = constraint_view(shared, cm, declaring_id, name);
        match (seen_by_accessor, seen_by_declaring) {
            (Some(a), Some(d)) if a != d => {
                violated = Some(name.to_string());
                break;
            }
            (Some(_), Some(_)) => {}
            _ if record && constrainable(name) => pending.push(name.to_string()),
            _ => {}
        }
    }
    if violated.is_none() && pending.is_empty() {
        return None;
    }
    Some(MemberConstraintFinding {
        what,
        accessor_id,
        declaring_id,
        member: member.to_string(),
        descriptor: descriptor.to_string(),
        violated,
        pending,
        itable: None,
    })
}

/// Settle a [`member_constraint_finding`] outside the class-manager guard:
/// record its pending constraints (`--jdk-only`), and `Some(message)` — HotSpot's
/// "loader constraint violation: when resolving …" — when the resolution
/// violates one, either directly or against what the table already recorded
/// (a third loader's resolution pinned the name). The caller counts it and,
/// under `--jdk-only`, throws (`field_access::loader_constraint_census`).
pub(crate) fn settle(shared: &SharedVm, finding: MemberConstraintFinding) -> Option<String> {
    // The loaders the message (now, or at a later define) names. Only those
    // without a stored label are read, outside every lock: reading a loader's
    // fields resolves field slots, which takes the class-manager read lock.
    let mut label_classes = vec![finding.accessor_id, finding.declaring_id];
    if !finding.pending.is_empty() {
        let cm = shared.classes.class_manager.read();
        for name in &finding.pending {
            label_classes.extend(constraint_view(shared, &cm, finding.accessor_id, name));
            label_classes.extend(constraint_view(shared, &cm, finding.declaring_id, name));
        }
    }
    store_loader_labels(shared, &label_classes);

    let cm = shared.classes.class_manager.read();
    let violated = match finding.violated.clone() {
        Some(name) => Some(name),
        None => record_pending(shared, &cm, &finding),
    }?;
    let accessor_loader = cm.get_loader_id(finding.accessor_id)?;
    let declaring_loader = cm.get_loader_id(finding.declaring_id)?;
    let accessor_name = cm.get_class(finding.accessor_id)?.name.replace('/', ".");
    let declaring_name = cm.get_class(finding.declaring_id)?.name.replace('/', ".");
    let table = cm.loader_constraint_table();
    let accessor_label = loader_text(&table, accessor_loader);
    let declaring_label = loader_text(&table, declaring_loader);
    let accessor_origin = cm.loader_constraint_class_origin(&table, finding.accessor_id)?;
    let declaring_origin = cm.loader_constraint_class_origin(&table, finding.declaring_id)?;
    drop(table);
    drop(cm);
    Some(resolution_message(
        &finding,
        &violated,
        &accessor_name,
        &accessor_label,
        &declaring_name,
        &declaring_label,
        &accessor_origin,
        &declaring_origin,
    ))
}

/// Record `finding`'s pending constraints under `cm` (the READ guard, so no
/// definition runs meanwhile: a define holds the write guard). Each side's
/// view is read again here, since a side may have loaded the name since the
/// finding. `Some(name)` for the first name the resolution violates.
fn record_pending(
    shared: &SharedVm,
    cm: &ClassManager,
    finding: &MemberConstraintFinding,
) -> Option<String> {
    let accessor_key = cm.get_loader_id(finding.accessor_id)?.to_native_id();
    let declaring_key = cm.get_loader_id(finding.declaring_id)?.to_native_id();
    // Views first: `constraint_view` takes the initiating memo's lock, which
    // is never taken under the table's.
    let views: Vec<(&str, Option<ClassId>, Option<ClassId>)> = finding
        .pending
        .iter()
        .map(|name| {
            (
                name.as_str(),
                constraint_view(shared, cm, finding.accessor_id, name),
                constraint_view(shared, cm, finding.declaring_id, name),
            )
        })
        .collect();
    let mut table = cm.loader_constraint_table();
    let mut violated = None;
    for (name, seen_by_accessor, seen_by_declaring) in views {
        let conflict = match (seen_by_accessor, seen_by_declaring) {
            (Some(a), Some(d)) => a != d,
            _ => {
                let mut conflict = table.impose(name, accessor_key, declaring_key).is_some();
                if let Some(a) = seen_by_accessor {
                    conflict |= table.pin(name, accessor_key, a.as_u32()).is_some();
                }
                if let Some(d) = seen_by_declaring {
                    conflict |= table.pin(name, declaring_key, d.as_u32()).is_some();
                }
                conflict
            }
        };
        if cratonvm_types::flags().loader.dbg_access {
            eprintln!(
                "[ACCESS-DBG] LOADER-CONSTRAINT RECORD {name}: {:?} sees {seen_by_accessor:?}, {:?} sees {seen_by_declaring:?}{}",
                cm.get_loader_id(finding.accessor_id),
                cm.get_loader_id(finding.declaring_id),
                if conflict { " (violated)" } else { "" }
            );
        }
        if conflict && violated.is_none() {
            violated = Some(name.to_string());
        }
    }
    violated
}

/// HotSpot's `LinkResolver` wording, measured on JDK 25
/// (`L5W37LoaderConstraintPending`, `both-loaded`):
///
/// * method: `loader constraint violation: when resolving method 'int
///   p.Api.take(p.Shared)' the class loader 'a' @1 of the current class,
///   p.User, and the class loader 'b' @2 for the method's defining class,
///   p.Api, have different Class objects for the type p/Shared used in the
///   signature (p.User is in unnamed module of loader 'a' @1, parent loader
///   'b' @2; p.Api is in unnamed module of loader 'b' @2, parent loader
///   'app')` — the type in internal form (wave 42,
///   `L5W42ConstraintMessageNames`)
/// * field: `… when resolving field "held" of type p.Shared, the class loader
///   … for the field's defining class, p.Api, have different Class objects for
///   type p.Shared (…; …)`; an array field prints its type as `[Lp.Shared;`.
#[allow(clippy::too_many_arguments)]
fn resolution_message(
    finding: &MemberConstraintFinding,
    violated: &str,
    accessor_name: &str,
    accessor_label: &str,
    declaring_name: &str,
    declaring_label: &str,
    accessor_origin: &str,
    declaring_origin: &str,
) -> String {
    if let (true, Some(site)) = (finding.what == "itable", finding.itable.as_ref()) {
        // HotSpot's `klassItable` wording, measured on JDK 25
        // (`L5W40ItableConstraint`): the accessor is the selected method's
        // holder, the declaring class the superinterface whose method it
        // implements; the method is printed with the interface as its holder.
        return format!(
            "loader constraint violation in interface itable initialization for class {}: \
             when selecting method '{}' the class loader {declaring_label} for super interface \
             {declaring_name}, and the class loader {accessor_label} of the selected method's \
             {}, {accessor_name} have different Class objects for the type {} used in the \
             signature ({declaring_origin}; {accessor_origin})",
            site.linked_name,
            external_method(declaring_name, &finding.member, &finding.descriptor),
            site.selected_kind,
            violated.replace('/', "."),
        );
    }
    if finding.what == "override" {
        // HotSpot's `klassVtable` wording, measured on JDK 25
        // (`L5W37LoaderConstraintOverride`): the accessor is the class being
        // linked, which declares the selected (overriding) method; the
        // declaring class is its super type that declares the overridden one.
        return format!(
            "loader constraint violation for class {accessor_name}: when selecting overriding \
             method '{}' the class loader {accessor_label} of the selected method's type \
             {accessor_name}, and the class loader {declaring_label} for its super type \
             {declaring_name} have different Class objects for the type {} used in the \
             signature ({accessor_origin}; {declaring_origin})",
            external_method(accessor_name, &finding.member, &finding.descriptor),
            violated.replace('/', "."),
        );
    }
    if finding.what == "field" {
        let field_type = external_field_type(&finding.descriptor);
        format!(
            "loader constraint violation: when resolving field \"{}\" of type {field_type}, \
             the class loader {accessor_label} of the current class, {accessor_name}, and the \
             class loader {declaring_label} for the field's defining class, {declaring_name}, \
             have different Class objects for type {field_type} ({accessor_origin}; \
             {declaring_origin})",
            finding.member
        )
    } else {
        // The type is printed in INTERNAL form here, unlike the field, override
        // and itable messages (measured on JDK 25, `L5W42ConstraintMessageNames`:
        // "… for the type q/Shared used in the signature"; the method itself is
        // printed with dots). Interpreter round i1 wave 42, lane L5: it was
        // dotted, which only a packaged type shows.
        format!(
            "loader constraint violation: when resolving method '{}' the class loader \
             {accessor_label} of the current class, {accessor_name}, and the class loader \
             {declaring_label} for the method's defining class, {declaring_name}, have \
             different Class objects for the type {violated} used in the signature \
             ({accessor_origin}; {declaring_origin})",
            external_method(declaring_name, &finding.member, &finding.descriptor),
        )
    }
}

/// `int p.Api.take(p.Shared, long[])` from a method descriptor (HotSpot's
/// `Method::print_external_name`; parameters separated by `", "`, measured).
fn external_method(declaring: &str, name: &str, descriptor: &str) -> String {
    let (params, ret) = match descriptor.strip_prefix('(').and_then(|d| d.split_once(')')) {
        Some(split) => split,
        None => return format!("{declaring}.{name}{descriptor}"),
    };
    let mut rest = params;
    let mut out = Vec::new();
    while !rest.is_empty() {
        match java_type(rest) {
            Some((ty, tail)) => {
                out.push(ty);
                rest = tail;
            }
            None => break,
        }
    }
    let ret = java_type(ret).map_or_else(|| ret.to_string(), |(ty, _)| ty);
    format!("{ret} {declaring}.{name}({})", out.join(", "))
}

/// One field type from the front of `desc`, in Java source form, and the rest.
fn java_type(desc: &str) -> Option<(String, &str)> {
    let first = desc.as_bytes().first()?;
    let prim = match first {
        b'B' => "byte",
        b'C' => "char",
        b'D' => "double",
        b'F' => "float",
        b'I' => "int",
        b'J' => "long",
        b'S' => "short",
        b'Z' => "boolean",
        b'V' => "void",
        b'[' => {
            let (inner, rest) = java_type(&desc[1..])?;
            return Some((format!("{inner}[]"), rest));
        }
        b'L' => {
            let end = desc.find(';')?;
            return Some((desc[1..end].replace('/', "."), &desc[end + 1..]));
        }
        _ => return None,
    };
    Some((prim.to_string(), &desc[1..]))
}

/// A field's type as HotSpot's field message prints it: the class's external
/// name, or an array's descriptor with dots (`[Lp.Shared;`, measured).
fn external_field_type(descriptor: &str) -> String {
    match descriptor.strip_prefix('L') {
        Some(name) => name.trim_end_matches(';').replace('/', "."),
        None => descriptor.replace('/', "."),
    }
}

/// `loader_name_and_id` text of `loader` from the table's labels.
fn loader_text(
    table: &cratonvm_classloading::loader_constraints::LoaderConstraints,
    loader: ClassLoaderId,
) -> String {
    match loader {
        ClassLoaderId::Bootstrap => "'bootstrap'".to_string(),
        ClassLoaderId::Extension => "'platform'".to_string(),
        ClassLoaderId::Application => "'app'".to_string(),
        ClassLoaderId::UserDefined(_) => table
            .label(loader.to_native_id())
            .map_or_else(|| format!("{loader}"), |l| l.name_and_id.clone()),
    }
}

/// Store a [`LoaderLabel`] for the user-defined loader of each class in
/// `classes` that has none yet. Takes no lock while it reads loader objects.
fn store_loader_labels(shared: &SharedVm, classes: &[ClassId]) {
    let wanted: Vec<(u32, ClassId)> = {
        let cm = shared.classes.class_manager.read();
        let table = cm.loader_constraint_table();
        let mut wanted: Vec<(u32, ClassId)> = Vec::new();
        for &class_id in classes {
            let Some(loader @ ClassLoaderId::UserDefined(_)) = cm.get_loader_id(class_id) else {
                continue;
            };
            let key = loader.to_native_id();
            if table.label(key).is_none() && !wanted.iter().any(|(k, _)| *k == key) {
                wanted.push((key, class_id));
            }
        }
        wanted
    };
    if wanted.is_empty() {
        return;
    }
    let labels: Vec<(u32, LoaderLabel)> = wanted
        .into_iter()
        .filter_map(|(key, class_id)| user_loader_label(shared, class_id).map(|l| (key, l)))
        .collect();
    let cm = shared.classes.class_manager.read();
    let mut table = cm.loader_constraint_table();
    for (key, label) in labels {
        table.set_label(key, label);
    }
}

/// An object field of `obj` by name: `Some(None)` for a null field, `None`
/// when the field cannot be found.
fn object_field(shared: &SharedVm, obj: ObjectRef, name: &str) -> Option<Option<ObjectRef>> {
    let heap = &shared.mem.heap;
    let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, heap.class_id_of(obj), name)?;
    match heap.get_field(obj, slot) {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// A loader object's `nameAndId`, as HotSpot prints it. The built-in loaders
/// are allocated without a constructor here, so their field is null; they are
/// named by class (`'app'`, `'platform'`), as `ClassLoader.nameAndId` would
/// have named them.
fn loader_object_text(shared: &SharedVm, loader: ObjectRef) -> Option<String> {
    if let Some(Some(s)) = object_field(shared, loader, "nameAndId") {
        if let Some(text) = crate::vm::read_java_string(&shared.mem.heap, s).filter(|t| !t.is_empty()) {
            return Some(text);
        }
    }
    let class_id = shared.mem.heap.class_id_of(loader);
    let cm = shared.classes.class_manager.read();
    match &*cm.get_class(class_id)?.name {
        "jdk/internal/loader/ClassLoaders$AppClassLoader" => Some("'app'".to_string()),
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader" => Some("'platform'".to_string()),
        _ => None,
    }
}

/// The [`LoaderLabel`] of `class_id`'s user-defined loader: its `nameAndId`
/// and its parent's (`'bootstrap'` for a null parent, measured).
fn user_loader_label(shared: &SharedVm, class_id: ClassId) -> Option<LoaderLabel> {
    let loader = cratonvm_native_builtins::classloader::defining_loader_for(
        shared.vm_identity,
        class_id.as_u32(),
    )?;
    let name_and_id = loader_object_text(shared, loader)?;
    let parent = match object_field(shared, loader, "parent") {
        Some(Some(parent)) => loader_object_text(shared, parent),
        Some(None) => Some("'bootstrap'".to_string()),
        None => None,
    };
    Some(LoaderLabel {
        name_and_id,
        parent,
    })
}

/// JVMS §5.3.4 at an initiating load: `loader`'s `loadClass`, driven by the
/// VM for `name`, answered `class_id`, defined by ANOTHER loader. A constraint
/// that pins `(name, loader)` to a different class makes it HotSpot's
/// `check_constraints` violation (`defining == false`), measured on JDK 25
/// (`L5W37LoaderConstraintPending`, `initiating`):
///
/// `loader constraint violation: loader 'a' @1 wants to load class p.Shared.
/// A different class with the same name was previously loaded by 'b' @2.
/// (p.Shared is in unnamed module of loader 'b' @2, parent loader 'app')`
///
/// `Err(LinkageError)` under `--jdk-only` (the only mode that records); an
/// unconstrained answer is pinned when a constraint names it. One read guard
/// and an `is_empty` test while no constraint exists.
pub(crate) fn check_initiating_load(
    shared: &SharedVm,
    loader: ClassLoaderId,
    name: &str,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if !shared.config.is_jdk_only() {
        return Ok(());
    }
    let message = {
        let cm = shared.classes.class_manager.read();
        if cm.loader_constraint_table().is_empty() {
            return Ok(());
        }
        let Some(class) = cm.get_class(class_id) else {
            return Ok(());
        };
        if class.loader_id == loader || &*class.name != name {
            // A definition (checked at define), or an answer under another
            // name, which the caller's own checks judge.
            return Ok(());
        }
        let kind = if class.is_interface() { "interface" } else { "class" };
        let mut table = cm.loader_constraint_table();
        let Some(violation) = table.pin_if_constrained(name, loader.to_native_id(), class_id.as_u32())
        else {
            return Ok(());
        };
        let existing_id = ClassId::new(violation.existing);
        let Some(existing) = cm.get_class(existing_id) else {
            return Ok(());
        };
        let existing_kind = if existing.is_interface() {
            "interface"
        } else {
            "class"
        };
        let me = loader_text(&table, loader);
        let previous = loader_text(&table, existing.loader_id);
        let origin = cm
            .loader_constraint_class_origin(&table, existing_id)
            .unwrap_or_default();
        format!(
            "loader constraint violation: loader {me} wants to load {kind} {}. A different \
             {existing_kind} with the same name was previously loaded by {previous}. ({origin})",
            name.replace('/', ".")
        )
    };
    let n = shared
        .classes
        .loader_constraint_violations
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    if cratonvm_types::flags().loader.dbg_access {
        eprintln!("[ACCESS-DBG] LOADER-CONSTRAINT DENY #{n} at initiating load: {message}");
    }
    Err(LinkageError::LoaderConstraintViolation { message }.into())
}

/// JVMS §5.3.4, second bullet, at link: every method `class_id` declares that
/// overrides (§5.4.5) a method of a superclass with ANOTHER defining loader
/// imposes the constraint on each class name of the descriptor, as HotSpot's
/// `klassVtable` does while it builds the vtable of the class being linked.
/// Both loaders already seeing different classes for a name is the violation
/// ("loader constraint violation for class …: when selecting overriding
/// method …"); a side that has not loaded the name yet is recorded, and the
/// later load is checked (wave 37's define and initiating-load checks).
///
/// The overridden method is the nearest superclass declaration the method
/// can override ([`cratonvm_classloading::method_override::can_override`]), which is the
/// vtable slot's occupant; a same-loader occupant imposes nothing (its own
/// link checked it against its super). Since wave 40 the superinterfaces too
/// (HotSpot's itable check): each superinterface method against the method
/// the class selects for it ([`itable_target`]), HotSpot's "loader
/// constraint violation in interface itable initialization for class …"
/// (probe `L5W40ItableConstraint`). `--jdk-only`, a user-defined loader's class that
/// is not an interface; once per link, from `vm_util::link_claimed_class`.
/// Interpreter round i1 wave 39, lane L5 (the part of
/// `i37-L5-proposal-loader-constraints-for-overrides` the i29-L5 page needs);
/// probe `tools/probes/interp/L5/L5W37LoaderConstraintOverride.java`.
///
/// `Err(LinkageError)` for a violation, counted in
/// `ClassRealm::loader_constraint_violations` and traced under
/// `CRATONVM_DBG=access` as `[ACCESS-DBG] LOADER-CONSTRAINT DENY #n at link: …`.
pub(crate) fn check_override_constraints_at_link(
    shared: &SharedVm,
    class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    if !shared.config.is_jdk_only() {
        return Ok(());
    }
    let findings: Vec<MemberConstraintFinding> = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(());
        };
        let loader = class.loader_id;
        if !matches!(loader, ClassLoaderId::UserDefined(_)) || class.is_interface() {
            return Ok(());
        }
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        let mut findings = Vec::new();
        for m in &class.methods {
            if m.is_static()
                || m.access_flags.contains(MethodAccessFlags::PRIVATE)
                || m.name.starts_with('<')
                || !descriptor_class_names(&m.descriptor)
                    .into_iter()
                    .any(constrainable)
            {
                continue;
            }
            let mut sup = class.superclass;
            let mut steps = 0usize;
            while let Some(super_id) = sup {
                steps += 1;
                if steps > cratonvm_classloading::method_override::OVERRIDE_WALK_BUDGET {
                    break;
                }
                let Some(super_class) = cm.get_class(super_id) else {
                    break;
                };
                if let Some(ma) = super_class.find_method(&m.name, &m.descriptor) {
                    let mut budget = cratonvm_classloading::method_override::OVERRIDE_WALK_BUDGET;
                    if cratonvm_classloading::method_override::can_override(
                        &cm.class_store,
                        m,
                        class_id,
                        ma,
                        super_id,
                        &mut budget,
                    ) {
                        if super_class.loader_id != loader {
                            findings.extend(member_constraint_finding(
                                shared,
                                &cm,
                                "override",
                                class_id,
                                super_id,
                                &m.name,
                                &m.descriptor,
                            ));
                        }
                        break;
                    }
                }
                sup = super_class.superclass;
            }
        }
        // The itable half (interpreter round i1 wave 40, lane L5): for every
        // method of every superinterface (HotSpot's `transitive_interfaces`,
        // `klassItable::initialize_itable_for_interface`), the method the
        // itable slot selects; when its holder's loader is not the
        // interface's, the pair goes through the same machinery, with the
        // holder as the accessor and the interface as the declaring class.
        // Probe `tools/probes/interp/L5/L5W40ItableConstraint.java`.
        let store = &cm.class_store;
        if let Some(interfaces) =
            crate::runtime::resolve::selection::superinterface_closure(store, class_id)
        {
            let linked_name = class.name.replace('/', ".");
            for iface_id in interfaces {
                let Some(iface) = cm.get_class(iface_id) else {
                    continue;
                };
                for im in &iface.methods {
                    if im.is_static()
                        || im.access_flags.contains(MethodAccessFlags::PRIVATE)
                        || im.name.starts_with('<')
                        || !descriptor_class_names(&im.descriptor)
                            .into_iter()
                            .any(constrainable)
                    {
                        continue;
                    }
                    let Some(holder_id) = itable_target(store, class_id, &im.name, &im.descriptor)
                    else {
                        continue;
                    };
                    let Some(holder) = cm.get_class(holder_id) else {
                        continue;
                    };
                    if holder.loader_id == iface.loader_id {
                        continue;
                    }
                    let selected_kind = if holder.is_interface() {
                        "interface"
                    } else {
                        "class"
                    };
                    if let Some(mut finding) = member_constraint_finding(
                        shared,
                        &cm,
                        "itable",
                        holder_id,
                        iface_id,
                        &im.name,
                        &im.descriptor,
                    ) {
                        finding.itable = Some(ItableSite {
                            linked_name: linked_name.clone(),
                            selected_kind,
                        });
                        findings.push(finding);
                    }
                }
            }
        }
        findings
    };
    for finding in findings {
        let Some(message) = settle(shared, finding) else {
            continue;
        };
        let n = shared
            .classes
            .loader_constraint_violations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        if cratonvm_types::flags().loader.dbg_access {
            eprintln!("[ACCESS-DBG] LOADER-CONSTRAINT DENY #{n} at link: {message}");
        }
        return Err(LinkageError::LoaderConstraintViolation { message }.into());
    }
    Ok(())
}

/// The class whose method an itable slot of `class_id` gets for `(name,
/// desc)`, when HotSpot checks its loader constraints: the first non-static,
/// non-private declaration on the superclass chain
/// (`LinkResolver::lookup_instance_method_in_klasses`, private methods
/// skipped), else the one concrete maximally-specific superinterface method
/// (the class's default methods). `None` when that method is not public or is
/// abstract (the slot then raises an error when called, and HotSpot checks
/// nothing), when there is none or several, or when the walk is cut.
fn itable_target(store: &ClassStore, class_id: ClassId, name: &str, desc: &str) -> Option<ClassId> {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let mut cur = Some(class_id);
    let mut steps = 0usize;
    while let Some(id) = cur {
        steps += 1;
        if steps > cratonvm_classloading::method_override::OVERRIDE_WALK_BUDGET {
            return None;
        }
        let class = store.get(id)?;
        if let Some(m) = class.find_method(name, desc) {
            if !m.is_static() && !m.access_flags.contains(MethodAccessFlags::PRIVATE) {
                return (m.access_flags.contains(MethodAccessFlags::PUBLIC) && !m.is_abstract())
                    .then_some(id);
            }
        }
        cur = class.superclass;
    }
    let maximal = crate::runtime::resolve::selection::maximally_specific(store, class_id, name, desc)?;
    let mut concrete = maximal
        .iter()
        .filter(|&&(_, is_abstract)| !is_abstract)
        .map(|&(id, _)| id);
    match (concrete.next(), concrete.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Whether `exc`, thrown by a user loader's `loadClass` that the VM drove, is
/// the `LinkageError` a JVMS §5.3.4 check raised inside it (the define-time
/// refusal, surfacing through `defineClass`). HotSpot propagates it out of the
/// resolution; the drive otherwise treats a failed `loadClass` as "not
/// answered" and falls back to global resolution, which would define the name
/// elsewhere and succeed. `--jdk-only` only; cold (a failed load).
pub(crate) fn is_loader_constraint_error(shared: &SharedVm, exc: ObjectRef) -> bool {
    if !shared.config.is_jdk_only() {
        return false;
    }
    let class_id = shared.mem.heap.class_id_of(exc);
    let is_linkage_error = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .is_some_and(|c| &*c.name == "java/lang/LinkageError");
    if !is_linkage_error {
        return false;
    }
    matches!(
        object_field(shared, exc, "detailMessage"),
        Some(Some(s)) if crate::vm::read_java_string(&shared.mem.heap, s)
            .is_some_and(|m| m.starts_with("loader constraint violation"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_method_matches_hotspots_spelling() {
        assert_eq!(
            external_method("p.Api", "take", "(Lp/Shared;)I"),
            "int p.Api.take(p.Shared)"
        );
        assert_eq!(
            external_method("PT2$Api", "multi", "(ILPT2$Shared;[JLjava/lang/String;)[LPT2$Shared;"),
            "PT2$Shared[] PT2$Api.multi(int, PT2$Shared, long[], java.lang.String)"
        );
        assert_eq!(external_method("p.A", "m", "()V"), "void p.A.m()");
    }

    #[test]
    fn the_override_message_is_hotspots_vtable_wording() {
        let finding = MemberConstraintFinding {
            what: "override",
            accessor_id: ClassId::new(1),
            declaring_id: ClassId::new(2),
            member: "take".to_string(),
            descriptor: "(Lp/Shared;)I".to_string(),
            violated: Some("p/Shared".to_string()),
            pending: Vec::new(),
            itable: None,
        };
        assert_eq!(
            resolution_message(
                &finding,
                "p/Shared",
                "p.Sub",
                "'a' @1",
                "p.Base",
                "'b' @2",
                "p.Sub is in unnamed module of loader 'a' @1, parent loader 'b' @2",
                "p.Base is in unnamed module of loader 'b' @2, parent loader 'app'",
            ),
            "loader constraint violation for class p.Sub: when selecting overriding method \
             'int p.Sub.take(p.Shared)' the class loader 'a' @1 of the selected method's type \
             p.Sub, and the class loader 'b' @2 for its super type p.Base have different Class \
             objects for the type p.Shared used in the signature (p.Sub is in unnamed module of \
             loader 'a' @1, parent loader 'b' @2; p.Base is in unnamed module of loader 'b' @2, \
             parent loader 'app')"
        );
    }

    #[test]
    fn the_itable_message_is_hotspots_wording() {
        let finding = MemberConstraintFinding {
            what: "itable",
            accessor_id: ClassId::new(1),
            declaring_id: ClassId::new(2),
            member: "take".to_string(),
            descriptor: "(Lp/Shared;)I".to_string(),
            violated: Some("p/Shared".to_string()),
            pending: Vec::new(),
            itable: Some(ItableSite {
                linked_name: "p.Impl2".to_string(),
                selected_kind: "class",
            }),
        };
        assert_eq!(
            resolution_message(
                &finding,
                "p/Shared",
                "p.Mid",
                "'a' @1",
                "p.Api",
                "'b' @2",
                "p.Mid is in unnamed module of loader 'a' @1, parent loader 'b' @2",
                "p.Api is in unnamed module of loader 'b' @2, parent loader 'app'",
            ),
            "loader constraint violation in interface itable initialization for class p.Impl2: \
             when selecting method 'int p.Api.take(p.Shared)' the class loader 'b' @2 for super \
             interface p.Api, and the class loader 'a' @1 of the selected method's class, p.Mid \
             have different Class objects for the type p.Shared used in the signature (p.Api is \
             in unnamed module of loader 'b' @2, parent loader 'app'; p.Mid is in unnamed module \
             of loader 'a' @1, parent loader 'b' @2)"
        );
    }

    #[test]
    fn the_method_message_prints_the_type_in_internal_form() {
        let finding = MemberConstraintFinding {
            what: "method",
            accessor_id: ClassId::new(1),
            declaring_id: ClassId::new(2),
            member: "take".to_string(),
            descriptor: "(Lq/Shared;)I".to_string(),
            violated: Some("q/Shared".to_string()),
            pending: Vec::new(),
            itable: None,
        };
        assert_eq!(
            resolution_message(
                &finding,
                "q/Shared",
                "p.User",
                "'a' @1",
                "p.Api",
                "'b' @2",
                "p.User is in unnamed module of loader 'a' @1, parent loader 'b' @2",
                "p.Api is in unnamed module of loader 'b' @2, parent loader 'app'",
            ),
            "loader constraint violation: when resolving method 'int p.Api.take(q.Shared)' the \
             class loader 'a' @1 of the current class, p.User, and the class loader 'b' @2 for \
             the method's defining class, p.Api, have different Class objects for the type \
             q/Shared used in the signature (p.User is in unnamed module of loader 'a' @1, parent \
             loader 'b' @2; p.Api is in unnamed module of loader 'b' @2, parent loader 'app')"
        );
    }

    #[test]
    fn external_field_type_keeps_an_array_descriptor() {
        assert_eq!(external_field_type("Lp/Shared;"), "p.Shared");
        assert_eq!(external_field_type("[Lp/Shared;"), "[Lp.Shared;");
    }
}
