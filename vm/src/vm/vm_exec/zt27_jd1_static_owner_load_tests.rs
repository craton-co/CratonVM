use super::{load_static_owner_before_dispatch, static_owner_load_pending, SharedVm};
use crate::error::MethodCallResult;
use crate::native::registry::NativeContext;
use crate::runtime::interpreter::StaticOwnerLoad;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::Value;
use cratonvm_native_api::NativeKind;

fn noop(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// A VM with `owner.m()V` registered as `kind` and `owner` loaded by no
/// loader.
fn vm(jdk_only: bool, owner: &str, kind: NativeKind) -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    if jdk_only {
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        // `JdkOnly` with the synthetic library is a rejected pair.
        config.use_synthetic_jdk = false;
    }
    let mut vm = SharedVm::new(config);
    vm.natives
        .native_methods
        .register(owner, "m", "()V", noop, kind);
    assert!(
        vm.classes
            .class_manager
            .read()
            .get_loaded_class_id(owner)
            .is_none(),
        "the fixture's owner must be absent"
    );
    vm
}

/// `--compatible`, a `Bridge` on an absent owner: the interpreter's
/// native route loads it when absent (`load_absent_native_static_owner`),
/// so a compiled call owes that load. An `Intrinsic` owes nothing: it wins
/// on a loaded owner too.
#[test]
fn a_compatible_bridge_owner_is_owed_the_absent_native_load() {
    const OWNER: &str = "cratonvm/zt27jd1/BridgeOwner";
    let shared = vm(false, OWNER, NativeKind::Bridge);
    assert_eq!(
        static_owner_load_pending(&shared, OWNER, "m", "()V"),
        Some(StaticOwnerLoad::AbsentNative)
    );
    assert_eq!(
        static_owner_load_pending(&shared, OWNER, "other", "()V"),
        None
    );
    let shared = vm(false, OWNER, NativeKind::Intrinsic);
    assert_eq!(static_owner_load_pending(&shared, OWNER, "m", "()V"), None);
}

/// `--compatible`, a `SyntheticStub` on a `real_protected_stub_class`
/// owner (the `getPlatformMXBean` shape of row `df7_nl_c`): the
/// interpreter takes the full owner route, so a compiled call owes it.
#[test]
fn a_compatible_real_protected_stub_owner_is_owed_the_full_load() {
    const OWNER: &str = "java/lang/management/ManagementFactory";
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    vm.natives.native_methods.register(
        OWNER,
        "zt27jd1Probe",
        "()V",
        noop,
        NativeKind::SyntheticStub,
    );
    if vm
        .natives
        .native_methods
        .kind_of(OWNER, "zt27jd1Probe", "()V")
        != Some(NativeKind::SyntheticStub)
    {
        // A registry that drops stubs on this image keeps nothing to owe.
        assert_eq!(
            static_owner_load_pending(&vm, OWNER, "zt27jd1Probe", "()V"),
            None
        );
        return;
    }
    if vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(OWNER)
        .is_some()
    {
        // A boot image that loads the class up front owes nothing.
        assert_eq!(
            static_owner_load_pending(&vm, OWNER, "zt27jd1Probe", "()V"),
            None
        );
        return;
    }
    assert_eq!(
        static_owner_load_pending(&vm, OWNER, "zt27jd1Probe", "()V"),
        Some(StaticOwnerLoad::Full)
    );
}

/// `--real-jdk`, a `Bridge` on an absent owner: every ordinary owner takes
/// the full route there.
#[test]
fn a_jdk_only_bridge_owner_is_owed_the_full_load() {
    const OWNER: &str = "cratonvm/zt27jd1/StrictBridgeOwner";
    let shared = vm(true, OWNER, NativeKind::Bridge);
    assert_eq!(
        static_owner_load_pending(&shared, OWNER, "m", "()V"),
        Some(StaticOwnerLoad::Full)
    );
}

/// The kill switch (`=0`): nothing is owed, the doors decide on the absent
/// owner as before.
#[test]
fn the_kill_switch_owes_nothing() {
    const OWNER: &str = "cratonvm/zt27jd1/KilledOwner";
    let mut shared = vm(false, OWNER, NativeKind::Bridge);
    shared.jit_static_owner_loads_first = false;
    assert_eq!(static_owner_load_pending(&shared, OWNER, "m", "()V"), None);
}

/// A name with no class file: the load fails once, is remembered for this
/// VM, and is not owed again (the site then caches its native instead of
/// scanning the class path on every call). The arguments survive the load.
#[test]
fn a_failed_owner_load_is_not_owed_again() {
    const OWNER: &str = "cratonvm/zt27jd1/NoClassFileOwner";
    let shared = vm(false, OWNER, NativeKind::Bridge);
    let mut thread = JvmThread::new(ThreadId(0), "zt27-jd1");
    let mut args = [Value::Int(7)];
    load_static_owner_before_dispatch(&shared, &mut thread, OWNER, "m", "()V", &mut args);
    assert!(matches!(args[0], Value::Int(7)));
    if shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(OWNER)
        .is_none()
    {
        assert!(shared.jit_static_owner_load_refused.lock().contains(OWNER));
        assert_eq!(static_owner_load_pending(&shared, OWNER, "m", "()V"), None);
    }
}

/// Text pins: the native site cache's fill declines (uncached) while an
/// absent owner is owed its load, the static tail loads it before the
/// owner initialization and the by-name dispatch, and the interpreter's
/// static door and the route share `static_owner_direct_native`.
#[test]
fn the_compiled_static_doors_load_the_owner_before_deciding() {
    let jit = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let fill = jit
        .find(concat!("\npub fn static_native_site", "_init_pending("))
        .expect("the fill gate");
    let fb = &jit[fill..];
    let fb = &fb[..fb.find("\n\u{7d}\n").expect("its end")];
    assert!(fb.contains(concat!("crate::vm::static_owner", "_load_pending(")));
    let load = jit
        .find(concat!("crate::vm::load_static_owner", "_before_dispatch("))
        .expect("the tail load");
    let rest = &jit[load..];
    let init = rest
        .find(concat!("jit_static_owner_init", "_before_native("))
        .expect("the owner initialization after it");
    let tail = rest
        .find(concat!("crate::vm::invoke_static_or_native", "_prepared("))
        .expect("the by-name tail after it");
    assert!(init < tail);
    let interp =
        include_str!("../../runtime/interpreter/dispatch_static.rs").replace("\r\n", "\n");
    let s = interp
        .find(concat!("pub(super) fn execute", "_invokestatic("))
        .expect("the slow path");
    let b = &interp[s..];
    let b = &b[..b.find("\n\u{7d}\n").expect("its end")];
    assert!(b.contains(concat!("static_owner_direct", "_native(")));
    let r = interp
        .find(concat!("pub(crate) fn static_owner", "_load_route("))
        .expect("the route");
    let rb = &interp[r..];
    let rb = &rb[..rb.find("\n\u{7d}\n").expect("its end")];
    assert!(rb.contains(concat!("static_owner_direct", "_native(")));
    assert!(rb.contains(concat!("resolve_static_owner", "_fully(")));
}
