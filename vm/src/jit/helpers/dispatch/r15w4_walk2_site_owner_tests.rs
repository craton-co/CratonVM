use super::*;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

fn noop_native(
    _ctx: &mut dyn cratonvm_native_api::NativeContext,
    _args: &[Value],
) -> crate::error::MethodCallResult {
    Ok(None)
}

/// A version-52 class `name` whose one method `static value()I` is
/// `ldc #8; ireturn`, `#8` being `CONSTANT_Integer constant`.
fn ldc_class(name: &str, constant: i32) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&constant.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    let code = [0x12, 8, 0xac]; // ldc #8; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

fn define(shared: &SharedVm, name: &str) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            name,
            &ldc_class(name, 1),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .ok()
}

/// What `redefine_class_with` does, in its order: the class's generation
/// under the writer, then the VM's redefine epoch. `false` when this
/// build's class manager refuses the redefinition.
fn redefine(shared: &SharedVm, cid: ClassId, name: &str) -> bool {
    let redefined = shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(name, 2), RedefineOptions::default())
        .is_ok();
    shared.jit.jit_cache.bump_redefine_epoch();
    redefined
}

fn stamp_of(key: JitSiteKey) -> Option<u32> {
    NATIVE_SITE_CACHE.with(|c| {
        c.borrow()
            .get(&key)
            .and_then(|(_, entry)| entry.as_ref().map(|e| e.fill_redefine_epoch))
    })
}

/// An entry serving an ancestor's native survives (re-stamped) the
/// redefinition of another class, and is dropped by its owner's.
#[test]
fn a_site_serving_an_ancestors_native_notices_the_owners_redefinition() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (Some(owner), Some(other)) = (
        define(&shared, "r15w4walk2/SiteOwner"),
        define(&shared, "r15w4walk2/SiteOther"),
    ) else {
        // The stripped test VM could not define them: nothing to run.
        return;
    };
    let key = jit_site_key(shared.vm_identity, 0x5157_2400);
    let generation = shared.natives.native_methods.generation();
    let epoch = shared.jit.jit_cache.redefine_epoch();
    let entry = NativeSiteCache {
        kind: LeafNativeKind::Callback,
        leaf: true,
        callback: noop_native,
        native_id: None,
        receiver_class_id: Some(0),
        capability_kind: None,
        desc: NativeSiteDescriptor::decode("()I"),
        poly: false,
        vh_value_site: 0,
        sync: None,
        native_owner: Some((owner, 0)),
        fill_redefine_epoch: epoch,
        owner_scoped: false,
        refused_for_receiver: false,
    };
    NATIVE_SITE_CACHE.with(|c| {
        c.borrow_mut().insert(key, (generation, Some(entry)));
    });
    if !redefine(&shared, other, "r15w4walk2/SiteOther") {
        return;
    }
    let moved = shared.jit.jit_cache.redefine_epoch();
    assert_ne!(moved, epoch, "the redefinition moved the epoch");
    assert!(native_site_owner_still_current(
        &shared, key, generation, owner, 0
    ));
    assert_eq!(stamp_of(key), Some(moved), "the entry was re-stamped");
    if !redefine(&shared, owner, "r15w4walk2/SiteOwner") {
        return;
    }
    assert!(!native_site_owner_still_current(
        &shared, key, generation, owner, 0
    ));
    assert!(
        NATIVE_SITE_CACHE.with(|c| c.borrow().get(&key).is_none()),
        "the owner's redefinition drops the entry"
    );
}

