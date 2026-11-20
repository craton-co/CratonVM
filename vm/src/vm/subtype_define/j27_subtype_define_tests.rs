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

fn cached(shared: &SharedVm, class: &str, method: &str, desc: &str, cid: crate::classloading::ClassId) -> bool {
    let cn: Arc<str> = class.into();
    let mn: Arc<str> = method.into();
    let desc: Arc<str> = desc.into();
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
    let Ok(leaf) = shared.load_class_concurrent("java/util/ArrayList")
    else {
        return; // No class library in this environment.
    };
    let ancestor = far_ancestor(&shared, leaf).expect("ArrayList has a superclass chain");
    let overridable = {
        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();
        let cls = store.get(leaf).expect("leaf loaded");
        cls.methods
            .iter()
            .find(|m| !m.is_static() && &*m.name != "<init>" && &*m.name != "<clinit>")
            .map(|m| (m.name.to_string(), m.descriptor.to_string()))
            .expect("ArrayList declares at least one virtual method")
    };
    let cid = crate::classloading::ClassId::new(0);
    let (Some(bound), Some(ctor)) = (
        body((ancestor.as_str(), overridable.0.as_str(), overridable.1.as_str())),
        body((ancestor.as_str(), "<init>", "()V")),
    ) else {
        return;
    };
    {
        let jit = shared.jit.jit_cache.write();
        jit.put("j27/Caller".into(), "bound".into(), "()V".into(), cid, bound);
        jit.put("j27/Caller".into(), "ctor".into(), "()V".into(), cid, ctor);
    }
    assert!(cached(&shared, "j27/Caller", "bound", "()V", cid));

    let withdrawn = shared.note_subtype_define(leaf);

    let closure: Vec<String> = {
        let cm = shared.classes.class_manager.read();
        super::supertype_closure(cm.class_store(), leaf, true)
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect()
    };
    assert!(
        withdrawn >= 1,
        "the body bound to {ancestor}.j27Overridable is withdrawn; closure {closure:?}"
    );
    assert!(!cached(&shared, "j27/Caller", "bound", "()V", cid));
    {
        assert!(
            cached(&shared, "j27/Caller", "ctor", "()V", cid),
            "a constructor keeps its one body whatever subtypes appear"
        );
    }
}

/// Source pins: every Java-loader define entry point runs the listener
/// after its class-manager guard is gone.
#[test]
fn the_java_loader_define_paths_run_the_subtype_listener() {
    let exec = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
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
    let jni = concat!(include_str!("../../native/jni/mod.rs"), "\n", include_str!("../../native/jni/j_boolean.rs"), "\n", include_str!("../../native/jni/raw_locals_counted.rs"), "\n", include_str!("../../native/jni/truncate_local_frames.rs"), "\n", include_str!("../../native/jni/jni_empty_array_elements.rs"), "\n", include_str!("../../native/jni/lambda_proxy_jni_construct.rs")).replace("\r\n", "\n");
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
    let exec = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
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
