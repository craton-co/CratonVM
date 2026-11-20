#[allow(unused_imports)]
use super::*;

impl DoorKeyMemoOwner {
    pub(in crate::runtime::stackwalker) fn matches(&self, owner: DoorKeyOwnerRef<'_>) -> bool {
        match (self, owner) {
            (Self::Id(a), DoorKeyOwnerRef::Id(b)) => *a == b,
            (Self::Name(a, ra), DoorKeyOwnerRef::Name(b, rb)) => *ra == rb && &**a == b,
            _ => false,
        }
    }

    pub(in crate::runtime::stackwalker) fn of(owner: DoorKeyOwnerRef<'_>) -> Self {
        match owner {
            DoorKeyOwnerRef::Id(id) => Self::Id(id),
            DoorKeyOwnerRef::Name(name, requester) => Self::Name(name.into(), requester),
        }
    }
}

/// Interpreter round i2 wave 64, lane L3 (`VmConfig::served_raise_door_memo`,
/// `CRATONVM_THROWABLE_SERVED_RAISE_DOOR_MEMO`): the methods this thread's
/// static JDK doors selected for their [`DoorInFlight`] notes, so a door
/// entered again (a method handle's `REF_invokeStatic` door in a loop, the
/// served `Method.invoke` of the same target) takes its key without the
/// class-manager read and the method scan ([`door_native_callee`]).
///
/// Direct-mapped, [`Self::WAYS`] entries; a colliding door replaces the
/// entry. The whole memo is dropped when a class is redefined
/// (`class_redefinition_count`). Only selections whose owner RESOLVED are
/// kept (an owner a name door could not resolve yet may load later). The
/// screened owners are `java/` / `jdk/` classes of the built-in loaders,
/// which are never unloaded, so a kept `(class, slot)` stays the method.
#[derive(Debug)]
pub struct DoorKeyMemo {
    pub(in crate::runtime::stackwalker) redefinitions: u64,
    pub(in crate::runtime::stackwalker) entries: [Option<DoorKeyMemoEntry>; DoorKeyMemo::WAYS],
}

impl DoorKeyMemo {
    /// Entries: a power of two.
    pub const WAYS: usize = 16;

    /// An empty memo, valid at class-redefinition count `redefinitions`.
    pub fn new(redefinitions: u64) -> Self {
        Self {
            redefinitions,
            entries: Default::default(),
        }
    }

    /// The entry index: the method name's bytes, the descriptor's length and
    /// last byte, the owner's id (or name length and requester). A collision
    /// only costs a miss: [`Self::lookup`] compares every input.
    pub(in crate::runtime::stackwalker) fn way(owner: DoorKeyOwnerRef<'_>, method: &str, descriptor: &str) -> usize {
        let mut h: u32 = 0x811c_9dc5;
        let mut mix = |b: u32| {
            h ^= b;
            h = h.wrapping_mul(0x0100_0193);
        };
        for &b in method.as_bytes() {
            mix(u32::from(b));
        }
        mix(u32::try_from(descriptor.len()).unwrap_or(u32::MAX));
        mix(descriptor.as_bytes().last().copied().map_or(0, u32::from));
        match owner {
            DoorKeyOwnerRef::Id(id) => mix(id.as_u32()),
            DoorKeyOwnerRef::Name(name, requester) => {
                mix(u32::try_from(name.len()).unwrap_or(u32::MAX));
                mix(requester.map_or(u32::MAX, ClassId::as_u32));
            }
        }
        ((h ^ (h >> 16)) as usize) & (Self::WAYS - 1)
    }

    /// The method a door on `owner` selected for `method` + `descriptor`, as
    /// remembered at class-redefinition count `redefinitions` (`None`: not
    /// remembered; `Some(None)`: the door selects nothing keyed). A count
    /// that moved empties the memo first.
    pub(crate) fn lookup(
        &mut self,
        redefinitions: u64,
        owner: DoorKeyOwnerRef<'_>,
        method: &str,
        descriptor: &str,
    ) -> Option<Option<(ClassId, u32)>> {
        if redefinitions != self.redefinitions {
            self.entries = Default::default();
            self.redefinitions = redefinitions;
            return None;
        }
        let entry = self.entries[Self::way(owner, method, descriptor)].as_ref()?;
        (entry.owner.matches(owner) && &*entry.method == method && &*entry.descriptor == descriptor)
            .then_some(entry.key)
    }

    /// Remember a door's selection (see [`Self::lookup`]).
    pub(crate) fn insert(
        &mut self,
        redefinitions: u64,
        owner: DoorKeyOwnerRef<'_>,
        method: &str,
        descriptor: &str,
        key: Option<(ClassId, u32)>,
    ) {
        if redefinitions != self.redefinitions {
            self.entries = Default::default();
            self.redefinitions = redefinitions;
        }
        self.entries[Self::way(owner, method, descriptor)] = Some(DoorKeyMemoEntry {
            owner: DoorKeyMemoOwner::of(owner),
            method: method.into(),
            descriptor: descriptor.into(),
            key,
        });
    }
}


/// `Method.invoke`'s JDK frames, OUTERMOST first (JDK 25; read with `javap
/// -c -p` from JDK 25.0.3: `Method.invoke` calls the accessor through
/// `invokeinterface MethodAccessor.invoke`, the accessor its private
/// `invokeImpl` through `invokevirtual`). Each stands at its call of the
/// next ([`StandinMethod::calls`]); [`STANDIN_TABLE`]'s
/// [`StandinFamily::MethodInvoke`] rows.
pub(in crate::runtime::stackwalker) const METHOD_INVOKE_STEPS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/reflect/Method",
        name: "invoke",
        descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        calls: Some((
            "jdk/internal/reflect/MethodAccessor",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        )),
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "jdk/internal/reflect/DirectMethodHandleAccessor",
        name: "invoke",
        descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        calls: Some((
            "jdk/internal/reflect/DirectMethodHandleAccessor",
            "invokeImpl",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        )),
        ..StandinMethod::ROW
    },
];

/// `Constructor.newInstance`'s JDK frames, OUTERMOST first (JDK 25, as
/// above); the [`StandinFamily::ConstructorNewInstance`] rows.
pub(in crate::runtime::stackwalker) const CONSTRUCTOR_NEW_INSTANCE_STEPS: &[StandinMethod] = &[
    StandinMethod {
        class: "java/lang/reflect/Constructor",
        name: "newInstance",
        descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
        calls: Some((
            "java/lang/reflect/Constructor",
            "newInstanceWithCaller",
            "([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object;",
        )),
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "java/lang/reflect/Constructor",
        name: "newInstanceWithCaller",
        descriptor: "([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object;",
        calls: Some((
            "jdk/internal/reflect/ConstructorAccessor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )),
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: "jdk/internal/reflect/DirectConstructorHandleAccessor",
        name: "newInstance",
        descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
        calls: Some((
            "jdk/internal/reflect/DirectConstructorHandleAccessor",
            "invokeImpl",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )),
        ..StandinMethod::ROW
    },
];

// `AccessController.doPrivileged` (interpreter round i1 wave 49, lane L3;
// `docs/feature-designs/real-jdk-mode.md`).
// The three overloads `native-builtins/src/security_manager.rs` serves with a
// bridge (which runs the action itself, keeping its privileged-frame stack)
// are one-frame JDK methods on JDK 25 (`javap -c -p`, 25.0.3):
//
// ```text
// doPrivileged(PrivilegedAction)            aload_0; invokeinterface PrivilegedAction.run (bci 1, line 74); astore_1; aload_1; areturn
// doPrivileged(PrivilegedAction, ACC)       the same (line 138)
// doPrivileged(PrivilegedExceptionAction)   the same (line 251), a RuntimeException rethrown,
//                                           any other Exception `throw wrapException(e)` (bci 14, line 256)
// ```
//
// so HotSpot lists `AccessController.doPrivileged` below the action in every
// view (the lambda proxy's `run` is hidden), and a `PrivilegedActionException`
// the bridge wraps starts at `AccessController.wrapException` (its `new`,
// line 394) above `doPrivileged` at its `wrapException` call. The bridges
// record their entry as the reflective natives do
// (`NativeExceptionAccess::enter_reflective_call`), so every capture that
// lists the reflective steps lists this frame too; the walks keep it (it is
// not a reflection frame). Fail-closed like every family: a JDK whose
// `doPrivileged` does not call `run()` itself (JDK 17's native
// `executePrivileged` path) lists nothing.

/// The [`StandinFamily::Privileged`] rows, one per bridged overload, in the
/// order of [`privileged_step`].
pub(in crate::runtime::stackwalker) const PRIVILEGED_STEPS: &[StandinMethod] = &[
    StandinMethod {
        class: PRIVILEGED_CLASS,
        name: "doPrivileged",
        descriptor: "(Ljava/security/PrivilegedAction;)Ljava/lang/Object;",
        calls: Some(("java/security/PrivilegedAction", "run", "()Ljava/lang/Object;")),
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: PRIVILEGED_CLASS,
        name: "doPrivileged",
        descriptor: "(Ljava/security/PrivilegedExceptionAction;)Ljava/lang/Object;",
        calls: Some((
            "java/security/PrivilegedExceptionAction",
            "run",
            "()Ljava/lang/Object;",
        )),
        throws: &[PRIVILEGED_ACTION_EXCEPTION],
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: PRIVILEGED_CLASS,
        name: "doPrivileged",
        descriptor: "(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;)Ljava/lang/Object;",
        calls: Some(("java/security/PrivilegedAction", "run", "()Ljava/lang/Object;")),
        ..StandinMethod::ROW
    },
];

pub(in crate::runtime::stackwalker) const PRIVILEGED_CLASS: &str = "java/security/AccessController";
pub(in crate::runtime::stackwalker) const PRIVILEGED_ACTION_EXCEPTION: &str = "java/security/PrivilegedActionException";
/// `AccessController.wrapException(Exception)`, which builds the
/// `PrivilegedActionException` of a checked exception on JDK 25.
pub(in crate::runtime::stackwalker) const PRIVILEGED_WRAP: (&str, &str) = (
    "wrapException",
    "(Ljava/lang/Exception;)Ljava/security/PrivilegedActionException;",
);

/// The index of `kind`'s row in [`PRIVILEGED_STEPS`], for a privileged kind.
pub(in crate::runtime::stackwalker) fn privileged_step(kind: cratonvm_native_api::ReflectiveCallKind) -> Option<usize> {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::DoPrivileged => Some(0),
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedException => Some(1),
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedInContext => Some(2),
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke
        | cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerWalk
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerWalkFunction
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachStream
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachAction
        | cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArguments
        | cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArgumentsList => None,
    }
}

/// The [`STANDIN_TABLE`] steps of a reflective `kind`; none for a served
/// `StackWalker` walk, whose frames are [`stack_walk_entries`]' (so the
/// raise and argument-check screens, which need these steps, answer nothing
/// for it).
pub(in crate::runtime::stackwalker) fn reflective_steps(kind: cratonvm_native_api::ReflectiveCallKind) -> &'static [StandinMethod] {
    // Wave 104, lane L3: a served `invokeWithArguments` has none either.
    if kind.is_served_java_frames() {
        return &[];
    }
    if let Some(step) = privileged_step(kind) {
        let rows = standin_rows(StandinFamily::Privileged);
        return rows.get(step..=step).unwrap_or(&[]);
    }
    standin_rows(match kind {
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance => {
            StandinFamily::ConstructorNewInstance
        }
        _ => StandinFamily::MethodInvoke,
    })
}

/// Can a throwable `kind`'s native raises itself be listed on top of a trace
/// at a site of its own ([`reflective_raise_entries`])? The reflective doors
/// (their `InvocationTargetException`s and argument checks) and
/// `doPrivileged(PrivilegedExceptionAction)` (its `PrivilegedActionException`);
/// the other `doPrivileged` bridges raise nothing, so a capture inside them
/// skips the raise screen's class-manager read.
pub(crate) fn reflective_kind_may_raise(kind: cratonvm_native_api::ReflectiveCallKind) -> bool {
    !kind.is_served_java_frames()
        && !matches!(
            kind,
            cratonvm_native_api::ReflectiveCallKind::DoPrivileged
                | cratonvm_native_api::ReflectiveCallKind::DoPrivilegedInContext
        )
}

/// The frames HotSpot lists on top of the `PrivilegedActionException` that
/// `doPrivileged(PrivilegedExceptionAction)` wraps a checked exception in
/// (`throwable_class`; anything else: `None`): `doPrivileged` at its
/// `wrapException` call and `wrapException` at its `PrivilegedActionException`
/// constructor call, OUTERMOST first, found in the image's bytes. `None` when
/// either is not found (the frame is then listed at its `run()` call).
pub(in crate::runtime::stackwalker) fn privileged_raise_entries(
    store: &ClassStore,
    step: &StandinMethod,
    throwable_class: &str,
) -> Option<Arc<[StackTraceEntry]>> {
    if !step.throws.contains(&throwable_class) {
        return None;
    }
    let (wrap_name, wrap_descriptor) = PRIVILEGED_WRAP;
    let (class_id, class, index, method) =
        named_method_in(store, step.class, step.name, step.descriptor)?;
    let call_pc = first_call_to(
        &class.constant_pool,
        &method.code()?.code,
        step.class,
        wrap_name,
        wrap_descriptor,
    )?;
    let (wrap_index, wrap) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == wrap_name && &*m.descriptor == wrap_descriptor)?;
    let init_pc = *throwable_init_sites(&class.constant_pool, &wrap.code()?.code, throwable_class)?
        .first()?;
    Some(Arc::from(vec![
        standin_entry(class, class_id, method, index, Some(call_pc)),
        standin_entry(class, class_id, wrap, wrap_index, Some(init_pc)),
    ]))
}

// ---------------------------------------------------------------------------
// Hidden method-handle frames (wave 48, lane L3)
// ---------------------------------------------------------------------------
//
// `MethodHandle.invokeExact` / `invoke`, `Method.invoke` and
// `Constructor.newInstance` are natives here, so none of the LambdaForm
// frames HotSpot runs for them exist. They are
// hidden (`@Hidden`), so only a `SHOW_HIDDEN_FRAMES` walk lists them; HotSpot
// 25.0.3 (measured; `L3W46InvokeFramesInWalks.java`,
// rows `*-hidden-shows-invoke-frames`), innermost first, for a static
// `target()V`:
//
// ```text
// mh.invokeExact()   target, DirectMethodHandle$Holder.invokeStatic(Object)V@9,
//                    LambdaForm$MH/0x….invokeExact_MT(Object,Object)V@18, caller
// method.invoke(..)  target, DirectMethodHandle$Holder.invokeStatic(Object)V@9,
//                    LambdaForm$MH/0x….invoke(Object,Object)Object@12,
//                    Invokers$Holder.invokeExact_MT(Object,Object,Object)Object@19,
//                    DirectMethodHandleAccessor.invokeImpl@41,
//                    DirectMethodHandleAccessor.invoke@23, Method.invoke@102, caller
// ctor.newInstance() <init>, DirectMethodHandle$Holder.newInvokeSpecial@16,
//                    LambdaForm$MH/0x….invoke@31, Invokers$Holder.invokeExact_MT@19,
//                    DirectConstructorHandleAccessor.invokeImpl, .newInstance@60, ...
// ```
//
// A walk that shows hidden frames lists the ones that can be named from the
// real class bytes ([`hidden_invoke_standin_frames_with`]). The `LambdaForm$MH/0x…`
// frames cannot: they are methods of classes HotSpot spins while it links the
// call, which this VM never spins, so a name would be invented and is left
// out. So is a holder frame whose erased shape the pregenerated `$Holder`
// class does not have (HotSpot spins a `LambdaForm$DMH/0x…` class for it),
// and the holder of an instance method (its kind -- `invokeVirtual`,
// `invokeSpecial`, `invokeInterface` -- depends on how the handle was looked
// up, which the frames do not say; a constructor's is always
// `newInvokeSpecial`, and since wave 108 a statically bound method's --
// private, final, or of a final class -- always `invokeSpecial`, and a
// virtually bound one no interface declares `invokeVirtual` unless the handle
// came from `findSpecial`, the one residual).

/// The [`StandinFamily::HiddenInvoke`] rows. A holder's `descriptor` is `""`:
/// it is the erased shape of the call it serves ([`lambda_form_descriptor`]).
pub(in crate::runtime::stackwalker) const HIDDEN_INVOKE_METHODS: &[StandinMethod] = &[
    // Directly below a static target: its direct method handle's
    // pregenerated LambdaForm, at its `linkToStatic` call.
    StandinMethod {
        class: HIDDEN_INVOKE_DMH_HOLDER,
        name: "invokeStatic",
        descriptor: "",
        hidden: true,
        ..StandinMethod::ROW
    },
    // Directly below a statically bound instance target (a private or final
    // method, or a method of a final class; not an interface's): its direct
    // method handle's pregenerated LambdaForm, at its `linkToSpecial` call.
    // HotSpot settles every handle of such a member as `REF_invokeSpecial`
    // (`init_method_MemberName`'s `direct_call`), however it was looked up.
    // Interpreter round i5 wave 108, lane L3.
    StandinMethod {
        class: HIDDEN_INVOKE_DMH_HOLDER,
        name: "invokeSpecial",
        descriptor: "",
        hidden: true,
        ..StandinMethod::ROW
    },
    // Directly below a virtually bound instance target of a class no
    // interface declares the method of (wave 108 hand-back): `invokeVirtual`,
    // at its `linkToVirtual` call.
    StandinMethod {
        class: HIDDEN_INVOKE_DMH_HOLDER,
        name: "invokeVirtual",
        descriptor: "",
        hidden: true,
        ..StandinMethod::ROW
    },
    // Directly below an interface member a LINKED reflective call runs (JIT
    // round 22 wave 2, lane runtime): the accessor's `findVirtual` handle on
    // an interface is `REF_invokeInterface`, at its `linkToInterface` call.
    // Named only where the kind is known (`hidden_invoke_standin_frames_with`).
    StandinMethod {
        class: HIDDEN_INVOKE_DMH_HOLDER,
        name: "invokeInterface",
        descriptor: "",
        hidden: true,
        ..StandinMethod::ROW
    },
    // The invoker an `invokeExact` / `invoke` call site links to (the
    // accessor's own `invokeExact` for `Method.invoke`), at its
    // `invokeBasic` call, when its erased shape is pregenerated.
    StandinMethod {
        class: HIDDEN_INVOKE_INVOKERS_HOLDER,
        name: "invokeExact_MT",
        descriptor: "",
        hidden: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: HIDDEN_INVOKE_INVOKERS_HOLDER,
        name: "invoke_MT",
        descriptor: "",
        hidden: true,
        ..StandinMethod::ROW
    },
    // `Method.invoke`: the accessor's `@Hidden` `invokeImpl`, at the
    // `invokeExact` its `paramCount` switch picks (`""`: by arity).
    StandinMethod {
        class: HIDDEN_INVOKE_METHOD_ACCESSOR,
        name: "invokeImpl",
        descriptor: "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        calls: Some(("java/lang/invoke/MethodHandle", "invokeExact", "")),
        hidden: true,
        ..StandinMethod::ROW
    },
    // `Constructor.newInstance`: the constructor's direct method handle's
    // pregenerated LambdaForm (a constructor's kind is always
    // `newInvokeSpecial`), and the accessor's `@Hidden` `invokeImpl`.
    StandinMethod {
        class: HIDDEN_INVOKE_DMH_HOLDER,
        name: "newInvokeSpecial",
        descriptor: "",
        hidden: true,
        ..StandinMethod::ROW
    },
    StandinMethod {
        class: HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR,
        name: "invokeImpl",
        descriptor: "([Ljava/lang/Object;)Ljava/lang/Object;",
        calls: Some(("java/lang/invoke/MethodHandle", "invokeExact", "")),
        hidden: true,
        ..StandinMethod::ROW
    },
];

pub(in crate::runtime::stackwalker) const HIDDEN_INVOKE_METHOD_ACCESSOR: &str = "jdk/internal/reflect/DirectMethodHandleAccessor";
pub(in crate::runtime::stackwalker) const HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR: &str =
    "jdk/internal/reflect/DirectConstructorHandleAccessor";

pub(in crate::runtime::stackwalker) const HIDDEN_INVOKE_DMH_HOLDER: &str = "java/lang/invoke/DirectMethodHandle$Holder";
pub(in crate::runtime::stackwalker) const HIDDEN_INVOKE_INVOKERS_HOLDER: &str = "java/lang/invoke/Invokers$Holder";

/// The pregenerated holder classes the hidden method-handle frames name,
/// which the VM never loads for a call it serves natively (HotSpot has them
/// loaded from its archive): the caller loads them before
/// [`hidden_invoke_standin_frames_with`] asks.
pub(crate) const HIDDEN_INVOKE_HOLDER_CLASSES: [&str; 2] =
    [HIDDEN_INVOKE_DMH_HOLDER, HIDDEN_INVOKE_INVOKERS_HOLDER];

/// Does an interface `class` (or a superclass of it) implements, directly or
/// through superinterfaces, declare `name descriptor`? Fail-closed: a class
/// the store does not hold, or a walk past its bounds, answers `true` (the
/// caller then names nothing). Interpreter round i5 wave 108, lane L3.
pub(in crate::runtime::stackwalker) fn superinterface_declares(store: &ClassStore, class: &Class, name: &str, descriptor: &str) -> bool {
    let mut pending: Vec<ClassId> = class.interfaces.clone();
    let mut next = class.superclass;
    for _ in 0..256 {
        let Some(id) = next else {
            break;
        };
        let Some(superclass) = store.get(id) else {
            return true;
        };
        pending.extend(superclass.interfaces.iter().copied());
        next = superclass.superclass.filter(|s| *s != id);
    }
    if next.is_some() {
        return true;
    }
    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(id) = pending.pop() {
        if seen.contains(&id) {
            continue;
        }
        if seen.len() >= 512 {
            return true;
        }
        seen.push(id);
        let Some(iface) = store.get(id) else {
            return true;
        };
        if iface
            .methods
            .iter()
            .any(|m| &*m.name == name && &*m.descriptor == descriptor)
        {
            return true;
        }
        pending.extend(iface.interfaces.iter().copied());
    }
    false
}

/// The [`StandinFamily::HiddenInvoke`] row of `class.name`.
pub(in crate::runtime::stackwalker) fn hidden_invoke_row(class: &str, name: &str) -> Option<&'static StandinMethod> {
    standin_rows(StandinFamily::HiddenInvoke)
        .iter()
        .find(|row| row.class == class && row.name == name)
}

