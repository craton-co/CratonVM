//! Review rev-w13 (JIT round 21 wave 14, item 1 of
//! `docs/feature-designs/real-jdk-mode.md`): both
//! define paths run `SharedVm::note_finalizable_define`, which records the
//! ancestor, drops its constructors' `jit_gate_pass` rows and is
//! idempotent.
use super::SharedVm;
use crate::config::VmConfig;
use std::sync::Arc;

#[test]
fn a_finalizable_define_bars_the_ancestor_once_from_either_listener() {
    let shared = SharedVm::new(VmConfig::default());
    let config = &shared.config;
    let Ok(leaf) = shared.load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
    else {
        return;
    };
    let base = {
        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();
        store
            .get(leaf)
            .and_then(|c| c.superclass)
            .filter(|&id| store.get(id).is_some_and(|b| !b.has_finalizer))
    };
    let Some(base) = base else {
        return; // Another JDK's hierarchy.
    };
    // Nothing to bar: no finalizer yet.
    shared.note_finalizable_define(leaf);
    assert!(!crate::runtime::interpreter::finalizer_bars_compiling(
        &shared, base, "<init>", "(I)V"
    ));
    shared
        .classes
        .class_manager
        .write()
        .class_store
        .get_mut(leaf)
        .expect("loaded")
        .has_finalizer = true;
    let stale = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::interpreter::finalizable_define_stale_ancestors(
            &shared.config,
            cm.class_store(),
            leaf,
        )
    };
    if !stale.iter().any(|(id, _)| *id == base) {
        return; // Another JDK's bodies: no constructor throws before init.
    }
    let row = (base, Arc::<str>::from("<init>"), Arc::<str>::from("(I)V"));
    shared.jit.jit_gate_pass.write().insert(row.clone(), (0, false));
    shared.note_finalizable_define(leaf);
    assert!(
        crate::runtime::interpreter::finalizer_bars_compiling(&shared, base, "<init>", "(I)V"),
        "the ancestor is recorded"
    );
    assert!(
        !shared.jit.jit_gate_pass.read().contains_key(&row),
        "its constructors' allowed answers are forgotten"
    );
    // The second listener (or a sibling's define) finds it recorded and
    // touches nothing: a row filled since stays.
    shared.jit.jit_gate_pass.write().insert(row.clone(), (0, false));
    shared.note_finalizable_define(leaf);
    assert!(shared.jit.jit_gate_pass.read().contains_key(&row));
    assert!(crate::runtime::interpreter::finalizer_bars_compiling(
        &shared, base, "<init>", "(I)V"
    ));
}