/// Round 15 wave 4 (lane walk3): `site_native_owner_scope` names the
/// class that declares the method as a static site's owner and keeps its
/// native until that class is redefined (its body wins then); an
/// ancestor owner is scoped only where the probe lands on it.
#[test]
fn the_owner_scope_names_the_declaring_class_and_refuses_its_woven_body() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.natives.native_methods.ancestor_redefine_rule() {
        return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the rule is not applied
    }
    let (Some(cid), Some(other)) = (
        define(&shared, "r15w4walk3/OwnStatic"),
        define(&shared, "r15w4walk3/OwnOther"),
    ) else {
        return;
    };
    let info = |method: &'static str, desc: &'static str| JitInvokeInfo {
        class_name: "r15w4walk3/OwnStatic",
        method_name: method,
        descriptor: desc,
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: cid.as_u32(),
        owner_class_id: 0,
    };
    let value = info("value", "()I");
    let mut owner = None;
    assert_eq!(
        site_native_owner_scope(&shared, &value, None, &mut owner),
        Some(true)
    );
    assert_eq!(
        owner,
        Some((cid, 0)),
        "the declaring class, at generation 0"
    );
    // An ancestor owner (preset, as the walk sets it): scoped only where
    // the funnel's probe lands on it.
    let mut ancestor = Some((cid, 0));
    assert_eq!(
        site_native_owner_scope(&shared, &value, None, &mut ancestor),
        Some(true)
    );
    // Round 16 (lane redefine): a landing other than the registering
    // ancestor is the owner (the funnel's probe reads it), judged now.
    let mut elsewhere = Some((other, 0));
    assert_eq!(
        site_native_owner_scope(&shared, &value, None, &mut elsewhere),
        Some(true)
    );
    assert_eq!(elsewhere, Some((cid, 0)), "the landing owns the entry");
    // Declared nowhere: an ancestor's native has nothing to weave over,
    // the start class's own native keeps the permanent tests.
    let absent = info("absent", "()V");
    let mut ancestor = Some((other, 0));
    assert_eq!(
        site_native_owner_scope(&shared, &absent, None, &mut ancestor),
        Some(true)
    );
    let mut own = None;
    assert_eq!(
        site_native_owner_scope(&shared, &absent, None, &mut own),
        Some(false)
    );
    assert_eq!(own, None);
    if !redefine(&shared, cid, "r15w4walk3/OwnStatic") {
        return;
    }
    // Redefined, with a body, not immune: the woven body wins.
    let mut owner = None;
    assert_eq!(
        site_native_owner_scope(&shared, &value, None, &mut owner),
        None
    );
    assert_eq!(owner, None);
}

/// Round 15 wave 5 (lane walk4; F1 of
/// `r15w4-walk3-review-findings-FIXED-20260930.md`): the compiled `invokespecial`
/// tail leaves the native registered on the constant-pool triple for the
/// owner's own body once, and only once, an agent could have woven it.
#[test]
fn the_special_tail_runs_a_woven_owners_body_over_its_native() {
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.natives.native_methods.ancestor_redefine_rule() {
        return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the tail is unchanged
    }
    const NAME: &str = "r15w5walk4/SpecialOwner";
    shared
        .natives
        .native_methods
        .register(NAME, "value", "()I", noop_native, cratonvm_native_api::NativeKind::SyntheticStub);
    let (Some(cid), Some(other)) = (
        define(&shared, NAME),
        define(&shared, "r15w5walk4/SpecialOther"),
    ) else {
        return;
    };
    let info = |method: &'static str, desc: &'static str| JitInvokeInfo {
        class_name: NAME,
        method_name: method,
        descriptor: desc,
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 1,
        declaring_class_id: cid.as_u32(),
        owner_class_id: 0,
    };
    let value = info("value", "()I");
    assert!(
        !special_site_runs_woven_body(&shared, cid, &value, &[]),
        "never redefined: the registered native runs"
    );
    if !redefine(&shared, cid, NAME) {
        return;
    }
    assert!(
        special_site_runs_woven_body(&shared, cid, &value, &[]),
        "redefined, a body, no immunity: the woven body runs"
    );
    // No native on the triple, an initializer, or an owner that is not the
    // constant-pool class (a substituted owner): the tail as before.
    assert!(!special_site_runs_woven_body(
        &shared,
        cid,
        &info("absent", "()V"),
        &[]
    ));
    assert!(!special_site_runs_woven_body(
        &shared,
        cid,
        &info("<init>", "()V"),
        &[]
    ));
    assert!(!special_site_runs_woven_body(&shared, other, &value, &[]));
}