/// Is `class_id` the class `ancestor` or a subtype of it: its superclass chain
/// and (JIT round 22 wave 2, lane runtime; review
/// `r22-review-w2-findings-FIXED-20261020.md` item 4) every superinterface of the
/// class and of its superclasses, transitively -- the types a virtual or
/// interface selection from `ancestor` can land in, a default method's
/// interface included. Bounded; fail-closed `false`.
pub(in crate::runtime::stackwalker) fn class_is_or_extends(store: &ClassStore, class_id: ClassId, ancestor: ClassId) -> bool {
    let mut pending: Vec<ClassId> = vec![class_id];
    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(id) = pending.pop() {
        if id == ancestor {
            return true;
        }
        if seen.contains(&id) {
            continue;
        }
        if seen.len() >= 512 {
            return false;
        }
        seen.push(id);
        let Some(class) = store.get(id) else {
            continue;
        };
        pending.extend(class.superclass.filter(|s| *s != id));
        pending.extend(class.interfaces.iter().copied());
    }
    false
}

/// The linked-member row (`ReflectLinkIds::adapters`) the frame `target` runs:
/// its name and descriptor, and its class the row's declaring class or a
/// subclass of it (a virtually selected override). `None` without one.
pub(in crate::runtime::stackwalker) fn linked_row_for<'l>(
    store: &ClassStore,
    target: &StackTraceEntry,
    linked: &'l [crate::vm::realms::jit_realm::ReflectLinkAdapter],
) -> Option<&'l crate::vm::realms::jit_realm::ReflectLinkAdapter> {
    if linked.is_empty() {
        return None;
    }
    let target_class = target.class_id?;
    let target_descriptor = target.method_descriptor.as_deref()?;
    let named = |row: &&crate::vm::realms::jit_realm::ReflectLinkAdapter| {
        row.method_name == &*target.method_name && row.descriptor == target_descriptor
    };
    linked
        .iter()
        .filter(named)
        .find(|row| class_is_or_extends(store, target_class, ClassId::new(row.declaring_class_id)))
        .or_else(|| {
            // Wave 2 (review item 4): an interface's default method selected
            // for a receiver whose CLASS chain declares the member abstract
            // (`abstract class A { abstract void m(); } interface I { default
            // void m() {} } class C extends A implements I`): the frame's
            // interface is no subtype of `A`, the receiver is. Only for a
            // default-method frame, and only a class-declared row.
            //
            // Interpreter round i7 wave 135, lane L3: and only a row whose
            // class declares the member ABSTRACT, the one shape the comment
            // above names. A class-declared row with a concrete body is
            // selected over any default method (JVMS §5.4.6), so its frame
            // is that class's or a subclass's and the first pass finds it.
            // Before, ANY class-declared row of the same name and descriptor
            // matched an unrelated interface's default-method frame (a linked
            // `A.run()V` row and a walk through an unlinked `J.run()V`
            // default), naming A's holder kind and A's spun adapter there.
            let interface_frame = store.get(target_class).is_some_and(|c| c.is_interface());
            interface_frame.then_some(())?;
            linked.iter().filter(named).find(|row| {
                store
                    .get(ClassId::new(row.declaring_class_id))
                    .is_some_and(|c| {
                        !c.is_interface()
                            && c
                                .find_method(row.method_name, row.descriptor)
                                .is_some_and(|m| m.is_abstract())
                    })
            })
        })
}

/// JIT round 22 wave 1, lane runtime (flip review R1 / R3,
/// `CRATONVM_JIT_REFLECT_ACCESSOR_LINK` / `CRATONVM_JIT_REFLECT_CTOR_LINK`):
/// the spun `LambdaForm$MH` adapter frame of a LINKED reflective call -- a
/// `target` frame directly under `DirectMethodHandleAccessor.invokeImpl` /
/// `DirectConstructorHandleAccessor.invokeImpl` standing at the `invokeExact`
/// of descriptor `site` (`jit::helpers::reflect_accessor_link_redirect` /
/// `reflect_ctor_link_redirect` call the member there, skipping the chain) --
/// from the adapter the link filed for that member (`linked`, rows of
/// `ReflectLinkIds::adapters`): the adapter's entry method of the call's basic
/// type (the handle, then the site's parameters) at its one `invokeBasic`
/// ([`spun_lambda_form_entry`]). `None` (nothing named) for any other caller,
/// no row for the member, or an entry the class does not have.
pub(in crate::runtime::stackwalker) fn linked_reflective_adapter_entry(
    store: &ClassStore,
    target: &StackTraceEntry,
    caller: &StackTraceEntry,
    site: Option<(&str, &str)>,
    linked: &[crate::vm::realms::jit_realm::ReflectLinkAdapter],
) -> Option<StackTraceEntry> {
    if linked.is_empty()
        || &*caller.method_name != "invokeImpl"
        || !matches!(
            &*caller.class_name,
            HIDDEN_INVOKE_METHOD_ACCESSOR | HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR
        )
    {
        return None;
    }
    let ("invokeExact", site_descriptor) = site? else {
        return None;
    };
    let row = linked_row_for(store, target, linked)?;
    let shape = lambda_form_descriptor(site_descriptor, 1, 0)?;
    spun_lambda_form_entry(
        store,
        ClassId::new(row.adapter_class_id),
        &row.adapter_method,
        &shape,
    )
}

/// The hidden method-handle frames a `SHOW_HIDDEN_FRAMES` walk lists between
/// the frames of `frames` (the walk's, INNERMOST first) -- see the section
/// comment -- as `(index, entries)`: `entries` (INNERMOST first) go before
/// `frames[index]`, the caller. A caller standing at an `invokevirtual
/// MethodHandle.invokeExact` / `invoke` gets its static target's (or
/// constructor's) `DirectMethodHandle$Holder` frame and the call site's
/// `Invokers$Holder` invoker frame; a `Method.invoke` /
/// `Constructor.newInstance` call (its accessor's `invoke` / `newInstance`
/// frame, which the reflective splice lists) gets the holder frame, the
/// accessor's `invokeImpl` and the `Invokers$Holder` frame of its
/// `invokeExact`. Fail-closed: a frame whose method, class or call is not
/// found is not listed.
///
/// With the adapters of linked reflective calls in `linked` (JIT round 22
/// wave 1, lane runtime; [`linked_reflective_adapter_entry`]): for such a call
/// the holder frame, the adapter frame and the invoker frame, HotSpot's three
/// hidden frames between the member and `invokeImpl`. (JIT round 22 wave 7,
/// lane review: the linked-free wrapper `hidden_invoke_standin_frames` had no
/// production caller left and is gone; pass `&[]` for no links.)
pub(crate) fn hidden_invoke_standin_frames_with(
    store: &ClassStore,
    frames: &[StackTraceEntry],
    linked: &[crate::vm::realms::jit_realm::ReflectLinkAdapter],
) -> Vec<(usize, Vec<StackTraceEntry>)> {
    let mut out = Vec::new();
    for (i, pair) in frames.windows(2).enumerate() {
        let (target, caller) = (&pair[0], &pair[1]);
        // Real linker/LambdaForm activations already contain the invocation
        // chain. The table describes only a served native door.
        let real_linker = |name: &str| {
            name.starts_with("java/lang/invoke/LambdaForm$")
                || name == HIDDEN_INVOKE_DMH_HOLDER
                || name == HIDDEN_INVOKE_INVOKERS_HOLDER
        };
        if real_linker(&target.class_name) || real_linker(&caller.class_name) {
            continue;
        }
        // The accessor frame the reflective splice lists for a
        // `Method.invoke` / `Constructor.newInstance` call.
        let accessor = match (&*caller.class_name, &*caller.method_name) {
            (HIDDEN_INVOKE_METHOD_ACCESSOR, "invoke") => Some(HIDDEN_INVOKE_METHOD_ACCESSOR),
            (HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR, "newInstance") => {
                Some(HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR)
            }
            _ => None,
        };
        // A real JDK accessor already has invokeImpl on the stack. Only a
        // served accessor has its target directly above invoke/newInstance.
        if accessor.is_some()
            && target.class_name == caller.class_name
            && &*target.method_name == "invokeImpl"
        {
            continue;
        }
        // JIT round 22 wave 1, lane runtime: a linked reflective call whose
        // compiled `invokeImpl` frame published no bci still stands at the
        // `invokeExact` of its member's arity (the link serves 0-3 parameters,
        // the specialised arms).
        let linked_site: Option<String> = if accessor.is_none()
            && caller.byte_code_index < 0
            && &*caller.method_name == "invokeImpl"
            && matches!(
                &*caller.class_name,
                HIDDEN_INVOKE_METHOD_ACCESSOR | HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR
            )
            && linked_row_for(store, target, linked).is_some()
        {
            target
                .method_descriptor
                .as_deref()
                .and_then(descriptor_parameter_count)
                .and_then(|n| invoke_impl_entry(store, &caller.class_name, n))
                .map(|(_, descriptor)| descriptor)
        } else {
            None
        };
        let site = if accessor.is_some() {
            None
        } else {
            match method_handle_invoke_at(store, caller) {
                Some(site) => Some(site),
                None => match linked_site.as_deref() {
                    Some(descriptor) => Some(("invokeExact", descriptor)),
                    None => continue,
                },
            }
        };
        let Some((_, target_class, _, target_method)) = entry_method(store, target) else {
            continue;
        };
        let constructor = &*target_method.name == "<init>";
        let accessor_mismatch = match accessor {
            Some(HIDDEN_INVOKE_CONSTRUCTOR_ACCESSOR) => !constructor,
            Some(_) => constructor,
            None => false,
        };
        if accessor_mismatch {
            // An accessor above a frame that is not its target: nothing this
            // table can name.
            continue;
        }
        let mut entries = Vec::new();
        // A spun adapter is a real activation, not the direct target of the
        // caller's handle. Its invokeBasic serves the direct target below it;
        // invokeExact above it contributes only the call-site invoker.
        let adapter = target
            .class_name
            .starts_with("java/lang/invoke/LambdaForm$");
        // JIT round 22 wave 2, lane runtime: a LINKED reflective call knows
        // its member's reference kind (the accessor's `findVirtual` /
        // `findStatic` handle), so its holder is named by that kind -- an
        // interface member's `invokeInterface` included, which the native
        // door's rule below must leave out (its handle's kind is not in the
        // frames).
        let linked_kind = if !adapter
            && !constructor
            && &*caller.method_name == "invokeImpl"
            && &*caller.class_name == HIDDEN_INVOKE_METHOD_ACCESSOR
        {
            linked_row_for(store, target, linked).map(|row| row.holder_kind)
        } else {
            None
        };
        let holder = if adapter {
            None
        } else if let Some(kind) = linked_kind {
            let (name, leading) = match kind {
                3 => ("invokeStatic", 1),
                1 => ("invokeSpecial", 2),
                2 => ("invokeInterface", 2),
                _ => ("invokeVirtual", 2),
            };
            lambda_form_descriptor(&target_method.descriptor, leading, 0)
                .and_then(|shape| holder_entry(store, HIDDEN_INVOKE_DMH_HOLDER, name, &shape))
        } else if constructor {
            // `newInvokeSpecial` returns the new object.
            target_method
                .descriptor
                .split_once(')')
                .and_then(|(params, _)| {
                    lambda_form_descriptor(&format!("{params})Ljava/lang/Object;"), 1, 0)
                })
                .and_then(|shape| {
                    holder_entry(store, HIDDEN_INVOKE_DMH_HOLDER, "newInvokeSpecial", &shape)
                })
        } else if target_method.is_static() {
            lambda_form_descriptor(&target_method.descriptor, 1, 0).and_then(|shape| {
                holder_entry(store, HIDDEN_INVOKE_DMH_HOLDER, "invokeStatic", &shape)
            })
        } else if !target_class.is_interface()
            && (target_class.is_final()
                || target_method.access_flags.intersects(
                    cratonvm_reader::class_access_flags::MethodAccessFlags::FINAL
                        | cratonvm_reader::class_access_flags::MethodAccessFlags::PRIVATE,
                ))
        {
            // Interpreter round i5 wave 108, lane L3: a statically bound
            // instance target's holder is `invokeSpecial` whatever the
            // lookup (the handle and its receiver lead the shape).
            lambda_form_descriptor(&target_method.descriptor, 2, 0).and_then(|shape| {
                holder_entry(store, HIDDEN_INVOKE_DMH_HOLDER, "invokeSpecial", &shape)
            })
        } else if !target_class.is_interface()
            && !superinterface_declares(
                store,
                target_class,
                &target_method.name,
                &target_method.descriptor,
            )
        {
            // Wave 108 hand-back (`L3W108FinalVirtualHandleKind` row `plain
            // findVirtual below`): a virtually bound method of a class that
            // no interface declares can only be reached by a `findVirtual` /
            // `unreflect` / bound handle on a class (`invokeVirtual`), or by a
            // `findSpecial` (`invokeSpecial`, the residual this names wrongly;
            // nothing was named before). A method an interface declares may
            // be an `$Interface` handle's (`invokeInterface`): left out.
            lambda_form_descriptor(&target_method.descriptor, 2, 0).and_then(|shape| {
                holder_entry(store, HIDDEN_INVOKE_DMH_HOLDER, "invokeVirtual", &shape)
            })
        } else {
            None
        };
        entries.extend(holder);
        // JIT round 22 wave 1, lane runtime: a linked reflective call's spun
        // adapter, between the holder and the call site's invoker.
        entries.extend(linked_reflective_adapter_entry(
            store, target, caller, site, linked,
        ));
        if let Some((name, descriptor)) = site {
            entries.extend(invoker_holder_entry(store, name, descriptor));
        }
        if let Some(accessor) = accessor {
            let params = descriptor_parameter_count(&target_method.descriptor);
            if let Some((invoke_impl, exact)) =
                params.and_then(|n| invoke_impl_entry(store, accessor, n))
            {
                entries.extend(invoker_holder_entry(store, "invokeExact", &exact));
                entries.push(invoke_impl);
            }
        }
        if !entries.is_empty() {
            out.push((i + 1, entries));
        }
    }
    hidden_invoke_trace_dbg(&out);
    out
}

