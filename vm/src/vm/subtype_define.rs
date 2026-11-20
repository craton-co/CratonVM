// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The class-hierarchy half of the class define listener (T5.4.4): a newly
//! defined class is a new subtype of every class in its supertype closure, so
//! every compiled body that inlined or devirtualised an OVERRIDABLE member of
//! one of them is withdrawn (`JitCache::invalidate_for_new_subtype`), and a
//! compile still running is refused at publication by the logged record.
//!
//! JIT round j27 wave 2 (lane REDEFINE;
//! `docs/feature-designs/class-resolution-and-caching.md`):
//! the listener used to live inline in `SharedVm::load_class_concurrent_for`,
//! so only the VM's own loads ran it. A class a Java loader defines
//! (`ClassLoader.defineClass1/2/0`, `Lookup.defineClass`, `Unsafe.defineClass`,
//! JNI `DefineClass`) reaches `define_class_full` and its siblings, which never
//! ran it: a CHA bind or receiver speculation on a supertype of a
//! `URLClassLoader` / webapp / Spring Boot class kept a guard that always
//! misses, with no path back to a recompile. HotSpot runs the dependency check
//! for every class added to the hierarchy, whatever its loader
//! (`SystemDictionary::add_to_hierarchy` -> `CodeCache::flush_dependents_on`).
//! Every bind this listener retires is guarded, so the defect cost performance,
//! never a wrong method.
//!
//! `CRATONVM_JIT_JAVA_DEFINE_SUBTYPE_WITHDRAW=0` keeps the Java-loader defines
//! off the listener (the pre-j27 behaviour); the VM's own loads always run it.

use crate::classloading::{ClassId, ClassStore};
use crate::vm::SharedVm;
use std::sync::Arc;

/// The members of one supertype a new subtype cannot rebind.
type FixedMembers = Arc<[(Arc<str>, Arc<str>)]>;

/// Kill switch for the Java-loader defines' run of the listener (default ON,
/// `=0` off). Read at each call through the VM's latched flags (no process
/// static): a define is not a hot path.
pub(crate) fn java_define_subtype_withdraw_enabled() -> bool {
    true
}

/// The static, private and final methods of `class` (all of them for a final
/// class): a new subtype overrides none of them. Empty when `scoped` is off
/// (`CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED=0`, the name-only match).
fn fixed_members(class: &crate::classloading::Class, scoped: bool) -> FixedMembers {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    if !scoped {
        return Arc::from(Vec::new());
    }
    let final_class = class.is_final();
    class
        .methods
        .iter()
        .filter(|m| {
            final_class
                || m.is_static()
                || m.access_flags.contains(MethodAccessFlags::PRIVATE)
                || m.access_flags.contains(MethodAccessFlags::FINAL)
        })
        .map(|m| (Arc::clone(&m.name), Arc::clone(&m.descriptor)))
        .collect()
}

/// Every proper supertype of `class_id` (superclasses and interfaces, at any
/// depth, each once) with its [`fixed_members`].

fn introduced_members(store: &ClassStore, class_id: ClassId) -> FixedMembers {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let Some(class) = store.get(class_id) else {
        return Arc::from(Vec::new());
    };
    let mut out: Vec<(Arc<str>, Arc<str>)> = class
        .methods
        .iter()
        .filter(|m| {
            !m.is_static()
                && !m.access_flags.contains(MethodAccessFlags::PRIVATE)
                && &*m.name != "<init>"
                && &*m.name != "<clinit>"
        })
        .map(|m| (Arc::clone(&m.name), Arc::clone(&m.descriptor)))
        .collect();
    if !class.is_abstract() && !class.is_interface() && !class.interfaces.is_empty() {
        let mut stack: Vec<ClassId> = class.interfaces.clone();
        let mut seen: std::collections::HashSet<ClassId> = std::collections::HashSet::new();
        while let Some(iface_id) = stack.pop() {
            if !seen.insert(iface_id) {
                continue;
            }
            let Some(iface) = store.get(iface_id) else {
                continue;
            };
            for m in &iface.methods {
                if !m.is_static()
                    && !m.access_flags.contains(MethodAccessFlags::PRIVATE)
                    && !m.access_flags.contains(MethodAccessFlags::ABSTRACT)
                    && &*m.name != "<init>"
                    && &*m.name != "<clinit>"
                {
                    out.push((Arc::clone(&m.name), Arc::clone(&m.descriptor)));
                }
            }
            stack.extend(iface.interfaces.iter().copied());
        }
    }
    Arc::from(out)
}

fn supertype_closure(
    store: &ClassStore,
    class_id: ClassId,
    scoped: bool,
) -> Vec<(Arc<str>, FixedMembers)> {
    let mut names: Vec<(Arc<str>, FixedMembers)> = Vec::new();
    let mut stack: Vec<ClassId> = Vec::new();
    let mut seen: std::collections::HashSet<ClassId> = std::collections::HashSet::new();
    if let Some(class) = store.get(class_id) {
        stack.extend(class.interfaces.iter().copied());
        stack.extend(class.superclass);
    }
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(class) = store.get(id) else {
            continue;
        };
        names.push((Arc::clone(&class.name), fixed_members(class, scoped)));
        stack.extend(class.interfaces.iter().copied());
        stack.extend(class.superclass);
    }
    names
}

impl SharedVm {
    /// Withdraw the compiled bodies the define of `class_id` makes stale as a
    /// new subtype of each class in its supertype closure. Takes the
    /// class-manager read lock itself, so the caller must hold no
    /// class-manager guard. Returns the number of bodies withdrawn.
    pub(crate) fn note_subtype_define(&self, class_id: ClassId) -> usize {
        let scoped = true;
        let (supertypes, introduced) = {
            let cm = self.classes.class_manager.read();
            let store = cm.class_store();
            let intro = if scoped {
                introduced_members(store, class_id)
            } else {
                Arc::from(Vec::new())
            };
            if scoped && intro.is_empty() {
                return 0;
            }
            (supertype_closure(store, class_id, scoped), intro)
        };
        let jit = self.jit.jit_cache.write();
        let withdrawn: usize = if scoped {
            jit.invalidate_for_new_subtypes(&supertypes, introduced)
        } else {
            supertypes
                .iter()
                .map(|(sup, _)| jit.invalidate_for_class_change(sup))
                .sum()
        };
        // `CRATONVM_DBG_JITC=1`: what a define withdrew (the probe's verify line).
        if withdrawn != 0 && crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] subtype-define class id {} withdrew {withdrawn} bod{}",
                class_id.as_u32(),
                if withdrawn == 1 { "y" } else { "ies" }
            );
        }
        withdrawn
    }

    /// [`Self::note_subtype_define`] for a class a Java loader (or JNI)
    /// defined, under [`java_define_subtype_withdraw_enabled`].
    pub(crate) fn note_java_loader_subtype_define(&self, class_id: ClassId) {
        if java_define_subtype_withdraw_enabled() {
            let _ = self.note_subtype_define(class_id);
        }
    }
}

#[cfg(test)]
mod j27_subtype_define_tests;
