use super::*;

/// Every family has its rows in the one table, exactly once, and the
/// family views are the rows each family had before the table (wave 48,
/// lane L3, stage 1 of the one-table proposal).
#[test]
fn every_family_is_listed_once_with_its_rows() {
    let families = [
        StandinFamily::Census,
        StandinFamily::Park,
        StandinFamily::ArgCheck,
        StandinFamily::ArgCheckTwoSite,
        StandinFamily::MethodInvoke,
        StandinFamily::ConstructorNewInstance,
        StandinFamily::HiddenInvoke,
        StandinFamily::Privileged,
        StandinFamily::MemberLookup,
    ];
    for family in families {
        let listed = STANDIN_TABLE.iter().filter(|(f, _)| *f == family).count();
        assert_eq!(listed, 1, "{family:?} listed {listed} times");
        assert!(!standin_rows(family).is_empty(), "{family:?} has no rows");
    }
    assert_eq!(STANDIN_TABLE.len(), families.len());
    assert_eq!(
        standin_rows(StandinFamily::Census).len(),
        STANDIN_METHODS.len()
    );
    assert_eq!(
        standin_rows(StandinFamily::Park).len(),
        STANDIN_PARK_METHODS.len()
    );
    assert_eq!(standin_rows(StandinFamily::MethodInvoke).len(), 2);
    assert_eq!(standin_rows(StandinFamily::ConstructorNewInstance).len(), 3);
    assert_eq!(standin_rows(StandinFamily::Privileged).len(), 3);
    assert_eq!(standin_rows(StandinFamily::MemberLookup).len(), 9);
}

/// Each `doPrivileged` bridge's kind has its own one-row step (wave 49,
/// lane L3): its overload, at its action's `run()` call; each kind has its
/// own memo slot and bit; only the exception-action bridge may raise.
#[test]
fn each_privileged_kind_names_its_own_overload() {
    use cratonvm_native_api::ReflectiveCallKind as Kind;
    let all = [
        Kind::MethodInvoke,
        Kind::ConstructorNewInstance,
        Kind::DoPrivileged,
        Kind::DoPrivilegedException,
        Kind::DoPrivilegedInContext,
    ];
    let mut slots: Vec<usize> = all.iter().map(|&k| reflective_kind_slot(k)).collect();
    slots.sort_unstable();
    slots.dedup();
    assert_eq!(slots, (0..REFLECTIVE_KINDS).collect::<Vec<_>>());
    for (kind, descriptor, action) in [
        (
            Kind::DoPrivileged,
            "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
            "java/security/PrivilegedAction",
        ),
        (
            Kind::DoPrivilegedException,
            "(Ljava/security/PrivilegedExceptionAction;)Ljava/lang/Object;",
            "java/security/PrivilegedExceptionAction",
        ),
        (
            Kind::DoPrivilegedInContext,
            "(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;)Ljava/lang/Object;",
            "java/security/PrivilegedAction",
        ),
    ] {
        assert!(kind.is_privileged());
        let steps = reflective_steps(kind);
        assert_eq!(steps.len(), 1, "{kind:?}");
        assert_eq!(steps[0].class, "java/security/AccessController");
        assert_eq!(steps[0].name, "doPrivileged");
        assert_eq!(steps[0].descriptor, descriptor);
        assert_eq!(steps[0].call(), (action, "run", "()Ljava/lang/Object;"));
        assert!(reflective_accessor_class(kind).is_none());
        assert_eq!(
            reflective_kind_may_raise(kind),
            kind == Kind::DoPrivilegedException,
            "{kind:?}"
        );
    }
    assert!(!Kind::MethodInvoke.is_privileged());
    assert_eq!(reflective_steps(Kind::MethodInvoke).len(), 2);
    assert_eq!(reflective_steps(Kind::ConstructorNewInstance).len(), 3);
    assert!(reflective_kind_may_raise(Kind::MethodInvoke));
}