/// The frame of `name descriptor` of the spun `LambdaForm$MH` class
/// `class_id` at its one `MethodHandle.invokeBasic` call, as a real
/// activation of it lists (`NativeContext::spun_lambda_form_frame`).
/// Interpreter round i6 wave 115, lane L3 (`CRATONVM_STANDIN_SPUN_IWA_FRAMES`):
/// the class is one the JDK's `InvokerBytecodeGenerator` defined, so its
/// bytecode is HotSpot's and the call's bci is HotSpot's bci. Fail-closed: a
/// class not named `java/lang/invoke/LambdaForm$MH...`, a method it lacks, or
/// a method with no or several `invokeBasic` calls answers `None`.
pub(crate) fn spun_lambda_form_entry(
    store: &ClassStore,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<StackTraceEntry> {
    let class = store.get(class_id)?;
    // Interpreter round i7 wave 137, lane L3: a spun `LambdaForm$DMH` class
    // too (a customized direct handle's form, whose frame HotSpot lists in
    // place of the `DirectMethodHandle$Holder` one), standing at its one
    // `MethodHandle.linkTo*` call.
    // Interpreter round i7 wave 138, lane L3: also the pregenerated
    // `Invokers$Holder` (its `invokeExact_MT` linker, which a served
    // `invokeWithArguments` whose adapter ran as a real activation leaves
    // out), at its one `invokeBasic` call.
    let callees: &[&str] = if class.name.starts_with("java/lang/invoke/LambdaForm$MH")
        || &*class.name == "java/lang/invoke/Invokers$Holder"
    {
        &["invokeBasic"]    } else if class.name.starts_with("java/lang/invoke/LambdaForm$DMH") {
        &["linkToStatic", "linkToVirtual", "linkToSpecial", "linkToInterface"]    } else {
        return None;
    };
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    let code = &method.code()?.code;
    let mut site = None;
    for callee in callees {
        let Some(first) = named_call_site(&class.constant_pool, code, callee, None, true) else {
            continue;
        };
        let last = named_call_site(&class.constant_pool, code, callee, None, false)?;
        if first != last || site.is_some() {
            return None;
        }
        site = Some(first);
    }
    Some(standin_entry(class, class_id, method, index, Some(site?)))
}

/// [`spun_lambda_form_entry`] for a spun `LambdaForm$MH` method with SEVERAL
/// `MethodHandle.invokeBasic` calls (an adapter whose argument or return
/// filters are bound handles, `MethodHandleImpl.makePairwiseConvertByEditor`):
/// the frame at the `ordinal`-th one (0-based, in code order), and only when
/// the method makes exactly `count` of them -- the caller counted the form's
/// `invokeBasic` names, which the JDK's `InvokerBytecodeGenerator` emits in
/// name order. Interpreter round i7 wave 139, lane L3 (page
/// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, shape 2:
/// `fresh-int`, an `(int)int` handle's `asType` adapter, whose target call is
/// the second of three).
pub(crate) fn spun_lambda_form_entry_at(
    store: &ClassStore,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
    ordinal: usize,
    count: usize,
) -> Option<StackTraceEntry> {
    let class = store.get(class_id)?;
    if !class.name.starts_with("java/lang/invoke/LambdaForm$MH") {
        return None;
    }
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    let code = &method.code()?.code;
    let sites = named_call_sites(&class.constant_pool, code, "invokeBasic")?;
    if sites.len() != count {
        return None;
    }
    let pc = *sites.get(ordinal)?;
    Some(standin_entry(class, class_id, method, index, Some(pc)))
}

/// Every pc of a call to a method named `name` in `code`, in code order, or
/// `None` when the code does not decode.
pub(in crate::runtime::stackwalker) fn named_call_sites(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    name: &str,
) -> Option<Vec<usize>> {
    use cratonvm_reader::instruction::Instruction;
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index)
            | Instruction::Invokeinterface { index, .. } => Some(index),
            _ => None,
        };
        if let Some((_, callee, _)) = index.and_then(|index| cp_method_ref(cp, index)) {
            if callee == name {
                out.push(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    Some(out)
}

/// The `STTRACE_DBG_STANDIN kind=hidden-invoke` lines of
/// [`hidden_invoke_standin_frames_with`]' answer.
pub(in crate::runtime::stackwalker) fn hidden_invoke_trace_dbg(out: &[(usize, Vec<StackTraceEntry>)]) {
    if crate::runtime::env_cache::dbg_sttrace() {
        for (at, entries) in out {
            for e in entries {
                eprintln!(
                    "STTRACE_DBG_STANDIN kind=hidden-invoke at={at} {}.{}{}",
                    e.class_name,
                    e.method_name,
                    e.method_descriptor.as_deref().unwrap_or("")
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lambda proxy frames in a `SHOW_HIDDEN_FRAMES` walk (interpreter round i1
// wave 50, lane L3; `VmConfig::standin_lambda_proxy_frames`).
//
// HotSpot runs a lambda's implementation method below the SAM method of the
// hidden proxy class `LambdaMetafactory` spun for its call site
// (`Outer$$Lambda/0x….apply`, a forwarding body: load the captures and the
// SAM's arguments, convert them, call the implementation). The frame is
// hidden, so only a walk that shows hidden frames lists it. This VM's lambda
// dispatch runs the implementation directly as the callee of the
// `invokeinterface` and pushes no proxy frame
// (docs/feature-designs/real-jdk-mode.md).
// Recording "entered through a proxy" at dispatch would be a store per lambda
// call; this reads it from the walk instead, fail-closed: a frame is listed
// only when the caller stands at an `invokeinterface` of the SAM, the frame
// above it is the implementation of exactly one proxy of that SAM, and the
// frame's class is not itself an implementation of the called interface (a
// direct call). Anything else lists nothing, as before.
// ---------------------------------------------------------------------------

/// A lambda proxy whose implementation method one of a walk's frames runs:
/// its call site and its internal name (`<host>$$Lambda/0x<id>`, the name
/// `Class.getName()` answers, `lang_class::lambda_proxy_class_name`).
pub(crate) struct LambdaProxyCandidate {
    pub(crate) site: Arc<crate::classloading::resolution::LambdaCallSite>,
    pub(crate) name: Arc<str>,
}

/// Is `site`'s implementation method the method `entry` runs?
pub(crate) fn lambda_site_implements(
    site: &crate::classloading::resolution::LambdaCallSite,
    entry: &StackTraceEntry,
) -> bool {
    let handle = &site.impl_handle;
    *handle.member_name == *entry.method_name
        && *handle.class_name == *entry.class_name
        && entry
            .method_descriptor
            .as_deref()
            .is_some_and(|d| *handle.descriptor == *d)
}

/// Is `entry` the `<clinit>` that `site`'s proxy triggers before its
/// implementation runs? HotSpot's proxy forwards with an `invokestatic` (or a
/// `new` for a constructor reference) of the implementation's class, which
/// initializes it, so the `<clinit>` runs ABOVE the proxy's SAM frame; this
/// VM's lambda dispatch initializes it with no proxy frame and the `<clinit>`
/// stands right above the call site. Only those two kinds: a receiver call's
/// class is initialized already. Interpreter round i5 wave 102, lane L3
/// (`L3W100ClinitTriggerWalks` row `lambda hidden`).
pub(crate) fn lambda_site_initializes(
    site: &crate::classloading::resolution::LambdaCallSite,
    entry: &StackTraceEntry,
) -> bool {
    use crate::classloading::resolution::MethodHandleKind as Kind;
    let handle = &site.impl_handle;
    matches!(handle.kind, Kind::InvokeStatic | Kind::NewInvokeSpecial)
        && &*entry.method_name == "<clinit>"
        && *handle.class_name == *entry.class_name
}

/// May `entry` be the frame a proxy of `site` stands below
/// ([`lambda_site_implements`] or [`lambda_site_initializes`])? The screen the
/// proxy-candidate scans use.
pub(crate) fn lambda_site_stands_below(
    site: &crate::classloading::resolution::LambdaCallSite,
    entry: &StackTraceEntry,
) -> bool {
    lambda_site_implements(site, entry) || lambda_site_initializes(site, entry)
}

/// The lambda proxy frames a `SHOW_HIDDEN_FRAMES` walk lists between the
/// frames of `frames` (INNERMOST first), as `(index, [proxy frame])` to insert
/// before `frames[index]` (the caller); see the section comment. `candidates`
/// are the proxies whose implementation some frame runs.
pub(crate) fn lambda_proxy_standin_frames(
    store: &ClassStore,
    frames: &[StackTraceEntry],
    candidates: &[LambdaProxyCandidate],
) -> Vec<(usize, Vec<StackTraceEntry>)> {
    let mut out = Vec::new();
    if candidates.is_empty() {
        return out;
    }
    for (i, pair) in frames.windows(2).enumerate() {
        let (target, caller) = (&pair[0], &pair[1]);
        // A `<clinit>` the proxy's forwarding call triggered (wave 102): the
        // proxy frame stands at that call (a constructor reference's `new`
        // at bci 0) and its forwarding call is the only one of its class's
        // proxies of this SAM (the screen below).
        let initializing = &*target.method_name == "<clinit>";
        let mut matching = candidates.iter().filter(|c| {
            if initializing {
                lambda_site_initializes(&c.site, target)
            } else {
                lambda_site_implements(&c.site, target)
            }
        });
        let Some(first) = matching.next() else {
            continue;
        };
        let Some((interface, name, descriptor)) = interface_call_at(store, caller) else {
            continue;
        };
        if *first.site.sam_method_name != *name || *first.site.sam_descriptor != *descriptor {
            continue;
        }
        // Two proxies of one implementation and SAM (a method reference
        // written twice): which one ran is not recorded.
        if matching
            .any(|c| *c.site.sam_method_name == *name && *c.site.sam_descriptor == *descriptor)
        {
            continue;
        }
        // A direct `invokeinterface` of a class that implements the interface
        // itself reaches the same method with no proxy.
        if target
            .class_id
            .is_none_or(|id| class_reaches_name(store, id, interface))
        {
            continue;
        }
        let Some(mut entry) = lambda_proxy_standin_entry(&first.site, &first.name) else {
            continue;
        };
        if initializing
            && matches!(
                first.site.impl_handle.kind,
                crate::classloading::resolution::MethodHandleKind::NewInvokeSpecial
            )
        {
            // The forwarding body's `new` (bci 0) initializes the class.
            entry.byte_code_index = 0;
        }
        out.push((i + 1, vec![entry]));
    }
    if crate::runtime::env_cache::dbg_sttrace() {
        for (at, entries) in &out {
            for e in entries {
                eprintln!(
                    "STTRACE_DBG_STANDIN kind=lambda-proxy at={at} {}.{}{} bci={}",
                    e.class_name,
                    e.method_name,
                    e.method_descriptor.as_deref().unwrap_or(""),
                    e.byte_code_index
                );
            }
        }
    }
    out
}

/// The SAM frame of the lambda proxy `site` (internal name `name`,
/// `<host>$$Lambda/0x<id>`) standing at its forwarding call of the
/// implementation, as [`lambda_proxy_standin_frames`] lists it; `None` when
/// that call's bci cannot be derived. Shared with `ThreadInfo`'s hidden
/// `Thread.run` frames (JIT round 19 wave 11, lane gc11,
/// `vm_exec::add_thread_info_hidden_run_entries`).
pub(crate) fn lambda_proxy_standin_entry(
    site: &crate::classloading::resolution::LambdaCallSite,
    name: &Arc<str>,
) -> Option<StackTraceEntry> {
    let bci = lambda_forwarding_call_bci(site)?;
    Some(StackTraceEntry {
        class_name: Arc::clone(name),
        method_name: Arc::clone(&site.sam_method_name),
        method_descriptor: Some(Arc::clone(&site.sam_descriptor)),
        // A spun proxy has no `SourceFile` and no line table.
        source_file: None,
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(site.proxy_class_id),
        method_index: None,
    })
}

/// `a` and `b` (each `(index, entries)`, indices ascending) as one list,
/// indices ascending; at one index `a`'s entries come first (innermost).
pub(crate) fn merge_standin_insertions(
    a: Vec<(usize, Vec<StackTraceEntry>)>,
    b: Vec<(usize, Vec<StackTraceEntry>)>,
) -> Vec<(usize, Vec<StackTraceEntry>)> {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let mut out: Vec<(usize, Vec<StackTraceEntry>)> = Vec::with_capacity(a.len() + b.len());
    let mut b = b.into_iter().peekable();
    for (at, mut entries) in a {
        while let Some(item) = b.next_if(|(bat, _)| *bat < at) {
            out.push(item);
        }
        if let Some((_, more)) = b.next_if(|(bat, _)| *bat == at) {
            entries.extend(more);
        }
        out.push((at, entries));
    }
    out.extend(b);
    out
}

/// The `(interface, name, descriptor)` of the `invokeinterface` the frame
/// `caller` stands at, if it does.
pub(crate) fn interface_call_at<'s>(
    store: &'s ClassStore,
    caller: &StackTraceEntry,
) -> Option<(&'s str, &'s str, &'s str)> {
    use cratonvm_reader::instruction::Instruction;
    let bci = usize::try_from(caller.byte_code_index).ok()?;
    let (_, class, _, method) = entry_method(store, caller)?;
    let code = method.code()?;
    match Instruction::decode(&code.code, bci).ok()?.0 {
        Instruction::Invokeinterface { index, .. } => cp_method_ref(&class.constant_pool, index),
        _ => None,
    }
}

/// Does class `id`, a superclass or a superinterface of it, have the name
/// `name`? Bounded: a malformed hierarchy answers `true` (fail-closed for the
/// caller, which then lists no proxy frame).
pub(in crate::runtime::stackwalker) fn class_reaches_name(store: &ClassStore, id: ClassId, name: &str) -> bool {
    let mut pending = vec![id];
    let mut seen = 0usize;
    while let Some(id) = pending.pop() {
        seen += 1;
        if seen > 4096 {
            return true;
        }
        let Some(class) = store.get(id) else {
            continue;
        };
        if *class.name == *name {
            return true;
        }
        pending.extend(class.superclass);
        pending.extend(class.interfaces.iter().copied());
    }
    false
}

/// The bci of the implementation call in the forwarding body HotSpot's
/// `InnerClassLambdaMetafactory.forwardingMethod` spins for `site` (JDK 25):
/// `new`/`dup` for a constructor, `aload_0`/`getfield` per capture, a
/// direct-method-handle `ldc` for an `invokespecial` implementation, then per
/// SAM argument its load and `TypeConvertingMethodAdapter.convertType`'s
/// conversion. `None` when a descriptor does not parse.
pub(in crate::runtime::stackwalker) fn lambda_forwarding_call_bci(
    site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<i32> {
    use crate::classloading::resolution::MethodHandleKind as Kind;
    let sam = descriptor_field_types(&site.sam_descriptor)?;
    let functional = descriptor_field_types(&site.instantiated_descriptor)?;
    let mut target = descriptor_field_types(&site.impl_handle.descriptor)?;
    let receiver = format!("L{};", site.impl_handle.class_name);
    if matches!(
        site.impl_handle.kind,
        Kind::InvokeVirtual | Kind::InvokeInterface | Kind::InvokeSpecial
    ) {
        target.insert(0, &receiver);
    }
    let captures = site.capture_types.len();
    if functional.len() != sam.len() || target.len() != captures + sam.len() {
        return None;
    }
    let mut pc = match site.impl_handle.kind {
        Kind::NewInvokeSpecial => 4,
        // `useImplMethodHandle`: an `ldc` of the class-data handle.
        Kind::InvokeSpecial => 2,
        _ => 0,
    };
    pc += 4 * captures;
    let mut slot = 1usize;
    for (i, arg) in sam.iter().enumerate() {
        pc += match slot {
            0..=3 => 1,
            4..=255 => 2,
            _ => 4,
        };
        slot += if matches!(*arg, "J" | "D") { 2 } else { 1 };
        pc += forwarding_conversion_len(arg, target[captures + i], functional[i]);
    }
    i32::try_from(pc).ok()
}

/// The byte length `TypeConvertingMethodAdapter.convertType(arg, target,
/// functional)` emits (JDK 25), on field descriptors.
pub(in crate::runtime::stackwalker) fn forwarding_conversion_len(arg: &str, target: &str, functional: &str) -> usize {
    fn primitive(d: &str) -> bool {
        d.len() == 1
    }
    fn wrapped(d: &str) -> Option<&'static str> {
        Some(match d {
            "Ljava/lang/Integer;" => "I",
            "Ljava/lang/Long;" => "J",
            "Ljava/lang/Boolean;" => "Z",
            "Ljava/lang/Short;" => "S",
            "Ljava/lang/Byte;" => "B",
            "Ljava/lang/Character;" => "C",
            "Ljava/lang/Float;" => "F",
            "Ljava/lang/Double;" => "D",
            _ => return None,
        })
    }
    fn loadable(d: &str) -> &str {
        match d {
            "Z" | "B" | "S" | "C" => "I",
            other => other,
        }
    }
    fn widen(from: &str, to: &str) -> usize {
        usize::from(loadable(from) != loadable(to))
    }
    fn cast(to: &str) -> usize {
        if to == "Ljava/lang/Object;" {
            0
        } else {
            3
        }
    }
    if arg == target && arg == functional {
        return 0;
    }
    if arg == "V" || target == "V" {
        return 0;
    }
    if primitive(arg) {
        if primitive(target) {
            return widen(arg, target);
        }
        return match wrapped(target) {
            Some(w) => widen(arg, w) + 3,
            None => 3 + cast(target),
        };
    }
    let (src, mut n) = if arg == functional || primitive(functional) {
        (arg, 0)
    } else {
        (functional, cast(functional))
    };
    if primitive(target) {
        n += match wrapped(src) {
            Some(w) if w == "C" || w == "Z" => 3 + widen(w, target),
            Some(_) => 3,
            None => 3 + 3,
        };
    } else if src != target {
        n += cast(target);
    }
    n
}

/// `descriptor`'s parameter types as field descriptors, or `None` when it
/// does not parse.
pub(in crate::runtime::stackwalker) fn descriptor_field_types(descriptor: &str) -> Option<Vec<&str>> {
    let body = descriptor.strip_prefix('(')?;
    let (params, _) = body.split_once(')')?;
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let start = pos;
        while bytes.get(pos) == Some(&b'[') {
            pos += 1;
        }
        match *bytes.get(pos)? {
            b'Z' | b'B' | b'S' | b'C' | b'I' | b'J' | b'F' | b'D' => pos += 1,
            b'L' => pos += bytes[pos..].iter().position(|b| *b == b';')? + 1,
            _ => return None,
        }
        out.push(&params[start..pos]);
    }
    Some(out)
}

/// `entry`'s class and method: `(class id, class, method slot, method)`.
pub(in crate::runtime::stackwalker) fn entry_method<'s>(
    store: &'s ClassStore,
    entry: &StackTraceEntry,
) -> Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> {
    let class_id = entry.class_id?;
    let class = store.get(class_id)?;
    let descriptor = entry.method_descriptor.as_deref()?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| *m.name == *entry.method_name && &*m.descriptor == descriptor)?;
    Some((class_id, class, index, method))
}

/// The `(name, descriptor)` of the `invokevirtual MethodHandle.invokeExact`
/// or `MethodHandle.invoke` the frame `caller` stands at, if it does.
pub(in crate::runtime::stackwalker) fn method_handle_invoke_at<'s>(
    store: &'s ClassStore,
    caller: &StackTraceEntry,
) -> Option<(&'static str, &'s str)> {
    let bci = usize::try_from(caller.byte_code_index).ok()?;
    let (_, class, _, method) = entry_method(store, caller)?;
    let code = method.code()?;
    if code.code.get(bci) != Some(&0xb6) {
        return None;
    }
    match invoke_target_at(&class.constant_pool, &code.code, bci)? {
        ("java/lang/invoke/MethodHandle", "invokeExact", descriptor) => {
            Some(("invokeExact", descriptor))
        }
        ("java/lang/invoke/MethodHandle", "invoke", descriptor) => Some(("invoke", descriptor)),
        ("java/lang/invoke/MethodHandle", "invokeBasic", descriptor) => {
            Some(("invokeBasic", descriptor))
        }
        _ => None,
    }
}

/// The type descriptor of the `invokevirtual MethodHandle.invokeExact` the
/// walk frame `caller` stands at ([`method_handle_invoke_at`]), or `None`.
/// Interpreter round i6 wave 121, lane L3: what
/// `NativeContext::method_handle_invoke_exact_site` answers.
pub(crate) fn method_handle_invoke_exact_site(
    store: &ClassStore,
    caller: &StackTraceEntry,
) -> Option<String> {
    match method_handle_invoke_at(store, caller)? {
        ("invokeExact", descriptor) => Some(descriptor.to_string()),
        _ => None,
    }
}

/// The type descriptor of the GENERIC `invokevirtual MethodHandle.invoke` the
/// walk frame `caller` stands at, or `None`. Interpreter round i7 wave 142,
/// lane L3: what `NativeContext::method_handle_generic_invoke_site` answers.
pub(crate) fn method_handle_generic_invoke_site(
    store: &ClassStore,
    caller: &StackTraceEntry,
) -> Option<String> {
    match method_handle_invoke_at(store, caller)? {
        ("invoke", descriptor) => Some(descriptor.to_string()),
        _ => None,
    }
}

/// The `Invokers$Holder` frame of the invoker a `MethodHandle.<name>` call
/// site of type `descriptor` links to (`invokeExact_MT` / `invoke_MT`: the
/// handle, the site's arguments as basic types, the appendix), or `None`
/// when that shape is not pregenerated (HotSpot spins a `LambdaForm$MH`
/// class for it, which is not listed).
pub(in crate::runtime::stackwalker) fn invoker_holder_entry(
    store: &ClassStore,
    name: &str,
    descriptor: &str,
) -> Option<StackTraceEntry> {
    let holder_name = match name {
        "invokeExact" => "invokeExact_MT",
        "invoke" => "invoke_MT",
        _ => return None,
    };
    let shape = lambda_form_descriptor(descriptor, 1, 1)?;
    holder_entry(store, HIDDEN_INVOKE_INVOKERS_HOLDER, holder_name, &shape)
}

/// The number of parameters `descriptor` declares, or `None` when it does
/// not parse.
pub(in crate::runtime::stackwalker) fn descriptor_parameter_count(descriptor: &str) -> Option<usize> {
    Some(descriptor_basic_types(descriptor)?.0.len())
}

/// `descriptor`'s parameter and return types as LambdaForm basic types
/// (`L` for a reference or array, `I` for every subword type, `J`, `F`, `D`,
/// and `V` for a void return), or `None` when it does not parse.
pub(in crate::runtime::stackwalker) fn descriptor_basic_types(descriptor: &str) -> Option<(Vec<char>, char)> {
    let body = descriptor.strip_prefix('(')?;
    let (params, ret) = body.split_once(')')?;
    fn basic(bytes: &[u8], pos: &mut usize) -> Option<char> {
        let tag = *bytes.get(*pos)?;
        *pos += 1;
        match tag {
            b'Z' | b'B' | b'S' | b'C' | b'I' => Some('I'),
            b'J' => Some('J'),
            b'F' => Some('F'),
            b'D' => Some('D'),
            b'V' => Some('V'),
            b'L' => {
                let end = bytes[*pos..].iter().position(|b| *b == b';')?;
                *pos += end + 1;
                Some('L')
            }
            b'[' => {
                while bytes.get(*pos) == Some(&b'[') {
                    *pos += 1;
                }
                basic(bytes, pos).map(|_| 'L')
            }
            _ => None,
        }
    }
    let bytes = params.as_bytes();
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        let t = basic(bytes, &mut pos)?;
        if t == 'V' {
            return None;
        }
        out.push(t);
    }
    let mut rpos = 0;
    let r = basic(ret.as_bytes(), &mut rpos)?;
    (rpos == ret.len()).then_some((out, r))
}

/// The descriptor of the LambdaForm method that serves a call of shape
/// `descriptor`: `leading` `Object` parameters first (the method handle),
/// then `descriptor`'s parameters as basic types, then `trailing` `Object`
/// parameters (an invoker's appendix), and its basic return type.
pub(in crate::runtime::stackwalker) fn lambda_form_descriptor(descriptor: &str, leading: usize, trailing: usize) -> Option<String> {
    const OBJECT: &str = "Ljava/lang/Object;";
    let (params, ret) = descriptor_basic_types(descriptor)?;
    let one = |t: char| match t {
        'L' => OBJECT.to_string(),
        other => other.to_string(),
    };
    let mut out = String::from("(");
    for _ in 0..leading {
        out.push_str(OBJECT);
    }
    for t in params {
        out.push_str(&one(t));
    }
    for _ in 0..trailing {
        out.push_str(OBJECT);
    }
    out.push(')');
    out.push_str(&one(ret));
    Some(out)
}

/// The frame of the holder method `class.name descriptor` (a
/// [`StandinFamily::HiddenInvoke`] row), at its last `invoke*` (the
/// `linkTo*` / `invokeBasic` the frame stands at while the target runs), or
/// `None` when the class is not loaded or has no such method.
pub(in crate::runtime::stackwalker) fn holder_entry(
    store: &ClassStore,
    class_name: &str,
    name: &str,
    descriptor: &str,
) -> Option<StackTraceEntry> {
    hidden_invoke_row(class_name, name)?;
    let (class_id, class, index, method) = named_method_in(store, class_name, name, descriptor)?;
    let pc = last_invoke_pc(&method.code()?.code)?;
    Some(standin_entry(class, class_id, method, index, Some(pc)))
}

/// The pc of the last `invoke*` instruction of `code`.
pub(in crate::runtime::stackwalker) fn last_invoke_pc(code: &[u8]) -> Option<usize> {
    use cratonvm_reader::instruction::Instruction;
    let mut found = None;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        if matches!(
            instruction,
            Instruction::Invokevirtual(_)
                | Instruction::Invokespecial(_)
                | Instruction::Invokestatic(_)
                | Instruction::Invokeinterface { .. }
        ) {
            found = Some(pc);
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    found}

/// The `invokeImpl` frame of `accessor` (`DirectMethodHandleAccessor` or
/// `DirectConstructorHandleAccessor`) for a target of `params` parameters,
/// at the `invokeExact` its `paramCount` switch runs -- `(Object, Object ×
/// params)Object` for a method (the receiver first), `(Object ×
/// params)Object` for a constructor, or the spreading `Object[]` form past
/// the specialised arities -- with that call's descriptor.
pub(in crate::runtime::stackwalker) fn invoke_impl_entry(
    store: &ClassStore,
    accessor: &str,
    params: usize,
) -> Option<(StackTraceEntry, String)> {
    const OBJECT: &str = "Ljava/lang/Object;";
    let row = hidden_invoke_row(accessor, "invokeImpl")?;
    let (owner, name, _) = row.call();
    let (class_id, class, index, method) =
        named_method_in(store, row.class, row.name, row.descriptor)?;
    let code = &method.code()?.code;
    let receiver = usize::from(accessor == HIDDEN_INVOKE_METHOD_ACCESSOR);
    let exact = format!("({}){OBJECT}", OBJECT.repeat(params + receiver));
    let spread = format!("({}[{OBJECT}){OBJECT}", OBJECT.repeat(receiver));
    let (pc, descriptor) = first_call_to(&class.constant_pool, code, owner, name, &exact)
        .map(|pc| (pc, exact))
        .or_else(|| {
            first_call_to(&class.constant_pool, code, owner, name, &spread).map(|pc| (pc, spread))
        })?;
    Some((
        standin_entry(class, class_id, method, index, Some(pc)),
        descriptor,
    ))
}

/// The accessor class `kind`'s JDK frames name, which the VM's native never
/// loads itself: the native entering the call loads it once per thread
/// (`JvmThread::reflective_accessors_loaded`), so a capture can name it.
/// `None` for a `doPrivileged` bridge: its frame names `AccessController`,
/// which is loaded when its native runs.
pub(crate) fn reflective_accessor_class(
    kind: cratonvm_native_api::ReflectiveCallKind,
) -> Option<&'static str> {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke => {
            Some("jdk/internal/reflect/DirectMethodHandleAccessor")
        }
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance => {
            Some("jdk/internal/reflect/DirectConstructorHandleAccessor")
        }
        cratonvm_native_api::ReflectiveCallKind::DoPrivileged
        | cratonvm_native_api::ReflectiveCallKind::DoPrivilegedException
        | cratonvm_native_api::ReflectiveCallKind::DoPrivilegedInContext => None,
        // JIT round 20, lane walkcompat: a served walk names classes its
        // native already loaded, except where it never builds the JDK
        // pipeline: then the traverser (and, for the `forEach` loop, the
        // stream head, [`stack_walk_extra_class`]) HotSpot's walk runs.
        cratonvm_native_api::ReflectiveCallKind::StackWalkerWalk
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachStream => None,
        cratonvm_native_api::ReflectiveCallKind::StackWalkerWalkFunction
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachAction => {
            Some(STACK_WALK_TRAVERSER)
        }
        // Wave 104: `MethodHandle` is loaded when its native runs.
        cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArguments
        | cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArgumentsList => None,
    }
}

/// The class a served walk of `kind` loads on its first entry on a thread
/// besides [`reflective_accessor_class`]'s: the stream head the `forEach`
/// loop's frames name.
pub(crate) fn stack_walk_extra_class(
    kind: cratonvm_native_api::ReflectiveCallKind,
) -> Option<&'static str> {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachAction => Some(STACK_WALK_HEAD),
        _ => None,
    }
}

/// `kind`'s slot in [`ReflectiveFramesNamed`] (`0..REFLECTIVE_KINDS`), or
/// `REFLECTIVE_KINDS` (past the memo) for a served `StackWalker` walk, whose
/// frames are named afresh at each capture rather than kept on the thread
/// (JIT round 20, lane walkcompat: four more slots would grow `JvmThread`
/// past its layout ratchet, and a capture inside a walk's function is rare).
/// Index the memo with `get` / `get_mut`.
pub(crate) fn reflective_kind_slot(kind: cratonvm_native_api::ReflectiveCallKind) -> usize {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::MethodInvoke => 0,
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance => 1,
        cratonvm_native_api::ReflectiveCallKind::DoPrivileged => 2,
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedException => 3,
        cratonvm_native_api::ReflectiveCallKind::DoPrivilegedInContext => 4,
        cratonvm_native_api::ReflectiveCallKind::StackWalkerWalk
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerWalkFunction
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachStream
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachAction
        | cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArguments
        | cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArgumentsList => {
            REFLECTIVE_KINDS
        }
    }
}

/// How many [`cratonvm_native_api::ReflectiveCallKind`]s have a memo slot.
pub(crate) const REFLECTIVE_KINDS: usize = 5;

/// `kind`'s bit in `JvmThread::reflective_accessors_loaded`: its memo
/// slot's; for a served walk, bit 5 or 6 for the two kinds that load a class
/// ([`reflective_accessor_class`]), none for the other two.
pub(crate) fn reflective_kind_bit(kind: cratonvm_native_api::ReflectiveCallKind) -> u8 {
    match kind {
        cratonvm_native_api::ReflectiveCallKind::StackWalkerWalkFunction => 1 << 5,
        cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachAction => 1 << 6,
        cratonvm_native_api::ReflectiveCallKind::StackWalkerWalk
        | cratonvm_native_api::ReflectiveCallKind::StackWalkerForEachStream
        | cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArguments
        | cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArgumentsList => 0,
        _ => 1 << reflective_kind_slot(kind),
    }
}

