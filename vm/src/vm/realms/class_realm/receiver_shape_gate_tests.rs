use super::*;
use crate::vm::SharedVm;
use crate::VmConfig;
use std::sync::Arc;

/// The range test in [`ClassRealm::is_lambda_proxy_class`] must never
/// answer "no" for an id that IS in the table, and must answer "no"
/// without touching the table for every ordinary class id. Both halves are
/// asserted: a range test that were merely conservative would be silently
/// wrong here, because the caller uses `true` to cede to the slow SAM route.
#[test]
fn is_lambda_proxy_class_agrees_with_the_table() {
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    // Every id the allocator hands out is inside the reserved range.
    let proxy = shared.alloc_lambda_proxy_id();
    assert!(
        proxy.as_u32() >= LAMBDA_PROXY_ID_BASE,
        "lambda proxy ids must come from the range the predicate tests"
    );

    // In range but not registered: the table stays the authority.
    assert!(!shared.classes.is_lambda_proxy_class(proxy));

    shared.classes.lambda_proxies.write().insert(
        proxy,
        Arc::new(LambdaCallSite {
            functional_interface_id: None,
            functional_interface: "java/lang/Runnable".into(),
            sam_method_name: "run".into(),
            sam_descriptor: "()V".into(),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: "test/A".into(),
                member_name: "lambda$0".into(),
                descriptor: "()V".into(),
            },
            instantiated_descriptor: "()V".into(),
            capture_types: vec![],
            proxy_class_id: proxy,
            serializable_flag: false,
        }),
    );
    assert!(shared.classes.is_lambda_proxy_class(proxy));
    // A registered proxy is initialized (it has no `<clinit>`), so
    // `Method.invoke` on a method of the lambda's own class never asks
    // `prepare_class_shared` for an id the class manager does not hold.
    assert!(crate::vm::vm_util::is_class_initialized_via_manager(
        &shared, proxy
    ));
    let unregistered = shared.alloc_lambda_proxy_id();
    assert!(!crate::vm::vm_util::is_class_initialized_via_manager(
        &shared,
        unregistered
    ));

    // Ordinary class ids are below the base and answer "no".
    for raw in [0u32, 1, 42, 65_535, LAMBDA_PROXY_ID_BASE - 1] {
        assert!(
            !shared.classes.is_lambda_proxy_class(ClassId::new(raw)),
            "class id {raw} is not a lambda proxy"
        );
    }

    // JIT round 20 wave 12 (lane qLam, W15L5-7): the hit above was
    // recorded for the lock-free path (when the switch is on), and a row
    // removed the way the unload paths remove one answers "no" again.
    if shared.classes.lambda_proxy_hit_bits.enabled {
        assert!(shared.classes.lambda_proxy_hit_bits.contains(proxy));
    }
    assert!(shared.classes.is_lambda_proxy_class(proxy));
    shared.classes.lambda_proxies.write().retain(|id, _| {
        let keep = *id != proxy;
        if !keep {
            shared.classes.forget_lambda_proxy_hit(*id);
        }
        keep
    });
    assert!(!shared.classes.lambda_proxy_hit_bits.contains(proxy));
    assert!(!shared.classes.is_lambda_proxy_class(proxy));
}

/// W15L5-7: the bitmap's own contract -- set by a note, cleared by a
/// forget, nothing outside the covered band, inert when switched off.
#[test]
fn lambda_proxy_hit_bits_note_and_forget() {
    let bits = LambdaProxyHitBits::new(true);
    let id = ClassId::new(LAMBDA_PROXY_ID_BASE + 70);
    let neighbour = ClassId::new(LAMBDA_PROXY_ID_BASE + 71);
    assert!(!bits.contains(id), "nothing is set before a note");
    bits.note(id);
    assert!(bits.contains(id));
    assert!(!bits.contains(neighbour), "one bit per id");
    bits.forget(id);
    assert!(!bits.contains(id));
    // Cast: the capacity is far below u32::MAX - LAMBDA_PROXY_ID_BASE.
    let past = ClassId::new(LAMBDA_PROXY_ID_BASE + LAMBDA_PROXY_HIT_BITS_CAPACITY as u32);
    bits.note(past);
    assert!(
        !bits.contains(past),
        "an id past the band keeps the locked lookup"
    );
    bits.note(ClassId::new(5));
    assert!(
        !bits.contains(ClassId::new(5)),
        "an id below the base is never a hit"
    );
    let off = LambdaProxyHitBits::new(false);
    off.note(id);
    assert!(!off.contains(id), "switched off, nothing is recorded");
    assert!(
        off.words.get().is_none(),
        "switched off, nothing is allocated"
    );
}

/// [`ClassRealm::is_annotation_proxy_class`] replaced a name comparison
/// under the class-manager lock with a `ClassId` identity test. The failure
/// mode that matters is the NEGATIVE one: the memo must not latch "absent"
/// from before the class was minted, because a stale `false` sends an
/// annotation proxy down a cached-target path that has no bytecode for it.
#[test]
fn is_annotation_proxy_class_learns_the_class_after_it_is_minted() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    // Before the class exists every id answers "no" -- and asking
    // repeatedly is exactly what would latch a wrong answer if the negative
    // memo were not keyed on the class-definition epoch.
    for _ in 0..4 {
        for raw in [0u32, 1, 7, 1234] {
            assert!(!shared.classes.is_annotation_proxy_class(ClassId::new(raw)));
        }
    }

    // Mint it the way `ensure_vm_internal_class` does.
    let proxy_cid = shared.classes.class_manager_write().ensure_generated_class(
        ANNOTATION_PROXY_CLASS,
        4,
        cratonvm_classloading::ClassOrigin::VmInternal,
    );

    assert!(
        shared.classes.is_annotation_proxy_class(proxy_cid),
        "the proxy class must be recognised once defined, even though every \
         earlier query answered `no`"
    );
    // Repeat reads take the memoized identity path and stay stable.
    assert!(shared.classes.is_annotation_proxy_class(proxy_cid));
    // ...and nothing else is the proxy.
    for raw in [0u32, 1, 7, 1234] {
        let other = ClassId::new(raw);
        if other != proxy_cid {
            assert!(!shared.classes.is_annotation_proxy_class(other));
        }
    }
}