/// A served walk's kinds (JIT round 20, lane walkcompat): no memo slot,
/// no reflective steps, no raise; their chains, outermost first, start at
/// `StackWalker.walk` / `forEach`, end where the native enters Java, and
/// only `callStackWalk` is a native leaf.
#[test]
fn jr20_served_walk_kinds_name_hotspots_walk_frames() {
    use cratonvm_native_api::ReflectiveCallKind as Kind;
    let names = |kind: Kind| -> Vec<&'static str> {
        stack_walk_steps(kind)
            .iter()
            .map(|step| step.name)
            .collect()
    };
    let entry = ["walk", "walkHelper", "beginStackWalk", "callStackWalk"];
    let mut walk = vec!["walk"];
    walk.extend(entry);
    assert_eq!(names(Kind::StackWalkerWalk), walk);
    walk.extend(["doStackWalk", "consumeFrames"]);
    assert_eq!(names(Kind::StackWalkerWalkFunction), walk);
    let mut for_each = vec!["forEach"];
    for_each.extend(entry);
    for_each.extend(["doStackWalk", "consumeFrames", "lambda$forEach$0"]);
    assert_eq!(names(Kind::StackWalkerForEachStream), for_each);
    for_each.extend(["forEach", "forEachRemaining"]);
    assert_eq!(names(Kind::StackWalkerForEachAction), for_each);
    let mut bits = 0u8;
    for kind in [
        Kind::MethodInvoke,
        Kind::ConstructorNewInstance,
        Kind::DoPrivileged,
        Kind::DoPrivilegedException,
        Kind::DoPrivilegedInContext,
    ] {
        assert!(!kind.is_stack_walk());
        assert!(stack_walk_steps(kind).is_empty());
        bits |= reflective_kind_bit(kind);
    }
    for kind in [
        Kind::StackWalkerWalk,
        Kind::StackWalkerWalkFunction,
        Kind::StackWalkerForEachStream,
        Kind::StackWalkerForEachAction,
    ] {
        assert!(kind.is_stack_walk() && !kind.is_privileged());
        assert_eq!(reflective_kind_slot(kind), REFLECTIVE_KINDS);
        assert!(reflective_steps(kind).is_empty());
        assert!(!reflective_kind_may_raise(kind));
        assert_eq!(bits & reflective_kind_bit(kind), 0, "{kind:?}");
        assert_eq!(
            reflective_kind_bit(kind) != 0,
            reflective_accessor_class(kind).is_some(),
            "{kind:?}"
        );
        for step in stack_walk_steps(kind) {
            assert_eq!(
                step.calls.is_none(),
                step.name == "callStackWalk",
                "{}",
                step.name
            );
        }
        let named: ReflectiveFramesNamed = Default::default();
        let row = ReflectiveCallRow {
            interp_depth: 1,
            jit_depth: 0,
            kind,
            check: None,
            raised: None,
            door_save: None,
            adapter_pin: None,
        };
        assert!(reflective_splices_memoized(&named, &[row], 0, None).is_empty());
        // JIT round 20 wave 7, lane revVM: the VM's memo serves the
        // memo-only readers (a published trace); a busy guard names
        // nothing.
        let walk = StackWalkFramesNamed::new();
        let count = cratonvm_classloading::class_redefinition_count();
        walk.name_if_stale(kind, || None);
        assert!(reflective_splices_memoized(&named, &[row], count, Some(&walk)).is_empty());
        walk.name_if_stale(kind, || Some(Some(Vec::new())));
        let spliced = reflective_splices_memoized(&named, &[row], count, Some(&walk));
        assert_eq!(spliced.len(), 1, "{kind:?}");
        assert_eq!(spliced[0].interp_depth, 1);
    }
    // A kind with a per-thread slot never reads the VM's walk memo.
    let walk = StackWalkFramesNamed::new();
    walk.name_if_stale(Kind::MethodInvoke, || Some(Some(Vec::new())));
    assert!(walk
        .get(
            Kind::MethodInvoke,
            cratonvm_classloading::class_redefinition_count()
        )
        .is_none());
}