/// The pc of the first `invoke*` in `code` that calls `owner.name
/// descriptor`, or `None` (a decode failure included).
pub(in crate::runtime::stackwalker) fn first_call_to(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    owner: &str,
    name: &str,
    descriptor: &str,
) -> Option<usize> {
    use cratonvm_reader::instruction::Instruction;
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let index = match instruction {
            Instruction::Invokevirtual(index)
            | Instruction::Invokespecial(index)
            | Instruction::Invokestatic(index) => Some(index),
            Instruction::Invokeinterface { index, .. } => Some(index),
            _ => None,
        };
        if let Some(index) = index {
            if matches!(cp_method_ref(cp, index), Some((o, n, d)) if o == owner && n == name && d == descriptor)
            {
                return Some(pc);
            }
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    None}

/// `kind`'s JDK frames, OUTERMOST first, each at its call of the next with
/// its line, or `None` when one of them cannot be named. The
/// `name_frames` of [`reflective_splices`], under the caller's class-manager
/// read guard.
pub(crate) fn reflective_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
) -> Option<Vec<StackTraceEntry>> {
    if kind.is_stack_walk() {
        return stack_walk_entries(store, kind);
    }
    if kind.is_invoke_with_arguments() {
        return invoke_with_arguments_entries(store, kind);
    }
    let steps = reflective_steps(kind);
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        let class_id = find_class_id_by_name_memoized(store, step.class)?;
        let class = store.get(class_id)?;
        let (index, method) = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == step.name && &*m.descriptor == step.descriptor)?;
        let code = method.code()?;
        let (owner, name, descriptor) = step.call();
        let pc = first_call_to(&class.constant_pool, &code.code, owner, name, descriptor)?;
        out.push(standin_entry(class, class_id, method, index, Some(pc)));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// A `StackWalker.walk` / `forEach` the VM serves natively (JIT round 20, lane
// walkcompat)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`.
// `StackWalker.walk` / `forEach` are natives (`p59_sw_walk` / `p59_sw_for_each`)
// that push no frame and run the JDK's `doStackWalk`, the walk's function, the
// JDK stream's `forEach` or the action themselves, so a stack captured INSIDE
// the function or action (a nested walk, a `Throwable`) lacked the JDK frames
// HotSpot runs between that call and the walk's caller. HotSpot 25.0.3
// (`R19Walk11Rows`, facts `walk11-known-nested-chain` /
// `walk11-known-nested-foreach-chain`), innermost first:
//
// ```text
// walk:    <function> consumeFrames doStackWalk callStackWalk* beginStackWalk walkHelper
//          AbstractStackWalker.walk StackWalker.walk <caller>
// forEach: <action> StackFrameTraverser.forEachRemaining ReferencePipeline$Head.forEach
//          StackWalker.lambda$forEach$0 consumeFrames doStackWalk callStackWalk* ...
//          AbstractStackWalker.walk StackWalker.forEach <caller>
// ```
//
// The natives record the call as the reflective doors do
// (`NativeExceptionAccess::enter_reflective_call`, one
// `ReflectiveCallKind::StackWalker*` kind per way the native enters Java), so
// every capture that lists the reflective steps lists these, right below the
// first frame the call pushed. Each frame stands at its first call of the next
// (its `LineNumberTable` line, found in the loaded class's bytes; looked up by
// NAME where a JDK's descriptor or the receiver's static type varies), and
// `callStackWalk` is the native leaf (line -2). None is hidden or a reflection
// frame, so every walk lists them; the walk's own top-of-stack screen
// (`lang_stackwalker::ordered_stack_walk_frames`) still strips a walk's own
// frames, which it never captures inside its own call. Fail-closed: a JDK
// whose walk runs other frames (JDK 17 has no `walkHelper`) lists none.

/// `java.lang.StackWalker`.
pub(in crate::runtime::stackwalker) const STACK_WALK_WALKER: &str = "java/lang/StackWalker";
/// `java.lang.StackStreamFactory$AbstractStackWalker`.
pub(in crate::runtime::stackwalker) const STACK_WALK_ABSTRACT: &str = "java/lang/StackStreamFactory$AbstractStackWalker";
/// `java.lang.StackStreamFactory$StackFrameTraverser`.
pub(crate) const STACK_WALK_TRAVERSER: &str = "java/lang/StackStreamFactory$StackFrameTraverser";
/// `java.util.stream.ReferencePipeline$Head`.
pub(crate) const STACK_WALK_HEAD: &str = "java/util/stream/ReferencePipeline$Head";
/// The erased `R` of the walk's own methods.
pub(in crate::runtime::stackwalker) const STACK_WALK_RETURNS_R: &str = "()Ljava/lang/Object;";
/// `forEach(Consumer)`, the stream's and the spliterator's.
pub(in crate::runtime::stackwalker) const STACK_WALK_FOR_EACH_DESC: &str = "(Ljava/util/function/Consumer;)V";

/// One JDK frame of a served walk: `class.name` (`descriptor`; `None`: the
/// class's first method of that name), standing at its first call of a
/// method named `calls.0` (descriptor `calls.1`, any when `None`), or the
/// native leaf when `calls` is `None`.
pub(in crate::runtime::stackwalker) struct StackWalkStep {
    pub(in crate::runtime::stackwalker) class: &'static str,
    pub(in crate::runtime::stackwalker) name: &'static str,
    pub(in crate::runtime::stackwalker) descriptor: Option<&'static str>,
    pub(in crate::runtime::stackwalker) calls: Option<(&'static str, Option<&'static str>)>,
}

/// `StackWalker.walk(Function)`, at its `makeStackTraverser(..).walk()` call.
pub(in crate::runtime::stackwalker) const STACK_WALK_WALK_HEAD: StackWalkStep = StackWalkStep {
    class: STACK_WALK_WALKER,
    name: "walk",
    descriptor: Some("(Ljava/util/function/Function;)Ljava/lang/Object;"),
    calls: Some(("walk", Some(STACK_WALK_RETURNS_R))),
};

/// `StackWalker.forEach(Consumer)`, at its `makeStackTraverser(..).walk()` call.
pub(in crate::runtime::stackwalker) const STACK_WALK_FOR_EACH_HEAD: StackWalkStep = StackWalkStep {
    class: STACK_WALK_WALKER,
    name: "forEach",
    descriptor: Some(STACK_WALK_FOR_EACH_DESC),
    calls: Some(("walk", Some(STACK_WALK_RETURNS_R))),
};

/// `AbstractStackWalker.walk` -> `walkHelper` -> `beginStackWalk` ->
/// `callStackWalk` (native), OUTERMOST first.
pub(in crate::runtime::stackwalker) const STACK_WALK_ENTRY_STEPS: [StackWalkStep; 4] = [
    StackWalkStep {
        class: STACK_WALK_ABSTRACT,
        name: "walk",
        descriptor: Some(STACK_WALK_RETURNS_R),
        calls: Some(("walkHelper", Some(STACK_WALK_RETURNS_R))),
    },
    StackWalkStep {
        class: STACK_WALK_ABSTRACT,
        name: "walkHelper",
        descriptor: Some(STACK_WALK_RETURNS_R),
        calls: Some(("beginStackWalk", Some(STACK_WALK_RETURNS_R))),
    },
    StackWalkStep {
        class: STACK_WALK_ABSTRACT,
        name: "beginStackWalk",
        descriptor: Some(STACK_WALK_RETURNS_R),
        calls: Some(("callStackWalk", None)),
    },
    StackWalkStep {
        class: STACK_WALK_ABSTRACT,
        name: "callStackWalk",
        descriptor: None,
        calls: None,
    },
];

/// `doStackWalk` -> `StackFrameTraverser.consumeFrames` -> the function's
/// `apply`, OUTERMOST first.
pub(in crate::runtime::stackwalker) const STACK_WALK_CONSUME_STEPS: [StackWalkStep; 2] = [
    StackWalkStep {
        class: STACK_WALK_ABSTRACT,
        name: "doStackWalk",
        descriptor: None,
        calls: Some(("consumeFrames", Some(STACK_WALK_RETURNS_R))),
    },
    StackWalkStep {
        class: STACK_WALK_TRAVERSER,
        name: "consumeFrames",
        descriptor: Some(STACK_WALK_RETURNS_R),
        calls: Some(("apply", Some("(Ljava/lang/Object;)Ljava/lang/Object;"))),
    },
];

/// `forEach`'s function, `s -> { s.forEach(action); return null; }`, at its
/// `forEach` call.
pub(in crate::runtime::stackwalker) const STACK_WALK_FOR_EACH_LAMBDA: StackWalkStep = StackWalkStep {
    class: STACK_WALK_WALKER,
    name: "lambda$forEach$0",
    descriptor: None,
    calls: Some(("forEach", Some(STACK_WALK_FOR_EACH_DESC))),
};

/// The stream's `forEach` -> the traverser's `forEachRemaining` -> the
/// action's `accept`, OUTERMOST first.
pub(in crate::runtime::stackwalker) const STACK_WALK_FOR_EACH_LOOP_STEPS: [StackWalkStep; 2] = [
    StackWalkStep {
        class: STACK_WALK_HEAD,
        name: "forEach",
        descriptor: Some(STACK_WALK_FOR_EACH_DESC),
        calls: Some(("forEachRemaining", Some(STACK_WALK_FOR_EACH_DESC))),
    },
    StackWalkStep {
        class: STACK_WALK_TRAVERSER,
        name: "forEachRemaining",
        descriptor: Some(STACK_WALK_FOR_EACH_DESC),
        calls: Some(("accept", Some("(Ljava/lang/Object;)V"))),
    },
];

/// A served walk's steps, OUTERMOST first (empty for any other kind).
pub(in crate::runtime::stackwalker) fn stack_walk_steps(kind: cratonvm_native_api::ReflectiveCallKind) -> Vec<&'static StackWalkStep> {
    use cratonvm_native_api::ReflectiveCallKind as Kind;
    let (head, consume, lambda, loop_steps) = match kind {
        Kind::StackWalkerWalk => (&STACK_WALK_WALK_HEAD, false, false, false),
        Kind::StackWalkerWalkFunction => (&STACK_WALK_WALK_HEAD, true, false, false),
        Kind::StackWalkerForEachStream => (&STACK_WALK_FOR_EACH_HEAD, true, true, false),
        Kind::StackWalkerForEachAction => (&STACK_WALK_FOR_EACH_HEAD, true, true, true),
        _ => return Vec::new(),
    };
    let mut steps: Vec<&'static StackWalkStep> = Vec::with_capacity(10);
    steps.push(head);
    steps.extend(STACK_WALK_ENTRY_STEPS.iter());
    if consume {
        steps.extend(STACK_WALK_CONSUME_STEPS.iter());
    }
    if lambda {
        steps.push(&STACK_WALK_FOR_EACH_LAMBDA);
    }
    if loop_steps {
        steps.extend(STACK_WALK_FOR_EACH_LOOP_STEPS.iter());
    }
    steps
}

/// A served walk's JDK frames ([`stack_walk_steps`]), OUTERMOST first, each
/// at its call of the next with its line, `callStackWalk` as the native leaf;
/// `None` when one of them cannot be named (a class not loaded, a JDK whose
/// walk runs other methods). The [`reflective_entries`] of a
/// `ReflectiveCallKind::StackWalker*` kind.
pub(in crate::runtime::stackwalker) fn stack_walk_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
) -> Option<Vec<StackTraceEntry>> {
    let steps = stack_walk_steps(kind);
    if steps.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        let class_id = find_class_id_by_name_memoized(store, step.class)?;
        let class = store.get(class_id)?;
        let (index, method) = class.methods.iter().enumerate().find(|(_, m)| {
            &*m.name == step.name && step.descriptor.is_none_or(|d| &*m.descriptor == d)
        })?;
        let call_pc = match step.calls {
            Some((name, descriptor)) => Some(named_call_site(
                &class.constant_pool,
                &method.code()?.code,
                name,
                descriptor,
                true,
            )?),
            None if method.is_native() => None,
            None => return None,
        };
        out.push(standin_entry(class, class_id, method, index, call_pc));
    }
    Some(out)
}

/// `MethodHandle.invokeWithArguments(Object...)`'s descriptor.
pub(in crate::runtime::stackwalker) const IWA_ARRAY_DESCRIPTOR: &str = "([Ljava/lang/Object;)Ljava/lang/Object;";
/// `MethodHandle.invokeWithArguments(List)`'s descriptor.
pub(in crate::runtime::stackwalker) const IWA_LIST_DESCRIPTOR: &str = "(Ljava/util/List;)Ljava/lang/Object;";

/// A served `MethodHandle.invokeWithArguments`'s JDK frames (interpreter round
/// i5 wave 104, lane L3; `L3W104WalkOptionsThroughHandles` row
/// `invokeWithArguments`), OUTERMOST first: for the `List` overload its frame
/// at its call of the array overload, then the array overload at its
/// `invokeExact` of the spread invoker (JDK 25:
/// `invocationType.invokers().spreadInvoker(0).invokeExact(asType(..), arguments)`).
/// HotSpot runs the target under those; the hidden spread-invoker and
/// `DirectMethodHandle$Holder` frames between are spun or shape-dependent and
/// are not listed (a `SHOW_HIDDEN_FRAMES` walk lacks them, as for
/// `invokeExact`). `None` when a frame cannot be named (fail-closed).
pub(in crate::runtime::stackwalker) fn invoke_with_arguments_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
) -> Option<Vec<StackTraceEntry>> {
    const HANDLE: &str = "java/lang/invoke/MethodHandle";
    let mut steps: Vec<(&str, &str, Option<&str>)> = Vec::with_capacity(2);
    if kind == cratonvm_native_api::ReflectiveCallKind::MethodHandleInvokeWithArgumentsList {
        steps.push((
            IWA_LIST_DESCRIPTOR,
            "invokeWithArguments",
            Some(IWA_ARRAY_DESCRIPTOR),
        ));
    }
    steps.push((IWA_ARRAY_DESCRIPTOR, "invokeExact", None));
    let mut out = Vec::with_capacity(steps.len());
    for (descriptor, callee, callee_descriptor) in steps {
        let (class_id, class, index, method) =
            named_method_in(store, HANDLE, "invokeWithArguments", descriptor)?;
        let pc = named_call_site(
            &class.constant_pool,
            &method.code()?.code,
            callee,
            callee_descriptor,
            true,
        )?;
        out.push(standin_entry(class, class_id, method, index, Some(pc)));
    }
    Some(out)
}

/// Is `frame` a `java.lang.invoke.MethodHandle.invokeWithArguments` frame?
pub(in crate::runtime::stackwalker) fn is_invoke_with_arguments_frame(frame: &BacktraceFrame) -> bool {
    backtrace_frame_identity(frame).is_some_and(|(class, method, _, _)| {
        method == "invokeWithArguments"
            && (class == "java/lang/invoke/MethodHandle" || class == "java.lang.invoke.MethodHandle")
    })
}

/// Interpreter round i5 wave 104 hand-back (`L3W56FieldHandleNullBase` row
/// `getter-with-arguments`, w104d): a served `invokeWithArguments` call's
/// splice lists `MethodHandle.invokeWithArguments` right below the first frame
/// the call pushed, and a served-raise rule run later in the capture (the
/// `with_arguments` arm of [`served_raise_frames`], the door-keyed and
/// `WrongMethodTypeException` rows) may list the same frame again next to it
/// (there: `checkBase` / `requireNonNull` under the door). HotSpot has one
/// such frame per call, so two ADJACENT `invokeWithArguments` frames of one
/// overload are one: the outer copy goes. `trace` is OUTERMOST-first. `true`
/// when it removed any. Asked only for a capture inside a served
/// `invokeWithArguments` call.
pub(crate) fn dedupe_adjacent_invoke_with_arguments(trace: &mut Vec<BacktraceFrame>) -> bool {
    let mut removed = false;
    let mut i = 1;
    while i < trace.len() {
        // One overload: the descriptors agree, or one copy names none (a
        // rule's entry); the `List` overload's frame above the array one's
        // (both named) is two frames.
        let same = is_invoke_with_arguments_frame(&trace[i - 1])
            && is_invoke_with_arguments_frame(&trace[i])
            && match (
                backtrace_frame_identity(&trace[i - 1]).and_then(|id| id.2),
                backtrace_frame_identity(&trace[i]).and_then(|id| id.2),
            ) {
                (Some(outer), Some(inner)) => outer == inner,
                _ => true,
            };
        if same {
            trace.remove(i - 1);
            removed = true;
        } else {
            i += 1;
        }
    }
    removed
}

/// A reflective call's JDK frames, ready to be listed in a capture.
#[derive(Clone)]
pub(crate) struct ReflectiveSplice {
    pub(in crate::runtime::stackwalker) interp_depth: u32,
    pub(in crate::runtime::stackwalker) jit_depth: u32,
    /// OUTERMOST first.
    pub(in crate::runtime::stackwalker) entries: Arc<[StackTraceEntry]>,
    /// The frames listed when the throwable being captured is one the call's
    /// native raised itself (the call's frames go ON TOP of the trace, the
    /// innermost at the accessor's `throw new <that class>` line:
    /// [`reflective_raise_entries`]); `None` lists `entries` there.
    /// Interpreter round i1 wave 45, lane L3.
    pub(in crate::runtime::stackwalker) raise: Option<Arc<[StackTraceEntry]>>,
}

/// A thread's reflective-call JDK frames as last named
/// (`JvmThread::reflective_frames_named`), per kind (index
/// [`reflective_kind_slot`]): the class-redefinition count they were named
/// at, and the frames (`None`: they could not be named). A capture inside a
/// reflective call reuses them while no class was redefined since, so a
/// throwable raised under a reflective call (every JUnit test method's) takes
/// no class-manager lock and scans no bytecode for them.
pub type ReflectiveFramesNamed = [Option<(u64, Option<Arc<[StackTraceEntry]>>)>; REFLECTIVE_KINDS];

/// Do `a` and `b` name the same frames at the same counts (by identity: a
/// memo slot is replaced, never edited)? What a running native's record
/// compares before it copies the memo (`jvm_thread::NativeRowsSnapshot`,
/// the i53 retirement).
pub(crate) fn reflective_named_same(a: &ReflectiveFramesNamed, b: &ReflectiveFramesNamed) -> bool {
    a.iter().zip(b.iter()).all(|pair| match pair {
        (None, None) => true,
        (Some((ca, ea)), Some((cb, eb))) => ca == cb && arc_slices_same(ea, eb),
        _ => false,
    })
}

/// Two memoized frame lists are one list (or both absent).
pub(in crate::runtime::stackwalker) fn arc_slices_same(a: &Option<Arc<[StackTraceEntry]>>, b: &Option<Arc<[StackTraceEntry]>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// The JDK frames of each of `calls` (a thread's `reflective_calls`,
/// outermost first) that can be named: from `named` while no class was
/// redefined since they were named, else from `name_frames`
/// ([`reflective_entries`] under the caller's class-manager read guard, taken
/// only then), which refreshes `named`.
pub(crate) fn reflective_splices(
    named: &mut ReflectiveFramesNamed,
    calls: &[ReflectiveCallRow],
    name_frames: &mut dyn FnMut(
        cratonvm_native_api::ReflectiveCallKind,
    ) -> Option<Vec<StackTraceEntry>>,
) -> Vec<ReflectiveSplice> {
    let count = cratonvm_classloading::class_redefinition_count();
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        // JIT round 20, lane walkcompat: a served walk's frames have no memo
        // slot (`reflective_kind_slot`); they are named afresh.
        let Some(slot) = named.get_mut(reflective_kind_slot(call.kind)) else {
            if let Some(entries) = name_frames(call.kind) {
                // Interpreter round i5 wave 104, lane L3: a served
                // `invokeWithArguments` lists nothing on top of a throwable
                // its native raised itself (before the target pushed a
                // frame): the served-raise chain names that frame
                // (`stackwalker::served_raise_frames`, `with_arguments`).
                let raise: Option<Arc<[StackTraceEntry]>> = call
                    .kind
                    .is_invoke_with_arguments()
                    .then(|| Arc::from(Vec::<StackTraceEntry>::new()));
                out.push(ReflectiveSplice {
                    interp_depth: call.interp_depth,
                    jit_depth: call.jit_depth,
                    entries: Arc::from(entries),
                    raise,                });            }
            continue;
        };
        if !matches!(&*slot, Some((at, _)) if *at == count) {
            let entries: Option<Arc<[StackTraceEntry]>> = name_frames(call.kind).map(Arc::from);
            *slot = Some((count, entries));
        }
        let Some(entries) = slot.as_ref().and_then(|(_, entries)| entries.clone()) else {
            continue;
        };
        out.push(ReflectiveSplice {
            interp_depth: call.interp_depth,
            jit_depth: call.jit_depth,
            entries,
            raise: None,
        });
    }
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "STTRACE_DBG_REFLECT calls={} named={}",
            calls.len(),
            out.len()
        );
    }
    out
}

/// [`reflective_splices`] from `named` alone: the calls whose frames are
/// memoized at the class-redefinition count `count`, and nothing else. For a
/// capture that may not take the class-manager lock (the published trace,
/// [`capture_published_trace`]: a blocking deposit may run while the thread
/// holds it). A call not named yet is left out, as a capture leaves out what
/// it cannot name. Interpreter round i1 wave 45, lane L3.
///
/// `walk` (JIT round 20 wave 7, lane revVM): the VM's memo of the served
/// `StackWalker` walks' frames ([`StackWalkFramesNamed`]), which have no slot
/// in `named`; `None` leaves those calls out, as before.
pub(in crate::runtime::stackwalker) fn reflective_splices_memoized(
    named: &ReflectiveFramesNamed,
    calls: &[ReflectiveCallRow],
    count: u64,
    walk: Option<&StackWalkFramesNamed>,
) -> Vec<ReflectiveSplice> {
    calls
        .iter()
        .filter_map(|call| {
            let entries = match named.get(reflective_kind_slot(call.kind)) {
                Some(Some((at, Some(entries)))) if *at == count => Arc::clone(entries),
                Some(_) => return None,
                // A served walk has no per-thread memo slot: the VM's memo.
                None => walk?.get(call.kind, count)?,
            };
            Some(ReflectiveSplice {
                interp_depth: call.interp_depth,
                jit_depth: call.jit_depth,
                entries,
                raise: None,
            })
        })
        .collect()
}

/// The JDK frames of each of `calls` memoized in `named` at the current
/// class-redefinition count, as `(interpreter depth, JIT entry-chain length,
/// frames OUTERMOST first)` (interpreter round i1 wave 47, lane L1): what a
/// debugger's listing splices in (`interpreter::build_frame_listing`), which
/// may run under the debug-state lock and so may not take the class-manager
/// lock. A call not named yet is left out ([`reflective_splices_memoized`]).
#[cfg(feature = "experimental-debug")]
pub(crate) fn reflective_frames_memoized(
    named: &ReflectiveFramesNamed,
    calls: &[ReflectiveCallRow],
    walk: Option<&StackWalkFramesNamed>,
) -> Vec<(u32, u32, Arc<[StackTraceEntry]>)> {
    reflective_splices_memoized(
        named,
        calls,
        cratonvm_classloading::class_redefinition_count(),
        walk,
    )
    .into_iter()
    .map(|splice| (splice.interp_depth, splice.jit_depth, splice.entries))
    .collect()
}

/// JIT round 20 wave 7, lane revVM
/// (`docs/feature-designs/real-jdk-mode.md`):
/// the JDK frames of the four served `StackWalker` kinds as last named, per VM
/// (`ThrowableLayoutCache::stack_walk_named`), keyed like
/// [`ReflectiveFramesNamed`] by the class-redefinition count. They have no slot
/// in the thread's memo ([`reflective_kind_slot`]: `JvmThread` stays inside its
/// layout ratchet), and the readers that may not take the class-manager lock
/// (a published trace, the debugger's listing) used to leave them out. The
/// frames depend only on the VM's loaded JDK classes, so one memo serves every
/// thread. The mutex is a leaf: never held while naming or taking another lock.
pub(crate) struct StackWalkFramesNamed {
    pub(in crate::runtime::stackwalker) slots: parking_lot::Mutex<[Option<(u64, Option<Arc<[StackTraceEntry]>>)>; 4]>,
}

