use super::*;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

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
/// under the writer, then the VM's redefinition epoch.
fn redefine(shared: &SharedVm, cid: ClassId, name: &str) {
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(name, 2), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    shared.jit.jit_cache.bump_redefine_epoch();
}

#[test]
fn a_site_quiesce_verdict_is_memoized_until_the_redefine_epoch_moves() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (Some(site_cid), Some(other_cid)) = (
        define(&shared, "r12rt/QuiesceSite"),
        define(&shared, "r12rt/QuiesceOther"),
    ) else {
        // The stripped test VM could not define them: nothing to run.
        return;
    };
    // Some OTHER class redefined: the process-wide flag is up, the site's
    // own class is not redefined.
    redefine(&shared, other_cid, "r12rt/QuiesceOther");
    assert!(crate::classloading::any_class_redefined());

    let info = JitInvokeInfo {
        class_name: "r12rt/QuiesceSite",
        method_name: "value",
        descriptor: "()I",
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: site_cid.as_u32(),
        owner_class_id: 0,
    };
    let key = jit_site_key(shared.vm_identity, &info as *const JitInvokeInfo as usize);
    QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());

    assert!(!site_class_was_redefined(&shared, &info, key));
    let epoch = shared.jit.jit_cache.redefine_epoch();
    assert_eq!(
        QUIESCE_VERDICT.with(|m| m.borrow().get(&key).copied()),
        Some((epoch, false)),
        "the verdict is memoized at the epoch read before it"
    );
    // Served from the memo while the epoch holds (a planted `true` proves
    // no lookup ran).
    QUIESCE_VERDICT.with(|m| {
        m.borrow_mut().insert(key, (epoch, true));
    });
    assert!(site_class_was_redefined(&shared, &info, key));
    // A `false` from an older epoch is never served once the site's own
    // class was redefined.
    QUIESCE_VERDICT.with(|m| {
        m.borrow_mut().insert(key, (epoch, false));
    });
    redefine(&shared, site_cid, "r12rt/QuiesceSite");
    assert!(
        site_class_was_redefined(&shared, &info, key),
        "a stale-epoch `false` survived the site's own redefinition"
    );
    assert_eq!(
        QUIESCE_VERDICT.with(|m| m.borrow().get(&key).copied()),
        Some((shared.jit.jit_cache.redefine_epoch(), true))
    );
    QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
}

/// Round 12 wave 2 (lane rt, R12W1-1): a redefined class's site stands its
/// compiled-entry arms down until this thread's quiesced-site flush has
/// run at the current redefinition epoch, and again after each later
/// redefinition; a site whose class was never redefined never does.
#[test]
fn a_redefined_site_is_quiesced_once_per_thread_per_redefinition() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (Some(site_cid), Some(other_cid)) = (
        define(&shared, "r12w2rt/QuiesceGenSite"),
        define(&shared, "r12w2rt/QuiesceGenOther"),
    ) else {
        return;
    };
    let info_for = |name: &'static str, cid: ClassId| JitInvokeInfo {
        class_name: name,
        method_name: "value",
        descriptor: "()I",
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: cid.as_u32(),
        owner_class_id: 0,
    };
    let site = info_for("r12w2rt/QuiesceGenSite", site_cid);
    let other = info_for("r12w2rt/QuiesceGenOther", other_cid);
    let key_of = |i: &JitInvokeInfo| {
        jit_site_key(shared.vm_identity, i as *const JitInvokeInfo as usize)
    };
    let (site_key, other_key) = (key_of(&site), key_of(&other));
    QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
    QUIESCED_SITE_FLUSH_STAMP.with(|s| s.set((0, u32::MAX)));

    redefine(&shared, site_cid, "r12w2rt/QuiesceGenSite");
    assert!(site_quiesced_for_redefinition(&shared, &site, site_key));
    assert!(
        !site_quiesced_for_redefinition(&shared, &other, other_key),
        "a class never redefined is never quiesced"
    );
    if !quiesce_by_generation_enabled() {
        QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
        return;
    }
    // What `jit_invoke_dispatch_body` does on that quiesced call.
    let epoch = shared.jit.jit_cache.redefine_epoch();
    QUIESCED_SITE_FLUSH_STAMP.with(|s| s.set((shared.vm_identity, epoch)));
    assert!(
        !site_quiesced_for_redefinition(&shared, &site, site_key),
        "flushed at this epoch: the compiled arms serve the site again"
    );
    assert!(
        site_class_was_redefined(&shared, &site, site_key),
        "the intrinsic shortcuts still see the class as redefined"
    );
    redefine(&shared, site_cid, "r12w2rt/QuiesceGenSite");
    assert!(
        site_quiesced_for_redefinition(&shared, &site, site_key),
        "a later redefinition quiesces the site again"
    );
    QUIESCED_SITE_FLUSH_STAMP.with(|s| s.set((0, u32::MAX)));
    QUIESCE_VERDICT.with(|m| m.borrow_mut().clear());
}

