// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The class-hierarchy half of the class define listener (T5.4.4): a newly
//! defined class is a new subtype of every class in its supertype closure, so
//! every compiled body that inlined or devirtualised an OVERRIDABLE member of
//! one of them is withdrawn (`JitCache::invalidate_for_new_subtype`), and a
//! compile still running is refused at publication by the logged record.
//!
//! JIT round j27 wave 2 (lane REDEFINE;
//! `docs/internal/jit/j27-java-loader-defines-skip-the-new-subtype-listener-FIXED-20261029.md`):
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_JAVA_DEFINE_SUBTYPE_WITHDRAW")
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
fn supertype_closure(
    store: &ClassStore,
    class_id: ClassId,
    scoped: bool,
) -> Vec<(String, FixedMembers)> {
    let mut names: Vec<(String, FixedMembers)> = Vec::new();
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
        names.push((class.name.to_string(), fixed_members(class, scoped)));
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
        let scoped = self.config.jit_subtype_define_scoped;
        let supertypes = {
            let cm = self.classes.class_manager.read();
            supertype_closure(cm.class_store(), class_id, scoped)
        };
        let jit = self.jit.jit_cache.write();
        let withdrawn: usize = supertypes
            .iter()
            .map(|(sup, fixed)| {
                if scoped {
                    jit.invalidate_for_new_subtype(sup, Arc::clone(fixed))
                } else {
                    jit.invalidate_for_class_change(sup)
                }
            })
            .sum();
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
mod j27_subtype_define_tests {
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;

    /// A body with `inlined` as its dependency record; `None` when no
    /// executable buffer could be mapped.
    fn body(inlined: (&str, &str, &str)) -> Option<cratonvm_jit::CompiledMethod> {
        let buffer = cratonvm_jit::ExecutableBuffer::new(64)?;
        let mut cm = cratonvm_jit::CompiledMethod::new(buffer);
        cm.inlined_methods
            .push((inlined.0.to_string(), inlined.1.to_string(), inlined.2.to_string()));
        Some(cm)
    }

    fn cached(shared: &SharedVm, class: &str, method: &str, cid: crate::classloading::ClassId) -> bool {
        let cn: Arc<str> = class.into();
        let mn: Arc<str> = method.into();
        let desc: Arc<str> = "()V".into();
        shared.jit.jit_cache.read().get(&cn, &mn, &desc, cid).is_some()
    }

    /// The name of the topmost proper ancestor of `leaf` other than
    /// `java/lang/Object`, read from the class store: the fixture follows the
    /// hierarchy the store actually holds (a `--compatible` store may hold a
    /// shorter chain than the JDK's source), instead of naming one.
    fn far_ancestor(shared: &SharedVm, leaf: crate::classloading::ClassId) -> Option<String> {
        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();
        let mut far: Option<String> = None;
        let mut next = store.get(leaf).and_then(|c| c.superclass);
        while let Some(id) = next {
            let class = store.get(id)?;
            if &*class.name != "java/lang/Object" {
                far = Some(class.name.to_string());
            }
            next = class.superclass;
        }
        far
    }

    /// A define withdraws a body that devirtualised an overridable member of
    /// an ANCESTOR (the topmost one below `Object`), and keeps a body that
    /// only inlined a constructor of it.
    #[test]
    fn a_define_withdraws_a_body_bound_to_an_ancestors_overridable_member() {
        let shared = SharedVm::new(VmConfig::default());
        let Ok(leaf) = shared.load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
        else {
            return; // No class library in this environment.
        };
        let ancestor = far_ancestor(&shared, leaf).expect("an exception has a superclass chain");
        let cid = crate::classloading::ClassId::new(0);
        // A member no ancestor declares, so none lists it as fixed: a new
        // subtype can always override it.
        let (Some(bound), Some(ctor)) = (
            body((ancestor.as_str(), "j27Overridable", "()V")),
            body((ancestor.as_str(), "<init>", "()V")),
        ) else {
            return;
        };
        {
            let jit = shared.jit.jit_cache.write();
            jit.put("j27/Caller".into(), "bound".into(), "()V".into(), cid, bound);
            jit.put("j27/Caller".into(), "ctor".into(), "()V".into(), cid, ctor);
        }
        assert!(cached(&shared, "j27/Caller", "bound", cid));

        let withdrawn = shared.note_subtype_define(leaf);

        let closure: Vec<String> = {
            let cm = shared.classes.class_manager.read();
            super::supertype_closure(cm.class_store(), leaf, true)
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        assert!(
            withdrawn >= 1,
            "the body bound to {ancestor}.j27Overridable is withdrawn; closure {closure:?}"
        );
        assert!(!cached(&shared, "j27/Caller", "bound", cid));
        if shared.config.jit_subtype_define_scoped {
            assert!(
                cached(&shared, "j27/Caller", "ctor", cid),
                "a constructor keeps its one body whatever subtypes appear"
            );
        }
    }

    /// Source pins: every Java-loader define entry point runs the listener
    /// after its class-manager guard is gone.
    #[test]
    fn the_java_loader_define_paths_run_the_subtype_listener() {
        let exec = include_str!("vm_exec.rs").replace("\r\n", "\n");
        for entry in [
            "    fn define_class_full(",
            "    fn define_class_from_bytes(",
            "    fn define_class_with_loader(",
        ] {
            let start = exec.find(entry).expect(entry);
            let rest = &exec[start..];
            let body = &rest[..rest.find("\n    }\n").expect("its end")];
            let release = body.find("drop(cm);").expect("the guard release");
            let listener = body
                .find("note_java_loader_subtype_define(")
                .unwrap_or_else(|| panic!("{entry} runs the listener"));
            assert!(release < listener, "{entry}: after the guard release");
        }
        let jni = include_str!("../native/jni.rs").replace("\r\n", "\n");
        let start = jni.find("extern \"C\" fn jni_define_class(").expect("jni_define_class");
        let rest = &jni[start..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let release = body.find("drop(cm);").expect("the guard release");
        let listener = body
            .find("shared.note_java_loader_subtype_define(cid);")
            .expect("the JNI define runs the listener");
        assert!(release < listener);
    }

    /// j27 wave 8 (lane W8CHA): the two VM-exec define paths that are not
    /// `define_class_full` run the finalizable-define listener too (they ran
    /// only the hierarchy half), after the guard release and before the
    /// hierarchy listener, as JNI `DefineClass` does.
    #[test]
    fn j27w8_the_other_define_paths_run_the_finalizable_listener() {
        let exec = include_str!("vm_exec.rs").replace("\r\n", "\n");
        for entry in ["    fn define_class_from_bytes(", "    fn define_class_with_loader("] {
            let start = exec.find(entry).expect(entry);
            let rest = &exec[start..];
            let body = &rest[..rest.find("\n    }\n").expect("its end")];
            let release = body.find("drop(cm);").expect("the guard release");
            let finalizable = body
                .find("shared.note_finalizable_define(cid);")
                .unwrap_or_else(|| panic!("{entry} runs the finalizable listener"));
            let hierarchy = body
                .find("note_java_loader_subtype_define(")
                .expect("the hierarchy listener");
            assert!(release < finalizable && finalizable < hierarchy, "{entry}: order");
        }
    }
}