impl StackWalkFramesNamed {
    pub(crate) fn new() -> Self {
        Self {
            slots: parking_lot::Mutex::new(Default::default()),
        }
    }

    /// `kind`'s slot; `None` for a kind that is not a served walk.
    pub(in crate::runtime::stackwalker) fn slot(kind: cratonvm_native_api::ReflectiveCallKind) -> Option<usize> {
        use cratonvm_native_api::ReflectiveCallKind as Kind;
        match kind {
            Kind::StackWalkerWalk => Some(0),
            Kind::StackWalkerWalkFunction => Some(1),
            Kind::StackWalkerForEachStream => Some(2),
            Kind::StackWalkerForEachAction => Some(3),
            _ => None,
        }
    }

    /// [`name_reflective_frames_if_stale`] for a served walk's `kind`: name its
    /// frames unless they are named at the current redefinition count;
    /// `name_frames` answering `None` (the class-manager guard was not free)
    /// leaves the memo for a later call.
    pub(crate) fn name_if_stale(
        &self,
        kind: cratonvm_native_api::ReflectiveCallKind,
        name_frames: impl FnOnce() -> Option<Option<Vec<StackTraceEntry>>>,
    ) {
        let Some(i) = Self::slot(kind) else {
            return;
        };
        let count = cratonvm_classloading::class_redefinition_count();
        let fresh = matches!(&self.slots.lock()[i], Some((at, _)) if *at == count);
        if fresh {
            return;
        }
        let Some(entries) = name_frames() else {
            return;
        };
        self.slots.lock()[i] = Some((count, entries.map(Arc::from)));    }

    /// `kind`'s frames named at `count`, if any.
    pub(in crate::runtime::stackwalker) fn get(
        &self,
        kind: cratonvm_native_api::ReflectiveCallKind,
        count: u64,
    ) -> Option<Arc<[StackTraceEntry]>> {
        let i = Self::slot(kind)?;
        match &self.slots.lock()[i] {
            Some((at, Some(entries))) if *at == count => Some(Arc::clone(entries)),
            _ => None,
        }
    }
}

/// Name `kind`'s JDK frames into `named` unless they are already named at the
/// current class-redefinition count. `name_frames` runs only then
/// ([`reflective_entries`] under a class-manager read guard); `None` from it
/// (the guard was not free) leaves `named` as it was, so a later call tries
/// again. The native entering a reflective call runs this, so the thread's
/// published trace ([`capture_published_trace`], which reads only `named`)
/// can list the call without the thread first building a throwable under it
/// (interpreter round i1 wave 45, lane L3).
pub(crate) fn name_reflective_frames_if_stale(
    named: &mut ReflectiveFramesNamed,
    kind: cratonvm_native_api::ReflectiveCallKind,
    name_frames: impl FnOnce() -> Option<Option<Vec<StackTraceEntry>>>,
) {
    let count = cratonvm_classloading::class_redefinition_count();
    // A served walk has no memo slot (`reflective_kind_slot`).
    let Some(slot) = named.get_mut(reflective_kind_slot(kind)) else {
        return;
    };
    if matches!(&*slot, Some((at, _)) if *at == count) {
        return;
    }
    if let Some(entries) = name_frames() {
        *slot = Some((count, entries.map(Arc::from)));
    }
}

// ---------------------------------------------------------------------------
// A throwable the reflective native raises itself (wave 45, lane L3)
// ---------------------------------------------------------------------------
//
// Item 1 of
// `docs/feature-designs/real-jdk-mode.md`.
// The natives serving `Method.invoke` / `Constructor.newInstance` build the
// `InvocationTargetException` wrapping a target's throw themselves (running its
// real `<init>`, `lang_class::wrap_as_invocation_target_exception`), inside
// the call's record. Its capture then has the call's first pushed frame at the
// throwable's own constructor, which the fill-frame trim removes, so the call
// was listed nowhere and the trace started at the caller. HotSpot (JDK 25.0.3,
// measured, `L3W45ReflectiveRaise.java`) starts it in
// the accessor, at its `throw new InvocationTargetException(e)`:
//
// ```text
// jdk.internal.reflect.DirectMethodHandleAccessor.invoke(DirectMethodHandleAccessor.java:119)
// java.lang.reflect.Method.invoke(Method.java:565)
// <caller>
// ```
//
// (line 116 for a target's `NullPointerException`, 110 for a
// `ClassCastException`: the accessor's three `catch` arms). A throwable
// filled by a constructor the call itself runs (`Constructor.newInstance` of a
// throwable class) lists the frames there at their call lines, as HotSpot does
// (`DirectConstructorHandleAccessor.newInstance:62`). So a call whose first
// pushed slot is the first slot the trim removed, or which pushed none, is
// listed on top of the trace: at the accessor's `new <throwable class>` site
// when its innermost JDK frame has one ([`reflective_raise_entries`]), else at
// the call lines.

/// What a raised throwable wraps, for the choice between the accessor's
/// `catch` arms ([`reflective_raise_entries`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReflectiveRaiseCause {
    /// A `ClassCastException` or `WrongMethodTypeException`: the first arm.
    ClassCast,
    /// A `NullPointerException`: the second arm.
    NullPointer,
    /// Anything else, or nothing known: the last arm.
    Other,
}

/// The `catch` arm `class_id`'s class picks ([`ReflectiveRaiseCause`]), from
/// its superclass chain.
pub(crate) fn reflective_raise_cause(
    store: &ClassStore,
    class_id: ClassId,
) -> ReflectiveRaiseCause {
    let mut id = class_id;
    for _ in 0..64 {
        let Some(class) = store.get(id) else {
            break;
        };
        match &*class.name {
            "java/lang/NullPointerException" => return ReflectiveRaiseCause::NullPointer,
            "java/lang/ClassCastException" | "java/lang/invoke/WrongMethodTypeException" => {
                return ReflectiveRaiseCause::ClassCast;
            }
            "java/lang/Throwable" | "java/lang/Object" => break,
            _ => {}
        }
        let Some(superclass) = class.superclass else {
            break;
        };
        id = superclass;    }
    ReflectiveRaiseCause::Other}

/// The innermost reflective call of `calls` when a throwable of class
/// `throwable_class` being filled on top of `frames` may be one its native
/// raised: the call pushed no interpreter frame, or its first is a
/// constructor of `throwable_class` itself. With that frame, whose arguments
/// name what the throwable wraps. A cheap screen (a throwable of another
/// class, built by a reflectively called constructor, fails it); the capture
/// decides whether the call is really on top ([`build_slots_with_splices`]).
pub(crate) fn reflective_raise_site<'f>(
    frames: &'f [Frame],
    calls: &[ReflectiveCallRow],
    throwable_class: ClassId,
) -> Option<(ReflectiveCallRow, Option<&'f Frame>)> {
    let call = *calls.last()?;
    match frames.get(usize::try_from(call.interp_depth).ok()?) {
        None => Some((call, None)),
        Some(frame) if frame.class_id == throwable_class && frame.method_name() == "<init>" => {
            Some((call, Some(frame)))
        }
        Some(_) => None,
    }
}

/// `kind`'s JDK frames (`entries`, OUTERMOST first, as [`reflective_entries`]
/// names them) for a `throwable_class` its native raised: the innermost at
/// the `new throwable_class` of its method the `cause` picks (the first, the
/// second or the last such site), or `None` when that method has no such
/// site (the frames are then listed at their call lines).
///
/// nio21 wave 15 L4: the raise site it finds is remembered in `memo` when one
/// is given (`CRATONVM_REFLECTIVE_RAISE_SITE_MEMO`, default ON). Finding the
/// site decodes the accessor's whole method and compares every `new`'s class
/// name, on every `InvocationTargetException` a
/// reflective call raises; the answer is a function of the call's kind, the
/// throwable's class name, the `catch` arm and the loaded class versions,
/// which is the memo's key. The privileged arm is not memoised.
pub(crate) fn reflective_raise_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
    entries: &Arc<[StackTraceEntry]>,
    throwable_class: &str,
    cause: ReflectiveRaiseCause,
    mut memo: Option<&mut ThrowableCaptureMemos>,
) -> Option<Arc<[StackTraceEntry]>> {
    let steps = reflective_steps(kind);
    let step = steps.last()?;
    if entries.len() != steps.len() {
        return None;
    }
    if kind.is_privileged() {
        // Wave 49: the bridge's `PrivilegedActionException` (the JDK builds
        // it in `wrapException`, not at a `new` of `doPrivileged` itself).
        return privileged_raise_entries(store, step, throwable_class);
    }
    let site = match memo.as_deref_mut() {
        Some(memo) => memo.reflective_raise_site(kind, throwable_class, cause, || {
            reflective_raise_site_in(store, step, throwable_class, cause)
        }),
        None => reflective_raise_site_in(store, step, throwable_class, cause).site(),
    };
    let site = site?;
    let build = || -> Option<Arc<[StackTraceEntry]>> {
        let (class_id, index, pc) = site;
        let class = store.get(class_id)?;
        let method = class.methods.get(index)?;
        let mut out: Vec<StackTraceEntry> = entries[..entries.len() - 1].to_vec();
        out.push(standin_entry(class, class_id, method, index, Some(pc)));
        Some(Arc::from(out))
    };
    // JIT round 20 wave 12 (lane qMisc, L4W15-2): the same list while its
    // inputs are the same, so a spliced capture memo can key on it.
    match memo {
        Some(memo) => memo.reflective_raise_built(entries, site, build),
        None => build(),
    }
}

/// `CRATONVM_THROWABLE_CAPTURE_MEMO_SPLICES` (JIT round 20 wave 12, lane
/// qMisc, L4W15-2; default ON, `=0` off): the capture memo is also made for
/// a capture under reflective splices, and the raise frames are kept per
/// thread. Read once per thread by the memo ways and once per VM by
/// `ThrowableLayoutCache::capture_memo_splices_enabled`.
pub(crate) fn capture_memo_splices_enabled() -> bool {
    true
}

/// The accessor method `step` names and the pc of its `new throwable_class`
/// that `cause` picks (the first, the second or the last such site), as
/// `(class id, method index, pc)` ([`RaiseSiteLookup::Found`]);
/// [`RaiseSiteLookup::Absent`] when the method decoded and has no such site,
/// [`RaiseSiteLookup::Unknown`] when the class or method is missing or its
/// code does not decode (nio21 wave 17 L1 split the old `None`).
pub(in crate::runtime::stackwalker) fn reflective_raise_site_in(
    store: &ClassStore,
    step: &StandinMethod,
    throwable_class: &str,
    cause: ReflectiveRaiseCause,
) -> RaiseSiteLookup {
    use cratonvm_reader::instruction::Instruction;
    let Some(class_id) = find_class_id_by_name_memoized(store, step.class) else {
        return RaiseSiteLookup::Unknown;
    };
    let Some(class) = store.get(class_id) else {
        return RaiseSiteLookup::Unknown;
    };
    let Some((index, method)) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == step.name && &*m.descriptor == step.descriptor)
    else {
        return RaiseSiteLookup::Unknown;
    };
    let Some(code) = method.code().map(|c| &c.code) else {
        return RaiseSiteLookup::Unknown;
    };
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let Ok((instruction, next)) = Instruction::decode(code, pc) else {
            return RaiseSiteLookup::Unknown;
        };
        if let Instruction::New(cp_index) = instruction {
            if class.constant_pool.get_class_name(cp_index) == Some(throwable_class) {
                sites.push(pc);
            }
        }
        if next <= pc {
            return RaiseSiteLookup::Unknown;        }
        pc = next;
    }
    let Some(last) = sites.len().checked_sub(1) else {
        return RaiseSiteLookup::Absent;
    };
    let pick = match cause {
        ReflectiveRaiseCause::ClassCast => 0,
        ReflectiveRaiseCause::NullPointer => 1,
        ReflectiveRaiseCause::Other => last,
    }
    .min(last);
    RaiseSiteLookup::Found((class_id, index, sites[pick]))
}

/// `class_name.name descriptor` in `store`: its class id, class, method
/// index and method.
pub(in crate::runtime::stackwalker) fn named_method_in<'s>(
    store: &'s ClassStore,
    class_name: &str,
    name: &str,
    descriptor: &str,
) -> Option<(ClassId, &'s Class, usize, &'s ClassFileMethod)> {
    let class_id = find_class_id_by_name_memoized(store, class_name)?;
    let class = store.get(class_id)?;
    let (index, method) = class
        .methods
        .iter()
        .enumerate()
        .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor)?;
    Some((class_id, class, index, method))
}

/// For each `new throwable_class` of `code`, in order, the pc of the
/// `invokespecial throwable_class.<init>` that completes it (the `new`'s own
/// pc when none does): the pc HotSpot's frame stands at while the
/// constructor fills the trace, whose line differs from the `new`'s when the
/// constructor's arguments span lines (`checkReceiver`'s
/// `throw new IllegalArgumentException("object of type " + ...` is line 198,
/// its constructor call line 199). `None` when `code` does not decode.
pub(in crate::runtime::stackwalker) fn throwable_init_sites(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    throwable_class: &str,
) -> Option<Vec<usize>> {
    use cratonvm_reader::instruction::Instruction;
    let mut sites = Vec::new();
    // The sites whose constructor call is still to come, innermost last.
    let mut open: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        match instruction {
            Instruction::New(cp_index) if cp.get_class_name(cp_index) == Some(throwable_class) => {
                open.push(sites.len());
                sites.push(pc);
            }
            Instruction::Invokespecial(cp_index) if matches!(cp_method_ref(cp, cp_index), Some((o, n, _)) if o == throwable_class && n == "<init>") => {
                if let Some(k) = open.pop() {
                    sites[k] = pc;
                }
            }
            _ => {}
        }
        if next <= pc {
            return None;
        }
        pc = next;
    }
    Some(sites)}

// ---------------------------------------------------------------------------
// A reflective call's argument checks (wave 46, lane L3)
// ---------------------------------------------------------------------------
//
// The rest of item 1 of
// `docs/feature-designs/real-jdk-mode.md`.
// The natives' argument checks note which check failed on the call's record
// (`NativeExceptionAccess::note_reflective_check`) and build the check's
// throwable by its real constructor inside the call, so its capture lists the
// call on top, as an `InvocationTargetException`'s does. HotSpot (JDK 25.0.3,
// measured, `L3W46ReflectiveChecks.java`) starts such a
// trace at the check:
//
// ```text
// argument count      DirectMethodHandleAccessor.checkArgumentCount:324, .invoke:102
// receiver type       DirectMethodHandleAccessor.checkReceiver:199, .invoke:100
// null receiver       Method.invoke:557 alone (its access check's obj.getClass())
//   (setAccessible)   DirectMethodHandleAccessor.checkReceiver:197, .invoke:100
// argument type       DirectMethodHandleAccessor.invoke:108
// null to primitive   DirectMethodHandleAccessor.invoke:114
// constructor         DirectConstructorHandleAccessor.newInstance:59 / :65 / :70
//                     (count / type / null to a primitive)
// ```
//
// each above the call's outer frames at their call lines (`Method.invoke:565`;
// `Constructor.newInstanceWithCaller:499`, `Constructor.newInstance:483`).

/// `kind`'s JDK frames (`entries`, OUTERMOST first, as [`reflective_entries`]
/// names them) for the throwable of class `throwable_class` the failed
/// argument check `check` raised: the frames HotSpot lists for that check
/// (see the section comment), found in the real class bytes. `None` when
/// the throwable is not the check's (`IllegalArgumentException`, or
/// `NullPointerException` for a null receiver), the check has no frames for
/// `kind`, or a method or site is not found; the capture then lists the frames
/// as [`reflective_raise_entries`] or the call lines say.
pub(crate) fn reflective_check_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
    entries: &[StackTraceEntry],
    throwable_class: &str,
    check: cratonvm_native_api::ReflectiveCheck,
) -> Option<Arc<[StackTraceEntry]>> {
    use cratonvm_native_api::{ReflectiveCallKind as Kind, ReflectiveCheck as Check};
    const OBJECT_GET_CLASS: (&str, &str, &str) =
        ("java/lang/Object", "getClass", "()Ljava/lang/Class;");
    let steps = reflective_steps(kind);
    if entries.len() != steps.len() {
        return None;
    }
    if let Check::NullToPrimitive(primitive) = check {
        if throwable_class == "java/lang/NullPointerException" {
            // The `IllegalArgumentException`'s cause: the call's frames at
            // their call lines, `ValueConversions.unbox<Wrapper>` on top.
            let unbox = null_unbox_frame(store, primitive)?;
            let mut out = entries.to_vec();
            out.push(unbox);
            return Some(Arc::from(out));
        }
    }
    if let Check::ArgumentCast(cast) = check {
        if throwable_class == "java/lang/ClassCastException" {
            // The constructor's `IllegalArgumentException`'s cause (wave 47):
            // the call's frames at their call lines, the cast on top.
            let mut out = entries.to_vec();
            out.extend(argument_cast_frames(store, cast)?);
            return Some(Arc::from(out));
        }
    }
    if check == Check::AbstractClass {
        // Wave 55, lane L3: an abstract class's constructor. The JDK's
        // `InstantiationExceptionConstructorAccessorImpl.newInstance` at its
        // first construction (the message-less one) in the accessor's place,
        // which the call's outer frame calls at the same line.
        if kind != Kind::ConstructorNewInstance
            || throwable_class != "java/lang/InstantiationException"
        {
            return None;
        }
        let (class_id, class, index, method) = named_method_in(
            store,
            INSTANTIATION_EXCEPTION_ACCESSOR,
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )?;
        let pc =
            *throwable_init_sites(&class.constant_pool, &method.code()?.code, throwable_class)?
                .first()?;
        let mut out: Vec<StackTraceEntry> = entries[..entries.len() - 1].to_vec();
        out.push(standin_entry(class, class_id, method, index, Some(pc)));
        return Some(Arc::from(out));
    }
    if check == Check::ClassInit {
        // Wave 58, lane L3: the declaring class's initialization failure,
        // raised unwrapped in the accessor factory.
        if !CLASS_INIT_FAILURES.contains(&throwable_class) {
            return None;
        }
        return class_init_entries(store, kind, entries).map(Arc::from);
    }
    // The `IllegalArgumentException` of a cast check is the type arm's.
    let check = match check {
        Check::ArgumentCast(_) => Check::ArgumentType,
        other => other,
    };
    let raised = match check {
        Check::NullReceiverAtAccessCheck | Check::NullReceiverAtReceiverCheck => {
            "java/lang/NullPointerException"
        }
        _ => "java/lang/IllegalArgumentException",
    };
    if throwable_class != raised {
        return None;
    }
    let accessor = steps.last()?;
    let mut out: Vec<StackTraceEntry> = entries[..entries.len() - 1].to_vec();
    match (kind, check) {
        (Kind::MethodInvoke, Check::NullReceiverAtAccessCheck) => {
            // `Method.invoke` alone, at its access check's `obj.getClass()`.
            let outer = steps.first()?;
            let (class_id, class, index, method) =
                named_method_in(store, outer.class, outer.name, outer.descriptor)?;
            let (owner, name, descriptor) = OBJECT_GET_CLASS;
            let pc = first_call_to(
                &class.constant_pool,
                &method.code()?.code,
                owner,
                name,
                descriptor,
            )?;
            out.clear();
            out.push(standin_entry(class, class_id, method, index, Some(pc)));
        }
        (
            Kind::MethodInvoke,
            Check::ArgumentCount | Check::Receiver | Check::NullReceiverAtReceiverCheck,
        ) => {
            // The accessor at its call of the check, the check on top.
            let (callee_name, callee_descriptor) = if check == Check::ArgumentCount {
                ("checkArgumentCount", "(I[Ljava/lang/Object;)V")
            } else {
                ("checkReceiver", "(Ljava/lang/Object;)V")
            };
            let (class_id, class, index, method) =
                named_method_in(store, accessor.class, accessor.name, accessor.descriptor)?;
            let call_pc = first_call_to(
                &class.constant_pool,
                &method.code()?.code,
                accessor.class,
                callee_name,
                callee_descriptor,
            )?;
            let (callee_index, callee) =
                class.methods.iter().enumerate().find(|(_, m)| {
                    &*m.name == callee_name && &*m.descriptor == callee_descriptor
                })?;
            let callee_code = &callee.code()?.code;
            let raise_pc = if check == Check::NullReceiverAtReceiverCheck {
                let (owner, name, descriptor) = OBJECT_GET_CLASS;
                first_call_to(&class.constant_pool, callee_code, owner, name, descriptor)?
            } else {
                *throwable_init_sites(&class.constant_pool, callee_code, throwable_class)?
                    .first()?
            };
            out.push(standin_entry(class, class_id, method, index, Some(call_pc)));
            out.push(standin_entry(
                class,
                class_id,
                callee,
                callee_index,
                Some(raise_pc),
            ));
        }
        _ => {
            // The accessor at the `new IllegalArgumentException` of the arm:
            // `DirectMethodHandleAccessor.invoke` has two (type, null), the
            // constructor accessor's `newInstance` three (count, type, null).
            let nth = match (kind, check) {
                (Kind::MethodInvoke, Check::ArgumentType) => 0,
                (Kind::MethodInvoke, Check::NullToPrimitive(_)) => 1,
                (Kind::ConstructorNewInstance, Check::ArgumentCount) => 0,
                (Kind::ConstructorNewInstance, Check::ArgumentType) => 1,
                (Kind::ConstructorNewInstance, Check::NullToPrimitive(_)) => 2,
                _ => return None,
            };
            let (class_id, class, index, method) =
                named_method_in(store, accessor.class, accessor.name, accessor.descriptor)?;
            let sites =
                throwable_init_sites(&class.constant_pool, &method.code()?.code, throwable_class)?;
            let pc = *sites.get(nth)?;
            out.push(standin_entry(class, class_id, method, index, Some(pc)));
        }
    }
    Some(Arc::from(out))
}

/// The class whose `unbox<Wrapper>(Object, boolean)` raises the
/// `NullPointerException` a null argument for a primitive parameter meets in
/// HotSpot's reflection ([`null_unbox_frame`]). The native noting that check
/// loads it, so the capture can name it.
pub(crate) const VALUE_CONVERSIONS_CLASS: &str = "sun/invoke/util/ValueConversions";

/// The JDK accessor whose `newInstance` raises an abstract class's
/// constructor's `InstantiationException` (`ReflectiveCheck::AbstractClass`);
/// the native noting that check loads it. Interpreter round i2 wave 55, lane
/// L3.
pub(crate) const INSTANTIATION_EXCEPTION_ACCESSOR: &str =
    "jdk/internal/reflect/InstantiationExceptionConstructorAccessorImpl";