/// A body of `value()I` owning one deopt point, and that point's address
/// (the boxed point keeps its address when the body moves into the cache).
fn body_with_a_point() -> (cratonvm_jit::CompiledMethod, usize) {
    use cratonvm_jit::deopt::{
        DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ResumeSemantics,
    };
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut body = cratonvm_jit::CompiledMethod::new(buf);
    let point = Box::new(DeoptimizationPoint {
        native_offset: 0,
        bci: 0,
        reason: DeoptReason::ClassCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 0,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    });
    let addr = &*point as *const DeoptimizationPoint as usize;
    body._deopt_point_boxes.push(point);
    (body, addr)
}

/// Round 12 wave 2 (lane rt): a trap stashed by a body of a redefined class
/// resumes only when the body published for the method owns its point and
/// was compiled after the class's last redefinition; a body compiled
/// before it, an unknown point and a point no published body owns are
/// refused, as before.
#[test]
fn a_trap_of_a_redefined_class_resumes_only_out_of_a_current_body() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let name = "r12w2rt/ResumeGate";
    let Some(cid) = define(&shared, name) else {
        return;
    };
    let publish = |body| {
        shared.jit.jit_cache.put(
            std::sync::Arc::from(name),
            std::sync::Arc::from("value"),
            std::sync::Arc::from("()I"),
            cid,
            body,
        )
    };
    let (old_body, old_point) = body_with_a_point();
    publish(old_body);
    // Held, as the call arm that entered a trapping body holds it, so the
    // new body's point cannot reuse the old one's address.
    let old_alive = shared.jit.jit_cache.get(name, "value", "()I", cid);
    assert!(old_alive.is_some(), "the old body was published");
    assert!(
        !stashed_callee_predates_redefinition(
            &shared,
            cid.as_u32(),
            name,
            "value",
            "()I",
            old_point
        ),
        "a class never redefined is not refused on this account"
    );
    // The production order: the class under the writer, then the JIT side
    // (withdrawal and barrier) through `note_class_redefinition_of`.
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(cid, ldc_class(name, 2), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    shared.jit.note_class_redefinition_of(cid, name, false);
    assert!(
        stashed_callee_predates_redefinition(
            &shared,
            cid.as_u32(),
            name,
            "value",
            "()I",
            old_point
        ),
        "the body compiled before the redefinition ran the old bytecode"
    );
    assert!(
        stashed_callee_predates_redefinition(&shared, cid.as_u32(), name, "value", "()I", 0),
        "a trap with no point cannot name its body"
    );
    let (new_body, new_point) = body_with_a_point();
    publish(new_body);
    if quiesce_by_generation_enabled() {
        assert!(
            !stashed_callee_predates_redefinition(
                &shared,
                cid.as_u32(),
                name,
                "value",
                "()I",
                new_point
            ),
            "the current body's trap resumes against the bytecode it ran"
        );
    }
    assert!(
        stashed_callee_predates_redefinition(
            &shared,
            cid.as_u32(),
            name,
            "value",
            "()I",
            old_point
        ),
        "the published body does not own the old body's point"
    );
}

/// Interpreter round i1 wave 29, lane L3: the class a trap is judged by
/// is the one the call site's loader resolves. With one name defined by
/// the application loader (never redefined) and by a user loader
/// (redefined), a stale body of the user loader's class used to be judged
/// by the application loader's class (`get_loaded_class_id` answers the
/// built-in loaders first) and its trap rebuilt against the NEW bytecode.
#[test]
fn a_trap_is_judged_by_the_class_the_sites_loader_resolves() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let name = "r29l3/TwoLoaders";
    let Some(app_cid) = define(&shared, name) else {
        return;
    };
    let user_cid = match shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            name,
            &ldc_class(name, 1),
            ClassLoaderId::UserDefined(29_003),
            DefineClassOptions::default(),
        ) {
        Ok(cid) => cid,
        Err(_) => return,
    };
    let resolves_own = shared
        .classes
        .class_manager
        .read()
        .find_class_by_name_for_class(name, user_cid)
        == Some(user_cid);
    if app_cid == user_cid || !resolves_own {
        // This build's resolution policy does not give the user loader
        // its own class back: nothing to tell apart.
        return;
    }
    let (old_body, old_point) = body_with_a_point();
    shared.jit.jit_cache.put(
        std::sync::Arc::from(name),
        std::sync::Arc::from("value"),
        std::sync::Arc::from("()I"),
        user_cid,
        old_body,
    );
    // Held, as the call arm that entered a trapping body holds it.
    let old_alive = shared.jit.jit_cache.get(name, "value", "()I", user_cid);
    assert!(old_alive.is_some(), "the old body was published");
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(user_cid, ldc_class(name, 2), RedefineOptions::default())
        .expect("a same-shape redefinition succeeds");
    shared.jit.note_class_redefinition_of(user_cid, name, false);
    assert!(
        stashed_callee_predates_redefinition(
            &shared,
            user_cid.as_u32(),
            name,
            "value",
            "()I",
            old_point
        ),
        "the user loader's class was redefined after its body was compiled"
    );
    assert!(
        !stashed_callee_predates_redefinition(
            &shared,
            app_cid.as_u32(),
            name,
            "value",
            "()I",
            old_point
        ),
        "the application loader's class of that name never was"
    );
}

