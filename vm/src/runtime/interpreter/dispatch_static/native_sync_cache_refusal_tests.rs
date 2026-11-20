use super::native_sync_site_may_cache_native;
use crate::vm::SharedVm;
use cratonvm_native_api::NativeKind;
use cratonvm_types::compat::CompatibilityMode;

fn stub(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[crate::types::Value],
) -> Result<Option<crate::types::Value>, cratonvm_types::error::MethodCallFailed> {
    Ok(None)
}

/// The native-sync cache refusal applies only where a native entry could
/// be built: an interpreter-intrinsic pair, or a registry row keyed by a
/// class on the dispatch class's superclass chain. A synchronized method
/// whose (name, descriptor) only an UNRELATED class registers can only be
/// cached as bytecode, whose hit path takes the monitor, so it must stay
/// cacheable.
#[test]
fn only_a_pair_that_could_be_cached_as_a_native_is_refused() {
    const OWNER: &str = "cratonvm/test/NativeSyncOwner";
    const SUB: &str = "cratonvm/test/NativeSyncSub";
    const UNRELATED: &str = "cratonvm/test/NativeSyncUnrelated";
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    shared.natives.native_methods.register(
        OWNER,
        "cratonvmNativeSyncProbe",
        "()V",
        stub,
        NativeKind::Bridge,
    );
    let (owner, sub, unrelated) = {
        let mut cm = shared.classes.class_manager.write();
        let owner = cm.try_ensure_synthetic_class(OWNER, 0).expect("fabricated");
        let sub = cm.try_ensure_synthetic_class(SUB, 0).expect("fabricated");
        let unrelated = cm
            .try_ensure_synthetic_class(UNRELATED, 0)
            .expect("fabricated");
        cm.set_superclass(sub, Some(owner));
        (owner, sub, unrelated)
    };
    let cm = shared.classes.class_manager.read();
    let may = |start, name: &str, desc: &str| {
        native_sync_site_may_cache_native(&shared, &cm, start, name, desc)
    };
    assert!(
        !may(owner, "cratonvmNativeSyncProbeNeverRegistered", "(IJ)V"),
        "a pair no native carries can only become a bytecode entry"
    );
    assert!(
        may(owner, "cratonvmNativeSyncProbe", "()V"),
        "the owner's own row"
    );
    assert!(
        may(sub, "cratonvmNativeSyncProbe", "()V"),
        "an ancestor's row"
    );
    assert!(
        !may(unrelated, "cratonvmNativeSyncProbe", "()V"),
        "a row on a class outside the chain cannot become this site's entry"
    );
    assert!(
        may(unrelated, "hashCode", "()I"),
        "an interpreter-intrinsic pair may be cached as an Intrinsic entry"
    );
}