/// Round 15 wave 6 (lane sweepB; WK4-2): `invoke_special_shared_impl`'s
/// native arm asks the compiled tail's question, by the caller's resolved
/// class or, without one, by the loaded class of that name.
#[test]
fn the_special_door_asks_the_woven_owner_with_or_without_a_resolved_class() {
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.natives.native_methods.ancestor_redefine_rule() {
        return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the door is unchanged
    }
    const NAME: &str = "r15w6sweepB/SpecialDoor";
    const UNLOADED: &str = "r15w6sweepB/SpecialDoorNeverLoaded";
    {
        let registry = &mut shared.natives.native_methods;
        registry.register(NAME, "value", "()I", noop_native, cratonvm_native_api::NativeKind::Bridge);
        registry.register(UNLOADED, "value", "()I", noop_native, cratonvm_native_api::NativeKind::Bridge);
    }
    let (Some(cid), Some(other)) = (
        define(&shared, NAME),
        define(&shared, "r15w6sweepB/SpecialDoorOther"),
    ) else {
        return;
    };
    let wins = |pre: Option<ClassId>, name: &str| {
        special_shared_owner_body_wins(&shared, pre, name, "value", "()I", None)
    };
    assert!(
        !wins(None, NAME),
        "never redefined: the registered native runs"
    );
    assert!(!wins(Some(cid), NAME));
    if !redefine(&shared, cid, NAME) {
        return;
    }
    assert!(
        wins(None, NAME),
        "redefined, found by its loaded name: the body"
    );
    assert!(
        wins(Some(cid), NAME),
        "redefined, the caller's class: the body"
    );
    assert!(
        !wins(Some(other), NAME),
        "a class that is not the one the native is registered under: the native"
    );
    assert!(
        !wins(None, UNLOADED),
        "a class never loaded was never woven"
    );
}

/// A version-52 class `name` extending `super_name` that declares nothing.
fn sub_class(name: &str, super_name: &str) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&5u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, super_name); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // interfaces, fields, methods, attributes
    b
}

/// Round 16 (lane redefine): a native registered on `R` for a method `R`
/// inherits from `A` is owned by `A` (the funnel's probe reads `A`): the
/// scope names `A`, keeps the native across `R`'s redefinition and
/// refuses it once `A`'s body may be woven.
#[test]
fn an_inherited_registration_is_owned_by_the_declaring_ancestor() {
    let mut shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.natives.native_methods.ancestor_redefine_rule() {
        return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the rule is not applied
    }
    const A: &str = "r16redefine/InhA";
    const R: &str = "r16redefine/InhR";
    shared
        .natives
        .native_methods
        .register(R, "value", "()I", noop_native, cratonvm_native_api::NativeKind::SyntheticStub);
    let Some(a) = define(&shared, A) else {
        return;
    };
    let Ok(r) = shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            R,
            &sub_class(R, A),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
    else {
        return;
    };
    let info = JitInvokeInfo {
        class_name: R,
        method_name: "value",
        descriptor: "()I",
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: r.as_u32(),
        owner_class_id: 0,
    };
    let mut owner = None;
    assert_eq!(
        site_native_owner_scope(&shared, &info, None, &mut owner),
        Some(true)
    );
    assert_eq!(
        owner,
        Some((a, 0)),
        "the declaring ancestor owns R's native"
    );
    let redefined_r = shared
        .classes
        .class_manager
        .write()
        .redefine_class(r, sub_class(R, A), RedefineOptions::default())
        .is_ok();
    shared.jit.jit_cache.bump_redefine_epoch();
    use crate::runtime::interpreter::{owner_redefinition, OwnerRedefinition};
    let rule_of = |cid| {
        owner_redefinition(
            &shared.classes.class_manager.read(),
            &shared.natives.native_methods,
            cid,
            "value",
            "()I",
        )
    };
    if redefined_r {
        let mut owner = None;
        assert_eq!(
            site_native_owner_scope(&shared, &info, None, &mut owner),
            Some(true),
            "R declares nothing an agent could weave"
        );
        assert_eq!(owner, Some((a, 0)));
        // The one rule (S1): R's registration is answered for A.
        let expected = if shared.natives.native_methods.redefine_inherited_owner() {
            OwnerRedefinition::Unchanged
        } else {
            OwnerRedefinition::Woven
        };
        assert_eq!(rule_of(r), expected);
    }
    if !redefine(&shared, a, A) {
        return;
    }
    if shared.natives.native_methods.redefine_inherited_owner() || redefined_r {
        assert_eq!(
            rule_of(r),
            OwnerRedefinition::Woven,
            "A's weave reaches R's registration"
        );
    }
    assert_eq!(rule_of(a), OwnerRedefinition::Woven);
    let mut owner = None;
    assert_eq!(
        site_native_owner_scope(&shared, &info, None, &mut owner),
        None,
        "A's body may carry advice: the woven body wins"
    );
}

/// A version-52 interface `name` whose one method is the default
/// `public value()I` (`iconst_1; ireturn`).
fn default_iface(name: &str) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.extend_from_slice(&[0x06, 0x01]); // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    b.extend_from_slice(&[0x00, 0x01, 0, 5, 0, 6, 0, 1]); // public value()I
    let code = [0x04, 0xac]; // iconst_1; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 1]); // max_stack 1, max_locals 1 (this)
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