/// Round 12 wave 2 (lane rt, R12W1-3): this file's `class_was_redefined`
/// memo records the verdict at its stamp, serves it while the stamp holds,
/// and never serves a `false` past the class's own redefinition.
#[test]
fn the_redefined_class_memo_is_served_only_at_its_stamp() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (Some(cid), Some(other_cid)) = (
        define(&shared, "r12w2rt/MemoClass"),
        define(&shared, "r12w2rt/MemoOther"),
    ) else {
        return;
    };
    redefine(&shared, other_cid, "r12w2rt/MemoOther");
    let stamp_now = || {
        (
            shared.vm_identity,
            shared.jit.jit_cache.redefine_epoch(),
            cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
                .class_definition_epoch(),
        )
    };
    let raw = cid.as_u32();
    REDEFINED_CLASS_MEMO.with(|m| m.borrow_mut().1.clear());
    assert!(!class_was_redefined(&shared, cid));
    assert!(class_was_redefined(&shared, other_cid));
    if !redefined_class_memo_enabled() {
        return;
    }
    assert_eq!(
        REDEFINED_CLASS_MEMO.with(|m| m.borrow().1.get(&raw).copied()),
        Some(false),
        "the verdict is recorded"
    );
    // Served while the stamp holds: a planted `true` proves no lookup ran.
    // The class-definition epoch is shared with other tests' VMs, so the
    // check counts only when the stamp did not move across the call (the
    // epochs are monotonic).
    let before = stamp_now();
    REDEFINED_CLASS_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        m.0 = before;
        m.1.insert(raw, true);
    });
    let served = class_was_redefined(&shared, cid);
    if stamp_now() == before {
        assert!(served, "served from the memo at its stamp");
    }
    // A `false` stamped before the class's own redefinition is not served.
    REDEFINED_CLASS_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        m.0 = stamp_now();
        m.1.insert(raw, false);
    });
    redefine(&shared, cid, "r12w2rt/MemoClass");
    assert!(
        class_was_redefined(&shared, cid),
        "a stale `false` survived the class's own redefinition"
    );
    REDEFINED_CLASS_MEMO.with(|m| m.borrow_mut().1.clear());
}

/// The `jit_lambda_int_to_double` memo answers only at its own stamp and
/// for its own receiver class, and stays bounded.
#[test]
fn the_lambda_get_memo_serves_only_its_stamp_and_receiver_class() {
    let key = (9001usize, 7u32);
    let stamp: LambdaGetStamp = (1, 2, 3);
    LAMBDA_GET_MEMO.with(|m| m.borrow_mut().clear());
    assert_eq!(lambda_get_memo_shape(key, stamp), None);
    lambda_get_memo_store(
        key,
        LambdaGetMemo {
            stamp,
            impl_class_name: Some(std::sync::Arc::from("r12rt/Impl")),
            route: None,
        },
    );
    assert_eq!(lambda_get_memo_shape(key, stamp), Some(true));
    assert_eq!(
        lambda_get_memo_shape(key, (2, 2, 3)),
        None,
        "another generation"
    );
    assert_eq!(
        lambda_get_memo_shape(key, (1, 3, 3)),
        None,
        "another definition"
    );
    assert_eq!(
        lambda_get_memo_shape(key, (1, 2, 4)),
        None,
        "another redefinition"
    );
    assert!(lambda_get_memo_route(key, stamp, ClassId::new(5)).is_none());
    lambda_get_memo_note_route(key, stamp, ClassId::new(5), LambdaGetRoute::Generic);
    assert!(matches!(
        lambda_get_memo_route(key, stamp, ClassId::new(5)),
        Some(LambdaGetRoute::Generic)
    ));
    assert!(
        lambda_get_memo_route(key, stamp, ClassId::new(6)).is_none(),
        "another receiver class"
    );
    // A route resolved at an older stamp does not land in a newer memo.
    lambda_get_memo_note_route(
        key,
        (0, 2, 3),
        ClassId::new(6),
        LambdaGetRoute::Direct(None),
    );
    assert!(lambda_get_memo_route(key, stamp, ClassId::new(6)).is_none());
    for i in 0..(LAMBDA_GET_MEMO_CAP as u32 + 5) {
        lambda_get_memo_store(
            (1, i),
            LambdaGetMemo {
                stamp,
                impl_class_name: None,
                route: None,
            },
        );
    }
    assert!(LAMBDA_GET_MEMO.with(|m| m.borrow().len()) <= LAMBDA_GET_MEMO_CAP);
    LAMBDA_GET_MEMO.with(|m| m.borrow_mut().clear());
}