/// Only the hidden family's rows are hidden, and every reflective step
/// names the call it stands at.
#[test]
fn hidden_rows_and_reflective_calls_are_marked() {
    for (family, rows) in STANDIN_TABLE {
        for row in *rows {
            assert_eq!(
                row.hidden,
                *family == StandinFamily::HiddenInvoke,
                "{}.{}",
                row.class,
                row.name
            );
        }
    }
    for kind in [
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke,
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance,
        cratonvm_native_api::ReflectiveCallKind::DoPrivileged,
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedException,
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedInContext,
    ] {
        for step in reflective_steps(kind) {
            let (owner, name, descriptor) = step.call();
            assert!(!owner.is_empty() && !name.is_empty() && !descriptor.is_empty());
        }
    }
    assert!(hidden_invoke_row(HIDDEN_INVOKE_DMH_HOLDER, "invokeStatic").is_some());
    assert!(hidden_invoke_row(HIDDEN_INVOKE_INVOKERS_HOLDER, "invokeExact_MT").is_some());
    assert!(hidden_invoke_row(HIDDEN_INVOKE_INVOKERS_HOLDER, "invoke_MT").is_some());
    assert!(hidden_invoke_row(HIDDEN_INVOKE_DMH_HOLDER, "newInvokeSpecial").is_some());
    assert!(hidden_invoke_row(HIDDEN_INVOKE_METHOD_ACCESSOR, "invokeImpl").is_some());
    assert!(hidden_invoke_row(HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR, "invokeImpl").is_some());
    assert!(hidden_invoke_row(HIDDEN_INVOKE_DMH_HOLDER, "invokeSpecial").is_some());
    assert!(hidden_invoke_row(HIDDEN_INVOKE_DMH_HOLDER, "invokeVirtual").is_some());
    // JIT round 22 wave 2, lane runtime: named where a linked reflective
    // call's kind is known (`hidden_invoke_standin_frames_with`).
    assert!(hidden_invoke_row(HIDDEN_INVOKE_DMH_HOLDER, "invokeInterface").is_some());
}

/// A LambdaForm's shape: the handle first, the call's arguments as basic
/// types (every subword type `I`, every reference and array `Object`),
/// an invoker's appendix last, and the basic return type. The shapes
/// HotSpot 25.0.3 ran for `L3W48HiddenInvokeFrames`.
#[test]
fn a_lambda_form_descriptor_erases_to_basic_types() {
    let object = "Ljava/lang/Object;";
    assert_eq!(
        lambda_form_descriptor("()V", 1, 0).as_deref(),
        Some("(Ljava/lang/Object;)V")
    );
    assert_eq!(
        lambda_form_descriptor("(I)I", 1, 0).as_deref(),
        Some("(Ljava/lang/Object;I)I")
    );
    assert_eq!(
        lambda_form_descriptor("(Ljava/lang/String;)Ljava/lang/Object;", 1, 0),
        Some(format!("({object}{object}){object}"))
    );
    assert_eq!(
        lambda_form_descriptor("(Ljava/lang/Object;)Ljava/lang/Object;", 1, 1),
        Some(format!("({object}{object}{object}){object}"))
    );
    assert_eq!(
        lambda_form_descriptor("(ZBSCJFD[[I[Ljava/lang/String;)Z", 1, 0),
        Some(format!("({object}IIIIJFD{object}{object})I"))
    );
    assert_eq!(lambda_form_descriptor("(V)V", 1, 0), None);
    assert_eq!(lambda_form_descriptor("(Lunterminated)V", 1, 0), None);
    assert_eq!(lambda_form_descriptor("()VX", 1, 0), None);
    assert_eq!(
        descriptor_parameter_count("(IJLjava/lang/String;[D)V"),
        Some(4)
    );
    assert_eq!(descriptor_parameter_count("no descriptor"), None);
}

/// The last `invoke*` of a holder body is the pc its frame stands at
/// (`DirectMethodHandle$Holder.invokeStatic(Object)V`: `linkToStatic` at
/// 9, after `internalMemberName` at 1).
#[test]
fn a_holder_frame_stands_at_its_last_invoke() {
    // aload_0; invokestatic #12; astore_1; aload_1; checkcast #17;
    // invokestatic #306; return
    let code = [
        0x2a, 0xb8, 0x00, 0x0c, 0x4c, 0x2b, 0xc0, 0x00, 0x11, 0xb8, 0x01, 0x32, 0xb1,
    ];
    assert_eq!(last_invoke_pc(&code), Some(9));
    assert_eq!(last_invoke_pc(&[0xb1]), None);
}

/// No frame is listed for a walk with no method-handle or reflective
/// call the store can place.
#[test]
fn no_hidden_invoke_frames_without_a_call_to_stand_for() {
    let store = ClassStore::new();
    let entry = |class: &str, method: &str| StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: Some(Arc::from("()V")),
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: 3,
        class_id: Some(ClassId::new(7)),
        method_index: None,
    };
    let frames = [entry("p/Target", "target"), entry("p/Caller", "call")];
    assert!(hidden_invoke_standin_frames_with(&store, &frames, &[]).is_empty());
    let reflective = [
        entry("p/Target", "target"),
        entry("jdk/internal/reflect/DirectMethodHandleAccessor", "invoke"),
    ];
    assert!(hidden_invoke_standin_frames_with(&store, &reflective, &[]).is_empty());
}