/// Source pin: the native define backend runs the listener after its
/// class-manager guard is gone.
#[test]
fn define_class_full_runs_the_finalizable_listener() {
    let src = concat!(include_str!("../vm_exec/mod.rs"), "\n", include_str!("../vm_exec/method.rs"), "\n", include_str!("../vm_exec/report_pad_access.rs"), "\n", include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../vm_exec/load_class.rs"), "\n", include_str!("../vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../vm_exec/new_object.rs"), "\n", include_str!("../vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../vm_exec/invoke_special_shared.rs"), "\n", include_str!("../vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    let start = src.find("    fn define_class_full(").expect("define_class_full");
    let body = &src[start..];
    let body = &body[..body.find("\n    }\n").expect("its end")];
    let release = body.find("drop(cm);\n            phase_end(DefinePhase::CmWriteRelease")
        .expect("the guard release");
    let listener = body
        .find("self.shared.note_finalizable_define(cid);")
        .expect("the listener");
    assert!(release < listener);
}

/// Source pin (JIT round 21 wave 17, item 3 of
/// `r21-review-w16-findings-FIXED-20261020.md`): the legacy JNI `DefineClass`
/// arm runs the listener too, after its class-manager guard is gone.
#[test]
fn the_legacy_jni_define_class_runs_the_finalizable_listener() {
    let src = concat!(include_str!("../../native/jni/mod.rs"), "\n", include_str!("../../native/jni/j_boolean.rs"), "\n", include_str!("../../native/jni/raw_locals_counted.rs"), "\n", include_str!("../../native/jni/truncate_local_frames.rs"), "\n", include_str!("../../native/jni/jni_empty_array_elements.rs"), "\n", include_str!("../../native/jni/lambda_proxy_jni_construct.rs")).replace("\r\n", "\n");
    let start = src.find("extern \"C\" fn jni_define_class(").expect("jni_define_class");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    let release = body.find("drop(cm);").expect("the guard release");
    let listener = body
        .find("shared.note_finalizable_define(cid);")
        .expect("the listener");
    assert!(release < listener);
}

/// JIT round 21 wave 23, lane finvm (item 8 of
/// `docs/feature-designs/real-jdk-mode.md`): a stub
/// upgrade's flip drops the moved class's published allocation recipe in
/// both directions, and a class that became finalizable runs the
/// define-time listener (its ancestor is barred); one that stopped records
/// nothing, as the define of a non-finalizable class does.
#[test]
fn a_stub_upgrade_flip_drops_the_recipe_and_runs_the_listener() {
    let shared = SharedVm::new(VmConfig::default());
    let config = &shared.config;
    let Ok(leaf) = shared.load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
    else {
        return;
    };
    let base = {
        let cm = shared.classes.class_manager.read();
        let store = cm.class_store();
        store
            .get(leaf)
            .and_then(|c| c.superclass)
            .filter(|&id| store.get(id).is_some_and(|b| !b.has_finalizer))
    };
    let Some(base) = base else {
        return; // Another JDK's hierarchy.
    };
    let publish = |shared: &SharedVm| {
        let recipe = {
            let cm = shared.classes.class_manager.read();
            crate::jit::alloc_class_cache::ClassAllocInfo::build(&cm.class_store, leaf)
        };
        recipe
            .and_then(|r| shared.jit.jit_alloc_class_cache.insert(leaf.as_u32(), r))
            .is_some()
    };
    // The reverse direction: the recipe goes and nothing is recorded.
    let published = publish(&shared);
    shared.note_stub_upgrade_finalizer_flips(&[(leaf, false)]);
    if published {
        assert!(shared.jit.jit_alloc_class_cache.get(leaf.as_u32()).is_none());
    }
    assert!(!crate::runtime::interpreter::finalizer_bars_compiling(
        &shared, base, "<init>", "(I)V"
    ));
    // A recipe published with the old (non-finalizable) bit, then the
    // upgrade moves the bit.
    let published = publish(&shared);
    shared
        .classes
        .class_manager
        .write()
        .class_store
        .get_mut(leaf)
        .expect("loaded")
        .has_finalizer = true;
    let stale = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::interpreter::finalizable_define_stale_ancestors(
            &shared.config,
            cm.class_store(),
            leaf,
        )
    };
    shared.note_stub_upgrade_finalizer_flips(&[(leaf, true)]);
    if published {
        assert!(
            shared.jit.jit_alloc_class_cache.get(leaf.as_u32()).is_none(),
            "the stale recipe is unpublished"
        );
        assert!(publish(&shared));
        assert!(
            shared
                .jit
                .jit_alloc_class_cache
                .get(leaf.as_u32())
                .is_some_and(|info| info.has_finalizer),
            "the next build reads the new bit"
        );
    }
    if stale.iter().any(|(id, _)| *id == base) {
        assert!(
            crate::runtime::interpreter::finalizer_bars_compiling(
                &shared, base, "<init>", "(I)V"
            ),
            "the listener recorded the ancestor"
        );
    }
}