// ---------------------------------------------------------------------------
// A reflective door's class-initialization failure (wave 58, lane L3)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`.
// JDK 25 builds a member's accessor in `MethodHandleAccessorFactory`, which
// initializes the declaring class first, OUTSIDE the method handle, "so that
// EIIE is propagated (not wrapped with ITE)". HotSpot 25.0.3
// (`L3W58ReflectiveClassInit.java`), innermost first:
//
// ```text
// Unsafe.ensureClassInitialized0 (Native Method)
// Unsafe.ensureClassInitialized
// MethodHandleAccessorFactory.ensureClassInitialized
// MethodHandleAccessorFactory.new<Method|Constructor|Field>Accessor
// ReflectionFactory.new<Method|Constructor|Field>Accessor
// <Method|Constructor|Field>.acquire<..>Accessor
// [Field.getFieldAccessor]  [Constructor.newInstanceWithCaller]
// Method.invoke / Constructor.newInstance / Field.<get|set..>
// ```
//
// The reflective doors note `ReflectiveCheck::ClassInit` before they
// initialize ([`class_init_entries`]); the `Field` natives note a served
// chain of the same hops (`lang_class.rs`), and a JDK `Unsafe`'s own call of
// the native lists its frame ([`native_leaf_frame`]'s initializing natives).

/// The throwables a class's initialization raises (JVMS 5.5).
pub(crate) const CLASS_INIT_FAILURES: [&str; 2] = [
    "java/lang/ExceptionInInitializerError",
    "java/lang/NoClassDefFoundError",
];

/// The classes [`class_init_entries`] names that a `--compatible` reflective
/// door never runs: loaded when the door notes the check, as HotSpot has them
/// loaded to build the accessor.
pub(crate) const CLASS_INIT_CHAIN_CLASSES: [&str; 3] = [
    "jdk/internal/reflect/ReflectionFactory",
    "jdk/internal/reflect/MethodHandleAccessorFactory",
    "jdk/internal/misc/Unsafe",
];

/// One hop of a class-initialization chain: `(class, name, descriptor)`, and
/// the name of the method it stands at its LAST call of, or `None` for the
/// innermost `native` hop (`Unsafe.ensureClassInitialized0`).
pub(in crate::runtime::stackwalker) type ClassInitHop = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
);

pub(in crate::runtime::stackwalker) const MHAF_CLASS: &str = "jdk/internal/reflect/MethodHandleAccessorFactory";
pub(in crate::runtime::stackwalker) const REFLECTION_FACTORY_CLASS: &str = "jdk/internal/reflect/ReflectionFactory";

/// The factory's initialization, from `MethodHandleAccessorFactory
/// .ensureClassInitialized` down to the native.
pub(in crate::runtime::stackwalker) const CLASS_INIT_TAIL: [ClassInitHop; 3] = [
    (
        MHAF_CLASS,
        "ensureClassInitialized",
        "(Ljava/lang/Class;)V",
        Some("ensureClassInitialized"),
    ),
    (
        "jdk/internal/misc/Unsafe",
        "ensureClassInitialized",
        "(Ljava/lang/Class;)V",
        Some("ensureClassInitialized0"),
    ),
    (
        "jdk/internal/misc/Unsafe",
        "ensureClassInitialized0",
        "(Ljava/lang/Class;)V",
        None,
    ),
];

/// `Method.invoke`'s hops below `Method.invoke` itself.
pub(in crate::runtime::stackwalker) const METHOD_CLASS_INIT_HOPS: [ClassInitHop; 3] = [
    (
        "java/lang/reflect/Method",
        "acquireMethodAccessor",
        "()Ljdk/internal/reflect/MethodAccessor;",
        Some("newMethodAccessor"),
    ),
    (
        REFLECTION_FACTORY_CLASS,
        "newMethodAccessor",
        "(Ljava/lang/reflect/Method;Z)Ljdk/internal/reflect/MethodAccessor;",
        Some("newMethodAccessor"),
    ),
    (
        MHAF_CLASS,
        "newMethodAccessor",
        "(Ljava/lang/reflect/Method;Z)Ljdk/internal/reflect/MethodAccessorImpl;",
        Some("ensureClassInitialized"),
    ),
];

/// `Constructor.newInstance`'s hops below `newInstanceWithCaller`.
pub(in crate::runtime::stackwalker) const CONSTRUCTOR_CLASS_INIT_HOPS: [ClassInitHop; 3] = [
    (
        "java/lang/reflect/Constructor",
        "acquireConstructorAccessor",
        "()Ljdk/internal/reflect/ConstructorAccessor;",
        Some("newConstructorAccessor"),
    ),
    (
        REFLECTION_FACTORY_CLASS,
        "newConstructorAccessor",
        "(Ljava/lang/reflect/Constructor;)Ljdk/internal/reflect/ConstructorAccessor;",
        Some("newConstructorAccessor"),
    ),
    (
        MHAF_CLASS,
        "newConstructorAccessor",
        "(Ljava/lang/reflect/Constructor;)Ljdk/internal/reflect/ConstructorAccessorImpl;",
        Some("ensureClassInitialized"),
    ),
];

/// `kind`'s JDK frames (`entries`, OUTERMOST first, as [`reflective_entries`]
/// names them) for the declaring class's initialization failure
/// (`ReflectiveCheck::ClassInit`): the call's outer frames, the door
/// (`Method.invoke` / `Constructor.newInstanceWithCaller`) at its call of
/// `acquire*Accessor`, then the accessor factory's chain down to
/// `Unsafe.ensureClassInitialized0 (Native Method)`, read from the real class
/// bytes. `None` when one of them cannot be named.
pub(in crate::runtime::stackwalker) fn class_init_entries(
    store: &ClassStore,
    kind: cratonvm_native_api::ReflectiveCallKind,
    entries: &[StackTraceEntry],
) -> Option<Vec<StackTraceEntry>> {
    use cratonvm_native_api::ReflectiveCallKind as Kind;
    let (acquire, hops) = match kind {
        Kind::MethodInvoke => ("acquireMethodAccessor", &METHOD_CLASS_INIT_HOPS),
        Kind::ConstructorNewInstance => {
            ("acquireConstructorAccessor", &CONSTRUCTOR_CLASS_INIT_HOPS)        }
        _ => return None,
    };
    let steps = reflective_steps(kind);
    let door_index = steps.len().checked_sub(2)?;
    let door = steps.get(door_index)?;
    let mut out: Vec<StackTraceEntry> = entries.get(..door_index)?.to_vec();
    let (class_id, class, index, method) =
        named_method_in(store, door.class, door.name, door.descriptor)?;
    let pc = named_call_site(
        &class.constant_pool,
        &method.code()?.code,
        acquire,
        None,
        false,
    )?;
    out.push(standin_entry(class, class_id, method, index, Some(pc)));
    out.extend(class_init_hop_entries(
        store,
        hops.iter().chain(CLASS_INIT_TAIL.iter()),
    )?);
    Some(out)
}

/// The frames of `hops`, OUTERMOST first: each at its LAST call of the next
/// hop's name, the last one a `native` method listed as `(Native Method)`.
/// `None` when one cannot be found, a Java hop has no such call, or the last
/// hop is not native.
pub(in crate::runtime::stackwalker) fn class_init_hop_entries<'h>(
    store: &ClassStore,
    hops: impl Iterator<Item = &'h ClassInitHop>,
) -> Option<Vec<StackTraceEntry>> {
    let mut out = Vec::new();
    for &(class_name, name, descriptor, next) in hops {
        let (class_id, class, index, method) =
            named_method_in(store, class_name, name, descriptor)?;
        match next {
            Some(next) => {
                let pc = named_call_site(
                    &class.constant_pool,
                    &method.code()?.code,
                    next,
                    None,
                    false,
                )?;
                out.push(standin_entry(class, class_id, method, index, Some(pc)));
            }
            None => {
                if !method.is_native() {
                    return None;
                }
                out.push(standin_entry(class, class_id, method, index, None));
            }
        }
    }
    Some(out)
}

/// `ValueConversions.unbox<Wrapper>(Object, boolean)` for the primitive
/// descriptor `primitive`, standing at its `primitiveConversion(...).<x>Value()`
/// call (JDK 25: `unboxInteger:81`, `unboxLong:126`, `unboxBoolean:108`, ...,
/// measured with HotSpot 25.0.3), or `None` when it cannot be named.
pub(in crate::runtime::stackwalker) fn null_unbox_frame(store: &ClassStore, primitive: char) -> Option<StackTraceEntry> {
    let (name, descriptor) = unbox_method(primitive)?;
    let (value, value_descriptor) = match primitive {
        'J' => ("longValue", "()J"),
        'B' => ("byteValue", "()B"),
        'S' => ("shortValue", "()S"),
        'F' => ("floatValue", "()F"),
        'D' => ("doubleValue", "()D"),
        // `I`, and `Z` / `C`, which `ValueConversions` reads through `intValue`.
        _ => ("intValue", "()I"),
    };
    let (class_id, class, index, method) =
        named_method_in(store, VALUE_CONVERSIONS_CLASS, name, descriptor)?;
    let pc = first_call_to(
        &class.constant_pool,
        &method.code()?.code,
        "java/lang/Number",
        value,
        value_descriptor,
    )?;
    Some(standin_entry(class, class_id, method, index, Some(pc)))
}

