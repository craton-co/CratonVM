use super::*;
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::ClassLoaderId;
use std::sync::Arc;

const OWNER: &str = "cratonvm/zt29jd/SplitNativeOwner";
const FORK: ClassLoaderId = ClassLoaderId::UserDefined(29_029);

fn noop(
    _: &mut dyn crate::native::registry::NativeContext,
    _: &[Value],
) -> crate::error::MethodCallResult {
    Ok(None)
}

/// `static native m()V`: no `Code`, as the JDK's own declaration.
fn native_m() -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from("m"),
        descriptor: Arc::from("()V"),
        access_flags: MethodAccessFlags::PUBLIC
            | MethodAccessFlags::STATIC
            | MethodAccessFlags::NATIVE,
        attributes: vec![],
    }
}

/// `static m()V { return; }`: the isolated loader's copy's Java body.
fn bytecode_m() -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from("m"),
        descriptor: Arc::from("()V"),
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

fn info(kind: u8, caller: ClassId) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: OWNER,
        method_name: "m",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: kind,
        declaring_class_id: caller.as_u32(),
        owner_class_id: 0,
    }
}

/// A `Bridge` on `OWNER.m()V`; the application loader's `OWNER` (what
/// the loader-blind name table answers) declares `m` native, an isolated
/// loader's `OWNER` gives it a Java body. Built as
/// `a_compiled_invokestatic_resolves_its_owner_in_the_callers_own_loader`
/// builds its two copies. Returns `(vm, app_copy, fork_copy, fork_caller,
/// app_caller)`.
fn two_copies() -> (SharedVm, ClassId, ClassId, ClassId, ClassId) {
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    vm.natives.native_methods.register(
        OWNER,
        "m",
        "()V",
        noop,
        cratonvm_native_api::NativeKind::Bridge,
    );
    let ids = {
        let mut cm = vm.classes.class_manager.write();
        let app_copy = cm
            .try_ensure_synthetic_class(OWNER, 0)
            .expect("Compatible mode fabricates");
        cm.class_store
            .get_mut(app_copy)
            .expect("fabricated")
            .methods
            .push(native_m());
        let fork_copy = cm
            .try_ensure_synthetic_class("cratonvm/zt29jd/SplitNativeOwner$Fork", 0)
            .expect("Compatible mode fabricates");
        {
            let c = cm.class_store.get_mut(fork_copy).expect("fabricated");
            c.name = cratonvm_types::intern_arc(OWNER);
            c.loader_id = FORK;
            c.methods.push(bytecode_m());
        }
        cm.register_class_name(FORK, OWNER, fork_copy);
        let fork_caller = cm
            .try_ensure_synthetic_class("cratonvm/zt29jd/ForkCaller", 0)
            .expect("Compatible mode fabricates");
        cm.class_store
            .get_mut(fork_caller)
            .expect("fabricated")
            .loader_id = FORK;
        cm.register_class_name(FORK, "cratonvm/zt29jd/ForkCaller", fork_caller);
        let app_caller = cm
            .try_ensure_synthetic_class("cratonvm/zt29jd/AppCaller", 0)
            .expect("Compatible mode fabricates");
        (app_copy, fork_copy, fork_caller, app_caller)
    };
    assert_eq!(
        vm.classes.class_manager.read().get_loaded_class_id(OWNER),
        Some(ids.0),
        "the name table must answer the application copy, or this measures nothing"
    );
    (vm, ids.0, ids.1, ids.2, ids.3)
}

fn admitted(vm: &SharedVm, owner: Option<ClassId>) -> bool {
    let id = vm.natives.native_methods.resolve_id(OWNER, "m", "()V");
    assert!(id.is_some(), "the Bridge is registered");
    admit_jit_fast_native_on_owner(vm, owner, OWNER, "m", "()V", noop, id).is_some()
}

/// The resolver question itself: on the isolated copy (Java body) the
/// `Bridge` yields; by name (the application copy, `native`) it runs.
#[test]
fn the_bridge_is_decided_on_the_owner_it_is_given() {
    let (vm, app_copy, fork_copy, _, _) = two_copies();
    assert!(!admitted(&vm, Some(fork_copy)), "the fork's Java body wins");
    assert!(
        admitted(&vm, Some(app_copy)),
        "the native declaration keeps it"
    );
    assert!(admitted(&vm, None), "by name: the application copy");
}

/// The owner a static or special site asks on: the caller's loader's
/// copy, so a site inside the isolated loader yields where the
/// interpreter runs the copy's body; a caller of the application loader
/// decides as by name.
#[test]
fn a_static_site_asks_on_the_callers_loaders_copy() {
    let (vm, app_copy, fork_copy, fork_caller, app_caller) = two_copies();
    for kind in [1u8, 3] {
        let owner =
            native_site_admission_owner(&vm, &info(kind, fork_caller), None, None, OWNER);
        assert_eq!(owner, Some(fork_copy), "kind {kind}");
        assert!(!admitted(&vm, owner), "kind {kind}");
        let owner =
            native_site_admission_owner(&vm, &info(kind, app_caller), None, None, OWNER);
        assert_ne!(owner, Some(fork_copy), "kind {kind}");
        assert!(owner.is_none() || owner == Some(app_copy), "kind {kind}");
        assert!(admitted(&vm, owner), "kind {kind}");
    }
}

/// A virtual site: the walk's ancestor, else the receiver's own class; an
/// id whose class is not named by the registration is never asked on.
#[test]
fn a_virtual_site_asks_on_its_receiver_or_ancestor_named_by_the_registration() {
    let (vm, _, fork_copy, fork_caller, app_caller) = two_copies();
    let site = info(0, app_caller);
    assert_eq!(
        native_site_admission_owner(&vm, &site, Some(fork_copy), None, OWNER),
        Some(fork_copy)
    );
    assert_eq!(
        native_site_admission_owner(&vm, &site, Some(app_caller), Some(fork_copy), OWNER),
        Some(fork_copy)
    );
    assert_eq!(
        native_site_admission_owner(&vm, &site, Some(fork_caller), None, OWNER),
        None,
        "a receiver of another name decides by name"
    );
    assert_eq!(
        native_site_admission_owner(&vm, &site, None, None, OWNER),
        None
    );
}

/// `CRATONVM_JIT_NATIVE_SITE_OWNER_BY_ID=0`: no owner, by name as before
/// (the fork's site keeps the application copy's native).
#[test]
fn the_kill_switch_decides_by_name() {
    let (mut vm, _, _, fork_caller, _) = two_copies();
    vm.jit_native_site_owner_by_id = false;
    let owner = native_site_admission_owner(&vm, &info(3, fork_caller), None, None, OWNER);
    assert_eq!(owner, None);
    assert!(admitted(&vm, owner));
}

/// Text pin: the fill passes the owner to the admission.
#[test]
fn the_fill_admits_on_the_site_owner() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let fill = src
        .find(concat!("\npub fn resolve_native_site", "_tracked("))
        .expect("the fill");
    let body = &src[fill..];
    let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains(concat!("native_site_admission", "_owner(")));
    assert!(body.contains(concat!("admit_jit_fast_native", "_on_owner(")));
}