/// JIT round 21 wave 23 follow-up (item 1 of
/// `r21-review-w23-findings-FIXED-20261020.md`): a class a stub upgrade made
/// finalizable loses its OWN constructors' allowed `jit_gate_pass` rows
/// (other methods' rows stay); a class that stopped keeps them; the
/// execute gate's fill re-reads the live bar after its insert.
#[test]
fn a_class_made_finalizable_by_an_upgrade_forgets_its_own_constructor_rows() {
    let shared = SharedVm::new(VmConfig::default());
    let Ok(leaf) = shared.load_class_concurrent("java/lang/ArrayIndexOutOfBoundsException")
    else {
        return;
    };
    let ctor = (leaf, Arc::<str>::from("<init>"), Arc::<str>::from("(I)V"));
    let other = (leaf, Arc::<str>::from("getMessage"), Arc::<str>::from("()Ljava/lang/String;"));
    let fill = |shared: &SharedVm| {
        let mut memo = shared.jit.jit_gate_pass.write();
        memo.insert(ctor.clone(), (0, false));
        memo.insert(other.clone(), (0, false));
    };
    // Stopped being finalizable: nothing is forgotten.
    fill(&shared);
    shared.note_stub_upgrade_finalizer_flips(&[(leaf, false)]);
    assert!(shared.jit.jit_gate_pass.read().contains_key(&ctor));
    // Became finalizable: the constructor row goes, the other stays.
    shared
        .classes
        .class_manager
        .write()
        .class_store
        .get_mut(leaf)
        .expect("loaded")
        .has_finalizer = true;
    fill(&shared);
    shared.note_stub_upgrade_finalizer_flips(&[(leaf, true)]);
    let memo = shared.jit.jit_gate_pass.read();
    assert!(!memo.contains_key(&ctor), "its own constructor row is forgotten");
    assert!(memo.contains_key(&other), "a non-constructor row stays");
    drop(memo);
    // The fill's re-read covers a row inserted after that drop.
    let src = concat!(include_str!("../../runtime/interpreter/mod.rs"), "\n", include_str!("../../runtime/interpreter/cached_invoke_target.rs"), "\n", include_str!("../../runtime/interpreter/execute.rs"), "\n", include_str!("../../runtime/interpreter/resume_continuation.rs"), "\n", include_str!("../../runtime/interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../runtime/interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../runtime/interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../runtime/interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let reread = src
        .find("finalizer_ancestor_recorded(shared, class_id)\n")
        .expect("the fill's re-read");
    assert!(
        src[reread..reread + 200]
            .contains("|| finalizer_bars_compiling(shared, class_id, method_name, method_descriptor)"),
        "the re-read asks the live bar too"
    );
}

/// JIT round 21 wave 23, lane finvm: the class-manager write guard's
/// release takes a stub upgrade's queued flips and settles them (the
/// subclass's recipe is gone and the queue is empty afterwards).
#[test]
fn the_write_guard_release_settles_a_stub_upgrades_finalizer_flips() {
    // The release settles only for the registered owner of the store.
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    super::set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
    let (base, sub) = {
        let mut cm = shared.classes.class_manager_write();
        let Ok(base) = cm.try_ensure_synthetic_class("fz23vm/Base", 0) else {
            return; // A strict mode fabricates nothing.
        };
        let Ok(sub) = cm.try_ensure_synthetic_class("fz23vm/Sub", 0) else {
            return;
        };
        cm.set_superclass(sub, Some(base));
        (base, sub)
    };
    let _ = shared.jit.jit_alloc_class_cache.insert(
        sub.as_u32(),
        crate::jit::alloc_class_cache::ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: Vec::new().into_boxed_slice(),
            ref_inits: Vec::new().into_boxed_slice(),
        },
    );
    // The temporary guard is released at the end of the statement.
    let defined = shared.classes.class_manager_write().define_class(
        "fz23vm/Base",
        &r21w23_finalizable_base_bytes(),
        crate::classloading::ClassLoaderId::Application,
    );
    if defined.ok() != Some(base) {
        eprintln!("skipped: this VM did not upgrade the fz23vm/Base stub in place");
        return;
    }
    {
        let cm = shared.classes.class_manager.read();
        assert!(cm.class_store().get(base).is_some_and(|c| c.has_finalizer));
        assert!(cm.class_store().get(sub).is_some_and(|c| c.has_finalizer));
    }
    assert!(
        shared.jit.jit_alloc_class_cache.get(sub.as_u32()).is_none(),
        "the subclass's recipe is unpublished"
    );
    assert!(
        shared
            .classes
            .class_manager
            .write()
            .take_stub_upgrade_finalizer_flips()
            .is_empty(),
        "the guard's release took the queue"
    );
}

/// JIT round 21 wave 25, lane rawwrite (item 3 of
/// `r21-review-w24-findings-FIXED-20261020.md`,
/// `CRATONVM_RAW_WRITE_SETTLES_FINALIZER_FLIPS`): a stub upgrade under the
/// settling write lock (`ClassRealm::class_manager_write_settling`, the
/// former raw `class_manager.write()` sites) is settled at that lock's own
/// release; under `=0` its rows still wait for a guarded release.
#[test]
fn the_settling_write_release_settles_a_stub_upgrades_finalizer_flips() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    super::set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
    let (base, sub) = {
        let mut cm = shared.classes.class_manager_write();
        let Ok(base) = cm.try_ensure_synthetic_class("fz23vm/Base", 0) else {
            return; // A strict mode fabricates nothing.
        };
        let Ok(sub) = cm.try_ensure_synthetic_class("fz23vm/Sub", 0) else {
            return;
        };
        cm.set_superclass(sub, Some(base));
        (base, sub)
    };
    let _ = shared.jit.jit_alloc_class_cache.insert(
        sub.as_u32(),
        crate::jit::alloc_class_cache::ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: Vec::new().into_boxed_slice(),
            ref_inits: Vec::new().into_boxed_slice(),
        },
    );
    // The temporary settling guard is released at the end of the statement.
    let defined = shared.classes.class_manager_write_settling().define_class(
        "fz23vm/Base",
        &r21w23_finalizable_base_bytes(),
        crate::classloading::ClassLoaderId::Application,
    );
    if defined.ok() != Some(base) {
        eprintln!("skipped: this VM did not upgrade the fz23vm/Base stub in place");
        return;
    }
    // Taken raw: a raw release settles nothing, so this sees the queue as
    // the settling release left it.
    let left = shared
        .classes
        .class_manager
        .write()
        .take_stub_upgrade_finalizer_flips();
    if shared.classes.raw_write_settles_finalizer_flips {
        assert!(left.is_empty(), "the settling release took the queue");
        assert!(
            shared.jit.jit_alloc_class_cache.get(sub.as_u32()).is_none(),
            "the subclass's recipe is unpublished"
        );
    } else {
        assert!(
            left.iter().any(|&(id, now)| id == sub && now),
            "`=0`: the rows wait for a guarded release"
        );
    }
}