/// A version-52 class `name` extending `Object`, implementing `iface`,
/// that declares nothing.
fn implementing_class(name: &str, iface: &str) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&7u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, "java/lang/Object"); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, iface); // #5
    b.extend_from_slice(&[7, 0, 5]); // #6 Class #5
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 1, 0, 6]); // one interface: #6
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // fields, methods, attributes
    b
}

/// j27 wave 4 (lane DISPROWS; RD16-4): a native registered on `R` for the
/// interface default `I.value` that `R` inherits is answered for `I` by
/// the one rule, as the JIT site scope's landing already was: a weave of
/// `I.value` reaches `R`'s registration (HotSpot runs the woven default
/// for every `R` receiver). Before, `inherited_declaring_class` answered
/// `None` for an interface default and `R`'s generation (0) kept the
/// native.
#[test]
fn an_inherited_interface_default_is_owned_by_the_interface() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if !shared.natives.native_methods.redefine_inherited_owner() {
        return; // `CRATONVM_REDEFINE_INHERITED_OWNER=0`: the old reading
    }
    const I: &str = "j27disprows/DefI";
    const R: &str = "j27disprows/DefR";
    let define_bytes = |name: &str, bytes: Vec<u8>| {
        shared
            .classes
            .class_manager
            .write()
            .define_class_with_options(
                name,
                &bytes,
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            )
            .ok()
    };
    let Some(i) = define_bytes(I, default_iface(I)) else {
        return;
    };
    let Some(r) = define_bytes(R, implementing_class(R, I)) else {
        return;
    };
    use crate::runtime::interpreter::{owner_redefinition, OwnerRedefinition};
    let rule_of_r = || {
        owner_redefinition(
            &shared.classes.class_manager.read(),
            &shared.natives.native_methods,
            r,
            "value",
            "()I",
        )
    };
    assert_eq!(rule_of_r(), OwnerRedefinition::Unchanged);
    let redefined_i = shared
        .classes
        .class_manager
        .write()
        .redefine_class(i, default_iface(I), RedefineOptions::default())
        .is_ok();
    if !redefined_i {
        return;
    }
    assert_eq!(
        rule_of_r(),
        OwnerRedefinition::Woven,
        "I's weave reaches R's registration of the inherited default"
    );
}

/// Round 16 (lane redefine; backlog sweepB S4): a redefined-owner refusal
/// of a virtual/interface site is keyed by its receiver. The same receiver
/// keeps declining; another receiver re-resolves once, and a second
/// receiver-keyed refusal becomes the site-wide one (no alternation
/// thrash). A positive entry keeps the monomorphic contract.
#[test]
fn a_redefined_owner_refusal_answers_only_its_own_receiver() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let info = JitInvokeInfo {
        class_name: "r16redefine/Poly",
        method_name: "value",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    };
    let refusal = NativeSiteCache::refusal_for_receiver(7, &info, 3);
    assert!(refusal.refused_for_receiver);
    assert_eq!(refusal.receiver_class_id, Some(7));
    assert_eq!(refusal.native_owner, None);
    assert!(!refusal.owner_scoped);
    assert!(
        !native_site_refill_for_receiver(Some(&refusal), Some(7)),
        "its own receiver"
    );
    assert!(
        native_site_refill_for_receiver(Some(&refusal), Some(8)),
        "another receiver"
    );
    assert!(
        !native_site_refill_for_receiver(None, Some(8)),
        "a site-wide refusal"
    );
    let mut positive = refusal;
    positive.refused_for_receiver = false;
    assert!(
        !native_site_refill_for_receiver(Some(&positive), Some(8)),
        "a positive entry is monomorphic: another receiver declines"
    );
    assert!(
        native_site_refill_outcome(Some(refusal)).is_none(),
        "second refusal: site-wide"
    );
    assert!(native_site_refill_outcome(Some(positive)).is_some());
    assert!(native_site_refill_outcome(None).is_none());
    // Static / special sites (no guard) keep the site-wide refusal.
    assert!(receiver_keyed_owner_refusal(&shared, None, &info, 0).is_none());
    let keyed = receiver_keyed_owner_refusal(&shared, Some(7), &info, 0);
    assert_eq!(
        keyed.is_some(),
        shared
            .natives
            .native_methods
            .jit_site_cache_owner_redefine(),
        "receiver-keyed exactly when the owner scope is on"
    );
}
