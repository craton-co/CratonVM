use super::elidable_ctor_native_would_run;
use crate::vm::SharedVm;
use cratonvm_native_api::NativeKind;
use cratonvm_types::compat::CompatibilityMode;

/// Not a real native — never invoked by these tests, which only ask the
/// POLICY question. A registration needs a callback, so this is one.
fn stub(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[crate::types::Value],
) -> Result<Option<crate::types::Value>, cratonvm_types::error::MethodCallFailed> {
    Ok(None)
}

fn vm_with(mode: CompatibilityMode, kind: NativeKind) -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = mode;
    // `JdkOnly` + the synthetic library is a rejected pair (the synthetic
    // library IS ~5,200 synthetic stubs), and `VmConfig::default()` selects
    // the synthetic library. Turn it off for BOTH arms rather than only the
    // strict one, so the two VMs differ in exactly the variable under test.
    config.use_synthetic_jdk = false;
    let mut vm = SharedVm::new(config);
    vm.natives.native_methods.register(
        "cratonvm/test/ElidableCtorFixture",
        "<init>",
        "()V",
        stub,
        kind,
    );
    vm
}

/// Concrete bytecode wins over a Bridge in both policies. Constructor
/// elision may therefore skip the bridge; a reviewed Intrinsic still wins
/// in both policies and must prevent elision.
#[test]
fn ctor_elision_asks_policy_not_just_registration() {
    const FIXTURE: &str = "cratonvm/test/ElidableCtorFixture";

    let compat_bridge = vm_with(CompatibilityMode::Compatible, NativeKind::Bridge);
    assert!(
        !elidable_ctor_native_would_run(&compat_bridge, FIXTURE),
        "Compatible executes the concrete constructor instead of its Bridge"
    );

    let strict_bridge = vm_with(CompatibilityMode::JdkOnly, NativeKind::Bridge);
    assert!(
        !elidable_ctor_native_would_run(&strict_bridge, FIXTURE),
        "JdkOnly sends a Bridge standing in front of concrete bytecode to the \
         bytecode (contract §7 step 3), so nothing is skipped by eliding and \
         the old blanket refusal was pessimism"
    );

    let strict_intrinsic = vm_with(CompatibilityMode::JdkOnly, NativeKind::Intrinsic);
    assert!(
        elidable_ctor_native_would_run(&strict_intrinsic, FIXTURE),
        "an Intrinsic is §1.4's reviewed exception and still runs under strict \
         policy, so eliding its constructor would skip it"
    );

    let compat_intrinsic = vm_with(CompatibilityMode::Compatible, NativeKind::Intrinsic);
    assert!(elidable_ctor_native_would_run(&compat_intrinsic, FIXTURE));
}

/// A class with NO registered `<init>()V` is answered `false` in both
/// modes — otherwise the predicate would refuse every elision and read as
/// working while doing nothing.
#[test]
fn an_unregistered_ctor_never_blocks_elision() {
    for mode in [CompatibilityMode::Compatible, CompatibilityMode::JdkOnly] {
        let vm = vm_with(mode, NativeKind::Bridge);
        assert!(
            !elidable_ctor_native_would_run(&vm, "cratonvm/test/NoSuchFixture"),
            "{mode:?}: an unregistered triple must not block elision"
        );
    }
}