/// `public class fz23vm/Base` with a non-empty `finalize()V`
/// (`iconst_0; pop; return`).
fn r21w23_finalizable_base_bytes() -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn class(out: &mut Vec<u8>, name_index: u16) {
        out.push(7);
        out.extend_from_slice(&name_index.to_be_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, "fz23vm/Base"); // #1
    class(&mut b, 1); // #2
    utf8(&mut b, "java/lang/Object"); // #3
    class(&mut b, 3); // #4
    utf8(&mut b, "finalize"); // #5
    utf8(&mut b, "()V"); // #6
    utf8(&mut b, "Code"); // #7
    b.extend_from_slice(&0x0021u16.to_be_bytes()); // public super
    b.extend_from_slice(&2u16.to_be_bytes()); // this_class
    b.extend_from_slice(&4u16.to_be_bytes()); // super_class
    b.extend_from_slice(&0u16.to_be_bytes()); // interfaces
    b.extend_from_slice(&0u16.to_be_bytes()); // fields
    b.extend_from_slice(&1u16.to_be_bytes()); // methods
    b.extend_from_slice(&0x0004u16.to_be_bytes()); // protected
    b.extend_from_slice(&5u16.to_be_bytes()); // finalize
    b.extend_from_slice(&6u16.to_be_bytes()); // ()V
    b.extend_from_slice(&1u16.to_be_bytes()); // attributes_count
    b.extend_from_slice(&7u16.to_be_bytes()); // Code
    b.extend_from_slice(&15u32.to_be_bytes()); // attribute_length
    b.extend_from_slice(&1u16.to_be_bytes()); // max_stack
    b.extend_from_slice(&1u16.to_be_bytes()); // max_locals
    b.extend_from_slice(&3u32.to_be_bytes()); // code_length
    b.extend_from_slice(&[0x03, 0x57, 0xB1]); // iconst_0; pop; return
    b.extend_from_slice(&0u16.to_be_bytes()); // exception_table_length
    b.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
    b.extend_from_slice(&0u16.to_be_bytes()); // class attributes
    b
}