/// The frames above the call's (innermost LAST) of the `ClassCastException`
/// HotSpot's reflection raises for an argument of the wrong type, as `cast`
/// says (JDK 25.0.3, measured by
/// `L3W47ConstructorArgCause.java`):
///
/// ```text
/// reference       Class.cast:3492
/// not a Number    unboxInteger:81, primitiveConversion:247 (its `(Number) x`)
/// unconvertible   unboxInteger:81, primitiveConversion:252, Class.cast:3492
/// ```
///
/// `None` when one of them cannot be named. Interpreter round i1 wave 47,
/// lane L3 (item 2 of
/// `docs/feature-designs/real-jdk-mode.md`).
pub(in crate::runtime::stackwalker) fn argument_cast_frames(
    store: &ClassStore,
    cast: cratonvm_native_api::ReflectiveArgumentCast,
) -> Option<Vec<StackTraceEntry>> {
    use cratonvm_native_api::ReflectiveArgumentCast as Cast;
    const PRIMITIVE_CONVERSION: (&str, &str) = (
        "primitiveConversion",
        "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;",
    );
    const CLASS_CAST: (&str, &str, &str) = (
        "java/lang/Class",
        "cast",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let class_cast = || {
        let (owner, name, descriptor) = CLASS_CAST;
        let (class_id, class, index, method) = named_method_in(store, owner, name, descriptor)?;
        let pc = *throwable_init_sites(
            &class.constant_pool,
            &method.code()?.code,
            "java/lang/ClassCastException",
        )?
        .first()?;
        Some(standin_entry(class, class_id, method, index, Some(pc)))
    };
    let primitive = match cast {
        Cast::Reference => return Some(vec![class_cast()?]),
        Cast::NotANumber(p) | Cast::Unconvertible(p) => p,
    };
    let (unbox_name, unbox_descriptor) = unbox_method(primitive)?;
    let (class_id, class, index, unbox) =
        named_method_in(store, VALUE_CONVERSIONS_CLASS, unbox_name, unbox_descriptor)?;
    let (conversion_name, conversion_descriptor) = PRIMITIVE_CONVERSION;
    let call_pc = first_call_to(
        &class.constant_pool,
        &unbox.code()?.code,
        VALUE_CONVERSIONS_CLASS,
        conversion_name,
        conversion_descriptor,
    )?;
    let (conversion_index, conversion) =
        class.methods.iter().enumerate().find(|(_, m)| {
            &*m.name == conversion_name && &*m.descriptor == conversion_descriptor
        })?;
    let conversion_code = &conversion.code()?.code;
    let mut out = vec![standin_entry(class, class_id, unbox, index, Some(call_pc))];
    match cast {
        Cast::NotANumber(_) => {
            let pc = last_checkcast_of_a_local_to(
                &class.constant_pool,
                conversion_code,
                "java/lang/Number",
            )?;
            out.push(standin_entry(
                class,
                class_id,
                conversion,
                conversion_index,
                Some(pc),
            ));
        }
        _ => {
            let (owner, name, descriptor) = CLASS_CAST;
            let pc = first_call_to(
                &class.constant_pool,
                conversion_code,
                owner,
                name,
                descriptor,
            )?;
            out.push(standin_entry(
                class,
                class_id,
                conversion,
                conversion_index,
                Some(pc),
            ));
            out.push(class_cast()?);
        }
    }
    Some(out)
}

/// `ValueConversions.unbox<Wrapper>(Object, boolean)`'s name and descriptor
/// for the primitive descriptor `primitive`.
pub(in crate::runtime::stackwalker) fn unbox_method(primitive: char) -> Option<(&'static str, &'static str)> {
    Some(match primitive {
        'I' => ("unboxInteger", "(Ljava/lang/Object;Z)I"),
        'J' => ("unboxLong", "(Ljava/lang/Object;Z)J"),
        'Z' => ("unboxBoolean", "(Ljava/lang/Object;Z)Z"),
        'C' => ("unboxCharacter", "(Ljava/lang/Object;Z)C"),
        'B' => ("unboxByte", "(Ljava/lang/Object;Z)B"),
        'S' => ("unboxShort", "(Ljava/lang/Object;Z)S"),
        'F' => ("unboxFloat", "(Ljava/lang/Object;Z)F"),
        'D' => ("unboxDouble", "(Ljava/lang/Object;Z)D"),
        _ => return None,
    })
}

/// The pc of the LAST `checkcast class_name` of `code` whose operand is a
/// local just loaded (`aload`): `primitiveConversion`'s unguarded
/// `res = (Number) x` in its final `else`, the one that fails. The first such
/// cast is the `x instanceof Number` arm's, which never fails (JDK 25:
/// line 239 against the failing one's 247), and a cast of a call's result
/// (`(Number) wrap.wrapperType().cast(x)`, line 252) has no local operand.
/// The line itself is always read from the loaded class's `LineNumberTable`,
/// so any JDK build answers with its own.
pub(in crate::runtime::stackwalker) fn last_checkcast_of_a_local_to(
    cp: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    class_name: &str,
) -> Option<usize> {
    use cratonvm_reader::instruction::Instruction;
    let mut pc = 0usize;
    let mut previous_loads_a_local = false;
    let mut found = None;
    while pc < code.len() {
        let (instruction, next) = Instruction::decode(code, pc).ok()?;
        let loads_a_local = matches!(instruction, Instruction::Aload(_));
        if let Instruction::Checkcast(index) = instruction {
            if previous_loads_a_local && cp.get_class_name(index) == Some(class_name) {
                found = Some(pc);
            }
        }
        previous_loads_a_local = loads_a_local;
        if next <= pc {
            return None;
        }
        pc = next;
    }
    found}

/// Give the splice of `call` (the innermost reflective call, from
/// [`reflective_raise_site`]) the frames [`reflective_raise_entries`] built
/// for a throwable its native raised. Returns whether one was given.
pub(crate) fn set_reflective_raise(
    splices: &mut [ReflectiveSplice],
    call: &ReflectiveCallRow,
    raise: Arc<[StackTraceEntry]>,
) -> bool {
    match splices.last_mut() {
        Some(last)
            if last.interp_depth == call.interp_depth && last.jit_depth == call.jit_depth =>
        {
            last.raise = Some(raise);
            true
        }
        _ => false,
    }
}

/// The JDK frames `splices`' innermost call lists (OUTERMOST first), for
/// [`reflective_raise_entries`].
pub(crate) fn innermost_splice_entries(
    splices: &[ReflectiveSplice],
) -> Option<Arc<[StackTraceEntry]>> {
    splices.last().map(|s| Arc::clone(&s.entries))
}

/// Where each of `splices` goes in a capture laid out as `slots`: as
/// `(position, splice index)`, the JDK frames going right before
/// `slots[position]`, the first slot the call put on the stack
/// ([`trace_anchor_position`]: the first interpreter frame it pushed, or the
/// first compiled activation entered at its depth since it, whichever comes
/// first). A call with no such slot is left out (see the section comment).
/// Positions never decrease (the calls nest).
pub(in crate::runtime::stackwalker) fn reflective_splice_positions(
    slots: &[TraceSlot<'_>],
    splices: &[ReflectiveSplice],
    unanchored_on_top: bool,
) -> Vec<(usize, usize)> {
    splices
        .iter()
        .enumerate()
        .filter_map(|(k, splice)| {
            let mut position = trace_anchor_position(slots, splice.interp_depth, splice.jit_depth);
            // Wave 45: a throwable capture lists a call that pushed nothing
            // on top (its native raised the throwable itself).
            if position.is_none() && unanchored_on_top {
                position = Some(slots.len());
            }
            if crate::runtime::env_cache::dbg_sttrace() {
                // `compiled_after=true`: the call's frames go right below a
                // compiled activation its target entered (wave 44); before
                // wave 44 such a call was left out (`compiled_since=true`).
                let compiled_after = matches!(
                    position.and_then(|p| slots.get(p)),
                    Some(TraceSlot::Compiled(_))
                );
                eprintln!(
                    "STTRACE_DBG_REFLECT depth={} jit_depth={} position={position:?} \
                     compiled_after={compiled_after}",
                    splice.interp_depth, splice.jit_depth
                );
            }
            Some((position?, k))
        })
        .collect()
}

/// Where a record kept outside the JIT entry chain goes in a capture laid out
/// as `slots`, given the thread's interpreter depth `depth` and JIT
/// entry-chain length `chain_len` when it was made: right before the first
/// slot pushed after it, which is interpreter frame `depth`, or a compiled
/// activation whose chain entry was pushed after the record
/// (`ActiveCompiledFrame::chain_index >= chain_len`) at that depth or deeper,
/// whichever comes first. `None` when no slot was pushed after it (the record
/// is the innermost thing on the stack).
///
/// The compiled activations at `depth` with `chain_index < chain_len` are the
/// record's compiled callers and stay below it. Interpreter round i1 wave 44,
/// lane L3 (the proposal
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// a reflective call's JDK frames ([`reflective_splice_positions`]) and a JNI
/// native's JVMTI row ([`capture_trace_with_anchor_positions`]).
pub(in crate::runtime::stackwalker) fn trace_anchor_position(slots: &[TraceSlot<'_>], depth: u32, chain_len: u32) -> Option<usize> {
    let mut interp = 0u32;
    for (i, slot) in slots.iter().enumerate() {
        match slot {
            TraceSlot::Interp { .. } => {
                if interp == depth {
                    return Some(i);
                }
                interp = interp.saturating_add(1);
            }
            TraceSlot::Compiled(compiled) => {
                if compiled.chain_index >= chain_len && compiled.interp_depth >= depth {
                    return Some(i);
                }
            }
            TraceSlot::Inlined(..) => {}
        }
    }
    None
}

/// The trace of `frames` a JVMTI stack function lists (the published
/// capture's entries, as [`capture_published_trace`] builds them, or one entry
/// per frame without compiled activations), and, for each of `anchors` (a JNI
/// native's row: the interpreter depth and the JIT entry-chain length at its
/// call, outermost first), the index of the entry it goes right before
/// ([`trace_anchor_position`]), `entries.len()` for on top. Positions never
/// decrease. Interpreter round i1 wave 44, lane L3: a compiled method a
/// native's own JNI upcall entered is listed above the native
/// (`docs/feature-designs/real-jdk-mode.md`).
/// Only `experimental-debug` builds have the JVMTI C table that reads it.
#[cfg(feature = "experimental-debug")]
pub(crate) fn capture_trace_with_anchor_positions(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    anchors: &[(u32, u32)],
) -> (Vec<StackTraceEntry>, Vec<usize>) {
    let jit = if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
        crate::jit::stack_trace::active_compiled_frames()
    } else {
        Vec::new()
    };
    if jit.is_empty() {
        let entries = capture_frames_no_lines_exact(frames);
        let len = entries.len();
        let positions = anchors
            .iter()
            .map(|&(depth, _)| usize::try_from(depth).map_or(len, |d| d.min(len)))
            .collect();
        return (entries, positions);
    }
    let (jit, osr_bci) = drop_osr_continuations(frames, jit);
    let mut frame_positions = Vec::with_capacity(frames.len());
    let mut entries = interleave_compiled_frames_without_store(
        frames,
        &jit,
        &osr_bci,
        Some(&mut frame_positions),
    );
    if cratonvm_classloading::class_redefinition_count() != 0 {
        publish_compiled_lines_at_their_stamps(shared, frames, &jit, &osr_bci, &mut entries);
    }
    let slots = collect_trace_slots(frames, &jit, &osr_bci);
    let len = entries.len();
    let positions: Vec<usize> = if slots.len() == len {
        anchors
            .iter()
            .map(|&(depth, chain_len)| {
                trace_anchor_position(&slots, depth, chain_len).unwrap_or(len)
            })
            .collect()
    } else {
        // The slots and the entries disagree (they never should): place by
        // interpreter frame alone, the wave-42 answer.
        anchors
            .iter()
            .map(|&(depth, _)| {
                usize::try_from(depth)
                    .ok()
                    .and_then(|d| frame_positions.get(d))
                    .and_then(|&p| usize::try_from(p).ok())
                    .unwrap_or(len)
            })
            .collect()
    };
    if crate::runtime::env_cache::dbg_sttrace() {
        eprintln!(
            "STTRACE_DBG_ANCHORS entries={len} slots={} anchors={anchors:?} positions={positions:?}",
            slots.len()
        );
    }
    (entries, positions)
}

/// `slots[..keep]` built, with the JDK frames of `splices` at `positions`
/// ([`reflective_splice_positions`]) spliced in. A position past `keep` (the
/// call is inside the frames the fill-frame trim removed) lists nothing. A
/// position AT `keep` -- the call's first pushed slot is the first one the
/// trim removed, or it pushed none -- is a throwable filled inside the call
/// with nothing of it above: its frames go on top, as its `raise` frames when
/// it has them (wave 45, lane L3; before, it listed nothing).
pub(in crate::runtime::stackwalker) fn build_slots_with_splices<T: FromTraceSlot>(
    slots: &[TraceSlot<'_>],
    keep: usize,
    splices: &[ReflectiveSplice],
    positions: &[(usize, usize)],
) -> Vec<T> {
    let extra: usize = positions
        .iter()
        .map(|&(_, k)| splices[k].entries.len())
        .sum();
    let mut out = Vec::with_capacity(keep + extra);
    let mut pending = positions.iter().peekable();
    for (i, slot) in slots[..keep].iter().enumerate() {
        while let Some(&&(position, k)) = pending.peek() {
            if position > i {
                break;
            }
            if position == i {
                out.extend(splices[k].entries.iter().cloned().map(T::from_entry));
            }
            pending.next();
        }
        if let Some(built) = T::from_slot(slot) {
            out.push(built);
        }
    }
    for &(position, k) in pending {
        if position != keep {
            continue;
        }
        let splice = &splices[k];
        let listed = splice.raise.as_ref().unwrap_or(&splice.entries);
        if crate::runtime::env_cache::dbg_sttrace() {
            eprintln!(
                "STTRACE_DBG_REFLECT on-top listed={} raise={}",
                listed.len(),
                splice.raise.is_some()
            );
        }
        out.extend(listed.iter().cloned().map(T::from_entry));
    }
    out
}

/// `trace`, a capture of `frames` OUTERMOST first with one entry per slot
/// [`collect_trace_slots`] lays out over them (`capture_full_trace`,
/// `capture_stack_walk_trace`), with the JDK frames of `splices` spliced in
/// (`Thread.getStackTrace` and the stack walks). A trace of another length is
/// left as it is.
pub(crate) fn splice_reflective_frames(
    frames: &[Frame],
    trace: &mut Vec<StackTraceEntry>,
    splices: &[ReflectiveSplice],
) {
    let _ = splice_frames_reporting(frames, trace, splices);
}

/// [`splice_reflective_frames`], answering where the frames went in: for each
/// spliced entry, the index of `trace` AS PASSED it was inserted before
/// (ascending; the convention of `vm_exec::add_thread_run_walk_entries`, so
/// a caller can shift indices it attributed to the entries as passed). `None`
/// when the trace is not one entry per slot (it is then left as it is).
/// Round 15 wave 3 (lane trace4): the served `Thread.run` rows
/// ([`ServedThreadRunCalls`]) go in through here.
pub(crate) fn splice_frames_reporting(
    frames: &[Frame],
    trace: &mut Vec<StackTraceEntry>,
    splices: &[ReflectiveSplice],
) -> Option<Vec<usize>> {
    if splices.is_empty() {
        return Some(Vec::new());
    }
    let jit = crate::jit::stack_trace::active_compiled_frames();
    let (jit, osr_bci) = if jit.is_empty() {
        (jit, OsrBciOverrides::default())
    } else {
        drop_osr_continuations(frames, jit)
    };
    let slots = collect_trace_slots(frames, &jit, &osr_bci);
    if slots.len() != trace.len() {
        return None;
    }
    let positions = reflective_splice_positions(&slots, splices, false);
    let mut inserted_before = Vec::new();
    for &(position, k) in &positions {
        inserted_before.extend(std::iter::repeat_n(position, splices[k].entries.len()));
    }
    // From the innermost, so an earlier position still indexes the entry it
    // named; calls at one position keep their order, the outer one first.
    for &(position, k) in positions.iter().rev() {
        let inner = trace.split_off(position);
        trace.extend(splices[k].entries.iter().cloned());
        trace.extend(inner);
    }
    Some(inserted_before)
}

// ---------------------------------------------------------------------------
// The served `Thread.run` records its call (round 15 wave 3, lane trace4)
// ---------------------------------------------------------------------------
//
// TR2W2-2 of `jit-r15w2-trace2-proposals-RETIRED-20261003.md`, landed from
// `r15w3-trace3-thread-run-served-call-row-patch-FIXED-20260930.md`. The VM serves
// `java.lang.Thread.run()` natively (`native-builtins` `lib.rs`, both
// registrations; force-listed over the real bytecode) and pushes no frame, so
// the round-14/15 rules rebuild HotSpot's `java.base/java.lang.Thread.run`
// frame from the frames and the heap (the bottom rule
// `vm_exec::prepend_thread_run_standin_frame`, the middle rule
// [`thread_run_middle_frame`]). Three shapes no such rule can prove:
//
// * a method-reference task under a call of `Thread.run`
//   (`new W(Main::work).run()`, `W.run() { super.run(); }`);
// * a `Thread` task under `super.run()` (`new W(new Thread(task)).run()`:
//   two frames, the inner receiver is in no frame);
// * a virtual thread whose task is a `Thread` (`VirtualThread.run` calls it).
//
// The native now records each call on its thread
// (`NativeExceptionAccess::enter_served_thread_run`: the interpreter depth and
// the JIT entry-chain length, as a reflective call's row), and a capture of
// that thread splices the `Thread.run` frame (at its `runWith` / `target.run()`
// call, [`thread_run_site_entry`]) right below the first frame the call
// pushed, through the reflective splice machinery. Every served call records
// one, so a thread with rows takes NO rule-built `Thread.run` frame (they
// would double); a thread without (the switch off, the main thread outside
// any `t.run()`) keeps the rules. The throwable capture, the `StackWalker`
// walk and the current thread's `getStackTrace` read the rows; another
// thread's PUBLISHED trace does not (it keeps the rules at read time).
// `CRATONVM_THROWABLE_THREAD_RUN_SERVED_ROWS=0` records nothing (read once
// per thread, at its first served call).

/// A thread's served `Thread.run` calls (`JvmThread::served_thread_run`).
/// Cloned into the record a running native's readers splice from
/// (`jvm_thread::NativeRowsSnapshot`, the i53 retirement).
#[derive(Clone, Default)]
pub(crate) struct ServedThreadRunCalls {
    /// `(interpreter depth, JIT entry-chain length)` at each call the thread
    /// is inside, outermost first.
    pub(in crate::runtime::stackwalker) calls: Vec<(u32, u32)>,
    /// `CRATONVM_THROWABLE_THREAD_RUN_SERVED_ROWS`, read at the first call.
    pub(in crate::runtime::stackwalker) enabled: Option<bool>,
    /// The `Thread.run` frame as last named: the class-redefinition count it
    /// was named at, and the frame (`None`: it could not be named).
    pub(in crate::runtime::stackwalker) named: Option<(u64, Option<Arc<[StackTraceEntry]>>)>,
    /// Round 15 wave 6 (lane trace6; item 2 of
    /// `r15w5-trace5-review-findings-CLOSED-20260930.md`): a call was left by a
    /// continuation YIELD ([`Self::leave_yielded`]). Its row is gone while its
    /// task's frames remount, so the rows no longer list every `Thread.run`
    /// the thread is inside: every capture answers from the rules instead
    /// (the answer for a thread with no rows). Never cleared: a virtual
    /// thread's `JvmThread` ends with it, and a platform thread never yields.
    pub(in crate::runtime::stackwalker) incomplete: bool,
}

impl ServedThreadRunCalls {
    /// Enter one call; `depths` (the interpreter depth and the JIT
    /// entry-chain length now) is asked only when the switch is on. Returns
    /// the mark for [`Self::leave`].
    pub(crate) fn enter(&mut self, depths: impl FnOnce() -> (u32, u32)) -> usize {
        let mark = self.calls.len();
        let enabled = *self.enabled.get_or_insert_with(|| {
            true
        });
        if enabled {
            self.calls.push(depths());
        }
        mark
    }

    /// Leave the call [`Self::enter`] returned `mark` for (and any left open
    /// inside it).
    pub(crate) fn leave(&mut self, mark: usize) {
        self.calls.truncate(mark);
    }

    /// [`Self::leave`] for a call its task left by a continuation yield: the
    /// task's frames come back at the remount without the call's row, so the
    /// rows stop speaking for the thread ([`Self::incomplete`]).
    /// `CRATONVM_THROWABLE_THREAD_RUN_ROWS_AFTER_YIELD=0` leaves as before.
    pub(crate) fn leave_yielded(&mut self, mark: usize) {
        self.calls.truncate(mark);
        {
            self.incomplete = true;
        }
    }

    /// The rows a lock-free publish lists ([`capture_published_trace`],
    /// TR4W3-2): from the memo only, when it was named at `count`; empty
    /// otherwise (the reader's rules answer then, as before).
    pub(crate) fn published_splices(&self, count: u64) -> Vec<ReflectiveSplice> {
        if self.incomplete || self.calls.is_empty() {
            return Vec::new();
        }
        match &self.named {
            Some((at, Some(entries))) if *at == count => self.rows(entries),
            _ => Vec::new(),
        }
    }

    /// Name the `Thread.run` frame into the memo unless it is current, so the
    /// thread's PUBLISHED trace (which names nothing itself) can list its
    /// rows. `name` answers `None` when it could not look (a busy
    /// class-manager lock), which keeps the memo as it was.
    pub(crate) fn name_if_stale(&mut self, name: impl FnOnce() -> Option<Option<StackTraceEntry>>) {
        let count = cratonvm_classloading::class_redefinition_count();
        if matches!(&self.named, Some((at, _)) if *at == count) {
            return;
        }
        if let Some(entry) = name() {
            self.named = Some((count, entry.map(|e| Arc::from(vec![e]))));
        }
    }

    /// Is the thread inside no recorded call?
    pub(crate) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// The largest interpreter depth any recorded call was entered at
    /// (`None`: none recorded). Interpreter round i6 wave 121, lane L3: the
    /// `Reflection.getCallerClass` fast arm's bound
    /// (`VmConfig::reflection_caller_fast_under_records`).
    pub(crate) fn deepest_interp_depth(&self) -> Option<u32> {
        self.calls.iter().map(|&(depth, _)| depth).max()
    }

    /// Would `self` and `other` splice the same rows ([`Self::published_splices`]
    /// at any count)? The calls, the yield mark and the memo, by identity.
    pub(crate) fn same_rows(&self, other: &Self) -> bool {
        self.calls == other.calls
            && self.incomplete == other.incomplete
            && match (&self.named, &other.named) {
                (None, None) => true,
                (Some((a, ea)), Some((b, eb))) => a == b && arc_slices_same(ea, eb),
                _ => false,
            }
    }

    /// How many recorded calls the thread is inside (round 15 wave 5, lane
    /// trace5: the `STTRACE_DBG_THREADRUN` census line, TR4W3-7).
    pub(crate) fn len(&self) -> usize {
        self.calls.len()
    }

    /// Compact, allocation-free repeat key for the common 0- or 1-call case
    /// (`Thread.run` on any spawned Java thread records 1 call). Returns `None`
    /// when `self.calls.len() > 1`.
    pub(crate) fn repeat_key(&self) -> Option<(usize, (u32, u32), bool, u64, usize)> {
        if self.calls.len() > 1 {
            return None;
        }
        let first = self.calls.first().copied().unwrap_or((0, 0));
        let (named_at, named_ptr) = match &self.named {
            None => (u64::MAX, 0),
            Some((at, None)) => (*at, 1),
            Some((at, Some(arc))) => (*at, arc.as_ptr() as usize),
        };
        Some((self.calls.len(), first, self.incomplete, named_at, named_ptr))
    }

    /// One splice per call, the `Thread.run` frame named by `name` unless it
    /// was already named at the current class-redefinition count (which then
    /// refreshes the memo). Empty when the thread is inside no call or the
    /// frame cannot be named (the rules then answer, and they name nothing
    /// either: they use the same frame).
    pub(crate) fn splices(
        &mut self,
        name: impl FnOnce() -> Option<StackTraceEntry>,
    ) -> Vec<ReflectiveSplice> {
        if self.incomplete || self.calls.is_empty() {
            return Vec::new();
        }
        let count = cratonvm_classloading::class_redefinition_count();
        if !matches!(&self.named, Some((at, _)) if *at == count) {
            self.named = Some((count, name().map(|entry| Arc::from(vec![entry]))));
        }
        match self.named.as_ref().and_then(|(_, entries)| entries.clone()) {
            Some(entries) => self.rows(&entries),
            None => Vec::new(),
        }
    }

    /// The `(interpreter depth, JIT entry-chain length)` of each served call
    /// the thread is inside, outermost first; `None` when the rows no longer
    /// list every one ([`Self::incomplete`]). For the debuggers' listings
    /// ([`served_thread_run_debugger_entries`]; the i54-L1 retirement).
    pub(crate) fn call_depths(&self) -> Option<&[(u32, u32)]> {
        (!self.incomplete).then_some(self.calls.as_slice())
    }

    /// [`Self::splices`] for a reader holding the thread shared: a stale memo
    /// is named afresh by `name` and not kept.
    pub(crate) fn splices_unmemoized(
        &self,
        name: impl FnOnce() -> Option<StackTraceEntry>,
    ) -> Vec<ReflectiveSplice> {
        if self.incomplete || self.calls.is_empty() {
            return Vec::new();
        }
        let count = cratonvm_classloading::class_redefinition_count();
        let entries: Option<Arc<[StackTraceEntry]>> = match &self.named {
            Some((at, entries)) if *at == count => entries.clone(),
            _ => name().map(|entry| Arc::from(vec![entry])),
        };
        match entries {
            Some(entries) => self.rows(&entries),
            None => Vec::new(),
        }
    }

    pub(in crate::runtime::stackwalker) fn rows(&self, entries: &Arc<[StackTraceEntry]>) -> Vec<ReflectiveSplice> {
        self.calls
            .iter()
            .map(|&(interp_depth, jit_depth)| ReflectiveSplice {
                interp_depth,
                jit_depth,
                entries: Arc::clone(entries),
                raise: None,
            })
            .collect()
    }
}

/// The `java.lang.Thread.run()` frame a served call's row lists: at its only
/// call of `runWith` (JDK 21+) or `target.run()` (JDK 17), with its line
/// ([`thread_run_site_entry`], the frame the rules build). `None` when
/// `java.lang.Thread` is not found or its `run` has no such single call.
pub(crate) fn served_thread_run_entry(store: &ClassStore) -> Option<StackTraceEntry> {
    let thread_id = find_class_id_by_name_memoized(store, THREAD_CLASS)?;
    let thread = store.get(thread_id)?;
    thread_run_site_entry(thread, thread_id)
}

/// The frames a debugger's listing shows for one served `Thread.run` call,
/// OUTERMOST first (the i54-L1 retirement): `Thread.run` at its call of
/// `runWith` ([`served_thread_run_entry`]), then (JDK 21+) the `@Hidden`
/// `Thread.runWith` at its `op.run()`, which JVMTI and JDWP list where a
/// stack trace hides it (MEASURED, HotSpot 25.0.3: `Thread.runWith at 5`
/// above `Thread.run at 19`, `tools/probes/interp/L1/L1K54JvmtiLateAgentLambdaFrames.java`).
/// `None` when `Thread.run` cannot be named.
pub(crate) fn served_thread_run_debugger_entries(
    store: &ClassStore,
) -> Option<Vec<StackTraceEntry>> {
    use cratonvm_reader::instruction::Instruction;
    let thread_id = find_class_id_by_name_memoized(store, THREAD_CLASS)?;
    let thread = store.get(thread_id)?;
    let mut entries = vec![thread_run_site_entry(thread, thread_id)?];
    let run_with = thread.methods.iter().enumerate().find(|(_, m)| {
        &*m.name == "runWith" && &*m.descriptor == "(Ljava/lang/Object;Ljava/lang/Runnable;)V"
    });
    let Some((index, method)) = run_with else {
        return Some(entries);
    };
    let Some(code) = method.code() else {
        return Some(entries);
    };
    let bytes: &[u8] = &code.code;
    let mut site = None;
    let mut pc = 0usize;
    while pc < bytes.len() {
        let Ok((instruction, next)) = Instruction::decode(bytes, pc) else {
            break;
        };
        if let Instruction::Invokeinterface {
            index: cp_index, ..
        } = instruction
        {
            if let Some(("java/lang/Runnable", "run", "()V")) =
                cp_method_ref(&thread.constant_pool, cp_index)
            {
                site = Some(pc);
                break;
            }
        }
        if next <= pc {
            break;
        }
        pc = next;
    }
    if let Some(site) = site {
        entries.push(standin_entry(thread, thread_id, method, index, Some(site)));
    }
    Some(entries)
}

/// `rows` ([`ServedThreadRunCalls::splices`]) merged into `splices` (a
/// thread's reflective splices, [`reflective_splices`]), ordered by where the
/// calls stand, `(interpreter depth, JIT depth)`: the calls nest, so both
/// only grow from an outer call to an inner one, and the positions the
/// capture computes never decrease. Stable: at equal depths the reflective
/// call (entered first: `Method.invoke` of `Thread.run`) stays outer. Merge
/// AFTER `vm_exec::attach_reflective_raise`, which reads `splices.last()` as
/// the innermost REFLECTIVE call.
pub(crate) fn merge_served_thread_run_splices(
    splices: &mut Vec<ReflectiveSplice>,
    rows: Vec<ReflectiveSplice>,
) {
    if rows.is_empty() {
        return;
    }
    splices.extend(rows);
    splices.sort_by_key(|s| (s.interp_depth, s.jit_depth));
}

// Round 14 wave 5 (lane trace4; T3-1 of `jit-r13-trace3-proposals-RETIRED-20260929.md`,
// `r13w13-trace3-vm-served-jdk-methods-residuals` item 4). HotSpot shows the
// frame of EVERY `native` method that raises a throwable -- the frame is
// pushed before the native body runs:
//
// ```text
// java.lang.ArrayIndexOutOfBoundsException: arraycopy: last source index 11 out of bounds for int[10]
//     at java.base/java.lang.System.arraycopy(Native Method)
//     at Main.main(Main.java:3)
// ```
//
// The VM calls a native without a Java frame, so the capture had the caller
// on top. The census above rebuilds whole chains for eleven methods; this is
// the one-frame rule for the rest: the innermost captured frame stands at an
// `invokestatic` / `invokespecial` / `invokevirtual` whose resolved method is
// `ACC_NATIVE` in the real class bytes, and the throwable is one the call
// cannot have raised BEFORE entering it. What an invoke raises itself:
//
// - `NullPointerException` for a null receiver (every non-static call);
// - resolution and initialization failures of the call (the
//   `IncompatibleClassChangeError` family: `NoSuchMethodError`,
//   `IllegalAccessError`, `AbstractMethodError`; `NoClassDefFoundError`,
//   `ExceptionInInitializerError`); `UnsatisfiedLinkError` is raised by the
//   native's own linking, inside its frame on HotSpot, and is kept;
// - `StackOverflowError`.
//
// And the call must enter THAT native: `invokestatic` / `invokespecial` do;
// an `invokevirtual` only when the method cannot be overridden (`final`,
// `private`, or a `final` class) or is `Object.clone` (whose `protected`
// access pins the receiver to the caller's own subclasses, whose overrides
// push frames). A non-final virtual native (`Object.hashCode`) may have
// dispatched to a VM-served override, whose throwable is not the native's.
// `MethodHandle` / `VarHandle` natives are signature-polymorphic: the VM runs
// their TARGET there, never a native frame, and are refused.
//
// The census runs first; this answers only when it did not.

/// Lock-free pre-screen of the innermost frame for [`native_leaf_frame`]:
/// `false` when it certainly does not stand at a call of a `native` method.
pub(crate) fn may_stand_at_native_call(innermost: &BacktraceFrame) -> bool {
    match innermost {
        BacktraceFrame::Method { method, bci, .. } => {
            usize::try_from(*bci).is_ok_and(|bci| code_may_call_native_at(&method.code, bci))
        }
        _ => true,
    }
}

/// `code[bci]` is an `invokevirtual` / `invokestatic`, or an `invokespecial`
/// not followed by `athrow`: `throw new X(..)` stands at `X.<init>` (never
/// native) when the capture runs, and that is most throws, so it is refused
/// here without the class-manager lock.
pub(in crate::runtime::stackwalker) fn code_may_call_native_at(code: &[u8], bci: usize) -> bool {
    match code.get(bci).copied() {
        Some(0xb6 | 0xb8) => true,
        Some(0xb7) => code.get(bci + 3).copied() != Some(0xbf),
        _ => false,
    }
}

/// See the section comment above: may `throwable` have been raised INSIDE a
/// native entered by the call (`static_call`: an `invokestatic`)?
pub(in crate::runtime::stackwalker) fn native_leaf_throwable_admitted(
    store: &ClassStore,
    throwable: &Class,
    static_call: bool,
) -> bool {
    match &*throwable.name {
        "java/lang/NullPointerException" => return static_call,
        "java/lang/UnsatisfiedLinkError" => return true,
        "java/lang/StackOverflowError" => return false,
        _ => {}
    }
    let mut class = Some(throwable);
    for _ in 0..64 {
        let Some(c) = class else {
            return true;
        };
        if matches!(
            &*c.name,
            "java/lang/IncompatibleClassChangeError"
                | "java/lang/NoClassDefFoundError"
                | "java/lang/ExceptionInInitializerError"
        ) {
            return false;
        }
        class = c.superclass.and_then(|id| store.get(id));
    }
    false
}

/// Wave 58, lane L3: a class-initialization failure (`throwable`, an internal
/// name) the call of `owner.name descriptor` cannot have raised before
/// entering it, because the call is an instance call of a native of
/// `jdk.internal.misc.Unsafe` -- a `final` class initialized at boot -- that
/// initializes the class it is handed: `ensureClassInitialized0` (the JDK's
/// `Lookup.ensureInitialized`, `MethodHandleAccessorFactory`, the
/// `DirectMethodHandle` barrier) and `allocateInstance` (`sun.misc.Unsafe`'s,
/// a constructor handle's allocation). HotSpot lists its `(Native Method)` frame
/// on top (`L3W58ReflectiveClassInit`, `--real-jdk`).
/// `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES=0` refuses it with the rest.
pub(in crate::runtime::stackwalker) fn initializing_native_raised(owner: &str, name: &str, descriptor: &str, throwable: &str) -> bool {
    owner == "jdk/internal/misc/Unsafe"
        && matches!(
            (name, descriptor),
            ("ensureClassInitialized0", "(Ljava/lang/Class;)V")
                | ("allocateInstance", "(Ljava/lang/Class;)Ljava/lang/Object;")
        )
        && CLASS_INIT_FAILURES.contains(&throwable)
}

/// A `NoClassDefFoundError` the call of `owner.name descriptor` cannot have
/// raised before entering it, because the call is one of the reflective
/// listing natives of `java.lang.Class` -- a class loaded, linked and
/// initialized at boot, whose private natives the call resolves without
/// loading anything -- which resolve every listed member's signature types
/// through the holder's loader (`CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES`,
/// as HotSpot's `Reflection::new_method` / `new_constructor` / `new_field`
/// do) and raise one for a type no loader finds. HotSpot lists the native's
/// `(Native Method)` frame on top (measured, JDK 25.0.3:
/// `L3W54ReflectionLoadsSignatureTypes.java`, row
/// `missing`). Item 3 of
/// `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007` (2026-10-07).
/// `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES=0` refuses it with the rest.
pub(in crate::runtime::stackwalker) fn reflective_listing_native_raised(
    owner: &str,
    name: &str,
    descriptor: &str,
    throwable: &str,
) -> bool {
    owner == "java/lang/Class"
        && matches!(
            (name, descriptor),
            ("getDeclaredMethods0", "(Z)[Ljava/lang/reflect/Method;")
                | ("getDeclaredFields0", "(Z)[Ljava/lang/reflect/Field;")
                | (
                    "getDeclaredConstructors0",
                    "(Z)[Ljava/lang/reflect/Constructor;"
                )
        )
        && throwable == "java/lang/NoClassDefFoundError"
}

/// j27 wave 5 (lane REFLECT; page
/// `j27-spoofed-member-icce-omits-the-getdeclaringclass0-frame-20261028.md`):
/// an `IncompatibleClassChangeError` or `NoClassDefFoundError` the call of
/// `owner.name descriptor` cannot have raised before entering it, because the
/// call is one of `java.lang.Class`'s private nesting natives (a boot class:
/// nothing to resolve or initialize) that run HotSpot's
/// `Reflection::check_for_inner_class` (`"<outer> and <inner> disagree on
/// InnerClasses attribute"`) and resolve classes through `klass_at` (its
/// NCDFE): `getDeclaringClass0` (`JVM_GetDeclaringClass` ->
/// `compute_enclosing_class`) and `getDeclaredClasses0`
/// (`JVM_GetDeclaredClasses`). HotSpot lists the native's `(Native Method)`
/// frame on top (`tools/probes/jit/j27/J27Mh2Rows.java`, `mh717-2-simple-name`:
/// `Class.getDeclaringClass0 | Class.isTopLevelClass | ...`).
/// `getSimpleBinaryName0` is not one: its `find_inner_classes_attr` runs no
/// check. `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES=0` refuses it with the rest.
pub(in crate::runtime::stackwalker) fn class_nesting_native_raised(owner: &str, name: &str, descriptor: &str, throwable: &str) -> bool {
    owner == "java/lang/Class"
        && matches!(
            (name, descriptor),
            ("getDeclaringClass0", "()Ljava/lang/Class;")
                | ("getDeclaredClasses0", "()[Ljava/lang/Class;")
        )
        && matches!(
            throwable,
            "java/lang/IncompatibleClassChangeError" | "java/lang/NoClassDefFoundError"
        )
}

/// May the `native` the call `owner.name descriptor` names have raised
/// `throwable` inside its own frame? The general rule
/// ([`native_leaf_throwable_admitted`]), then the natives that raise what it
/// refuses.
pub(in crate::runtime::stackwalker) fn native_leaf_raise_admitted(
    store: &ClassStore,
    throwable: &Class,
    static_call: bool,
    (owner, name, descriptor): (&str, &str, &str),
) -> bool {
    let raised = &*throwable.name;
    native_leaf_throwable_admitted(store, throwable, static_call)
        || initializing_native_raised(owner, name, descriptor, raised)
        || reflective_listing_native_raised(owner, name, descriptor, raised)
        || class_nesting_native_raised(owner, name, descriptor, raised)
}

/// The `(Native Method)` frame of the `native` method the innermost captured
/// frame was calling, or `None`. See the section comment above.
/// `resolve_class(requester, name)` resolves a class name as the requester's
/// loader would. `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES=0` turns it off (read
/// only once a frame was found).
pub(crate) fn native_leaf_frame(
    store: &ClassStore,
    innermost: &BacktraceFrame,
    throwable: &Class,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<BacktraceFrame> {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use cratonvm_reader::instruction::Instruction;
    #[derive(Clone, Copy, PartialEq)]
    enum Call {
        Static,
        Special,
        Virtual,
    }
    let (caller_id, caller_name, caller_descriptor, bci) = standin_call_site(innermost)?;
    let caller_class = store.get(caller_id)?;
    let caller = caller_class.find_method(&caller_name, &caller_descriptor)?;
    let code = caller.code()?;
    let (cp_index, call) = match Instruction::decode(&code.code, bci).ok()?.0 {
        Instruction::Invokestatic(cp_index) => (cp_index, Call::Static),
        Instruction::Invokespecial(cp_index) => (cp_index, Call::Special),
        Instruction::Invokevirtual(cp_index) => (cp_index, Call::Virtual),
        _ => return None,
    };
    let (owner, name, descriptor) = cp_method_ref(&caller_class.constant_pool, cp_index)?;
    if name.starts_with('<')
        || !native_leaf_raise_admitted(
            store,
            throwable,
            call == Call::Static,
            (owner, name, descriptor),
        )
    {
        return None;
    }
    let owner_id = if owner == &*caller_class.name {
        caller_id
    } else {
        resolve_class(caller_id, owner)?
    };
    // Interface methods are never `native` (JVMS 4.6).
    if store.get(owner_id)?.is_interface() {
        return None;
    }
    // JVMS 6.5 `invokespecial` of a superclass method (`super.clone()`):
    // selection starts at the caller's direct superclass.
    let mut id = if call == Call::Special && owner_id != caller_id {
        caller_class.superclass?
    } else {
        owner_id
    };
    for _ in 0..64 {
        let class = store.get(id)?;
        let found = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor);
        let Some((index, method)) = found else {
            id = class.superclass?;
            continue;
        };
        if !method.is_native()
            || method.is_static() != (call == Call::Static)
            || matches!(
                &*class.name,
                "java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle"
            )
        {
            return None;
        }
        // A hidden native (`@Hidden`, a hidden class) is no frame on HotSpot;
        // and the `--real-jdk` hidden drop may have left the innermost frame
        // standing at a call INTO hidden frames that raised the throwable.
        if method_is_hidden(store, id, name, Some(descriptor), u32::try_from(index).ok()) {
            return None;
        }
        let entered = match call {
            Call::Static | Call::Special => true,
            Call::Virtual => {
                method
                    .access_flags
                    .intersects(MethodAccessFlags::FINAL | MethodAccessFlags::PRIVATE)
                    || class.is_final()
                    || (&*class.name == "java/lang/Object" && name == "clone")
            }
        };
        if !entered
        {
            return None;
        }
        return Some(BacktraceFrame::Entry(standin_entry(
            class, id, method, index, None,
        )));
    }
    None
}

/// JIT round 23 wave 3, lane L3
/// (`docs/feature-designs/native-dispatch-architecture.md`;
/// `CRATONVM_THROWABLE_C2_INTRINSIC_LEAF_DROP`, default ON, `=0` off): drop
/// from `extra` (the stand-in frames about to go on top of a trace,
/// OUTERMOST-first, so its last element is the new top) the `(Native Method)`
/// leaf HotSpot's C2 does not have, when the innermost captured frame is a
/// COMPILED activation (`innermost` not an interpreter frame).
///
/// C2 intrinsifies `System.arraycopy` and `Array.newArray`; their failing
/// checks raise from the runtime with the compiled caller on top, so a hot
/// trace has no native frame (measured, JDK 25.0.3,
/// `C:\craton\n23p\w3L3\A1Copy.java`, `-XX:-OmitStackTraceInFastThrow`):
///
/// * `System.arraycopy` raising `ArrayIndexOutOfBoundsException` or
///   `ArrayStoreException`: no `arraycopy` frame (its NPE is an uncommon trap
///   back into the interpreter, which calls the native: the frame stays);
/// * `Array.newArray` raising `NegativeArraySizeException`: no `newArray`
///   frame, `Array.newInstance` (inlined) stays on top.
///
/// HotSpot's C1 keeps both frames (`-XX:TieredStopAtLevel=1`), so only an
/// activation that stands for C2 code drops the leaf: `activation_is_ir(label)`
/// answers it for the innermost compiled activation
/// [`innermost_compiled_activation_label`] names -- the caller applies the
/// VM's rule (`exceptions::body_is_c2_equivalent`: an optimizing-tier body, or
/// a single-pass body past HotSpot's C2 entry threshold) to the live
/// activation, and answers `false` for one it cannot find, which keeps the
/// frame (the interpreter's and C1's answer). An interpreted innermost frame
/// is asked about too ([`InnermostActivation::Interpreted`], wave 4): a method
/// this VM keeps interpreted that HotSpot would run as C2 code. Returns
/// whether a frame was dropped.
pub(crate) fn drop_c2_intrinsic_native_leaf(
    trace: &[BacktraceFrame],
    throwable_class: &str,
    extra: &mut Vec<BacktraceFrame>,
    activation_is_ir: &mut dyn FnMut(InnermostActivation<'_>) -> bool,
) -> bool {
    let Some(activation) = innermost_activation(trace) else {
        return false;
    };
    let Some(top) = extra.last() else {
        return false;
    };
    let entry = top.to_entry();
    if !c2_intrinsic_drops_native_leaf(&entry.class_name, &entry.method_name, throwable_class)
        || !activation_is_ir(activation)
    {
        return false;
    }
    extra.pop();
    true
}

/// What [`drop_c2_intrinsic_native_leaf`] asks its caller about: the compiled
/// activation the innermost frame belongs to (by label), or -- JIT round 23
/// wave 4, lane L3 -- the innermost INTERPRETER frame's method. HotSpot runs
/// C2 code for a method it has invoked past its C2 threshold; this VM may keep
/// such a method interpreted (`n23-w4`: `H8Msgs.k`, 370 932 invocations,
/// `ineligible-by-policy` after repeated recompiles), and the trace must still
/// read as HotSpot's hot one.
#[derive(Clone, Copy)]
pub(crate) enum InnermostActivation<'a> {
    /// A compiled activation, by its artifact label.
    Compiled(&'a str),
    /// An interpreter frame, by its method.
    Interpreted(&'a crate::classloading::resolution::CachedBytecodeMethod),
}

/// [`InnermostActivation`] of `trace` (OUTERMOST-first): the compiled
/// activation [`innermost_compiled_activation_label`] names, else the
/// innermost frame's method when it is an interpreter frame
/// ([`BacktraceFrame::Method`]); `None` for any other frame. Pure.
pub(crate) fn innermost_activation(trace: &[BacktraceFrame]) -> Option<InnermostActivation<'_>> {
    if let Some(label) = innermost_compiled_activation_label(trace) {
        return Some(InnermostActivation::Compiled(label));
    }
    match trace.last()? {
        BacktraceFrame::Method { method, .. } => Some(InnermostActivation::Interpreted(&**method)),
        _ => None,
    }
}

/// The label of the compiled ACTIVATION the innermost frame of `trace`
/// (OUTERMOST-first) belongs to: the innermost frame itself when it is a
/// [`BacktraceFrame::Compiled`], or the nearest `Compiled` frame below a run
/// of [`BacktraceFrame::Inlined`] levels (the callees that activation inlined).
/// `None` when the innermost frame is an interpreter (or any other) frame.
/// Pure.
pub(crate) fn innermost_compiled_activation_label(trace: &[BacktraceFrame]) -> Option<&str> {
    for frame in trace.iter().rev() {
        match frame {
            BacktraceFrame::Inlined { .. } => continue,
            BacktraceFrame::Compiled { label, .. } => return Some(&**label),
            _ => return None,
        }
    }
    None
}

/// The rows of [`drop_c2_intrinsic_native_leaf`]: does HotSpot's C2 raise
/// `throwable_class` out of its intrinsic of native `class.method` with no
/// frame of that native? Pure.
pub(crate) fn c2_intrinsic_drops_native_leaf(
    class: &str,
    method: &str,
    throwable_class: &str,
) -> bool {
    match (class, method) {
        ("java/lang/System", "arraycopy") => matches!(
            throwable_class,
            "java/lang/ArrayIndexOutOfBoundsException" | "java/lang/ArrayStoreException"
        ),
        ("java/lang/reflect/Array", "newArray") => {
            throwable_class == "java/lang/NegativeArraySizeException"
        }
        _ => false,
    }
}


/// `Reflection.getCallerClass()`'s fast answer
/// (`NativeExceptionAccess::reflection_caller_class_fast`; interpreter round
/// i2 wave 60, lane L3): may the frame of class `caller` right below the
/// asking `@CallerSensitive` frame of class `asker` be the answer without the
/// walkers' capture? Only when JDK 25's security walk can never skip it --
/// not `java.lang.reflect.Method` (its `invoke`), nothing in
/// `jdk.internal.reflect` (the method accessors) or `java.lang.invoke` (the
/// adapter frames) -- and the asker is no stream plumbing (whose collapsed
/// inline chain the whole rule treats apart). Anything else answers `false`
/// and runs the whole rule.
pub(crate) fn caller_frame_never_skipped(asker: &str, caller: &str) -> bool {
    let plumbing = |name: &str| {
        name.starts_with("java/util/stream/")
            || name == "java/util/ServiceLoader"
            || (name.starts_with("java/util/") && name.contains("Spliterator"))
    };
    !(caller == "java/lang/reflect/Method"
        || caller.starts_with("jdk/internal/reflect/")
        || caller.starts_with("java/lang/invoke/")
        || plumbing(asker))
}


// ---------------------------------------------------------------------------
// A native entered by a by-name door (interpreter round i2 wave 60, lane L3)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`.
// `Method.invoke` (its served bridge, or the JDK accessor's method handle) and
// `MethodHandle.invokeExact` reach their target through the VM's by-name
// doors (`NativeInvokeAccess::invoke_*`), with no bytecode call site naming
// it: [`native_leaf_frame`] decodes the caller's `invokevirtual
// Method.invoke` / `MethodHandle.invokeExact` and answers nothing. HotSpot
// lists the native's `(Native Method)` frame on top, as for a direct call
// (`L3W59ReflectiveNativeTargetFrame`). The door that entered a native which
// raised an unbuilt error (the funnel's mark, set inside the door) notes the
// native on the mark ([`ServedNativeRaise::native_callee`]); the capture lists
// it when every other rule answered nothing and nothing but the throwable's
// own construction stands above the marked frame -- whatever is on top of
// it in the trace (a reflective splice, a dropped hidden frame) is what
// HotSpot lists below the native. Under
// `CRATONVM_THROWABLE_NATIVE_LEAF_FRAMES`, the one-frame rule's switch.

/// The `native` method the by-name door that entered it names: `name` and
/// `descriptor` selected from `start` up its superclasses (the receiver's
/// class for a virtual door, the named class otherwise), as `(class, slot)`,
/// or `None` when the selected method is not `ACC_NATIVE` in the real class
/// bytes, is a signature-polymorphic `MethodHandle` / `VarHandle` invoker
/// (whose target the VM runs there), or is hidden.
///
/// `any_kind` (interpreter round i2 wave 61, lane L3;
/// `VmConfig::served_raise_keyed`): a method with code is taken too -- a JDK
/// Java method a native served (`Object.wait(long)`, `Array.newInstance`
/// under `--compatible`), whose frames [`keyed_served_raise_frames`] lists.
/// An abstract selection still answers nothing.
pub(crate) fn door_native_callee(
    store: &ClassStore,
    start: ClassId,
    name: &str,
    descriptor: &str,
    any_kind: bool,
) -> Option<(ClassId, u32)> {
    if name.starts_with('<') {
        return None;
    }
    let mut id = start;
    for _ in 0..64 {
        let class = store.get(id)?;
        let found = class
            .methods
            .iter()
            .enumerate()
            .find(|(_, m)| &*m.name == name && &*m.descriptor == descriptor);
        let Some((index, method)) = found else {
            id = class.superclass?;
            continue;
        };
        let kind_taken = if any_kind {
            !method.is_abstract()
        } else {
            method.is_native()
        };
        if !kind_taken
            || class.is_interface()
            || matches!(
                &*class.name,
                "java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle"
            )
        {
            return None;
        }
        let slot = u32::try_from(index).ok()?;
        if method_is_hidden(store, id, name, Some(descriptor), Some(slot)) {
            return None;
        }
        return Some((id, slot));
    }
    None
}

/// The `(Native Method)` frame of the mark's door callee
/// ([`ServedNativeRaise::native_callee`]), re-read from the class store (a
/// redefinition that moved the slot answers nothing).
pub(crate) fn door_native_callee_frame(
    store: &ClassStore,
    (class_id, slot): (ClassId, u32),
) -> Option<BacktraceFrame> {
    let class = store.get(class_id)?;
    let index = usize::try_from(slot).ok()?;
    let method = class.methods.get(index)?;
    if !method.is_native()
    {
        return None;
    }
    Some(BacktraceFrame::Entry(standin_entry(
        class, class_id, method, index, None,
    )))
}

// ---------------------------------------------------------------------------
// The served-raise families, keyed by method (interpreter round i2 wave 61,
// lane L3)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`.
// Every family above learns WHICH method a native served by decoding the
// innermost captured frame's call instruction. A served method a by-name door
// entered (the served `Method.invoke`'s target, a native calling a native)
// has no call site naming it: `--compatible`'s `Object.wait(long)` and
// `Array.newInstance` reached by `Method.invoke` listed no JDK frame
// (`L3W60CompatibleServedReflectiveFrames`). Under
// `VmConfig::served_raise_keyed` the door notes the method it selected,
// whatever its kind, on the funnel's mark (`ServedNativeRaise::native_callee`),
// and [`keyed_served_raise_frames`] gives the families' answer for that
// METHOD -- the same bodies the call-site decoders reach
// ([`standin_chain_from`], [`standin_arg_check_frames`],
// [`callback_native_chain`], [`served_raise_method_frames`]) -- which the
// capture appends above whatever stands on the marked frame, as wave 60
// appends a native callee's own frame.

/// The frames HotSpot lists for the method `(class_id, slot)` raising a
/// `throwable_class`, OUTERMOST-first, or `None`: a native's own frame; a
/// census method's chain down to its native leaf (`virtual_thread`: the
/// virtual-thread screen); an argument check's one frame; a `Class.forName`'s
/// chain down to `forName0`; else the served-raise rule's frames for the
/// method (its one raise site, its one raising call, a noted chain).
pub(crate) fn keyed_served_raise_frames(
    store: &ClassStore,
    (class_id, slot): (ClassId, u32),
    throwable_class: &str,
    virtual_thread: bool,
    hints: ServedRaiseHints<'_>,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    if SERVED_RAISE_EXCLUDED.contains(&throwable_class) {
        return None;
    }
    let class = store.get(class_id)?;
    let index = usize::try_from(slot).ok()?;
    let method = class.methods.get(index)?;
    if method.is_native() {
        return door_native_callee_frame(store, (class_id, slot)).map(|frame| vec![frame]);
    }
    if method.is_abstract() {
        return None;
    }
    let (name, descriptor): (&str, &str) = (&method.name, &method.descriptor);
    let frames = keyed_family_frames(
        store,
        (class_id, class),
        (name, descriptor),
        throwable_class,
        virtual_thread,
        resolve_class,
    )
    .or_else(|| {
        served_raise_method_frames(
            store,
            (class_id, class, index, method),
            throwable_class,
            hints,
            resolve_class,
        )
    });
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[SERVED_RAISE_KEYED] {}.{name}{descriptor} raised {throwable_class}: {} frame(s)",
            class.name,
            frames.as_ref().map_or(0, Vec::len)
        );
    }
    frames
}

/// The stand-in families of [`keyed_served_raise_frames`]: the census, the
/// argument checks and the `Class.forName` chain, for the method
/// `(name, descriptor)` of `class`.
pub(in crate::runtime::stackwalker) fn keyed_family_frames<'s>(
    store: &'s ClassStore,
    (class_id, class): (ClassId, &'s Class),
    (name, descriptor): (&str, &str),
    throwable_class: &str,
    virtual_thread: bool,
    resolve_class: &mut dyn FnMut(ClassId, &str) -> Option<ClassId>,
) -> Option<Vec<BacktraceFrame>> {
    let row_here =
        |m: &StandinMethod| m.class == &*class.name && m.name == name && m.descriptor == descriptor;
    if throwable_class == STANDIN_ARG_CHECK_THROWABLE
        && standin_rows(StandinFamily::ArgCheck).iter().any(row_here)
    {
        if let Some(frames) = standin_arg_check_frames(store, class_id, name, descriptor) {
            return Some(frames);
        }
    }
    if is_standin_throwable(throwable_class) {
        let screen = if virtual_thread {
            StandinScreen::VirtualThrowable(throwable_class)
        } else {
            StandinScreen::Throwable(throwable_class)
        };
        let entry = screen.rows().any(|m| {
            row_here(m) && screen.enters_at(m) && screen.admits(m) && standin_row_enabled(m)
        });
        if entry {
            let frames = standin_chain_from(
                store,
                (class_id, class),
                (name, descriptor),
                screen,
                resolve_class,
            );
            let virtual_wait_off = virtual_thread
                && &*class.name == "java/lang/Object"
                && false;
            if frames.is_some() && !virtual_wait_off {
                return frames;
            }
        }
    }
    if throwable_class != "java/lang/StackOverflowError"
        && &*class.name == CALLBACK_NATIVE_CLASS
        && CALLBACK_NATIVE_ENTRIES
            .iter()
            .any(|&(n, d)| n == name && d == descriptor)
    {
        let (name, descriptor) = CALLBACK_NATIVE_ENTRIES
            .iter()
            .copied()
            .find(|&(n, d)| n == name && d == descriptor)?;
        if let Some(frames) = callback_native_chain(class_id, class, (name, descriptor)) {
            return callback_native_frames_enabled().then_some(frames);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// A served `Class` member lookup's miss (interpreter round i1 wave 52, lane L3)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/native-dispatch-architecture.md`.
// The VM serves `Class.getField`, `getDeclaredField`, `getMethod`,
// `getDeclaredMethod`, `getConstructor` and `getDeclaredConstructor` with
// Bridges (`--compatible` all six, `--real-jdk` all but `getDeclaredMethod`),
// which raise the miss's `NoSuchFieldException` / `NoSuchMethodException`
// from Rust, so the capture had the caller on top. HotSpot 25 runs the JDK
// methods and starts the trace at the one `throw new ...` of the method
// (`L3W51MemberLookupThrowFrames`, `L3W52MemberLookupFrames`):
//
// ```text
// getField / getDeclaredField / getMethod / getDeclaredMethod:
//     Class.<that method>(Class.java:<its throw's line>), caller
// getConstructor / getDeclaredConstructor:
//     Class.getConstructor0(Class.java:<its throw's line>),
//     Class.<that method>(Class.java:<its getConstructor0 call's line>), caller
// ```
//
// The same holds for `Class.cast` / `asSubclass`, which `--compatible` serves
// with Bridges: HotSpot's `ClassCastException` starts at the method's one
// `throw new ClassCastException(..)`.
//
// When the innermost captured frame stands at an `invokevirtual` of one of
// those methods of `java.lang.Class` (final: the call enters that method),
// and the throwable is the one the method raises, the frames are rebuilt from
// the real class bytes: the method at its ONE call of `getConstructor0` where
// the row names it ([`only_call_to`]), and the throwing method at its ONE
// construction of the throwable ([`throwable_init_sites`]). Any other shape answers nothing. A
// lookup whose JDK body ran has its own frame on top and never stands here.
// `CRATONVM_THROWABLE_MEMBER_LOOKUP_FRAMES` (default on since the lane's last
// commit; read only once frames were rebuilt).

pub(in crate::runtime::stackwalker) const MEMBER_LOOKUP_CLASS: &str = "java/lang/Class";
pub(in crate::runtime::stackwalker) const NO_SUCH_FIELD_EXCEPTION: &str = "java/lang/NoSuchFieldException";
pub(in crate::runtime::stackwalker) const NO_SUCH_METHOD_EXCEPTION: &str = "java/lang/NoSuchMethodException";
pub(in crate::runtime::stackwalker) const CLASS_CAST_EXCEPTION: &str = "java/lang/ClassCastException";
/// `Class.getConstructor0(Class[], int)`, which both public constructor
/// lookups call and which raises their `NoSuchMethodException`.
pub(in crate::runtime::stackwalker) const GET_CONSTRUCTOR0: (&str, &str, &str) = (
    MEMBER_LOOKUP_CLASS,
    "getConstructor0",
    "([Ljava/lang/Class;I)Ljava/lang/reflect/Constructor;",
);

