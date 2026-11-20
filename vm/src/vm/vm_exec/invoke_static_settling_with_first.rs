#[allow(unused_imports)]
use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn invoke_static_settling_with_first_class(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owner: Option<ClassId>,
    settled: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    first_class: Option<ClassId>,
) -> (MethodCallResult, Option<ClassId>) {
    let (owner, fresh) = match owner {
        Some(owner) => (owner, None),
        None => {
            let (owner, fresh) = load_class_pinning_args(shared, thread, class_name, args);
            match owner {
                Ok(owner) => (owner, fresh),
                Err(e) => return (Err(e.into()), None),
            }
        }
    };
    let args = fresh.as_deref().unwrap_or(args);
    if settled == Some(owner) {
        // Round 13 wave 11 (lane callcost5): the static half of the
        // native->Java callback memo (`runtime::native_callee_memo`). A hit is
        // the ordinary tail's own `interpreter::execute`, on the declaring
        // class the tail reached for this owner, first-argument class, name
        // and descriptor. `CRATONVM_NATIVE_CALLBACK_MEMO_STATIC=0` disables it.
        let memo_key = (crate::runtime::env_cache::native_callback_memo()
            && crate::runtime::env_cache::native_callback_memo_static()
            && (crate::runtime::native_callee_memo::SURVIVES_REDEFINITION
                || !crate::classloading::any_class_redefined()))
        .then(|| first_class.map(|cid| cid.as_u32())
            .or_else(|| crate::runtime::native_callee_memo::static_first_arg_key(shared, args)))
        .flatten();
        if let Some(first_key) = memo_key {
            if let Some(declaring) = crate::runtime::native_callee_memo::lookup_static(
                shared,
                owner,
                first_key,
                method_name,
                descriptor,
            ) {
                let result = crate::runtime::interpreter::execute(
                    shared,
                    thread,
                    declaring,
                    method_name,
                    descriptor,
                    args,
                );
                return (result, settled);
            }
        }
        let save = memo_key.map(|first_key| {
            crate::runtime::native_callee_memo::arm_static(
                owner,
                first_key,
                method_name,
                descriptor,
                thread.frames.len(),
            )
        });
        let result =
            invoke_static_on_class_shared(shared, thread, owner, method_name, descriptor, args);
        if let (Some(save), Some(first_key)) = (save, memo_key) {
            if let Some(declaring) = crate::runtime::native_callee_memo::disarm(save) {
                crate::runtime::native_callee_memo::fill_static(
                    shared,
                    owner,
                    first_key,
                    method_name,
                    descriptor,
                    declaring,
                );
            }
        }
        return (result, settled);
    }
    let result = invoke_static_on_owner(
        shared,
        thread,
        owner,
        class_name,
        method_name,
        descriptor,
        args,
    );
    // nio21 w21 L2 (`r21-ite-reflective-invoke-pays-the-by-name-ladder-twice-CLOSED-20261012.md`
    // item 1): a call whose CALLEE threw settles too. Settling records only
    // that the owner needs no initialization any more -- the settled arm runs
    // the same `invoke_static_on_class_shared` this arm reaches once the init
    // is done, plus the static callback memo -- and a callee's own throwable
    // says nothing about that. Requiring `is_ok()` kept a target that always
    // throws (`Nio20JitRows ite`'s `thrower`) unsettled for good, so every
    // reflective call paid the whole by-name ladder. A failed `<clinit>`
    // still never settles: the owner is then not initialized, and the init
    // test below refuses it. Kill switch `CRATONVM_STATIC_SETTLE_ON_THROW=0`
    // (read only for a call that threw).
    let settles_on = match &result {
        Ok(_) => true,
        Err(MethodCallFailed::ExceptionThrown(_)) => {
            true
        }
        Err(_) => false,
    };
    let now_settled = (settles_on
        && static_owner_init_pending(shared, owner, class_name, method_name, descriptor).is_none())
    .then_some(owner);
    (result, now_settled)
}


/// Initialize the declaring class of a by-name STATIC call before a
/// registered native can answer it (JVMS §5.5): `invoke_or_native`'s registry
/// arms and `invoke_static_shared_on_class`'s native override run the native
/// without initializing. No policy of its own — every caller that reaches it
/// initializes. `args`' references are re-read after the initializer.
/// Interpreter round i1 waves 12-13, lane L5.
pub fn init_static_native_owner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owner_override: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &mut [Value],
) -> Result<(), MethodCallFailed> {
    match static_native_init_pending(shared, owner_override, class_name, method_name, descriptor) {
        Some(init_class) => initialize_pinning_args(shared, thread, init_class, args),
        None => Ok(()),
    }
}

/// zt t27/jd1 (`CRATONVM_JIT_STATIC_OWNER_LOADS_FIRST`, default ON, `=0`
/// kill; read once per VM, `SharedVm::jit_static_owner_loads_first`): the
/// load a compiled `invokestatic` of `class_name.method_name descriptor` owes
/// its symbolic owner before any door decides native vs bytecode, or `None`.
///
/// Every door decides on the LOADED declaration (`resolve_dispatch` through
/// [`registered_native_yields_to_loaded_bytecode`], or the allow-listed
/// `SyntheticStub` yield), and an owner no loader holds answers "the native
/// runs" at all of them. `execute_invokestatic` never asks on an absent
/// owner: it loads it first, by the route
/// `interpreter::static_owner_load_route` names. The JIT's static doors (the
/// native site cache's fill, the dispatch helper's static tail) did not, so a
/// compiled site reached before any interpreted execution of it (an OSR
/// body's cold tail) ran a `Bridge` or a `SyntheticStub` over the JDK body the
/// interpreter runs, and the site cache kept serving it after the owner was
/// loaded: `--compatible` `ManagementFactory.getPlatformMXBean` answered
/// `null` from compiled code (row `df7_nl_c`) and the bean when interpreted
/// (`x20_mf_c`); page `zgc-zt27jd1-compiled-static-call-decides-on-an-unloaded-owner-FIXED-20261024.md`.
///
/// `Some` only when the switch is on, the owner is not an array and is not
/// loaded, a registration other than an `Intrinsic` sits on the exact triple
/// (an `Intrinsic` wins over bytecode either way, `resolve_dispatch` step 2,
/// and a name with no registration has no native to run), the interpreter's
/// route loads such an owner at all, and, on the `--compatible` native route,
/// the load would not fabricate a compatibility stub (the route's own rule).
/// Cost: one class-manager lookup (a loaded owner, the steady state, stops
/// there), then one registry probe, on the fill and on the static tail only.
pub fn static_owner_load_pending(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<crate::runtime::interpreter::StaticOwnerLoad> {
    if !shared.jit_static_owner_loads_first || class_name.starts_with('[') {
        return None;
    }
    // The loaded owner first: the steady state of every hot tail call.
    let absent = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .is_none();
    if !absent {
        return None;
    }
    let kind = shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor)?;
    if kind == cratonvm_native_api::NativeKind::Intrinsic
        || shared
            .jit_static_owner_load_refused
            .lock()
            .contains(class_name)
    {
        return None;
    }
    let route = crate::runtime::interpreter::static_owner_load_route(
        shared,
        class_name,
        method_name,
        descriptor,
    )?;
    if route == crate::runtime::interpreter::StaticOwnerLoad::AbsentNative
        && shared
            .classes
            .class_manager
            .read()
            .would_fabricate_synthetic_stub(class_name)
    {
        return None;
    }
    Some(route)
}

/// Load a compiled `invokestatic`'s symbolic owner as `execute_invokestatic`
/// does when [`static_owner_load_pending`] says one is owed, with the
/// references in `args` pinned across the load (it can run a user loader) and
/// read back afterwards. The `--compatible` native route loads with no
/// transformer (`load_absent_native_static_owner`), the full route through
/// `load_class_transformed`. A failed load is ignored: the caller's tail then
/// decides on the absent owner exactly as before (the interpreter's full route
/// would raise its `NoClassDefFoundError` there), and the name is remembered
/// for this VM (`SharedVm::jit_static_owner_load_refused`) so it is not owed
/// a load again: a native-only name keeps one native site-cache entry instead
/// of a class-path scan per call. zt t27/jd1.
pub fn load_static_owner_before_dispatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &mut [Value],
) {
    let Some(route) = static_owner_load_pending(shared, class_name, method_name, descriptor) else {
        return;
    };
    let loaded = with_args_pinned(shared, thread, args, |shared, thread| match route {
        crate::runtime::interpreter::StaticOwnerLoad::AbsentNative => {
            shared.load_class_concurrent(class_name)
        }
        crate::runtime::interpreter::StaticOwnerLoad::Full => {
            shared.load_class_transformed(thread, class_name)
        }
    });
    if loaded.is_err() {
        shared
            .jit_static_owner_load_refused
            .lock()
            .insert(Box::from(class_name));
    }
}


/// [`invoke_or_native`] for a caller that knows its target is a STATIC
/// method, which `invoke_or_native` itself cannot tell from an instance one:
/// under [`static_by_name_init_enforced`] the declaring class of a registered
/// static native is initialized first, and the bytecode tail initializes the
/// declaring class rather than the symbolic owner ([`invoke_static_on_owner`],
/// wave 14) — both as `execute_invokestatic` does (JVMS §5.5).
///
/// Were the policy ever unenforced, the prelude would only print one
/// `[static-native-init-census]` line per class whose `<clinit>` it skips,
/// with the door census armed (`CRATONVM_DBG_FIELD_SITE`). `door` names the
/// caller in that line.
pub fn invoke_static_or_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    door: &'static str,
) -> MethodCallResult {
    let enforced = static_by_name_init_enforced(shared);
    if enforced || crate::runtime::interpreter::site_cache::site_stats::on() {
        if let Some(init_class) =
            static_native_init_pending(shared, None, class_name, method_name, descriptor)
        {
            if enforced {
                let mut fresh = args.to_vec();
                initialize_pinning_args(shared, thread, init_class, &mut fresh)?;
                return invoke_or_native_impl(
                    shared,
                    thread,
                    class_name,
                    method_name,
                    descriptor,
                    &fresh,
                    ByNameKind::Static,
                );
            }
            note_static_native_init_census(
                shared,
                door,
                init_class,
                class_name,
                method_name,
                descriptor,
            );
        }
    }
    invoke_or_native_impl(
        shared,
        thread,
        class_name,
        method_name,
        descriptor,
        args,
        ByNameKind::Static,
    )
}

/// [`invoke_static_or_native`] for a caller that has already initialized the
/// declaring class of a registered static native itself — the JIT funnel
/// tail (`jit_static_owner_init_before_native`): the by-name route with the
/// static tail ([`invoke_static_on_owner`]) and no second prelude.
pub fn invoke_static_or_native_prepared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_or_native_impl(
        shared,
        thread,
        class_name,
        method_name,
        descriptor,
        args,
        ByNameKind::Static,
    )
}

/// The `--compatible` census line of [`invoke_static_or_native`]: once per
/// class whose initialization the unenforced entry skips.
#[cold]
pub(in crate::vm::vm_exec) fn note_static_native_init_census(
    shared: &SharedVm,
    door: &str,
    init_class: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    static SEEN: std::sync::OnceLock<parking_lot::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    let (init_name, state) = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(init_class) {
            Some(class) => (class.name.to_string(), class.state),
            None => return,
        }
    };
    let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(Default::default()));
    if !seen.lock().insert(init_name.clone()) {
        return;
    }
    eprintln!(
        "[static-native-init-census] {init_name} state={state:?} door={door} \
         via {class_name}.{method_name}{descriptor}"
    );
}


/// Try native method first (for synthetic/native-only classes), then invoke_shared.
///
/// A STATIC caller goes through [`invoke_static_or_native`] instead: this
/// entry's bytecode tail initializes the class it dispatches on, which for an
/// inherited static is the symbolic owner JVMS §5.5 leaves alone.
pub fn invoke_or_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_or_native_impl(
        shared,
        thread,
        class_name,
        method_name,
        descriptor,
        args,
        ByNameKind::Virtual,
    )
}

/// [`invoke_or_native`] for a caller that may RETRY the call with the same
/// arguments when it fails (the JIT's S111r12 by-name rescues, which resend a
/// `NoSuchMethodError` miss through the constant-pool class): with
/// `keep_current`, `args` is pinned across the attempt and, when it fails,
/// re-read from those pins, so the retry sends current words. The first
/// attempt can load and initialize classes, and so collect, before it fails;
/// its own dispatch pins are released on return and never written back
/// (`gch-h1fr-compiled-frames-and-dispatch-doors-review-findings-FIXED-20261008.md`
/// item 3, gch h1/jp).
///
/// The pins add no retention on the default path: every arm the attempt can
/// take roots the same objects for its whole run already (the bytecode tail's
/// `PinnedDispatchArgs`, a native's own pins). Under
/// `CRATONVM_JIT_BY_NAME_TAIL_UNPINS_ARGS` they would outlive the tail's
/// release, so they are not taken there, and the retry keeps the pre-attempt
/// copy as before.
pub fn invoke_or_native_keeping_args_current(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &mut [Value],
    keep_current: bool,
) -> MethodCallResult {
    if !keep_current || shared.jit.jit_cache.by_name_tail_unpins_args() {
        return invoke_or_native(shared, thread, class_name, method_name, descriptor, args);
    }
    let roots = PinnedDispatchArgs::new(thread, args);
    let result = invoke_or_native(shared, thread, class_name, method_name, descriptor, args);
    if result.is_err() {
        roots.refresh(args);
    }
    drop(roots);
    result
}

/// The invoke kind a by-name route was entered with — the `ByNameKind` of
/// `docs/feature-designs/real-jdk-mode.md`
/// (stages 1-2, interpreter round i1 wave 16, lane L3). It replaces the
/// `static_call: bool` of [`invoke_or_native_impl`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::vm::vm_exec) enum ByNameKind {
    /// [`invoke_static_or_native`] / [`invoke_static_or_native_prepared`]:
    /// no receiver. The bytecode tail initializes the method's declaring
    /// class, and the receiver-keyed arms are skipped — `args[0]` is an
    /// ordinary argument, never a receiver to classify.
    Static,
    /// [`invoke_or_native`]: a virtual or interface call, or a caller that
    /// cannot tell. Every arm is asked, exactly as before the enum.
    Virtual,
}

impl ByNameKind {
    /// Whether `args[0]` may be a receiver, i.e. whether the receiver-keyed
    /// arms of [`invoke_or_native_impl`] (the `DowncallHandle`,
    /// `AnnotationProxy`, `Proxy$Instance`, `ClassLoader` and primitive
    /// functional-interface redirects) may apply.
    #[inline]
    pub(in crate::vm::vm_exec) fn may_have_receiver(self) -> bool {
        self == ByNameKind::Virtual
    }
}

// The `CRATONVM_DBG_DISPATCH_TALLY` site labels of the arm an
// `invoke_or_native_impl` call ended in — stage 1 of
// `docs/feature-designs/real-jdk-mode.md`,
// which asks how much of this door's traffic ends in the bytecode tail on a
// loaded class (the verdict a by-name memo would cache) before building one.
// Summed per label, the rows give the arm shares; unarmed, each is the
// tally's one cached flag load.
pub(in crate::vm::vm_exec) const ARM_NATIVE: &str = "invoke_or_native.arm.native";
pub(in crate::vm::vm_exec) const ARM_ALIAS: &str = "invoke_or_native.arm.alias";
pub(in crate::vm::vm_exec) const ARM_PARENT: &str = "invoke_or_native.arm.parent";
pub(in crate::vm::vm_exec) const ARM_TAIL_LOADED: &str = "invoke_or_native.arm.tail-loaded";
pub(in crate::vm::vm_exec) const ARM_TAIL_LOAD: &str = "invoke_or_native.arm.tail-load";
/// A `tail-loaded` dispatch whose class is the receiver's own linked copy of
/// a supertype name the global table answers otherwise (or not at all):
/// `by_name_tail_dispatch_class`'s supertype arm (SELECT17-2). Counted as
/// well as `tail-loaded`, keyed by the supertype name only.
pub(in crate::vm::vm_exec) const ARM_TAIL_SUPERTYPE: &str = "invoke_or_native.arm.tail-supertype";
/// The loaded class `invoke_or_native_impl`'s bytecode tail dispatches on:
/// the receiver's own class when it carries `effective_class`'s name (the
/// authoritative answer — the by-name table is ambiguous once two loaders
/// define the name), else the receiver's SUPERTYPE of that name
/// ([`receiver_supertype_named`], only when `receiver_supertypes`: the call
/// may have a receiver), else `by_name`. `None` when there is none, or when it
/// has no method table to dispatch on (a class whose only entries are
/// NATIVE-flagged synthetic ctors, `Class::dispatch_lacks_class_file`): the
/// call then falls through to the load-then-dispatch tail.
///
/// The supertype arm is what makes a JVMS §5.4.6 retarget loader-faithful
/// (round 17 wave 3, lane select,
/// `r17-mega3-retarget-dropped-for-an-ambiguous-selected-name-FIXED-20261005.md`):
/// the JIT's `receiver_selection_retarget` names the SELECTED class, which is
/// always a supertype of the receiver, and the copy of that name the
/// receiver's own loader linked is the one selection chose. The global table
/// answers `None` for a name two user loaders define, and a builtin loader's
/// copy for a name a builtin and a user loader both define.
/// `CRATONVM_SELECTION_RECEIVER_SUPERTYPE=0` restores the old answer; it is
/// read only when the supertype found is not `by_name`'s class.
pub(in crate::vm::vm_exec) fn by_name_tail_dispatch_class(
    cm: &crate::classloading::ClassManager,
    receiver_cid: Option<ClassId>,
    effective_class: &str,
    by_name: Option<ClassId>,
    receiver_supertypes: bool,
) -> Option<ClassId> {
    let mut receiver_class_id = None;
    if let Some(cid) = receiver_cid {
        if let Some(class) = cm.get_class(cid) {
            if class.name.as_ref() == effective_class {
                receiver_class_id = Some(cid);
            } else if receiver_supertypes {
                receiver_class_id =
                    by_name_receiver_supertype_copy(cm, cid, effective_class, by_name);
                // j27 wave 4 (lane DISPROWS, SELECT17-2): the census of the
                // supertype answers the name table would not have given.
                if receiver_class_id.is_some() && receiver_class_id != by_name {
                    dbg_dispatch_tally(ARM_TAIL_SUPERTYPE, effective_class, "", "");
                }
            }
        }
    }
    let class_id = receiver_class_id.or(by_name)?;
    cm.class_store
        .get(class_id)
        .is_some_and(|class| !class.dispatch_lacks_class_file())
        .then_some(class_id)
}

/// The receiver's own linked copy of the supertype name `effective_class`
/// (the receiver's class does NOT carry the name): `by_name` when it is a
/// supertype of the receiver (the usual case, one subtype-display compare),
/// else [`receiver_supertype_named`]'s answer while
/// `CRATONVM_SELECTION_RECEIVER_SUPERTYPE` is on (read only here, where the
/// answer differs from the table's), else `None` (the caller keeps the table).
pub(in crate::vm::vm_exec) fn by_name_receiver_supertype_copy(
    cm: &crate::classloading::ClassManager,
    receiver: ClassId,
    effective_class: &str,
    by_name: Option<ClassId>,
) -> Option<ClassId> {
    if by_name.is_some_and(|b| cm.is_subclass_of(receiver, b)) {
        return by_name;
    }
    receiver_supertype_named(cm, receiver, effective_class)
        .filter(|_| selection_receiver_supertype_enabled())
}

/// t1 wave 8, lane I13: the class `invoke_or_native_impl_walk`'s FIRST
/// registry arm decides "does the registered native yield to loaded
/// bytecode?" on (`registered_native_yields_to_bytecode_on_owner`), when that
/// differs from the loader-blind name table: the receiver's own class when it
/// carries `effective_class`'s name, else the receiver's own linked copy of
/// that supertype name (the tail's rule, [`by_name_tail_dispatch_class`]).
///
/// The arm used to ask by NAME only, so for a name two user loaders define
/// the table answered `None`, `has_real` came back false and a `Bridge` ran
/// over the receiver class's own Code, where the interpreter (which decides on
/// the resolved method's declaring class, by id) and the walk/tail below run
/// the bytecode. For a name a builtin and a user loader both define it judged
/// the builtin copy. `None` means "ask the name table", the old answer: no
/// receiver (a static call, `receiver_arms` false), an unknown class, or the
/// same class the table answers. `CRATONVM_BYNAME_NATIVE_OWNER_BY_RECEIVER=0`
/// restores the name-only reading; it is read only when the answers differ.
pub(in crate::vm::vm_exec) fn by_name_native_owner(
    cm: &crate::classloading::ClassManager,
    receiver_cid: Option<ClassId>,
    effective_class: &str,
    receiver_arms: bool,
) -> Option<ClassId> {
    if !receiver_arms {
        return None;
    }
    let cid = receiver_cid?;
    let class = cm.get_class(cid)?;
    let by_name = cm.get_loaded_class_id(effective_class);
    let owner = if class.name.as_ref() == effective_class {
        Some(cid)
    } else {
        by_name_receiver_supertype_copy(cm, cid, effective_class, by_name)
    };
    owner
        .filter(|&o| Some(o) != by_name)
        .filter(|_| true)
}



/// The class `invoke_or_native_impl_walk`'s ancestor-native walk, its
/// own-bytecode check and its redefine probe start from (round 17 wave 6,
/// lane mh6, `r17-select-by-name-walk-starts-ignore-the-receivers-supertype-FIXED-20261005.md`):
/// the tail's rule ([`by_name_tail_dispatch_class`]) for a receiver of a
/// SUBCLASS of the named class — the receiver's own copy of the supertype
/// name — else `by_name`, the table's answer. A receiver whose class carries
/// the name itself keeps `by_name` here: those readers already start from the
/// receiver under `CRATONVM_ANCESTOR_REDEFINE_RULE`. One rule for the four
/// answers, so they agree by construction; `=0` on
/// `CRATONVM_SELECTION_RECEIVER_SUPERTYPE` restores the table's start.
pub(in crate::vm::vm_exec) fn by_name_walk_start(
    cm: &crate::classloading::ClassManager,
    receiver_cid: Option<ClassId>,
    effective_class: &str,
    by_name: Option<ClassId>,
    receiver_supertypes: bool,
) -> Option<ClassId> {
    receiver_cid
        .filter(|_| receiver_supertypes)
        .filter(|&cid| {
            cm.get_class(cid)
                .is_some_and(|c| c.name.as_ref() != effective_class)
        })
        .and_then(|cid| by_name_receiver_supertype_copy(cm, cid, effective_class, by_name))
        .or(by_name)
}

/// gci i11/cf (F1 of `gci-i10cr-jni-call-path-review-findings-FIXED-20261016`): the
/// class `invoke_or_native_impl_walk` treats as the call's receiver. `args[0]`
/// is a receiver only when the call may have one; for a STATIC call under
/// `static_arg_rule` it is an ordinary argument, and an argument of another
/// loader's same-named class must not decide the dispatch (JVMS §5.4.3.3: the
/// method is resolved in the class the caller's loader named) nor feed the
/// ancestor walk's retired-row mask. `static_arg_rule` is false for a call
/// with receiver arms, so a virtual call keeps `first_arg`.
#[inline]
pub(in crate::vm::vm_exec) fn by_name_receiver_cid(first_arg: Option<ClassId>, static_arg_rule: bool) -> Option<ClassId> {
    first_arg.filter(|_| !static_arg_rule)
}

/// gci i11/cf (F1): the receiver the by-name bytecode tail
/// ([`by_name_tail_dispatch_class`]) is asked with. Under `static_arg_rule` a
/// static call's first argument is asked only as the LAST RESORT, when the
/// name table has no answer (`by_name` is `None`: two user loaders define the
/// name), which is the one case the old reading made work and the table cannot
/// (the load-then-dispatch tail would ask a loader-blind `load_class_concurrent`).
/// Everything else keeps `receiver_cid`.
#[inline]
pub(in crate::vm::vm_exec) fn by_name_tail_receiver(
    receiver_cid: Option<ClassId>,
    first_arg: Option<ClassId>,
    static_arg_rule: bool,
    by_name: Option<ClassId>,
) -> Option<ClassId> {
    if static_arg_rule && by_name.is_none() {
        first_arg
    } else {
        receiver_cid
    }
}

/// gci i11/cf (F1): `CRATONVM_BYNAME_STATIC_ARG_NOT_RECEIVER`, default ON (a
/// correctness fix; `=0` restores the old reading of a static's first
/// argument as a receiver), latched once per thread on its
/// [`NameOverrideMemo`] (`0` unread, `1` on, `2` off). Asked only for a call
/// without receiver arms.
#[inline]
pub(in crate::vm::vm_exec) fn byname_static_arg_rule_on(thread: &mut JvmThread) -> bool {
    let memo = &mut thread.name_override_memo;
    if memo.static_arg_state == 0 {
        memo.static_arg_state = {
            1
        };
    }
    memo.static_arg_state == 1
}

/// gci i12/mf (F9 of `gci-i10cr-jni-call-path-review-findings-FIXED-20261016`): the
/// object `invoke_or_native_impl_walk` hands the ZIP redefine-immunity waiver
/// (`redefine_immune_forced_native_for_receiver`) as the call's receiver.
/// `args[0]` is a receiver only when the call may have one; a STATIC call's
/// first argument is an ordinary value, and an argument that is not an archive
/// this VM opened must not stand the immunity down on the static's behalf.
/// With no receiver the waiver answers "not waived", the rule of every gate
/// that carries none. Generic so the rule is testable without a heap.
#[inline]
pub(in crate::vm::vm_exec) fn by_name_redefine_receiver<T>(first_arg: Option<T>, receiver_arms: bool) -> Option<T> {
    first_arg.filter(|_| receiver_arms)
}

/// `CRATONVM_SELECTION_RECEIVER_SUPERTYPE` (default ON, `=0` off; round 17
/// wave 3, lane select): the by-name tail dispatches a supertype name on the
/// receiver's own linked copy ([`by_name_tail_dispatch_class`]), and the JIT's
/// §5.4.6 retarget survives a selected class whose name another loader also
/// defines (`receiver_selection_retarget` in `vm/src/jit/helpers.rs`). One
/// switch for both, since the retarget without the tail would send the call to
/// the ambiguous global table. Read per call, only where the answer differs.
pub(crate) fn selection_receiver_supertype_enabled() -> bool {
    true
}

/// The proper supertype of `receiver` (superclass chain first, then every
/// superinterface) whose binary name is `name`: the class of that name the
/// receiver's loader linked. `None` when no supertype carries the name (an
/// unrelated class, a lambda proxy's interface named elsewhere), or past a
/// generous step bound. Loads nothing; asked under the caller's read guard.
pub(in crate::vm::vm_exec) fn receiver_supertype_named(
    cm: &crate::classloading::ClassManager,
    receiver: ClassId,
    name: &str,
) -> Option<ClassId> {
    let store = &cm.class_store;
    let start = store.get(receiver)?;
    // Superclasses first: the §5.4.6 retarget's usual answer, no allocation.
    let mut cursor = start.superclass;
    let mut steps = 0usize;
    while let Some(id) = cursor {
        steps += 1;
        if steps > 512 {
            return None;
        }
        let class = store.get(id)?;
        if class.name.as_ref() == name {
            return Some(id);
        }
        cursor = class.superclass;
    }
    // Then the superinterfaces of the receiver and of every superclass (a
    // default method §5.4.6 selected over a private or static class method).
    let mut pending: Vec<ClassId> = Vec::new();
    let mut cursor = Some(receiver);
    while let Some(id) = cursor {
        let class = store.get(id)?;
        pending.extend(class.interfaces.iter().copied());
        cursor = class.superclass;
    }
    let mut seen: Vec<ClassId> = Vec::new();
    while let Some(id) = pending.pop() {
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        if seen.len() > 512 {
            return None;
        }
        let Some(class) = store.get(id) else {
            continue;
        };
        if class.name.as_ref() == name {
            return Some(id);
        }
        pending.extend(class.interfaces.iter().copied());
    }
    None
}



/// The body of [`invoke_or_native`] and [`invoke_static_or_native`].
/// `kind` selects the bytecode tail and whether the receiver-keyed arms are
/// asked ([`ByNameKind`]); the registry arms are unchanged for both.
pub(in crate::vm::vm_exec) fn invoke_or_native_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    kind: ByNameKind,
) -> MethodCallResult {
    // SBR wave 6 (lane dispatch): the signature-polymorphic MethodHandle
    // dispatch, memoised per `(receiver class, class name, name, descriptor)`.
    // See `native_callee_memo`'s poly section for what fills it and what
    // retires an entry; a hit replays the body of the polymorphic arm of
    // `invoke_on_class_shared_inner` (publish the call-site descriptor, run the
    // registered bridge, unbox against the descriptor).
    if kind.may_have_receiver()
        && arms_poly_call_site(class_name, method_name)
        && crate::runtime::native_callee_memo::poly_memo_enabled()
    {
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            let recv_cid = shared.mem.heap.class_id_of(recv);
            if let Some(callback) = crate::runtime::native_callee_memo::poly_lookup(
                shared,
                recv_cid,
                class_name,
                method_name,
                descriptor,
            ) {
                cratonvm_native_api::poly_call_site::arm(descriptor);
                let r = safe_native_call(shared, thread, callback, args)?;
                return unbox_poly_return_checked(shared, thread, r, descriptor, method_name);
            }
            let save = crate::runtime::native_callee_memo::poly_arm(
                recv_cid,
                method_name,
                descriptor,
                thread.frames.len(),
            );
            let result = invoke_or_native_impl_walk(
                shared,
                thread,
                class_name,
                method_name,
                descriptor,
                args,
                kind,
            );
            if let Some(callback) = crate::runtime::native_callee_memo::poly_disarm(save) {
                crate::runtime::native_callee_memo::poly_fill(
                    shared,
                    recv_cid,
                    class_name,
                    method_name,
                    descriptor,
                    callback,
                );
            }
            return result;
        }
    }
    invoke_or_native_impl_walk(
        shared,
        thread,
        class_name,
        method_name,
        descriptor,
        args,
        kind,
    )
}

/// The polymorphic arm's claim on the armed poly witness, for a MethodHandle
/// receiver about to run `callback` (see `native_callee_memo::note_poly_native`).
/// A `DowncallHandle` receiver is never claimed: its dispatch is decided by the
/// receiver's class name, and the bridge it runs does not take the call-site
/// channel.
#[inline]
pub(in crate::vm::vm_exec) fn note_poly_mh_native(
    shared: &SharedVm,
    thread: &JvmThread,
    callback: NativeCallback,
    args: &[Value],
    method_name: &str,
    descriptor: &str,
) {
    if let Some(Value::Object(Some(recv))) = args.first().copied() {
        let recv_cid = shared.mem.heap.class_id_of(recv);
        crate::runtime::native_callee_memo::note_poly_native(
            callback,
            Some(recv_cid),
            method_name,
            descriptor,
            thread.frames.len(),
        );
    }
}

/// The body of [`invoke_or_native_impl`] proper (the by-name resolution walk).
pub(in crate::vm::vm_exec) fn invoke_or_native_impl_walk(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    kind: ByNameKind,
) -> MethodCallResult {
    let receiver_arms = kind.may_have_receiver();
    // Second denominator for the per-invoke lookup census: the calls that get
    // here are the ones an inline cache did NOT serve, and they are the ones
    // that walk the long `(class, method, descriptor)` comparison chain below.
    cratonvm_native_api::registry::lookup_census::probe(
        cratonvm_native_api::registry::lookup_census::INVOKE_GENERAL,
    );
    dbg_dispatch_tally("invoke_or_native", class_name, method_name, descriptor);
    // Publish the CALL-SITE descriptor for the two signature-polymorphic names
    // that cannot do their job without it. `arms_poly_call_site` owns WHICH,
    // and why, so this site and the two others cannot drift apart: `invoke`
    // needs the type the caller WROTE to decide collect-or-passthrough for a
    // trailing `null`, and `invokeExact` needs it because "the call site must
    // match the handle's type exactly" is a statement ABOUT the call site.
    //
    // Never CLEARED here — the signature-polymorphic block in
    // `invoke_on_class_shared_inner` owns the clearing, and this door may run
    // before it.
    if arms_poly_call_site(class_name, method_name) {
        cratonvm_native_api::poly_call_site::arm(descriptor);
    }
    // Residual-6 diagnosis (env-gated, CRATONVM_TRACE_CLASSVALUE): log every
    // get(Class) dispatch entering the general resolver, with its dispatch
    // class and receiver identity, so the failing call's route is visible.
    if method_name == "get"
        && descriptor == "(Ljava/lang/Class;)Ljava/lang/Object;"
        && crate::jit::helpers::cv_trace_enabled()
    {
        let recv = match args.first() {
            Some(Value::Object(Some(o))) => o.as_ptr() as usize,
            _ => 0,
        };
        eprintln!(
            "[cv-ion-entry] class={} recv={:#x} nargs={}",
            class_name,
            recv,
            args.len()
        );
    }
    // Skip expensive class loading for the VM's own placeholder names. Every
    // one of them is built here and every one starts with '<' — "<unknown
    // class {id}>", "<unknown class_id={id}>" — so '<' alone catches the set.
    //
    // A SPACE used to be part of this test and it is not a marker of anything:
    // JVMS 4.2.1 forbids only '.', ';', '[' and '/' in a binary name, and
    // Kotlin mints classes with spaces routinely — every anonymous object
    // inside a backtick-quoted test method lands in a class named after that
    // method. Refusing them here made a compiled virtual call on such a
    // receiver raise NoSuchMethodError instead of dispatching, which
    // `ParameterizedTypeReference.equals` then reported as "not equal":
    // RestOperationsExtensionsTests, one test, JIT-only (--nojit passes, and
    // so does CRATONVM_JIT_DENY=kotlin/jvm/internal/Intrinsics.areEqual).
    // Two instances of the SAME class, same class bytes, differing only in a
    // space in the binary name, compare equal 200000/200000 with the plain
    // name and 1000/200000 with the spaced one.
    if class_name.contains('<') {
        // Optional operator diagnostic: surface exactly which call had no
        // implementation. Gated on CRATONVM_TRACE_UNIMPLEMENTED so it never
        // spams normal runs. Uses var_os directly (no new env_cache accessor).
        if crate::runtime::env_cache::trace_unimplemented() {
            eprintln!(
                "[cratonvm] unimplemented: {}.{}{} — no native implementation and no loadable bytecode (invalid class name)",
                class_name, method_name, descriptor
            );
        }
        return Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::NoSuchMethodError {
                class_name: class_name.to_string(),
                method_name: method_name.to_string(),
                method_descriptor: descriptor.to_string(),
            },
        )));
    }

    // T15: Array types (`[LFoo;`, `[I`, etc.) don't have their own class
    // files вЂ” JVMS В§4.4.1 maps their method dispatch to `java.lang.Object`.
    // The only methods arrays define beyond `Object` are `clone()` and a
    // length accessor вЂ” both satisfied by our Object.clone native override
    // (which branches on ObjectKind::Array) and the `arraylength` opcode.
    let effective_class = if class_name.starts_with('[') {
        "java/lang/Object"
    } else {
        class_name
    };

    // Fast-path: a virtual call whose dispatch class is the synthetic
    // DowncallHandle has a compact synthetic layout, not the JDK
    // MethodHandle layout. Intercept its signature-polymorphic calls before
    // the JDK invokeBasic bytecode can read incompatible receiver fields.
    //
    // This arm and every receiver-keyed arm after it are asked only when
    // `args[0]` may be a receiver (`receiver_arms`): a STATIC call's first
    // argument is an ordinary value, and the `invoke*` arm below took the
    // class manager on every static call named `invoke` with an object
    // argument, and would have run the downcall native for a static user
    // method whose first argument happened to be a `DowncallHandle`.
    if receiver_arms && method_name == "type" && descriptor == "()Ljava/lang/invoke/MethodType;" {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let is_downcall = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(*receiver))
                .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
                .unwrap_or(false);
            if is_downcall {
                if let Some(callback) = shared.natives.native_methods.find(
                    "java/lang/foreign/DowncallHandle",
                    "type",
                    "()Ljava/lang/invoke/MethodType;",
                ) {
                    census_general_native_dispatch(
                        shared,
                        callback,
                        "java/lang/foreign/DowncallHandle",
                        "type",
                        "()Ljava/lang/invoke/MethodType;",
                    );
                    return safe_native_call(shared, thread, callback, args)
                        .map(|value| coerce_native_return(value, descriptor));
                }
            }
        }
    }

    if receiver_arms && matches!(method_name, "invoke" | "invokeExact" | "invokeBasic") {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let is_downcall = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(*receiver))
                .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
                .unwrap_or(false);
            if is_downcall {
                if let Some(callback) = shared.natives.native_methods.find(
                    "java/lang/foreign/DowncallHandle",
                    method_name,
                    "([Ljava/lang/Object;)Ljava/lang/Object;",
                ) {
                    census_general_native_dispatch(
                        shared,
                        callback,
                        "java/lang/foreign/DowncallHandle",
                        method_name,
                        "([Ljava/lang/Object;)Ljava/lang/Object;",
                    );
                    return safe_native_call(shared, thread, callback, args)
                        .map(|value| coerce_native_return(value, descriptor));
                }
            }
        }
    }

    // `java/lang/annotation/AnnotationProxy`. The proxy has no bytecode methods,
    // so without this every call (`annotationType`/`value`/`equals`/element
    // accessors — heavily exercised by JUnit/Arquillian/Spring annotation
    // scanning) would traverse the entire class-load + resolution-miss cascade
    // below before reaching the identical "last resort" annotation rescue near
    // the end of this function. `annotation_proxy_dispatch_impl` handles every
    // method (annotation members + Object equals/hashCode/toString/getClass), so
    // routing all of them here is correct.
    //
    // Keyed on the DISPATCH class (`effective_class`), which is `AnnotationProxy`
    // only for a virtual call resolved on a proxy receiver — so a *static* call
    // that merely passes an annotation as its first argument (e.g.
    // `Objects.requireNonNull(annotation)`, whose `class_name` is the declaring
    // class) is unaffected.
    if receiver_arms && effective_class == "java/lang/annotation/AnnotationProxy" {
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            note_annotation_proxy_cid(shared, shared.mem.heap.class_id_of(recv).as_u32());
            return annotation_proxy_invoke_shared(shared, thread, recv, method_name, &args[1..]);
        }
    }

    // Dynamic proxy dispatch (WP2.5): this is the JIT's cache-miss resolver
    // for `invokevirtual`/`invokeinterface` (see `virtual_dispatch_target_for_receiver`
    // in `vm/src/jit/helpers.rs`, which passes the RECEIVER's own runtime
    // class as `class_name` here — not the constant-pool-declared class). A
    // `java.lang.reflect.Proxy.newProxyInstance` receiver's runtime class is
    // always the synthetic `java/lang/reflect/Proxy$Instance` (every proxy,
    // regardless of interface set, currently lands on this single class —
    // see `PROXY_INSTANCE_CLASS`'s doc comment in `vm/src/runtime/proxy.rs`),
    // and that synthetic class has no real vtable/itable entries for the
    // interfaces it implements. Without this check, normal resolution falls
    // through and — for an interface method target — resolves straight to
    // the interface's own ABSTRACT declaration, raising AbstractMethodError
    // instead of forwarding to the `InvocationHandler`. Observed face:
    // `net.bytebuddy.utility.dispatcher.JavaDispatcher$Dispatcher$ForNonStaticMethod.invoke`
    // throwing `AbstractMethodError: method net/bytebuddy/utility/Invoker.invoke(...)
    // has no Code attribute` once `JavaDispatcher`'s `invoke` tiers up to JIT
    // (interpreted calls never hit this function — they route through
    // `is_proxy_dispatch` in `execute_invoke_kind`,
    // `vm/src/runtime/interpreter/invoke.rs`, which this mirrors).
    //
    // Keyed on the DISPATCH class (`effective_class`) exactly like the
    // `AnnotationProxy` check above — a literal string compare, zero
    // additional cost on every non-proxy dispatch — so a *static* call that
    // merely takes a proxy instance as an ordinary argument is unaffected:
    // its dispatch class is the static method's declaring class (resolved
    // from the constant pool), never the receiver's runtime class, so it can
    // never equal `Proxy$Instance` here.
    if receiver_arms && effective_class == crate::runtime::proxy::PROXY_INSTANCE_CLASS {
        if let Some(Value::Object(Some(proxy_ref))) = args.first().copied() {
            // Handle getClass() directly, like the interpreter's
            // `is_proxy_dispatch` block — return the proxy's own class
            // mirror rather than routing it through the handler.
            if method_name == "getClass" && descriptor == "()Ljava/lang/Class;" {
                let class_id = shared.mem.heap.class_id_of(proxy_ref);
                // gcd d2/f (`gengc-r5w1-oom5-oom-path-review-residuals` item
                // 5): the interpreter's twin (`invoke.rs`, `is_proxy_dispatch`)
                // has taken the fallible mirror since gen r5w1/oom5; this JIT
                // door still took the infallible one, which aborts the process
                // on a full heap. Nothing here reads `args` after the call (the
                // arm returns the mirror), and every caller of this door
                // already survives a collection: the handler arm just below
                // runs Java.
                let mirror = crate::runtime::interpreter::constants::class_mirror_or_oom(
                    shared,
                    thread,
                    class_id,
                    "proxy-getClass",
                )?;
                return Ok(Some(Value::Object(Some(mirror))));
            }
            return proxy_unbox_primitive_return(
                shared,
                descriptor,
                proxy_invoke_handler_shared(
                    shared,
                    thread,
                    proxy_ref,
                    method_name,
                    descriptor,
                    &args[1..],
                ),
            );
        }
    }

    // Forked Surefire calls `ClassLoader.setDefaultAssertionStatus` very early on
    // the context loader (`AppClassLoader` / `BuiltinClassLoader`). Inline-cache
    // promotion can still land on JDK bytecode for `java/lang/ClassLoader` when
    // `assertionLock` is not yet assigned, so `synchronized (assertionLock)` throws
    // NPE. The built-in Rust override is a correct no-op — always prefer it.
    // Only for a receiver that IS a ClassLoader (wave 13): the name and
    // descriptor alone also match a user type's own method, instance or
    // static, which this arm used to answer with the no-op (or `Ok(None)`)
    // instead of running it — the `getResource` defect below, again.
    if receiver_arms
        && method_name == "setDefaultAssertionStatus"
        && descriptor == "(Z)V"
        && receiver_is_class_loader(shared, args)
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/lang/ClassLoader", method_name, descriptor)
        {
            census_general_native_dispatch(
                shared,
                callback,
                "java/lang/ClassLoader",
                method_name,
                descriptor,
            );
            return safe_native_call(shared, thread, callback, args)
                .map(|v| coerce_native_return(v, descriptor));
        }
        return Ok(None);
    }

    // A JIT helper resolves virtual calls against the runtime custom-loader
    // class, while these callbacks are deliberately registered on ClassLoader.
    // Preserve the JDK null-name contract before inherited cached bytecode can
    // turn a null resource lookup into a null return. Only for a receiver that
    // IS a ClassLoader: the name and descriptor alone also match any user type
    // with a `getResource(String)` of its own, whose method must run
    // (round 11, `R11RtGetResourceNull`: it threw NPE 60 000 times, --nojit too).
    // The name test runs before the receiver test: the latter takes the class
    // manager, and every two-argument call with a null second argument used to
    // pay it (wave 13).
    // Retired over a real JDK by `CRATONVM_RETIRE_CLASSLOADER_NULL_NAME_INTERCEPTS`
    // (`ClassRealm::classloader_null_name_intercepts`; interpreter round i2
    // wave 55, lane L5): the inherited JDK body raises the same NPE.
    if receiver_arms
        && args.len() == 2
        && args.get(1).is_some_and(Value::is_null)
        && !shared.classes.classloader_null_name_intercepts
    {
        note_classloader_null_name_retired(
            shared,
            Some(args),
            "invoke_or_native",
            method_name,
            descriptor,
        );
    }
    if receiver_arms
        && args.len() == 2
        && args.get(1).is_some_and(Value::is_null)
        && shared.classes.classloader_null_name_intercepts
        && matches!(
            (method_name, descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
        // Only where the receiver INHERITS `ClassLoader`'s method: an
        // override runs its own body, as on HotSpot (wave 54, lane L5).
        && receiver_inherits_class_loader_method(shared, args, method_name, descriptor)
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/lang/ClassLoader", method_name, descriptor)
        {
            census_general_native_dispatch(
                shared,
                callback,
                "java/lang/ClassLoader",
                method_name,
                descriptor,
            );
            return safe_native_call(shared, thread, callback, args)
                .map(|v| coerce_native_return(v, descriptor));
        }
    }

    // In real-JDK mode ClassLoader's registered bridge can be tagged as a
    // synthetic stub and therefore lose to the JDK bytecode selector. That
    // bytecode uses the flat global class store and breaks child/fork-loader
    // identity. The bridge is the authoritative implementation for these two
    // base overloads, so invoke it directly once dispatch has selected
    // `java/lang/ClassLoader`; subclass overrides remain outside this branch.
    if receiver_arms
        && effective_class == "java/lang/ClassLoader"
        && method_name == "loadClass"
        && matches!(
            descriptor,
            "(Ljava/lang/String;)Ljava/lang/Class;" | "(Ljava/lang/String;Z)Ljava/lang/Class;"
        )
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/lang/ClassLoader", method_name, descriptor)
        {
            census_general_native_dispatch(
                shared,
                callback,
                "java/lang/ClassLoader",
                method_name,
                descriptor,
            );
            return safe_native_call(shared, thread, callback, args)
                .map(|v| coerce_native_return(v, descriptor));
        }
    }

    if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
        let bytes = effective_class.as_bytes();
        eprintln!("[invoke_or_native] effective_class={:?} (len={}) class_name={:?} method={:?} desc={:?}",
                  effective_class, bytes.len(), class_name, method_name, descriptor);
        eprintln!("[invoke_or_native] effective_class bytes: {:?}", bytes);
    }
    if crate::runtime::env_cache::dbg_vdisp()
        && effective_class == "java/util/Optional"
        && (method_name == "hashCode" || method_name == "equals")
    {
        let cm = shared.classes.class_manager.read();
        let recv_cid = match args.first() {
            Some(Value::Object(Some(o))) => Some(shared.mem.heap.class_id_of(*o)),
            _ => None,
        };
        let name_cid = cm.get_loaded_class_id(effective_class);
        let recv_stub = recv_cid
            .and_then(|c| cm.get_class(c))
            .map(|c| c.origin.is_compatibility_stub());
        let recv_has_method = recv_cid
            .and_then(|c| cm.get_class(c))
            .map(|c| c.find_method(method_name, descriptor).is_some());
        let name_stub = name_cid
            .and_then(|c| cm.get_class(c))
            .map(|c| c.origin.is_compatibility_stub());
        let name_has_method = name_cid
            .and_then(|c| cm.get_class(c))
            .map(|c| c.find_method(method_name, descriptor).is_some());
        eprintln!("[vdisp] INVOKE_OR_NATIVE Optional.{method_name}: recv_cid={recv_cid:?} name_cid={name_cid:?} recv_stub={recv_stub:?} recv_has_method={recv_has_method:?} name_stub={name_stub:?} name_has_method={name_has_method:?}");
    }
    // Always check native registry first вЂ” this provides "native override"
    // for both synthetic stubs AND real JDK classes.  Many JDK Java methods
    // (e.g. VM.getSavedProperty) depend on JVM-internal state we haven't set up,
    // so our Rust native registration must take priority over bytecode.
    // JDK-ONLY-WAVE2, RETIRED 2026-08-06: a `ThreadPoolExecutor.execute`
    // receiver-shape probe used to run here, ahead of the registry lookup,
    // because the registered native (`native_es_execute`) assumed CratonVM's
    // synthetic 2-field `Executors.new*ThreadPool()` receiver and the general
    // `real_protected_stub` logic below is CLASS-scoped while the question was
    // per-INSTANCE. It is no longer per-instance: every `Executors.*` factory
    // shortcut drives the real `ThreadPoolExecutor.<init>`
    // (`initialize_real_thread_pool_executor`), so on an image with the real
    // class bytes there is no synthetic receiver left. The native is tagged
    // `SyntheticStub` and the class is on `real_protected_stub_class`'s
    // allow-list, so the `has_real` arm below answers it — for every receiver,
    // with no field probe, and it is what keeps `ctx.invoke_virtual(pool,
    // "execute", ...)` from recursing into the same native forever (a real
    // stack overflow, confirmed via gdb, in the bug this probe was born from).
    // JVMTI REDEFINE GUARD. "Always check native registry first" is right
    // until an agent has woven advice into the class's own bytecode, at which
    // point that bytecode is authoritative and a registered native shadowing it
    // silently drops the instrumentation.
    //
    // This door is the one REFLECTION uses. `Method.invoke` reaches it as
    // `native_method_invoke` -> `invoke_virtual` -> `invoke_or_native`, which is
    // exactly how an instrumentation agent calls the real method, and it is how
    // `org.springframework.http.client.SimpleClientHttpResponseTests` came to
    // hang for a day as a suspected GC fragmentation bug. Named by a backtrace
    // taken inside the native itself; guarding the four dispatch_virtual /
    // invoke.rs doors first was not enough, because none of them is on this
    // path.
    //
    // Mockito's inline mock maker weaves `java.io.InputStream`;
    // `willCallRealMethod` then reflects into the real `transferTo`, landed
    // here, and ran CratonVM's registered `transferTo` native. The advice never
    // re-entered, so Mockito's `SelfCallInfo` self-call grant was never consumed
    // and leaked one step: the NEXT intercepted call (`read([BII)`) was
    // swallowed as a self-call and ran the real JDK body, which loops on
    // `read()` -- answered with an unstubbed default 0 -- filling the buffer and
    // reporting progress forever. The stubbed NullPointerException the test
    // asserts on is never thrown and `transferTo` never returns.
    //
    // Which body ran is settled without a debugger by the buffer size: the
    // native copies through 16 MiB, the JDK body through 16384. The mock saw
    // 16777216 on every call where HotSpot saw 16384, while `readNBytes` and
    // `skip` -- same shape, no native registered -- were intercepted correctly
    // in the same run.
    //
    // The immunity allow-list is consulted exactly as the other doors consult
    // it: mocking one `StringBuilder` or one synthetic collection must not
    // expose real JDK bytecode that would read a layout CratonVM's instances do
    // not carry. `native_shadow_suppressed_by_redefine`'s own first line is
    // `if !any_class_redefined() { return false }`, a relaxed atomic load, so a
    // process with no agent pays that and nothing else.
    // ...and only where there is BYTECODE TO FALL BACK TO.
    //
    // "The class was redefined" is not on its own a reason to drop a native.
    // Mockito's inline mock maker instruments `java.lang.Object` as well, so a
    // bare class-level test drops `Object.hashCode()I` -- which is `ACC_NATIVE`
    // in the real JDK and has no `Code` at all, so there is nothing for the
    // fall-through to run and CratonVM's identity hash would go with it, for
    // every object in the process, the moment anything anywhere is mocked.
    // Measured: with the class-level test alone this fired on
    // `java/lang/Object.hashCode()I` and on nothing else useful.
    //
    // The right question is the one `method.is_abstract()` asks one door over
    // (contract §7 step 3b), widened by one word: a registered native may only
    // yield to a body that EXISTS. So resolve the method and require a
    // non-native one carrying `Code`. `transferTo` is ordinary Java bytecode
    // and passes; `Object.hashCode` is not and does not.
    // Resolved ONCE, against the exact declaring class, and reused by both the
    // guard and its trace. A name lookup is a different question: it answers
    // `get_loaded_class_id(effective_class)`, and for a boot class reached
    // through a mock that is not necessarily the id the agent actually
    // retransformed.
    //
    // Asked only once some class was redefined: both readers below (the guard
    // and its trace) require `any_class_redefined()`, and the probe is a
    // class-manager read plus a hierarchy walk that every call through this
    // door paid for nothing before any agent ran. One relaxed load, read once
    // so the probe and its readers agree.
    let any_redefined = crate::classloading::any_class_redefined();
    let redefine_probe: Option<(ClassId, bool, u32)> = if !any_redefined {
        None
    } else {
        let cm = shared.classes.class_manager.read();
        // Round 15 wave 4 (lane walk2): from the receiver's own class when it
        // carries the dispatch name, the tail's rule (`by_name_tail_dispatch_class`):
        // the name table answers another loader's class of that name.
        // `CRATONVM_ANCESTOR_REDEFINE_RULE=0` starts at the name, as before.
        let receiver_start = match args.first() {
            Some(Value::Object(Some(receiver))) => Some(*receiver),
            _ => None,
        }
        .filter(|_| receiver_arms && shared.natives.native_methods.ancestor_redefine_rule())
        .map(|receiver| shared.mem.heap.class_id_of(receiver))
        .filter(|&cid| {
            cm.get_class(cid)
                .is_some_and(|c| c.name.as_ref() == effective_class)
        });
        // Round 17 wave 6 (lane mh6): a SUBCLASS receiver starts at its own
        // copy of the supertype name (`by_name_walk_start`, the tail's rule).
        let subclass_receiver = match args.first() {
            Some(Value::Object(Some(receiver))) => Some(shared.mem.heap.class_id_of(*receiver)),
            _ => None,
        };
        receiver_start
            .or_else(|| {
                by_name_walk_start(
                    &cm,
                    subclass_receiver,
                    effective_class,
                    cm.get_loaded_class_id(effective_class),
                    receiver_arms,
                )
            })
            .and_then(|cid| {
                crate::classloading::find_method_recursive(
                    cid,
                    method_name,
                    descriptor,
                    &cm.class_store,
                )
                .map(|(m, declaring_id)| {
                    (
                        declaring_id,
                        !m.is_native() && m.code().is_some(),
                        cm.class_redefine_generation(declaring_id),
                    )
                })
            })
    };
    // The ZIP arm of the immunity is receiver-aware: a Mockito inline mock of
    // `JarFile`/`ZipFile` is not an archive this VM opened, so the native it
    // keeps has nothing to answer from and the woven advice is the only body
    // that can. See `zip_immunity_waived_for_receiver`.
    // gci i12/mf (F9 of `gci-i10cr-jni-call-path-review-findings-FIXED-20261016`): a
    // STATIC call has no receiver, so its first argument waives nothing
    // (`by_name_redefine_receiver`); the immunity then stands, as at every
    // gate that carries no receiver.
    let redefine_receiver = by_name_redefine_receiver(
        match args.first() {
            Some(Value::Object(Some(obj))) => Some(*obj),
            _ => None,
        },
        receiver_arms,
    );
    let native_shadow_dropped_by_redefine = any_redefined
        && redefine_probe.is_some_and(|(_, has_body, generation)| has_body && generation > 0)
        && !crate::runtime::interpreter::redefine_immune_forced_native_for_receiver(
            shared,
            effective_class,
            method_name,
            descriptor,
            redefine_receiver,
        );
    if crate::runtime::env_cache::dbg_native_shadow()
        && any_redefined
        && shared
            .natives
            .native_methods
            .find(effective_class, method_name, descriptor)
            .is_some()
    {
        // One line per distinct triple: this sits on a hot dispatch path.
        static SEEN: std::sync::OnceLock<
            parking_lot::Mutex<std::collections::BTreeSet<(String, String, String)>>,
        > = std::sync::OnceLock::new();
        let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(Default::default()));
        let key = (
            effective_class.to_string(),
            method_name.to_string(),
            descriptor.to_string(),
        );
        if seen.lock().insert(key) {
            eprintln!(
                "[native-shadow] {effective_class}.{method_name}{descriptor} \
                 dropped={native_shadow_dropped_by_redefine} probe={redefine_probe:?} \
                 immune={}",
                crate::runtime::interpreter::redefine_immune_forced_native_for_receiver(
                    shared,
                    effective_class,
                    method_name,
                    descriptor,
                    redefine_receiver,
                ),
            );
        }
    }
    if native_shadow_dropped_by_redefine {
        NATIVE_SHADOW_DROPPED_BY_REDEFINE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some((callback, native_kind, native_id)) = if native_shadow_dropped_by_redefine {
        None
    } else {
        shared.natives.native_methods.find_with_kind_and_id(
            effective_class,
            method_name,
            descriptor,
        )
    } {
        if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
            eprintln!("[invoke_or_native] direct native hit");
        }
        // Name-only native lookup used to protect only an allow-listed stub.
        // Resolve the loaded declaration instead: a Bridge must yield to Code
        // in either policy, including calls made from another native.
        // t1 w8 I13: decided on the receiver's own copy of the name when the
        // name table would answer another loader's class (or none at all);
        // see `by_name_native_owner`. Registry hits are rare at this door, so
        // the extra class-manager read is off the common path.
        let native_owner = match args.first() {
            Some(Value::Object(Some(receiver))) if receiver_arms => {
                let receiver_cid = shared.mem.heap.class_id_of(*receiver);
                let cm = shared.classes.class_manager.read();
                by_name_native_owner(&cm, Some(receiver_cid), effective_class, receiver_arms)
            }
            _ => None,
        };
        let has_real = registered_native_yields_to_bytecode_on_owner(
            shared,
            native_owner,
            effective_class,
            method_name,
            descriptor,
            callback,
            native_kind,
        );
        match resolve_native_dispatch_wave1_in(
            &shared.natives.native_shadow_sink,
            DispatchDoor::InvokeOrNative,
            dispatch_policy(shared),
            effective_class,
            method_name,
            descriptor,
            Some((callback, native_kind)),
            !has_real,
            has_real
                || crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                    shared,
                    effective_class,
                    method_name,
                    descriptor,
                    native_kind,
                ),
        ) {
            Some(DispatchDecision::Reject(violation)) => {
                return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
            }
            Some(decision) => {
                if let Some(callback) = decision.native_callback() {
                    // CAPABILITY GATE, dispatch site 1 of 3. Deliberately here
                    // and not up at the `find_with_kind` hit: this is the last
                    // point before the native actually runs, and the arms above
                    // can still route the call to real bytecode instead. A
                    // capability is only exercised by a native that executes.
                    check_native_dispatch_capability(
                        shared,
                        effective_class,
                        method_name,
                        descriptor,
                    )?;
                    census_general_native_dispatch(
                        shared,
                        callback,
                        effective_class,
                        method_name,
                        descriptor,
                    );
                    let native_id = by_name_native_id(
                        shared,
                        native_id,
                        effective_class,
                        method_name,
                        descriptor,
                    );
                    dbg_dispatch_tally(ARM_NATIVE, effective_class, method_name, descriptor);
                    return by_name_native_call(
                        shared,
                        thread,
                        callback,
                        native_id,
                        args,
                        (effective_class, method_name, descriptor),
                    )
                    .map(|v| coerce_native_return(v, descriptor));
                }
            }
            // "No opinion": under `Compatible` this is exactly the old
            // `has_real` fall-through; under `JdkOnly` it can additionally mean
            // §7 step 3 (concrete bytecode beats this bridge).
            None => {}
        }
    }
    if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
        eprintln!("[invoke_or_native] no direct native; checking hierarchy walk");
    }
    // Also try the original class name in case the caller registered a
    // specific override for the array type.
    if effective_class != class_name {
        // `find` -> `find_with_kind`: the same 128-bit triple hash and the same
        // callback, plus the `NativeKind` without which no policy question can
        // be asked here at all. This retry is one of three dispatch sites in
        // this function that never went through §7 routing, so the dial is the
        // only policy it consults -- routing them properly is a larger change
        // and is nominated, not smuggled in here.
        if let Some((callback, native_kind, native_id)) = shared
            .natives
            .native_methods
            .find_with_kind_and_id(class_name, method_name, descriptor)
        {
            let dial = crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                shared,
                class_name,
                method_name,
                descriptor,
                native_kind,
            );
            note_dial_door(
                DispatchDoor::InvokeOrNativeAlias,
                dispatch_policy(shared),
                class_name,
                method_name,
                descriptor,
                native_kind,
                dial,
            );
            if !dial {
                census_general_native_dispatch(
                    shared,
                    callback,
                    class_name,
                    method_name,
                    descriptor,
                );
                let native_id =
                    by_name_native_id(shared, native_id, class_name, method_name, descriptor);
                dbg_dispatch_tally(ARM_ALIAS, class_name, method_name, descriptor);
                return by_name_native_call(
                    shared,
                    thread,
                    callback,
                    native_id,
                    args,
                    (class_name, method_name, descriptor),
                )
                .map(|v| coerce_native_return(v, descriptor));
            }
        }
    }
    // Walk the superclass chain: the constant pool may reference a subclass
    // (e.g. RunnerClassLoader.registerAsParallelCapable) but the native is
    // registered on the declaring superclass (ClassLoader).
    // IMPORTANT: skip hierarchy walk for <init> вЂ” constructors are NOT inherited.
    // IMPORTANT: skip hierarchy walk if the target class has bytecode for this
    // method вЂ” the subclass's bytecode override must take priority over a parent's
    // native (e.g. URI.toString() must NOT be short-circuited by Object.toString()).
    //
    // `tail_target` keeps the walk's answer for the dispatch tail below
    // (`by_name_tail_dispatch_class`, asked under the walk's read guard): the
    // tail used to ask the name table the same question again (until wave
    // 14) and to take the class manager a second time (until wave 15).
    let mut tail_target: Option<Option<ClassId>> = None;
    let first_arg_cid = match args.first() {
        Some(Value::Object(Some(receiver))) => Some(shared.mem.heap.class_id_of(*receiver)),
        _ => None,
    };
    // gci i11/cf (F1 of `gci-i10cr-jni-call-path-review-findings-FIXED-20261016`): a
    // STATIC call has no receiver, so its first argument's class is not asked
    // by the ancestor walk's retired-row mask or the tail's exact-name arm
    // (`by_name_receiver_cid`); only when the name table has no answer does the
    // tail keep the argument's copy as its last resort (`by_name_tail_receiver`).
    // `CRATONVM_BYNAME_STATIC_ARG_NOT_RECEIVER=0` restores the old reading.
    let static_arg_rule = !receiver_arms && byname_static_arg_rule_on(thread);
    let receiver_cid = by_name_receiver_cid(first_arg_cid, static_arg_rule);
    if method_name != "<init>" {
        // Check if the target class has its own bytecode for this method.
        // One class-manager read and one name lookup serve the check and the
        // walk (they were two of each on every call reaching this point).
        let cm = shared.classes.class_manager.read();
        // Round 17 wave 6 (lane mh6): the walk, `has_own_bytecode`, the
        // redefine probe and the tail start from one class
        // (`by_name_walk_start`): a subclass receiver's own copy of the name.
        let start = by_name_walk_start(
            &cm,
            receiver_cid,
            effective_class,
            cm.get_loaded_class_id(effective_class),
            receiver_arms,
        );
        let has_own_bytecode = start
            .and_then(|cid| cm.get_class(cid))
            .map(|cls| cls.find_method(method_name, descriptor).is_some())
            .unwrap_or(false);
        if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
            eprintln!("[invoke_or_native] has_own_bytecode={}", has_own_bytecode);
        }
        if shared.natives.native_methods.ancestor_walk_helper() {
            // JO-1 (round 15 wave 3, lane walk): the loop below on the shared
            // walk (SH14-1). Acceptance is the loop's: a live callback and the
            // dial, noted under the arm the loop would have taken (a parent
            // that declares the method is `ParentShadow`); plus the owner's
            // stub yield (`ancestor_stub_yields`), which the loop never asked,
            // so a subclass receiver reaches the bytecode the vtable path and
            // the warm revalidation already run for it. The kind is the
            // slot's (`kind_of_id`), where the loop's `find_with_kind_and_id`
            // reports `Bridge` on the descriptor-quirk path: class-file
            // descriptors never take that path. The redefine guard is the one
            // rule (the loop has none); under it the walk also starts where
            // the tail dispatches (`by_name_tail_dispatch_class`): the
            // receiver's own class when it carries the name.
            // `CRATONVM_ANCESTOR_WALK_HELPER=0` runs the loop below.
            let registry = &shared.natives.native_methods;
            let receiver_start = receiver_cid
                .filter(|_| receiver_arms && registry.ancestor_redefine_rule())
                .filter(|&cid| {
                    cm.get_class(cid)
                        .is_some_and(|c| c.name.as_ref() == effective_class)
                });
            let walk_from = receiver_start.or(start);
            let walk_declares = if receiver_start.is_some() {
                walk_from
                    .and_then(|cid| cm.get_class(cid))
                    .is_some_and(|cls| cls.find_method(method_name, descriptor).is_some())
            } else {
                has_own_bytecode
            };
            let walk_receiver = redefine_receiver.filter(|_| receiver_arms);
            if let Some(from) = walk_from.filter(|_| !walk_declares) {
                let found = crate::runtime::interpreter::ancestor_native(
                    shared,
                    &cm,
                    (from, receiver_cid),
                    method_name,
                    descriptor,
                    |parent_id, parent| {
                        crate::runtime::interpreter::redefined_ancestor_guard(
                            shared,
                            &cm,
                            parent_id,
                            &parent.name,
                            method_name,
                            descriptor,
                            crate::runtime::interpreter::AncestorWalkDoor::InvokeOrNative,
                            walk_receiver,
                        )
                    },
                    |parent, id, kind| {
                        // JIT round 20 wave 17, lane bridge
                        // (`jr20-invoke-or-native-ancestor-walk-lets-a-bridge-beat-code-20261013.md`):
                        // the warm populate's admission, so a cold call here
                        // runs what the warm one does -- an ancestor's `Bridge`
                        // or `SyntheticStub` over the Code it resolves yields
                        // (`resolve_dispatch` step 3); only an `Intrinsic`, or
                        // a native over no Code, is taken.
                        // Both modes (wave 19): the `--compatible` stand-ins a
                        // walk must still reach over Code -- `ClassLoader`'s
                        // resource lookups, `loadClass` x2, `getUnnamedModule`
                        // -- are reviewed `--compatible` `Intrinsic`s, which this
                        // term admits.
                        if registry.callback_of(id).is_none()
                            || crate::runtime::interpreter::resolve_cached_native_registration_with_cm(
                                shared,
                                &cm,
                                &parent.name,
                                method_name,
                                descriptor,
                            )
                            .is_none()
                            || crate::runtime::interpreter::ancestor_stub_yields(
                                shared,
                                &cm,
                                &parent.name,
                                method_name,
                                descriptor,
                                kind,
                            )
                        {
                            return false;
                        }
                        // `cm` is held: the dial's probe takes the class
                        // manager `read_recursive()`, as in the loop below.
                        let dial = crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                            shared,
                            &parent.name,
                            method_name,
                            descriptor,
                            kind,
                        );
                        note_dial_door(
                            if parent.find_method(method_name, descriptor).is_some() {
                                DispatchDoor::InvokeOrNativeParentShadow
                            } else {
                                DispatchDoor::InvokeOrNativeParent
                            },
                            dispatch_policy(shared),
                            &parent.name,
                            method_name,
                            descriptor,
                            kind,
                            dial,
                        );
                        !dial
                    },
                );
                if let crate::runtime::interpreter::AncestorNative::Native { owner, id, .. } = found
                {
                    let owner_name = cm.get_class(owner).map(|c| std::sync::Arc::clone(&c.name));
                    if let (Some(callback), Some(parent_name)) =
                        (registry.callback_of(id), owner_name)
                    {
                        census_general_native_dispatch(
                            shared,
                            callback,
                            &parent_name,
                            method_name,
                            descriptor,
                        );
                        dbg_dispatch_tally(ARM_PARENT, &parent_name, method_name, descriptor);
                        drop(cm);
                        return by_name_native_call(
                            shared,
                            thread,
                            callback,
                            Some(id),
                            args,
                            (&*parent_name, method_name, descriptor),
                        )
                        .map(|v| coerce_native_return(v, descriptor));
                    }
                }
            }
        } else if !has_own_bytecode {
            if let Some(mut cid) = start {
                // For the retired-row mask (round 14 wave 2, lane shadow).
                let walk_start = cid;
                while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass) {
                    if let Some(parent) = cm.get_class(parent_id) {
                        // S107 collection-toString fix: if this parent has its
                        // own bytecode for the method (e.g.
                        // AbstractCollection.toString), the bytecode override
                        // wins over any deeper native ancestor (e.g.
                        // Object.toString). Stop walking so the bytecode
                        // dispatch path runs.
                        //
                        // Round 19 (peaceful-sammet) exception, narrowed by JIT
                        // round 20 wave 17 (lane bridge): if the parent has BOTH
                        // bytecode AND a native, the native wins only when
                        // `resolve_dispatch` admits it over that bytecode (a
                        // reviewed `Intrinsic`), exactly as the warm populate
                        // decides (`populate_virtual_invoke_cache`).
                        if parent.find_method(method_name, descriptor).is_some() {
                            if let Some((callback, native_kind, native_id)) = shared
                                .natives
                                .native_methods
                                .find_with_kind_and_id(&parent.name, method_name, descriptor)
                                // `--real-jdk`: a `Bridge` above a retired row
                                // is masked, so the `break` below runs the
                                // bytecode (round 14 wave 2, lane shadow).
                                .filter(|(_, kind, _)| {
                                    !crate::runtime::interpreter::retired_row_masks_ancestor_bridge(
                                        shared,
                                        &cm,
                                        (walk_start, receiver_cid),
                                        parent_id,
                                        method_name,
                                        descriptor,
                                        *kind,
                                    )
                                })
                                // JIT round 20 wave 17, lane bridge: the warm
                                // populate's admission (`resolve_dispatch` over
                                // this parent's own Code), so a `Bridge` here
                                // yields and the `break` below runs the bytecode,
                                // as the warm call does.
                                .filter(|_| {
                                    crate::runtime::interpreter::resolve_cached_native_registration_with_cm(
                                        shared,
                                        &cm,
                                        &parent.name,
                                        method_name,
                                        descriptor,
                                    )
                                    .is_some()
                                })
                            {
                                // The `cm` read guard is HELD across this whole
                                // walk, which is why the dial's bytecode probe
                                // takes the class manager with
                                // `read_recursive()`. A plain `read()` here
                                // deadlocks against a queued writer, and in a
                                // release build -- where lock-order enforcement
                                // is compiled out -- it does so silently.
                                let dial =
                                    crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                                        shared,
                                        &parent.name,
                                        method_name,
                                        descriptor,
                                        native_kind,
                                    );
                                note_dial_door(
                                    DispatchDoor::InvokeOrNativeParentShadow,
                                    dispatch_policy(shared),
                                    &parent.name,
                                    method_name,
                                    descriptor,
                                    native_kind,
                                    dial,
                                );
                                if !dial {
                                    census_general_native_dispatch(
                                        shared,
                                        callback,
                                        &parent.name,
                                        method_name,
                                        descriptor,
                                    );
                                    // Registry only; the fact's refill takes
                                    // the class manager, so it runs after
                                    // `drop(cm)`.
                                    let native_id = by_name_native_id(
                                        shared,
                                        native_id,
                                        &parent.name,
                                        method_name,
                                        descriptor,
                                    );
                                    dbg_dispatch_tally(
                                        ARM_PARENT,
                                        &parent.name,
                                        method_name,
                                        descriptor,
                                    );
                                    let parent_name = parent.name.clone();
                                    drop(cm);
                                    return by_name_native_call(
                                        shared,
                                        thread,
                                        callback,
                                        native_id,
                                        args,
                                        (&*parent_name, method_name, descriptor),
                                    )
                                    .map(|v| coerce_native_return(v, descriptor));
                                }
                            }
                            break;
                        }
                        if let Some((callback, native_kind, native_id)) = shared
                            .natives
                            .native_methods
                            .find_with_kind_and_id(&parent.name, method_name, descriptor)
                            // `--real-jdk`: masked above a retired row; the
                            // walk goes on (round 14 wave 2, lane shadow).
                            .filter(|(_, kind, _)| {
                                !crate::runtime::interpreter::retired_row_masks_ancestor_bridge(
                                    shared,
                                    &cm,
                                    (walk_start, receiver_cid),
                                    parent_id,
                                    method_name,
                                    descriptor,
                                    *kind,
                                )
                            })
                            // JIT round 20 wave 17, lane bridge: the warm
                            // populate's admission; a native over the Code
                            // this parent inherits yields and the walk goes
                            // on, as the warm call's does.
                            .filter(|_| {
                                crate::runtime::interpreter::resolve_cached_native_registration_with_cm(
                                    shared,
                                    &cm,
                                    &parent.name,
                                    method_name,
                                    descriptor,
                                )
                                .is_some()
                            })
                        {
                            if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
                                eprintln!(
                                    "[invoke_or_native] hierarchy walk hit on parent={}",
                                    parent.name
                                );
                            }
                            // Same `read_recursive()` reason as the arm above:
                            // `cm` is held.
                            let dial =
                                crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
                                    shared,
                                    &parent.name,
                                    method_name,
                                    descriptor,
                                    native_kind,
                                );
                            note_dial_door(
                                DispatchDoor::InvokeOrNativeParent,
                                dispatch_policy(shared),
                                &parent.name,
                                method_name,
                                descriptor,
                                native_kind,
                                dial,
                            );
                            if !dial {
                                census_general_native_dispatch(
                                    shared,
                                    callback,
                                    &parent.name,
                                    method_name,
                                    descriptor,
                                );
                                let native_id = by_name_native_id(
                                    shared,
                                    native_id,
                                    &parent.name,
                                    method_name,
                                    descriptor,
                                );
                                dbg_dispatch_tally(
                                    ARM_PARENT,
                                    &parent.name,
                                    method_name,
                                    descriptor,
                                );
                                let parent_name = parent.name.clone();
                                drop(cm);
                                return by_name_native_call(
                                    shared,
                                    thread,
                                    callback,
                                    native_id,
                                    args,
                                    (&*parent_name, method_name, descriptor),
                                )
                                .map(|v| coerce_native_return(v, descriptor));
                            }
                        }
                    }
                    cid = parent_id;
                }
            }
        }
        tail_target = Some(by_name_tail_dispatch_class(
            &cm,
            by_name_tail_receiver(receiver_cid, first_arg_cid, static_arg_rule, start),
            effective_class,
            start,
            receiver_arms,
        ));
    }
    if crate::runtime::env_cache::bd_debug() && method_name == "intValue" {
        if let Some(Value::Object(Some(recv))) = args.first() {
            eprintln!(
                "[invoke_or_native] before invoke_on_class_shared recv={:p} args.len={}",
                recv.as_ptr(),
                args.len()
            );
            // Read field 0,1,2,3,4 to see what's there
            for i in 0..6 {
                let v = shared.mem.heap.get_field(*recv, i);
                eprintln!("  field[{}] = {:?}", i, v);
            }
        }
        eprintln!("[invoke_or_native] falling through to invoke_on_class_shared/invoke_shared");
    }

    // For real (non-synthetic) classes, dispatch via invoke_on_class_shared
    // which handles ACC_NATIVE methods and bytecode execution.
    //
    // jit-invokestatic-clinit-gap fix (2026-07-17): fourth and final
    // occurrence of the JVMS 5.5 class-init gap this session's sibling
    // fixes have been closing (see jit_getstatic/jit_putstatic_*
    // /jit_new_object in vm/src/jit/helpers.rs, and the
    // callee_compiler/resolve_inline_site/try_jit_compile_callee_slow
    // gates in vm/src/runtime/interpreter.rs) -- and the one that
    // actually explained a synthetic repro (a JIT-compiled static method
    // whose first-ever call reached, via a rare branch, an invokestatic
    // to a static method of a class loaded-but-not-yet-initialized) that
    // kept observing the bug even after all three JIT-side gates above
    // were in place and correctly refusing their own fast paths: every
    // one of them falls back, one way or another, to invoke_or_native --
    // and THIS shortcut, taken whenever the target class is already
    // LOADED (as opposed to not-yet-loaded, which the invoke_shared tail
    // below handles correctly), calls straight into
    // invoke_on_class_shared with no ensure_class_initialized_shared
    // anywhere in between.
    //
    // Class.forName(name, false, loader) (JLS-sanctioned load-without-
    // initialize) is the sharpest way to hit this -- LOADED but not yet
    // INITIALIZED -- but it is not the only one. execute_invokestatic
    // and execute_invoke_kind (this crate's own interpreter dispatch)
    // both already call ensure_class_initialized_shared BEFORE reaching
    // invoke_or_native, which is exactly why the plain-interpreted
    // (CRATONVM_DISABLE_JIT=1) form of the repro was unaffected --
    // masking this gap for every caller that happens to check first.
    // Any caller that does NOT -- jit_invoke_dispatch's slow-path
    // fallback being the concrete one found this session -- silently
    // skips <clinit> for a call whose target class merely happens to
    // already be loaded. Class-init state is monotonic, so this call is
    // cheap (a single atomic load) once initialized and can never
    // regress an already-initialized class back to needing the check.
    // Resolving the dispatch class by NAME is ambiguous whenever more than one
    // loader has defined that name -- `get_loaded_class_id` / `invoke_shared`
    // then answer `None` / `ClassNotFound` and the call surfaces as a
    // `NoClassDefFoundError` for a class that is very much loaded. Groovy is
    // the everyday case: `GroovyScriptFactory` compiles the same script class
    // through a FRESH `GroovyClassLoader` per application context, so by the
    // second context `org/springframework/scripting/groovy/TestFactoryBean`
    // names two distinct classes (`scripting.groovy.GroovyScriptFactoryTests`,
    // 11 of 38 methods, JIT-only because the interpreter's own dispatch is
    // ClassId-based and never re-resolves the name).
    //
    // A virtual call's receiver IS the authoritative answer: when its runtime
    // class carries exactly this name, dispatch on that ClassId instead of
    // asking the (ambiguous) global name table.
    //
    // One class-manager read answers both questions (it was two per call),
    // and it is the walk's read when the call walked. Nothing between the
    // walk and here loads a class, so its answer stands; `<init>` did not
    // walk and asks here.
    let tail_target = tail_target.unwrap_or_else(|| {
        let cm = shared.classes.class_manager.read();
        let by_name = cm.get_loaded_class_id(effective_class);
        by_name_tail_dispatch_class(
            &cm,
            by_name_tail_receiver(receiver_cid, first_arg_cid, static_arg_rule, by_name),
            effective_class,
            by_name,
            receiver_arms,
        )
    });
    if let Some(class_id) = tail_target {
        dbg_dispatch_tally(ARM_TAIL_LOADED, effective_class, method_name, descriptor);
        // A static call initializes the DECLARING class (wave 14): the
        // instance tail below would run `<clinit>` of this symbolic owner
        // even when the method is inherited.
        if kind == ByNameKind::Static {
            return invoke_static_on_owner(
                shared,
                thread,
                class_id,
                effective_class,
                method_name,
                descriptor,
                args,
            );
        }
        // gen r4w3/rooting: `ensure_class_initialized_shared` runs <clinit>
        // (a moving-GC point) while `args` sit in a raw slice.
        // `invoke_by_class_id_shared` is exactly
        // init-then-`invoke_on_class_shared` on this ClassId, but pins the
        // object args across the init and dispatches on the refreshed copies.
        return invoke_by_class_id_shared(shared, thread, class_id, method_name, descriptor, args);
    }

    // Final fallback: full invoke_shared (loads class, resolves method), or
    // its static form, which initializes the declaring class instead.
    dbg_dispatch_tally(ARM_TAIL_LOAD, effective_class, method_name, descriptor);
    if kind == ByNameKind::Static {
        return invoke_static_shared(
            shared,
            thread,
            effective_class,
            method_name,
            descriptor,
            args,
        );
    }
    invoke_shared(
        shared,
        thread,
        effective_class,
        method_name,
        descriptor,
        args,
    )
}

/// Resolve a bare library name to a full path by searching `java.library.path`.
/// If `name` already contains a directory separator it is returned unchanged.
pub(in crate::vm) fn resolve_library_path(shared: &SharedVm, name: &str) -> String {
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return name.to_string();
    }
    let lib_path_prop = shared
        .system_properties
        .read()
        .get("java.library.path")
        .cloned()
        .unwrap_or_default();

    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in lib_path_prop.split(sep) {
        if dir.is_empty() {
            continue;
        }
        let candidate = format!("{}/{}", dir.trim_end_matches(['/', '\\']), name);
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    name.to_string()
}

impl<'a> NativeContextImpl<'a> {
    /// The body of [`Self::monitor_wait_with_state`] under
    /// `CRATONVM_MONITOR_WAIT_RELEASE_FIRST` (JIT round 16 lane monitor,
    /// proposal MON16-1; default on): the monitor is released BEFORE the wait
    /// prologue instead of after it. `(the wait's outcome, when the wait
    /// started, whether the prologue ran -- i.e. whether this thread deposited
    /// and entered the blocked region, which the caller must leave)`; `obj` is
    /// remapped if a pause this thread arrived at moved it.
    ///
    /// Order, and why it is GC-safe and keeps JLS 17.2.1:
    ///
    /// 1. As a RUNNING mutator that owns the monitor: publish the JMX waiting
    ///    monitor (before enrolment, which MON14-2's interrupt argument
    ///    needs), find the monitor (`ensure_inflated` reads `obj`, which no
    ///    collection can move while this thread runs), then -- in one
    ///    state-lock section -- check ownership, enrol in the wait set and
    ///    release (`Monitor::wait_with_prologue`). Release and wait-set entry
    ///    are atomic with respect to every `notify`, which takes the same lock.
    /// 2. The prologue, still running, no longer owning the monitor, state lock
    ///    dropped: TLAB retire, the debugger's frame publish, the root
    ///    deposit, `enter_blocked` (arriving at a pause already requested, whose
    ///    pointer map remaps `obj`), the debugger window, the wait-site note.
    ///    Exactly the old prologue, in the old order. Contenders can take the
    ///    monitor meanwhile; that is the point.
    /// 3. GC-blocked, under the state lock again: a notification marked during
    ///    2 is consumed without parking, an interrupt set during 2 is seen
    ///    (both are made before their maker takes the state lock), and only
    ///    then does the waiter park. The re-acquisition is unchanged and runs
    ///    GC-blocked as before; the monitor's `Arc` keeps it alive whatever a
    ///    collection does to `obj`.
    ///
    /// Not the owner: `Err` from step 1, the prologue never ran, nothing to
    /// undo but the JMX publish (the caller retracts it).
    pub(in crate::vm::vm_exec) fn monitor_wait_released_first(
        &mut self,
        obj: &mut ObjectRef,
        timeout_ms: Option<u64>,
    ) -> (
        Result<crate::threading::monitor::WaitOutcome, crate::error::MethodCallFailed>,
        std::time::Instant,
        bool,
    ) {
        let shared = self.shared;
        let tid = self.thread.thread_id;
        let lazy_snapshot = shared.threads.monitors.wait_snapshot_lazy();
        // Owned handles to the two words the monitor reads while this thread
        // parks, so the prologue closure alone borrows `self`.
        let interrupted = std::sync::Arc::clone(&self.thread.interrupted);
        let gc_block_state = std::sync::Arc::clone(&self.thread.gc_block_state);
        // BLOCKED for a contended re-acquire, in both state words (wave 54,
        // lane L7; `WaitReenterStatus`). One cached load after the thread's
        // first blocking region.
        let reenter_status = WaitReenterStatus {
            shared,
            tid,
            java_state: &gc_block_state.java_state,
            slots: {
                self.thread_status_slots()
            },
        };
        let entered = *obj;
        let wait_start = std::time::Instant::now();
        // The JMX waiting monitor was published by the caller, before the
        // wait state (`monitor_wait_with_state`).
        let mut blk = None;
        let mut moved_to: Option<ObjectRef> = None;
        let outcome = {
            let mut prologue = || {
                // GCAUDIT-0711-FIX (finding 1a, adjacent): retire BEFORE the
                // deposit raises `in_blocked_region` (see the other path).
                self.thread.tlab.retire();
                #[cfg(feature = "experimental-debug")]
                let debugger_published =
                    crate::runtime::interpreter::publish_blocked_frames(shared, self.thread);
                self.deposit_root_snapshot();
                let guard = shared.mem.gc_barrier.enter_blocked();
                if guard.pre_stw {
                    // As on the other path (CRIT Thread.join monitor-desync,
                    // GCAUDIT-0711-FIX `_auto`): arrive, and remap `obj` through
                    // the pause's pointer map. The monitor itself was found
                    // before this pause and is held by its `Arc`.
                    let pm = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
                    if let Some(&new) = pm.get(&(entered.as_ptr() as usize)) {
                        // SAFETY: `new` is the relocated header address from
                        // the GC pointer map for the object this wait holds a
                        // live reference to.
                        moved_to = Some(unsafe { ObjectRef::from_raw(new as *mut u8) });
                    }
                }
                #[cfg(feature = "experimental-debug")]
                crate::runtime::interpreter::open_blocked_inspection(
                    shared,
                    self.thread,
                    debugger_published,
                );
                blk = Some(guard);
                // Last: nothing below uses `self` mutably until the wait
                // returns, so a lazily rendered wait-site dump may read the
                // thread through the pointer this records (MB1).
                crate::vm::vm_init::note_wait_site(&*self.thread, lazy_snapshot);
            };
            // JIT round 16 wave 5 (lane monitor, MON16-6,
            // `CRATONVM_MONITOR_WAIT_LAZY_PARK`): a running phase first,
            // skipped when a pause is already requested. Counted as a running
            // parker for the whole call, so a pause initiator wakes it
            // (`MonitorTable::wake_lazy_parkers`); a waiter that blocks after
            // it only gets a spurious signal from that count.
            let no_pause_requested = || {
                !shared
                    .mem
                    .gc_barrier
                    .stw_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            };
            match shared.threads.monitors.wait_lazy_budget() {
                Some(budget) if no_pause_requested() => {
                    shared.threads.monitors.lazily_parked(|| {
                        shared.threads.monitors.wait_outcome_lazily(
                            entered,
                            tid,
                            timeout_ms,
                            Some(&interrupted),
                            Some(&reenter_status),
                            Some(&mut prologue as &mut dyn FnMut()),
                            Some(crate::threading::monitor::LazyWait {
                                budget,
                                keep_running: &no_pause_requested,
                            }),
                        )
                    })
                }
                _ => shared.threads.monitors.wait_outcome_with_prologue(
                    entered,
                    tid,
                    timeout_ms,
                    Some(&interrupted),
                    Some(&reenter_status),
                    Some(&mut prologue as &mut dyn FnMut()),
                ),
            }
        };
        let blocked = blk.is_some();
        #[cfg(feature = "experimental-debug")]
        {
            if blocked {
                crate::runtime::interpreter::close_blocked_inspection(shared, self.thread);
            }
        }
        drop(blk);
        if let Some(new) = moved_to {
            *obj = new;
        }
        (outcome, wait_start, blocked)
    }

    /// `NativeThreadAccess::monitor_wait` and `monitor_wait_reporting_timeout`.
    /// `timed` says whether a positive `timeout_ms` is the JAVA caller's
    /// timeout (`Object.wait(ms)`), which `Thread.getState()` reports as
    /// TIMED_WAITING; most natives that pass a timeout poll in bounded slices
    /// on behalf of an untimed Java wait and must keep reporting WAITING.
    ///
    /// `served_parked` (interpreter round i2 wave 65, lane L1,
    /// `CRATONVM_SERVED_WAIT_KINDS`; only from `monitor_wait_as` with the
    /// switch on): the wait stands for JDK code that parks, and publishes
    /// `jvm_thread::JAVA_WAIT_KIND_MONITOR_PARKED` (JVMTI `PARKED`) where an
    /// `Object.wait` publishes `JAVA_WAIT_KIND_OBJECT_WAIT`.
    pub(in crate::vm::vm_exec) fn monitor_wait_with_state(
        &mut self,
        mut obj: ObjectRef,
        timeout_ms: Option<u64>,
        timed: bool,
        served_parked: bool,
    ) -> MethodCallResult {
        // JLS §17.2.1 orders the ownership check BEFORE the interrupt check,
        // as HotSpot's `ObjectMonitor::wait` does (`CHECK_OWNER()` first): an
        // interrupted thread calling `wait()` on a monitor it does not own gets
        // `IllegalMonitorStateException` and KEEPS its interrupt flag. The
        // interrupt arm below used to answer first -- InterruptedException, flag
        // cleared (interpreter round i1 wave 23, lane L7;
        // `L7W23Interrupts.java` `pre ownWait`). Asked
        // only when the flag is set, so an ordinary wait's owner check stays
        // where it always was (`MonitorTable::wait`), with the same message.
        if self
            .thread
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire)
            && !self
                .shared
                .threads
                .monitors
                .holds(obj, self.thread.thread_id)
        {
            return Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(
                    crate::error::RuntimeError::IllegalMonitorStateException {
                        message: "current thread is not owner".to_string(),
                    },
                ),
            ));
        }
        // JLS В§17.2.1: Check interrupt before waiting вЂ” clear flag and throw.
        // This is the entry-time check: if the thread was already interrupted
        // before wait() was called, consume the flag and throw immediately.
        // SBR wave 6 (lane `thread`): the consumed interrupt clears the Java
        // field too (`InterruptedException` leaves it false); nio21 wave 5
        // L1: both under the per-thread interrupt lock.
        if consume_thread_interrupt(
            self.shared,
            &self.thread.interrupted,
            self.thread.java_thread_obj,
        ) {
            // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
            // `MonitorWait` / `MonitorWaited` (not timed out), as HotSpot
            // posts them before throwing for a thread interrupted already.
            #[cfg(feature = "experimental-debug")]
            if self
                .shared
                .debug
                .debugger_gates
                .native_event_armed(crate::debug::NativeEvent::Monitor)
            {
                let tid = self.thread.thread_id.0;
                let timeout = timeout_ms.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX));
                use crate::jvmti::native_env::{post_monitor_event, MonitorEvent};
                // gci i11/ja: delivered under this thread's own handle.
                let shared = self.shared;
                obj = crate::runtime::interpreter::post_on_own_thread(self.thread, || {
                    let moved = post_monitor_event(shared, tid, MonitorEvent::Wait(timeout), obj);
                    let _ = post_monitor_event(shared, tid, MonitorEvent::Waited(false), moved);
                    moved
                });
            }
            return Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(crate::error::RuntimeError::InterruptedException),
            ));
        }
        // Wave 63: `MonitorWait`, the owner checked (an owner that is not the
        // thread gets its `IllegalMonitorStateException` below, no event).
        #[cfg(feature = "experimental-debug")]
        let posts_monitor_events = self
            .shared
            .debug
            .debugger_gates
            .native_event_armed(crate::debug::NativeEvent::Monitor)
            && self
                .shared
                .threads
                .monitors
                .holds(obj, self.thread.thread_id);
        #[cfg(feature = "experimental-debug")]
        if posts_monitor_events {
            let timeout = timeout_ms.map_or(0, |ms| i64::try_from(ms).unwrap_or(i64::MAX));
            // gci i11/ja: delivered under this thread's own handle.
            let (shared, tid, at) = (self.shared, self.thread.thread_id.0, obj);
            obj = crate::runtime::interpreter::post_on_own_thread(self.thread, || {
                crate::jvmti::native_env::post_monitor_event(
                    shared,
                    tid,
                    crate::jvmti::native_env::MonitorEvent::Wait(timeout),
                    at,
                )
            });
        }
        // JIT round 16 (lane monitor, MON16-1, `CRATONVM_MONITOR_WAIT_RELEASE_FIRST`,
        // default on): enrol and release FIRST, as a running mutator, then
        // run the prologue below (TLAB retire, deposit, GC-blocked entry) --
        // see `monitor_wait_released_first`. Off: the round-16-wave-1 order,
        // which holds the monitor through the whole prologue.
        let (was_interrupted, wait_start, blocked) =
            if self.shared.threads.monitors.wait_releases_first() {
                let java_state = if timed && timeout_ms.is_some_and(|ms| ms > 0) {
                    3
                } else {
                    1
                };
                self.thread
                    .gc_block_state
                    .java_state
                    .store(java_state, std::sync::atomic::Ordering::Release);
                // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
                // the wait's kind, after its state (a reader that sees the kind
                // sees the state: `ThreadRegistry::java_block_state` reads the
                // lazy phase from it) and before the waiting monitor and the
                // flag; lowered after the waiting monitor is taken below.
                publish_monitor_wait_kind(self.shared, self.thread, served_parked);
                // Interpreter round i2 wave 54 (lane L7, probe
                // `L7W54WaitStateAtRelease`): `holder.threadStatus` says WAITING /
                // TIMED_WAITING BEFORE the release, as HotSpot's state is set before
                // `ObjectMonitor::wait` exits the monitor. The prologue's deposit
                // wrote it only after the release -- after the lazy running phase
                // (up to 2 ms) -- so the thread that took the monitor next read
                // RUNNABLE. A running mutator here: the plain store is safe. A wait
                // that never blocks (not the owner, or a lazy-phase win) is set back
                // to RUNNABLE below.
                //
                // Interpreter round i2 wave 57 (lane L1, probe
                // `L1W57JvmtiWaitStates`): the JMX waiting monitor goes up FIRST
                // (it went up inside `monitor_wait_released_first`, after this
                // publish). `ThreadRegistry::java_block_state` -- JVMTI's
                // `GetThreadState`, JDWP's `ThreadReference.Status` -- answers the
                // wait's state from it while the waiter is not yet GC-blocked, so
                // a reader that saw the mirror's WAITING never reads RUNNABLE.
                // Still before enrolment, as MON14-2 needs; taken below whatever
                // the outcome.
                self.shared
                    .threads
                    .thread_registry
                    .set_jmx_waiting_monitor(self.thread.thread_id, obj);
                self.publish_thread_status(thread_status_word_for_java_state(java_state));
                self.monitor_wait_released_first(&mut obj, timeout_ms)
            } else {
                // GCAUDIT-0711-FIX (finding 1a, adjacent): retire BEFORE deposit —
                // see `monitor_enter_blocking`. Moved ahead of the snapshot deposit
                // below (which raises `in_blocked_region`) so no heap-mutating TLAB
                // filler write can happen after a concurrent census has already
                // decided it may proceed without waiting for this thread.
                self.thread.tlab.retire();
                // `Thread.getState()`: `wait(ms > 0)` is TIMED_WAITING, `wait()` /
                // `wait(0)` WAITING, as in HotSpot (interpreter round i1 wave 23,
                // lane L7: this stored WAITING for both). Only when the timeout is the
                // Java caller's (`timed`): a native polling in 10 ms slices for an
                // untimed `CountDownLatch.await()` (`--compatible`) is WAITING, and
                // reporting it TIMED_WAITING hung every probe that waits for a parked
                // thread's WAITING state.
                self.thread.gc_block_state.java_state.store(
                    if timed && timeout_ms.is_some_and(|ms| ms > 0) {
                        3
                    } else {
                        1
                    },
                    std::sync::atomic::Ordering::Release,
                );
                // Wave 64 (lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`): as on the
                // release-first path, after the state and before the deposit's flag.
                publish_monitor_wait_kind(self.shared, self.thread, served_parked);
                // JDWP (interpreter round i1 wave 25, lane L1): as
                // `begin_blocking_region` does, so `ThreadReference.Frames` answers
                // for a suspended thread in `Object.wait` (it listed no frame,
                // `L1W24JdiThreads`, `waiter`); the window opens once blocked and
                // closes before the thread leaves the blocked state.
                #[cfg(feature = "experimental-debug")]
                let debugger_published =
                    crate::runtime::interpreter::publish_blocked_frames(self.shared, self.thread);
                // Interpreter round i4 wave 99, lane L1: the quiet leave's pause
                // terms, before the deposit, as `begin_blocking_region_with_state`.
                let leave_terms = self.blocking_leave_terms(
                    self.thread
                        .gc_block_state
                        .java_state
                        .load(std::sync::atomic::Ordering::Acquire),
                );
                // Deposit root snapshot before blocking so GC can scan this thread
                self.deposit_root_snapshot();
                // Wave 99 (lane L1): the key of this wait's region.
                self.record_blocking_leave_key(leave_terms);
                // KC16-watchdog: stash a snapshot of the current frame chain in a
                // thread-local so the watchdog's wait-site dump callback can emit
                // it if the stack-dump flag fires while we are parked in
                // `wait_condvar.wait_for`. See `vm_init::dump_wait_site_thread_local`.
                // JIT round 16 (lane monitor, MB1, `CRATONVM_MONITOR_WAIT_SNAPSHOT_LAZY`):
                // only this thread is recorded; the dump is rendered if asked for.
                crate::vm::vm_init::note_wait_site(
                    &*self.thread,
                    self.shared.threads.monitors.wait_snapshot_lazy(),
                );
                let wait_start = std::time::Instant::now();
                // Object.wait releases the monitor while parked. Publish that state so
                // ThreadInfo sees WAITING/TIMED_WAITING on the actual monitor, then
                // clear it unconditionally once Monitor::wait has reacquired or failed.
                self.shared
                    .threads
                    .thread_registry
                    .set_jmx_waiting_monitor(self.thread.thread_id, obj);
                // T19.H1 — mark this thread GC-blocked for the duration of the
                // park so a stop-the-world GC excludes it from `wait_for_all`.
                // The `BlockedGuard`'s `Drop` clears the mark unconditionally —
                // even if `monitors.wait` returns `Err` and the `?` below
                // early-returns — so the barrier's `expected` accounting cannot
                // leak. The guard is dropped immediately after the wait so a
                // *subsequent* STW correctly waits for this now-running thread.
                // (TLAB already retired above, before the root-snapshot deposit —
                // see the GCAUDIT-0711-FIX comment there.)
                let was_interrupted = {
                    let blk = self.shared.mem.gc_barrier.enter_blocked();
                    if blk.pre_stw {
                        // A STW was already in progress when we became blocked —
                        // arrive at the barrier so its `wait_for_all` completes.
                        //
                        // CRIT (Thread.join monitor-desync, scratch_churn/Churn.java):
                        // the collection we just let finish may have RELOCATED `obj`
                        // (the very monitor we are about to wait on) and zeroed its old
                        // slot. `obj` is a raw `ObjectRef` captured before we blocked and
                        // is NOT remapped by the blocked-thread fixup (that runs on wake,
                        // against our frames). If we hand the stale address to
                        // `monitors.wait`, `ensure_inflated` reads a zeroed (NEUTRAL)
                        // mark word and synthesises a FRESH owner=None monitor → the
                        // owner check fails → `IllegalMonitorStateException` → the JDK
                        // `synchronized` exit handler loops on it forever → the joiner
                        // livelocks off the GC safepoint and wedges the next STW. Remap
                        // `obj` through the pointer map the barrier hands back.
                        //
                        // GCAUDIT-0711-FIX (finding 1a): `_auto` — the deposit above
                        // already raised `in_blocked_region` before this check, so
                        // this pause's own census may have excluded us; the pointer
                        // map is still returned either way (see `arrive_and_wait_inner`),
                        // only the `arrived` quota bookkeeping differs.
                        let pm = self
                            .shared
                            .mem
                            .gc_barrier
                            .arrive_and_wait_auto(self.thread.thread_id);
                        if let Some(&new) = pm.get(&(obj.as_ptr() as usize)) {
                            // SAFETY: `new` is the relocated header address from the GC
                            // pointer map for the object we hold a live reference to.
                            obj = unsafe { ObjectRef::from_raw(new as *mut u8) };
                        }
                    }
                    #[cfg(feature = "experimental-debug")]
                    crate::runtime::interpreter::open_blocked_inspection(
                        self.shared,
                        self.thread,
                        debugger_published,
                    );
                    // The slots were resolved by the deposit above (it published
                    // WAITING through them): the cached word only, as this thread is
                    // GC-blocked now and must not read its mirror.
                    let reenter_status = WaitReenterStatus {
                        shared: self.shared,
                        tid: self.thread.thread_id,
                        java_state: &self.thread.gc_block_state.java_state,
                        slots: match self
                            .thread
                            .thread_status_slots
                            .load(std::sync::atomic::Ordering::Relaxed)
                        {
                            0 => None,
                            packed => Some(unpack_thread_status_slots(packed)),
                        },
                    };
                    let r = self.shared.threads.monitors.wait_outcome(
                        obj,
                        self.thread.thread_id,
                        timeout_ms,
                        Some(&self.thread.interrupted),
                        Some(&reenter_status),
                    );
                    #[cfg(feature = "experimental-debug")]
                    crate::runtime::interpreter::close_blocked_inspection(self.shared, self.thread);
                    drop(blk);
                    r
                };
                (was_interrupted, wait_start, true)
            };
        self.shared
            .threads
            .thread_registry
            .take_jmx_waiting_monitor(self.thread.thread_id);
        // Wave 64 (lane L1): the waiting monitor is taken; the wait's
        // published kind goes with it, on every outcome.
        lower_wait_kind(self.thread);
        crate::vm::vm_init::clear_wait_site_snapshot();
        let wait_dur = wait_start.elapsed();
        // Check if GC happened while we were blocked. Finding 1(a) hygiene:
        // run this BEFORE propagating a wait() error — the old `}?;` early
        // return skipped it, leaving `in_blocked_region` raised on a RUNNING
        // thread (deposit set it; nothing on the error path cleared it), so
        // every later census permanently excluded the thread and a moving
        // collection could run concurrently with its bytecode.
        // MON16-1: a wait that failed before its prologue ran (not the owner)
        // never deposited nor blocked, so it has no blocked region to leave.
        if blocked {
            // Wave 99 (lane L1): the quiet leave (the key recorded at this
            // wait's deposit; the release-first path records none, so it
            // rebuilds as before).
            let _kept = self.check_post_block_gc_quiet_refs(&mut []);
        } else {
            // The release-first path published the wait state before the
            // release (wave 54, lane L7); with no blocked region to leave,
            // `check_post_block_gc` will not restore RUNNABLE.
            self.publish_thread_status(THREAD_STATUS_RUNNABLE);
        }
        let outcome = was_interrupted?;
        let was_interrupted = outcome == crate::threading::monitor::WaitOutcome::Interrupted;
        // Wave 63: `MonitorWaited`, whether the timeout ended the wait.
        #[cfg(feature = "experimental-debug")]
        if posts_monitor_events {
            // gci i11/ja: delivered under this thread's own handle.
            let (shared, tid, at) = (self.shared, self.thread.thread_id.0, obj);
            let timed_out = outcome == crate::threading::monitor::WaitOutcome::TimedOutOrSpurious;
            obj = crate::runtime::interpreter::post_on_own_thread(self.thread, || {
                crate::jvmti::native_env::post_monitor_event(
                    shared,
                    tid,
                    crate::jvmti::native_env::MonitorEvent::Waited(timed_out),
                    at,
                )
            });
        }
        // Emit JFR monitor wait event. JIT round 16 (lane monitor, MB2,
        // `CRATONVM_MONITOR_WAIT_JFR_PRECHECK`): the VM-global recorder mutex is
        // taken only when JFR is on -- the emitter's own first test -- not by
        // every wait while it still owns the monitor.
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
            cratonvm_native_builtins_jfr::jfr_vm_events::monitor_waited(
                self,
                obj,
                timeout_ms.map(|ms| ms as i64).unwrap_or(0),
                false,
                obj.as_ptr() as i64,
                wait_dur.as_nanos() as i64,
            );
        }
        if self.shared.threads.monitors.wait_jfr_event_wanted() {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            // JFR's periodic recorder uses a sentinel-sized timeout. Preserve
            // that as a saturated JFR duration instead of overflowing i64.
            let timeout_ns = timeout_ms
                .map(|ms| ms.saturating_mul(1_000_000).min(i64::MAX as u64) as i64)
                .unwrap_or(0);
            let mut jfr = self.shared.debug.flight_recorder.lock();
            cratonvm_jfr::builtin::emit_monitor_wait_event(
                &mut jfr,
                "java/lang/Object",
                "unknown",
                timeout_ns,
                was_interrupted,
                obj.as_ptr() as i64,
                self.thread.thread_id.0 as u64,
                now_ns.saturating_sub(wait_dur.as_nanos() as u64),
                wait_dur.as_nanos() as u64,
            );
        }
        // JLS В§17.2.1 / Object.wait spec: when wait() throws
        // InterruptedException, the thread's interrupt status MUST be cleared
        // (the exception consumes the interrupt). This mirrors the entry-time
        // check above, which already `swap(false)`s the flag before throwing.
        //
        // We atomically read-and-clear the flag with `swap(false)`: if it was
        // set (interrupted during the wait), we consume it and throw. This is
        // race-safe under concurrency вЂ” a cross-thread interrupt that lands
        // *after* this swap simply re-sets the flag, and the next blocking
        // call (or an explicit `Thread.interrupted()`/`isInterrupted()` check)
        // observes it, exactly as the JLS prescribes. `was_interrupted` is set
        // by `monitors.wait` when the interrupt is what woke us; that layer only
        // *reads* the flag (never clears it), so here is the single authoritative
        // consume point. We throw on either signal and clear regardless.
        //
        // EXCEPT after a notification (HotSpot's `WasNotified`): a waiter that
        // consumed a `notify` returns normally and keeps its interrupt pending,
        // for the next blocking call or `Thread.interrupted()` to observe.
        // Throwing here consumed the notification AND threw, so a `notify()`
        // followed by `interrupt()` of the same waiter was a lost wakeup
        // (JLS §17.2.4; interpreter round i1 wave 29, lane L4, probe
        // `L7W29TWaitNotify.java`).
        if outcome == crate::threading::monitor::WaitOutcome::Notified {
            return Ok(None);
        }
        // SBR wave 6 (lane `thread`): as at the entry check above; nio21
        // wave 5 L1: under the per-thread interrupt lock.
        let flag_was_set = consume_thread_interrupt(
            self.shared,
            &self.thread.interrupted,
            self.thread.java_thread_obj,
        );
        if was_interrupted || flag_was_set {
            if !flag_was_set {
                // Woken by an interrupt whose flag is already clear: clear the
                // field only while the flag still reads clear (a racing
                // `interrupt()` that raised both again keeps both).
                sync_thread_interrupted_field_to_clear_flag(
                    self.shared,
                    &self.thread.interrupted,
                    self.thread.java_thread_obj,
                );
            }
            return Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(crate::error::RuntimeError::InterruptedException),
            ));
        }
        Ok(None)
    }

    /// The running phase of a platform thread's park (interpreter round i3
    /// wave 71, lane L1, `CRATONVM_PARK_LAZY`; page
    /// `interpreter-i71-L1-proposal-a-running-phase-before-a-blocking-park-DONE-20261224`).
    ///
    /// Every `Unsafe.park` that finds no permit used to pay the whole
    /// blocking prologue at once -- the `parkBlocker` pin, the TLAB retire,
    /// the root deposit (frames, a conservative scan of the compiled band,
    /// the pinned-root publish), the GC-blocked entry -- and on wake the
    /// post-block drain and snapshot refresh. A handoff whose `unpark`
    /// comes microseconds later (an AQS release, a `CountDownLatch`
    /// countdown) pays all of it per side. `Object.wait` and a contended
    /// `monitorenter` already wait as running mutators for the monitor
    /// lazy-park budget first (MON16-6, round 11 wave 19); this is the same
    /// for a park. The thread publishes its WAITING / TIMED_WAITING state and
    /// its `PARKED` kind first (read by `ThreadRegistry::java_block_state`
    /// and the mirror's `threadStatus`, as the lazy `Object.wait` does),
    /// waits on its own `ParkState` for up to `budget` (or the park's
    /// timeout, if shorter) while no stop-the-world pause is requested, and:
    ///
    /// * `None`: the park is over (the permit came, an interrupt is
    ///   pending, or the timeout elapsed); state and kind are restored;
    /// * `Some(rest)`: still waiting (budget out, or a pause requested):
    ///   take the blocking path with what is left of the timeout.
    ///
    /// A pause requested during the phase waits at most one
    /// `jvm_thread::LAZY_PARK_SLICE` for this thread to notice it and take
    /// the blocking path, whose GC-blocked entry arrives at the pause.
    #[cold]
    #[inline(never)]
    pub(in crate::vm::vm_exec) fn park_lazily(
        &mut self,
        budget: std::time::Duration,
        timeout: Option<std::time::Duration>,
        timed: bool,
    ) -> Option<Option<std::time::Duration>> {
        let shared = self.shared;
        let stw = &shared.mem.gc_barrier.stw_requested;
        if stw.load(std::sync::atomic::Ordering::Acquire) {
            return Some(timeout);
        }
        // Interpreter round i3 wave 72, lane L6
        // (`CRATONVM_PARK_LAZY_SUSPEND_AWARE`, default off): while a debugger
        // or C JVMTI suspension is in force, a park blocks at once (deposit,
        // blocked region, frames published for the debugger), as before wave
        // 71, instead of running out its budget as a running native first.
        #[cfg(feature = "experimental-debug")]
        let suspend_aware = shared.threads.thread_registry.park_lazy_suspend_aware();
        #[cfg(feature = "experimental-debug")]
        let suspended = || suspend_aware && shared.debug.debugger_gates.suspension_in_force();
        #[cfg(not(feature = "experimental-debug"))]
        let suspended = || false;
        if suspended() {
            return Some(timeout);
        }
        let java_state: u8 = if timed && timeout.is_some() { 3 } else { 1 };
        self.thread
            .gc_block_state
            .java_state
            .store(java_state, std::sync::atomic::Ordering::Release);
        publish_wait_kind(
            shared,
            self.thread,
            crate::threading::jvm_thread::JAVA_WAIT_KIND_PARKED,
        );
        self.publish_thread_status(thread_status_word_for_java_state(java_state));
        let lazy = timeout.map_or(budget, |t| t.min(budget));
        let start = std::time::Instant::now();
        let keep_running = || !stw.load(std::sync::atomic::Ordering::Acquire) && !suspended();
        // Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_STW_WAKE`,
        // default off): listed for the pause initiator's
        // `ThreadRegistry::wake_lazy_park_phases`, the phase waits in one wait
        // of its whole length instead of 100 us slices. Off, the registry call
        // is `phase()` and the slice is `LAZY_PARK_SLICE`, as in wave 71.
        let registry = &shared.threads.thread_registry;
        let slice = if registry.park_lazy_stw_wake() {
            lazy
        } else {
            crate::threading::jvm_thread::LAZY_PARK_SLICE
        };
        let park_state = &self.thread.park_state;
        let interrupted = &self.thread.interrupted;
        // Interpreter round i3 wave 79, lane L6 (`CRATONVM_PARK_LAZY_SPIN_NS`,
        // default off; page
        // `i73-L6-proposal-spin-on-the-atomic-permit-before-the-lazy-wait`):
        // spin on the permit (lock-free, it is an `AtomicBool` either way)
        // for a bounded time before the phase's condvar wait, so a handoff
        // whose `unpark` lands within it costs neither side a futex sleep
        // and wake. It stops for a pause or a suspension (`keep_running`)
        // and on an interrupt, and its time counts against the phase.
        if let Some(spin) = registry.park_lazy_spin() {
            let spin = spin.min(lazy);
            if park_state.spin_for_permit(spin, interrupted, &keep_running) {
                if registry.park_lazy_adaptive() {
                    park_state.note_lazy_handoff();
                }
                lower_wait_kind(self.thread);
                self.publish_thread_status(THREAD_STATUS_RUNNABLE);
                return None;
            }
        }
        let over = registry.lazily_parked(park_state, || {
            park_state.park_lazily_sliced(lazy, slice, interrupted, &keep_running)
        });
        // Wave 72 (lane L6): "the phase ran its whole length" is read off the
        // clock, not off a second `keep_running()` -- a suspension lifted
        // between the phase's stop and that re-read
        // (`CRATONVM_PARK_LAZY_SUSPEND_AWARE`) made a stopped phase of a
        // short timed park look timed out, an early return.
        let ran_out = start.elapsed() >= lazy;
        let rest = timeout.map(|t| t.saturating_sub(start.elapsed()));
        let timed_out = rest == Some(std::time::Duration::ZERO)
            || (ran_out && timeout.is_some_and(|t| t <= budget));
        // Interpreter round i3 wave 81, lane L6 (`CRATONVM_PARK_LAZY_ADAPTIVE`,
        // default off): a phase that ended on the permit (or an interrupt)
        // resets the thread's streak; one that ran out its whole budget
        // without a timeout ending the park extends it. A phase stopped by a
        // pause or a suspension, or a short park's timeout, says neither.
        if registry.park_lazy_adaptive() {
            if over {
                park_state.note_lazy_handoff();
            } else if ran_out && !timed_out {
                park_state.note_lazy_phase_ran_out();
            }
        }
        // JIT round 25 (`r25-locked-frame-depth-misses-the-park-frame-FIXED-20261024`): a
        // phase a pause (or a suspension) stopped after its time ran out
        // still takes the blocking park, for no time: the thread then waits
        // the pause out IN the park -- GC-blocked, its deposit standing at
        // the park's call, TIMED_WAITING -- as HotSpot's parker, woken during
        // a safepoint, stays parked until it ends. Returning here sent it to
        // its next poll instead, where it sat RUNNABLE for the whole pause
        // with a trace in its caller (a long first-`getThreadInfo` pause put
        // `MonitorInfo.getLockedStackDepth()` at 0 where HotSpot reads 2).
        let paused = !over && (stw.load(std::sync::atomic::Ordering::Acquire) || suspended());
        if over || (timed_out && !paused) {
            lower_wait_kind(self.thread);
            self.publish_thread_status(THREAD_STATUS_RUNNABLE);
            return None;
        }
        if timed_out {
            return Some(Some(std::time::Duration::ZERO));
        }
        Some(rest)
    }

    /// `Thread.start` of a bound (`--compatible`) virtual thread
    /// (`thread_obj` a `java.lang.BaseVirtualThread`): initialize
    /// `java.lang.Thread$Constants` first, on this starting PLATFORM thread
    /// (interpreter round i5 wave 100, lane L6; page
    /// `i100-L6-compatible-first-virtual-thread-group-is-null`). Its
    /// `<clinit>` builds the VirtualThreads group from
    /// `currentCarrierThread().getThreadGroup()`, which on a bound virtual
    /// thread answers the virtual thread itself, so the virtual thread cannot
    /// run it (`native-builtins` `virtual_thread_group` answers null instead).
    /// Answers `thread_obj`, re-read after the Java the initializer ran.
    /// A failed initializer is left for the class's next use to report.
    pub(in crate::vm::vm_exec) fn init_virtual_thread_group_before_start(&mut self, thread_obj: ObjectRef) -> ObjectRef {
        const CONSTANTS: &str = "java/lang/Thread$Constants";
        if matches!(self.thread.kind, crate::threading::ThreadKind::Virtual) {
            return thread_obj;
        }
        let (virtual_obj, constants) = {
            let class_id = self.shared.mem.heap.get_header(thread_obj).class_id;
            let cm = self.shared.classes.class_manager.read();
            let virtual_obj = cm
                .get_loaded_class_id("java/lang/BaseVirtualThread")
                .is_some_and(|base_id| cm.is_subclass_of(class_id, base_id));
            (virtual_obj, cm.get_loaded_class_id(CONSTANTS))
        };
        // The class-manager guard is released before asking (no nested read).
        if !virtual_obj || constants.is_some_and(|cid| self.is_class_initialized(cid)) {
            return thread_obj;
        }
        let pin = self.pin_native_root(thread_obj);
        let _ = self.ensure_class_initialized(CONSTANTS);
        let fixed = self.read_native_pin(pin, thread_obj);
        self.unpin_native_roots(pin);
        fixed
    }

    /// `NativeThreadAccess::park` and `park_reporting_timeout`; `timed` as
    /// in [`Self::monitor_wait_with_state`].
    pub(in crate::vm::vm_exec) fn park_with_state(&mut self, timeout: Option<std::time::Duration>, timed: bool) {
        self.park_with_state_until(timeout, timed, None, None)
    }

    /// [`Self::park_with_state`] with an absolute park's deadline for its
    /// `jdk.ThreadPark` event (`until_ms`, epoch milliseconds; interpreter
    /// round i3 wave 79, lane L6).
    ///
    /// Wave 79 (page `i73-L6-jfr-thread-park-fields-differ-from-hotspot`):
    /// only a Java-level park (`timed`: the `park_reporting_timeout` /
    /// `park_until_reporting` doors that `Unsafe.park` and the `LockSupport`
    /// stand-ins use) posts the event, as HotSpot posts one per
    /// `Unsafe_Park`; a VM-internal slice wait through plain `park` (a
    /// stand-in's poll, `StructuredTaskScope.joinUntil`'s slices) posts none.
    ///
    /// gci i23/pk: `raw_timeout_ns` is a negative relative park's raw time
    /// for the event's `timeout` (`park_relative_reporting_raw`); `None` for
    /// every other park.
    pub(in crate::vm::vm_exec) fn park_with_state_until(
        &mut self,
        timeout: Option<std::time::Duration>,
        timed: bool,
        until_ms: Option<i64>,
        raw_timeout_ns: Option<i64>,
    ) {
        self.park_with_state_inner(timeout, timed, until_ms, raw_timeout_ns, timed)
    }

    /// [`Self::park_with_state_until`]; `java_level` (JIT round 21 lane refs,
    /// REFS21-6) says the park is a Java-level one -- every timed door, and
    /// the untimed `LockSupport.park` stand-ins
    /// (`NativeThreadAccess::park_java_untimed`) -- which arms the opt-in
    /// helper-window park credit. A VM-internal slice wait never is.
    pub(in crate::vm::vm_exec) fn park_with_state_inner(
        &mut self,
        mut timeout: Option<std::time::Duration>,
        timed: bool,
        until_ms: Option<i64>,
        raw_timeout_ns: Option<i64>,
        java_level: bool,
    ) {
        // gci i25/vt: a carrier's Java-level park is past the `afterYield()`
        // of its last yield; a virtual thread that `afterYield` left
        // unscheduled is re-queued (one bit test when no window is open).
        // gci i26/rv: unless this park is inside that `afterYield` itself
        // (its `schedule(..)` can park); the `_at_park` arm keeps the window.
        if timed && self.thread.jdk_continuation.post_yield_window() {
            super::jdk_continuation::resolve_post_yield_window_at_park(self);
        }
        // JFR (interpreter round i3 wave 73, lane L6; page
        // `i72-L6-jfr-thread-park-misses-the-lazy-park-phase-20261105`): as
        // HotSpot's `Unsafe_Park`, the `jdk.ThreadPark` span starts here, if
        // a recording runs now, and is committed from every exit below with
        // the caller's timeout -- the permit fast path, the running phase
        // (`CRATONVM_PARK_LAZY`) and the blocking wait alike. One relaxed load
        // when nothing records; the legacy recorder is asked only past the
        // permit fast path (its events never covered that path).
        let mut jfr_span = crate::threading::jvm_thread::ParkEventSpan::begin_until(
            timed && park_event_wanted(),
            timeout,
            until_ms,
        )
        .map(|span| span.with_raw_timeout(raw_timeout_ns));
        // The caller's timeout, for the events; `timeout` below is the wait.
        let reported_timeout = timeout;
        // Interpreter round i4 wave 98, lane L6 (page
        // `i98-L6-windows-parknanos-whole-milliseconds`): on Windows, HotSpot
        // 25's `Parker::park` (os_windows.cpp) waits a relative park in whole
        // milliseconds -- `time /= 1000000`, 0 -> 1 -- with one
        // `WaitForSingleObject`; Linux's waits out the exact nanoseconds. A
        // Java-level relative park (`timed`, no `until_ms`) waits that long
        // here too; the `jdk.ThreadPark` event keeps the caller's nanoseconds,
        // as `Unsafe_Park` posts them. Linux and macOS unchanged.
        #[cfg(windows)]
        {
            if timed && until_ms.is_none() {
                timeout = timeout.map(windows_parker_relative_wait);
            }
        }
        // Interpreter round i5 wave 108, lane L6
        // (`L6W108WindowsParkNoWaitKeepsPermit`): HotSpot 25's WINDOWS
        // `Parker::park` (os_windows.cpp) decodes its time before it looks at
        // the permit or the interrupt -- `time < 0` returns, and an absolute
        // deadline already passed (`time - javaTimeMillis() <= 0`, an absolute
        // `0` included) returns -- without touching `_ParkHandle`, the event
        // that IS its permit; only the waiting path and the interrupted /
        // already-signalled return `ResetEvent` it. So on Windows
        // `unpark(t); parkUntil(0); parkNanos(1 s)` returns at once, where
        // this door took the permit as POSIX's `Parker::park` does (its
        // `Atomic::xchg(&_counter, 0)` comes first; unchanged off Windows).
        // `Some(ZERO)` of a Java-level park is exactly that decode
        // (`unsafe_park_timeout`; a positive relative time is at least 1 ms
        // here, `windows_parker_relative_wait`). Merge of the gci round
        // (2026-10-17): not for a VM-scheduled virtual thread, whose park is
        // not a `Parker::park` (`vt27_ls_vtc0_virtual_*` print HotSpot's
        // `second=waited`); the same rule as gci i47w/jw1's block below.
        #[cfg(windows)]
        {
            if timed
                && timeout == Some(std::time::Duration::ZERO)
                && !vm_scheduled_virtual(self.thread)
            {
                if let Some(span) = jfr_span {
                    self.commit_thread_park_event(span);
                }
                return;
            }
        }
        // `CRATONVM_PARK_LAZY_ADAPTIVE` (wave 81): the budget and the start of
        // a park that skipped its running phase.
        let mut adaptive_skipped: Option<(std::time::Duration, std::time::Instant)> = None;
        // gci i47w/jw1 (item A of
        // `gci-i23pk-park-permit-residuals-windows-and-stand-ins-FIXED-20261017`):
        // HotSpot 25's Windows `Parker::park` (os_windows.cpp) decodes the
        // time FIRST and returns for `time < 0` and for an elapsed absolute
        // deadline before it tests the interrupt or the event, so the permit
        // is KEPT (Linux's os_posix.cpp takes `_counter` first). The Windows
        // HotSpot control (`vi27-vt.list` `vt27_ls_*_platform`, i47x) prints
        // `second=at-once` for until0 / until0-blocker / untilpast. A
        // VM-scheduled virtual thread's park is not a `Parker::park`.
        #[cfg(windows)]
        {
            if timed
                && reported_timeout == Some(std::time::Duration::ZERO)
                && !vm_scheduled_virtual(self.thread)
            {
                if let Some(span) = jfr_span {
                    self.commit_thread_park_event(span);
                }
                return;
            }
        }
        // JDK spec: if interrupted, park returns immediately (no exception, flag NOT cleared)
        if self
            .thread
            .interrupted
            .load(std::sync::atomic::Ordering::Acquire)
        {
            // gci i23/pk (finding 5 of
            // `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): the
            // permit is consumed by this return too. HotSpot 25's
            // `Parker::park` takes `_counter` (`Atomic::xchg`) BEFORE its
            // interrupt test on Linux, and Windows' `ResetEvent`s the handle
            // on the interrupted return; `VirtualThread.park` likewise runs
            // `getAndSetParkPermit(false) || interrupted`. So after
            // `t.interrupt(); unpark(t)`, `park(); Thread.interrupted();
            // park();` blocks on HotSpot; it returned at once here, the
            // permit kept. A VM-scheduled virtual thread's permit is the
            // manager's `unpark_permit` (`begin_carrier_park`). Only a
            // Java-level park (`timed`: `Unsafe.park` and the stand-in
            // doors); a VM-internal slice wait through plain `park` is not a
            // `Parker::park` and leaves the permit to the Java park after it.
            // Interpreter round i5 wave 102, lane L6
            // (`L6W102ParkInterruptedConsumesPermit`) landed the same HotSpot
            // rule on dev for the platform-thread arm only; merged 2026-10-16
            // into this arm, which also covers a VM-scheduled virtual thread.
            if timed {
                if vm_scheduled_virtual(self.thread) {
                    self.shared
                        .threads
                        .virtual_thread_manager
                        .take_unpark_permit(self.thread.thread_id.0);
                } else if self.shared.threads.thread_registry.park_atomic_permit() {
                    self.thread.park_state.take_permit_lock_free();
                } else {
                    self.thread.park_state.take_permit();
                }
            }
            if let Some(span) = jfr_span {
                self.commit_thread_park_event(span);
            }
            return;
        }
        // A park that cannot block returns before the blocking protocol, as
        // HotSpot's `Parker::park` does (it takes `_counter` first, then
        // returns for an elapsed deadline): an `unpark` already delivered
        // (the permit), or a deadline already passed (`Some(ZERO)`, what the
        // native makes of a negative relative or a past absolute time). The
        // blocker pin, the TLAB retire, the root deposit, the blocked region
        // and the post-block check below are all for a thread that may sleep;
        // under the JIT the deposit alone made `unpark(); park()` ~20 us
        // against HotSpot's ~0.1 (`R16MonitorBlockCost`, page
        // `r16-monitor-wait-notify-handovers-are-10-to-150x-hotspot-FIXED-20261006`). Taking
        // the permit here is what `park_interruptible` would have done first.
        // A virtual thread's permit is the manager's (`begin_carrier_park`),
        // so it keeps the full path (a JDK continuation's park is its
        // carrier's park: `vm_scheduled_virtual`).
        if !vm_scheduled_virtual(self.thread) {
            // Interpreter round i3 wave 73, lane L6
            // (`CRATONVM_PARK_ATOMIC_PERMIT`, default off): the permit is
            // taken without the `ParkState` mutex, as HotSpot's
            // `Parker::park` starts with an `xchg` of `_counter`.
            let had_permit = if self.shared.threads.thread_registry.park_atomic_permit() {
                self.thread.park_state.take_permit_lock_free()
            } else {
                self.thread.park_state.take_permit()
            };
            if had_permit || timeout == Some(std::time::Duration::ZERO) {
                if let Some(span) = jfr_span {
                    self.commit_thread_park_event(span);
                }
                return;
            }
            if jfr_span.is_none() {
                jfr_span = crate::threading::jvm_thread::ParkEventSpan::begin_until(
                    timed && legacy_park_event_wanted(),
                    reported_timeout,
                    until_ms,
                )
                .map(|span| span.with_raw_timeout(raw_timeout_ns));
            }
            // Interpreter round i3 wave 71, lane L1 (`CRATONVM_PARK_LAZY`,
            // default on since the wave-71 triage): a running phase before
            // the blocking prologue below, as `Object.wait` and a contended
            // `monitorenter` have.
            if let Some(budget) = self.shared.threads.thread_registry.park_lazy_budget() {
                // Interpreter round i3 wave 81, lane L6
                // (`CRATONVM_PARK_LAZY_ADAPTIVE`, default off): a thread whose
                // parks keep outlasting the phase skips it, unless this park's
                // timeout fits in the budget (then the phase is the whole park).
                let skip = self.shared.threads.thread_registry.park_lazy_adaptive()
                    && timeout.is_none_or(|t| t > budget)
                    && self.thread.park_state.lazy_phase_skip();
                if skip {
                    adaptive_skipped = Some((budget, std::time::Instant::now()));
                } else {
                    match self.park_lazily(budget, timeout, timed) {
                        None => {
                            if let Some(span) = jfr_span {
                                self.commit_thread_park_event(span);
                            }
                            return;
                        }
                        Some(rest) => timeout = rest,
                    }
                }
            }
        } else if jfr_span.is_none() {
            jfr_span = crate::threading::jvm_thread::ParkEventSpan::begin_until(
                timed && legacy_park_event_wanted(),
                reported_timeout,
                until_ms,
            )
            .map(|span| span.with_raw_timeout(raw_timeout_ns));
        }
        // AQS-PARK-PIN (Tomcat DoHead JIT×GC young-sweep corruption): real-JDK
        // `LockSupport.park()` callers that track a blocker object (notably
        // `AbstractQueuedSynchronizer$ConditionObject.await()`, via
        // `LockSupport.setCurrentBlocker(this)` before the blocking call) write
        // it into the real `java.lang.Thread.parkBlocker` field — which SHOULD
        // already keep the ConditionObject (and transitively its `firstWaiter`
        // wait-queue chain, e.g. the awaiting `ConditionNode`) reachable via
        // ordinary field tracing from this thread's mirror (always a root, see
        // `deposit_root_snapshot` below). But a JIT-compiled `await()` /
        // `ConditionNode.block()` frame can ALSO hold the same objects purely
        // in a callee-saved register that is never spilled to any scannable
        // stack slot for the entire blocked duration — invisible to
        // `scan_active_jit_frames`'s conservative stack scan (see
        // docs/known-issues on the register-invisibility family). Re-read
        // `parkBlocker` and pin it as an extra, register-independent
        // `native_pin_roots` entry for the duration of the block: belt-and-
        // suspenders alongside the field write, using the SAME object the JDK
        // already considers "what this thread is parked on", so an AQS
        // executor/poller worker's Condition chain survives a concurrent young
        // GC even if every register-resident copy is missed. No-op (and thus
        // free) whenever the mirror has no real `parkBlocker` field (synthetic
        // JDK) or the field is currently null (park() called without a tracked
        // blocker).
        //
        // DBG (`CRATONVM_DBG_PARKLAT`, interpreter round i3 wave 75, lane L6):
        // the blocking prologue and the post-wake epilogue are timed and
        // reported past the diagnostic's threshold, to tell a late wake from a
        // slow way back to Java. One cached load when the diagnostic is off.
        let parklat = crate::threading::jvm_thread::parklat_threshold_nanos().is_some();
        let parklat_prologue = parklat.then(std::time::Instant::now);
        let mut parklat_woke: Option<std::time::Instant> = None;
        let mut blocker_pin = None;
        if let Some(thread_obj) = self.thread.java_thread_obj {
            // Round 15 wave 5 (lane threadvm2; `r15w4-threadvm-review-findings`
            // F1): the `parkBlocker` `java.lang.Thread` declares, through the
            // lock-free layout memo; by name, a subclass's own `parkBlocker`
            // field was pinned instead. `CRATONVM_THREAD_LAYOUT_CACHE=0` or
            // `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS=0` restore the walk /
            // the by-name answer.
            let blocker = match thread_field_slot(
                self.shared,
                self.shared.mem.heap.class_id_of(thread_obj),
                "parkBlocker",
            ) {
                Some(slot) => self.shared.mem.heap.get_field(thread_obj, slot),
                None => Value::Object(None),
            };
            if let Value::Object(Some(blocker)) = blocker {
                blocker_pin = Some(self.pin_native_root(blocker));
            }
        }
        // GCAUDIT-0711-FIX (finding 1a, adjacent): retire BEFORE deposit —
        // see `monitor_enter_blocking`. CRIT (TLAB UAF): a STW GC can
        // grow/realloc the young arena while this thread is parked, freeing
        // the buffer the TLAB points into.
        self.thread.tlab.retire();
        // `Thread.getState()`: a timed park (`parkNanos`, `parkUntil`) is
        // TIMED_WAITING, an untimed one WAITING, as in HotSpot (interpreter
        // round i1 wave 23, lane L7: this stored WAITING for both).
        // Only a Java-level timed park (`timed`); natives park in slices too.
        self.thread.gc_block_state.java_state.store(
            if timed && timeout.is_some() { 3 } else { 1 },
            std::sync::atomic::Ordering::Release,
        );
        // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
        // a park says so (JVMTI `PARKED`, HotSpot's `JavaThreadParkedState`),
        // before the deposit raises the flag; lowered after the post-block
        // check below.
        publish_wait_kind(
            self.shared,
            self.thread,
            crate::threading::jvm_thread::JAVA_WAIT_KIND_PARKED,
        );
        // JDWP (interpreter round i1 wave 25, lane L1): a parked thread's
        // frames for a debugger, as `monitor_wait` publishes them.
        #[cfg(feature = "experimental-debug")]
        let debugger_published =
            crate::runtime::interpreter::publish_blocked_frames(self.shared, self.thread);
        // JIT round 21 lane refs, REFS21-6 (opt-in
        // `CRATONVM_XT_HELPER_WINDOW_PARK_CREDIT`): a Java-level park (`timed`:
        // the `Unsafe.park` / `LockSupport` doors, which pin every object
        // argument; this function pins `parkBlocker` above and holds no other
        // object) of a platform thread with no JNI local frame open arms the
        // park credit before the flag-raising deposit below, so the
        // helper-window pass may read this thread for its captures and shadow
        // stack only once the deposit has proved its compiled chain
        // (`ThreadRegistry::park_credited_os_tids`). Cleared on every exit.
        struct ParkCreditDisarm(Option<std::sync::Arc<crate::threading::jvm_thread::GcBlockState>>);
        impl Drop for ParkCreditDisarm {
            fn drop(&mut self) {
                if let Some(state) = self.0.take() {
                    state
                        .park_credit_deposited
                        .store(false, std::sync::atomic::Ordering::Release);
                    state
                        .park_credit_armed
                        .store(false, std::sync::atomic::Ordering::Release);
                }
            }
        }
        let credit_on = cratonvm_gc::gc_quiescence::xt_park_credit_enabled();
        // JIT round 21 lane parkcredit: never inside an open blocked region (a
        // park nested in one). There the flag is already up, so a pause may be
        // reading this thread WHILE the deposit below runs: it would see the
        // credit (`park_credit_deposited` is stored before the deposit's pins
        // and snapshot are published) and drop the band read the outer
        // window's deposit may not cover. Only the park's own flag-raising
        // deposit publishes everything before any pause can look.
        let flag_already_up = self
            .thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
        let arm = credit_on
            && !flag_already_up
            && java_level
            && !vm_scheduled_virtual(self.thread)
            && !self.thread.jdk_continuation.jdk()
            && crate::native::jni::local_frame_depth() == 0;
        // First gate's DBG line (`CRATONVM_DBG_REFERENT_HOLDER`): every park
        // that reaches the blocking prologue with the switch on, armed or not,
        // with the facts that decided it.
        if credit_on && crate::runtime::env_cache::dbg_referent_holder() {
            eprintln!(
                "[xt-park-credit] gate=armed thread={} armed={arm} java_level={java_level} \
                 timed={timed} flag_up={flag_already_up} vm_virtual={} jdk_virtual={} jni_frames={}",
                self.thread.thread_id.0,
                vm_scheduled_virtual(self.thread),
                self.thread.jdk_continuation.jdk(),
                crate::native::jni::local_frame_depth(),
            );
        }
        let _park_credit = ParkCreditDisarm(arm.then(|| {
            self.thread
                .gc_block_state
                .park_credit_deposited
                .store(false, std::sync::atomic::Ordering::Relaxed);
            self.thread
                .gc_block_state
                .park_credit_armed
                .store(true, std::sync::atomic::Ordering::Release);
            std::sync::Arc::clone(&self.thread.gc_block_state)
        }));
        // Deposit root snapshot before blocking so GC can scan this thread.
        // Interpreter round i3 wave 72, lane L6
        // (`CRATONVM_PARK_DEPOSIT_FROZEN_BAND`, default off): under the
        // frozen-band memo, as `begin_blocking_region_with_state` deposits
        // (SBR5 lane ncall): a park is a blocking native like any other, and
        // the band above its outermost compiled entry is frozen while the
        // chain is untouched. Off, the deposit probes the band word by word.
        // Interpreter round i4 wave 99, lane L1: the quiet leave's pause terms
        // before the deposit, the key right after it (as
        // `begin_blocking_region_with_state`).
        let leave_terms = self.blocking_leave_terms(
            self.thread
                .gc_block_state
                .java_state
                .load(std::sync::atomic::Ordering::Acquire),
        );
        // gci i47w lane pr47 (holder page item 2): on G1, with the deferred-
        // chain demotion on (`CRATONVM_XT_HELPER_WINDOW_DEFERRED_CHAIN_NATIVE`,
        // default ON, `=0` restores the eager park deposit too), the park
        // deposits through the blocking region's lazy deposit, so the
        // helper-window pass demotes a parked carrier instead of pinning every
        // in-region word of its band on every pause. See
        // `park_deposit_defers_jit_chain`.
        let lazy_chain = park_deposit_defers_jit_chain(
            self.shared.mem.heap.is_g1(),
            self.shared.threads.thread_registry.park_defers_jit_chain(),
        );
        if self
            .shared
            .threads
            .thread_registry
            .park_deposit_frozen_band()
        {
            crate::jit::conservative_roots::with_frozen_band_memo(None, || {
                if lazy_chain {
                    self.deposit_root_snapshot_for_blocking_region();
                } else {
                    self.deposit_root_snapshot();
                }
            });
        } else if lazy_chain {
            self.deposit_root_snapshot_for_blocking_region();
        } else {
            self.deposit_root_snapshot();
        }
        self.record_blocking_leave_key(leave_terms);

        // NEW-15.4: virtual-thread aware park — pinned VTs emit
        // `jdk.VirtualThreadPinned`.
        //
        // A non-pinned VT used to `virtual_scheduler.release()` here and
        // `.acquire()` after the park. That was removed 2026-07-26: the permit
        // pool was disjoint from the real carriers, so the release freed no
        // carrier (this OS thread stays blocked in `park_interruptible`
        // either way — see §7.2 of
        // `virtual-threads.md`), while the
        // post-park `acquire()` was a live hang risk. Nothing else in the tree
        // acquires from that pool, so with more concurrently-parked virtual
        // threads than `carrier_count`, the surplus acquirers blocked on a
        // permit no one would return — a carrier wedged waiting on a
        // bookkeeping counter. Removing it loses no backpressure, because
        // there was none to lose.
        let is_virtual = vm_scheduled_virtual(self.thread);
        let pin_count = self.thread.pin_count;
        // A native beneath the park pins too (wave 26, `vt_pinned_by_native`).
        let native_pinned = pin_count == 0 && vt_pinned_by_native(&*self.thread);
        if is_virtual && (pin_count > 0 || native_pinned) {
            let reason = if native_pinned {
                "NativeMethod"
            } else if self.thread.pin_reason.is_empty() {
                "Pinned (park)"
            } else {
                self.thread.pin_reason
            };
            // Inline JFR emission (avoids borrowing self twice).
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut jfr = self.shared.debug.flight_recorder.lock();
            cratonvm_jfr::builtin::emit_virtual_thread_pinned_event(
                &mut jfr,
                &self.thread.name,
                reason,
                self.thread.thread_id.0,
                self.thread.thread_id.0,
                now_ns,
            );
        }

        // A virtual thread parking here blocks its CARRIER. Its one
        // `LockSupport` permit is the manager's `unpark_permit`, consumed here
        // or by an unmounting park alike; the `ParkState` below only carries
        // an `unpark`'s wake while `carrier_park` is set (interpreter round i1
        // wave 28, lane L4: one `unpark` used to leave a permit in each, so
        // the thread's next park of the other kind returned at once).
        let carrier_park_vt = is_virtual.then_some(self.thread.thread_id.0);
        let permit_taken = carrier_park_vt.is_some_and(|vt_id| {
            self.shared
                .threads
                .virtual_thread_manager
                .begin_carrier_park(vt_id, &self.thread.park_state)
        });
        // T19.H1 — mark GC-blocked across the park so a stop-the-world
        // GC does not wait for this (parked, GC-safe) thread. The
        // `BlockedGuard` clears the mark on drop regardless of how the
        // park returns. A watchdog stack-dump request unparks this thread
        // (see `request_stack_dump` → `unpark_all_for_stack_dump`); on
        // return it re-enters the interpreter loop, whose top-of-loop poll
        // emits its current frames — no per-park snapshot cost needed.
        {
            let blk = self.shared.mem.gc_barrier.enter_blocked();
            // DIAGNOSTIC (2026-07-13, STW takeover 5-class cluster
            // investigation): correlate against [stw-expected]'s identity
            // list to see whether this thread's park() call landed before
            // or after the pause was requested, and whether pre_stw-gated
            // arrival actually fires.
            // gcj j2/cm2: the per-VM latch (`ThreadRegistry::dbg_stw_expected_ids`).
            if self.shared.threads.thread_registry.dbg_stw_expected_ids() {
                eprintln!(
                    "[stw-park] tid={} pre_stw={}",
                    self.thread.thread_id.0, blk.pre_stw
                );
            }
            if blk.pre_stw {
                // GCAUDIT-0711-FIX (finding 1a): `_auto` — the deposit above
                // already raised `in_blocked_region` before this check.
                let _ = self
                    .shared
                    .mem
                    .gc_barrier
                    .arrive_and_wait_auto(self.thread.thread_id);
            }
            #[cfg(feature = "experimental-debug")]
            crate::runtime::interpreter::open_blocked_inspection(
                self.shared,
                self.thread,
                debugger_published,
            );
            if !permit_taken {
                // JIT round 18 wave 10 (lane ffmeintr, EINTR2-1): the wait
                // alone, inside the blocked-wait trampoline (a no-op unless
                // `CRATONVM_XT_HELPER_UNPARKED_READ`), so the helper-window
                // pass can read this parked thread's band without a signal.
                let park_state = &self.thread.park_state;
                let interrupted = &self.thread.interrupted;
                // Interpreter round i3 wave 74, lane L6
                // (`CRATONVM_PARK_UNTIMED_NO_POLL`, default off): the wait
                // without the 5 ms interrupt poll; an interrupt that only
                // sets the flag wakes it through `set_interrupted`.
                let no_poll = self.shared.threads.thread_registry.park_no_poll();
                // Interpreter round i5 wave 107, lane L6
                // (`CRATONVM_PARK_UNTIMED_EVENT`, default off): on Windows an
                // untimed unpolled park waits on the state's wake event, as
                // HotSpot's Windows `Parker` does (`ParkState::park_wait_with`).
                let untimed_event = self.shared.threads.thread_registry.park_untimed_event();
                if let Some(start) = parklat_prologue {
                    crate::threading::jvm_thread::parklat_report_phase("prologue", start);
                }
                // Interpreter round i4 wave 99, lane L6 (page
                // `i98-L6-windows-parknanos-whole-milliseconds`): on Windows a
                // Java-level timed park (relative or absolute; `Unsafe.park`
                // and the `--compatible` `LockSupport` stand-ins alike) waits
                // ONCE, at the default timer resolution, and returns, as
                // HotSpot 25's Windows `Parker::park` does with its one
                // `WaitForSingleObject`; it no longer loops to a deadline on
                // the high-resolution timer. Linux and macOS keep `park_wait`
                // (HotSpot's POSIX `Parker` waits out its deadline).
                #[cfg(windows)]
                let one_wait = if timed { timeout } else { None };
                // Interpreter round i5 wave 108, lane L6
                // (`CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK`, default off): the
                // waker raises the one wait's event after it released the
                // state's mutex, as HotSpot's `Parker::unpark` does.
                #[cfg(windows)]
                let signal_after_unlock = self
                    .shared
                    .threads
                    .thread_registry
                    .park_event_signal_after_unlock();
                // Interpreter round i5 wave 114, lane L1
                // (`CRATONVM_PARK_EVENT_LOCKLESS_WAKE`, default off; page
                // `i97-L1-process-exit-wake-latency`, "Wave 114"): on Windows a
                // platform thread's Java-level park -- the timed one wait, and
                // the untimed unpolled wait -- waits on the state's wake event
                // and returns WITHOUT re-taking the state's mutex after the OS
                // wait, as HotSpot's Windows `Parker::park` returns from its
                // `WaitForSingleObject`. A virtual thread's carrier park keeps
                // the mutex path (its `ParkState` is shared with the
                // carrier-park protocol).
                #[cfg(windows)]
                let lockless = timed
                    && carrier_park_vt.is_none()
                    && self
                        .shared
                        .threads
                        .thread_registry
                        .park_event_lockless_wake();
                crate::threading::jvm_thread::run_blocked_wait(&self.thread.gc_block_state, || {
                    #[cfg(windows)]
                    if let Some(t) = one_wait {
                        if lockless {
                            park_state.park_wait_once_lockless(t, interrupted);
                        } else {
                            park_state.park_wait_once_with(t, interrupted, signal_after_unlock);
                        }
                        return;
                    }
                    #[cfg(windows)]
                    if lockless && no_poll && timeout.is_none() {
                        park_state.park_wait_untimed_lockless(interrupted);
                        return;
                    }
                    park_state.park_wait_with(timeout, interrupted, !no_poll, untimed_event)
                });
                if parklat {
                    parklat_woke = Some(std::time::Instant::now());
                }
            }
            #[cfg(feature = "experimental-debug")]
            crate::runtime::interpreter::close_blocked_inspection(self.shared, self.thread);
            drop(blk);
        }
        if let Some(vt_id) = carrier_park_vt.filter(|_| !permit_taken) {
            self.shared
                .threads
                .virtual_thread_manager
                .end_carrier_park(vt_id, &self.thread.park_state);
        }
        // Check if GC happened while we were blocked. Wave 99 (lane L1): the
        // quiet leave keeps the deposit's snapshot when its key still matches.
        let _kept = self.check_post_block_gc_quiet_refs(&mut []);
        // Interpreter round i3 wave 81, lane L6 (`CRATONVM_PARK_LAZY_ADAPTIVE`):
        // a park that skipped the phase and still ended within the budget was
        // a handoff the phase would have caught: run the phase again.
        if let Some((budget, start)) = adaptive_skipped {
            if start.elapsed() < budget {
                self.thread.park_state.note_lazy_handoff();
            }
        }
        // Wave 64 (lane L1): the flag is down; the park's kind goes with it.
        lower_wait_kind(self.thread);
        // AQS-PARK-PIN: drop the extra `parkBlocker` root now that we're done
        // blocking — we never read the value back (unlike `monitor_wait_keepalive`)
        // since nothing here continues using it; it only needed to survive as a
        // GC root for the parked duration above.
        if let Some(pin) = blocker_pin {
            self.unpin_native_roots(pin);
        }
        if let Some(woke) = parklat_woke {
            crate::threading::jvm_thread::parklat_report_phase("epilogue", woke);
        }
        // Interpreter round i5 wave 110, lane L6 (`CRATONVM_VTHREAD_COMPILED_LEARNED`,
        // page `i91-L6`): a JDK continuation that parked here (a pinned park,
        // on its carrier) starts running again now, so the next pin's
        // long-run exemption (`learn_compiled_pin`) does not count this park.
        if self.thread.vt_slice_started.is_some() {
            self.thread.vt_slice_started = Some(std::time::Instant::now());
        }
        // Emit JFR thread park event: the whole park, from the span taken at
        // entry (wave 73; it began only if a recording ran then).
        if let Some(span) = jfr_span {
            self.commit_thread_park_event(span);
        }
    }

    /// The class and address of this thread's `Thread.parkBlocker` (the
    /// field `java.lang.Thread` declares, through the thread-layout memo):
    /// `(None, 0)` without a blocker or without the field. For the
    /// `jdk.ThreadPark` event only (interpreter round i3 wave 79, lane L6).
    #[cold]
    #[inline(never)]
    pub(in crate::vm::vm_exec) fn current_park_blocker_class_and_address(&self) -> (Option<ClassId>, i64) {
        let Some(thread_obj) = self.thread.java_thread_obj else {
            return (None, 0);
        };
        let heap = &self.shared.mem.heap;
        let (thread_obj, validated) = heap.load_and_forward_checked(thread_obj);
        if !validated {
            return (None, 0);
        }
        match thread_field_slot(self.shared, heap.class_id_of(thread_obj), "parkBlocker")
            .map(|slot| heap.get_field(thread_obj, slot))
        {
            Some(Value::Object(Some(blocker))) => (
                Some(heap.class_id_of(blocker)),
                blocker.as_ptr() as usize as i64,
            ),
            _ => (None, 0),
        }
    }

    /// Commit the `jdk.ThreadPark` event of a park that began at `span`
    /// (interpreter round i3 wave 73, lane L6): its duration is the whole
    /// park and its timeout the caller's. The recorder's mutex is VM-global;
    /// it is taken only while JFR records (the emitter's own first test), as
    /// `Object.wait` does (MB2, `MonitorTable::wait_jfr_event_wanted`); the
    /// backend's `admit` applies the recording's threshold to that duration.
    #[cold]
    #[inline(never)]
    pub(in crate::vm::vm_exec) fn commit_thread_park_event(&mut self, span: crate::threading::jvm_thread::ParkEventSpan) {
        let park_dur = span.duration();
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
            // Wave 79, lane L6: `parkedClass` and `address` are the thread's
            // `parkBlocker` read now, as HotSpot reads
            // `current_park_blocker()` at the commit (null and 0 without
            // one); both taken before the emitter, which may allocate.
            let (parked_class, address) = self.current_park_blocker_class_and_address();
            cratonvm_native_builtins_jfr::jfr_vm_events::thread_parked(
                self,
                span.timeout_field(),
                span.until_field(),
                parked_class,
                address,
                park_dur.as_nanos().min(i64::MAX as u128) as i64,
            );
        }
        if cratonvm_jfr::is_enabled() {
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut jfr = self.shared.debug.flight_recorder.lock();
            cratonvm_jfr::builtin::emit_thread_park_event(
                &mut jfr,
                "java/util/concurrent/locks/LockSupport",
                span.legacy_timeout_ns(),
                0, // address
                self.thread.thread_id.0 as u64,
                now_ns.saturating_sub(park_dur.as_nanos() as u64),
                park_dur.as_nanos() as u64,
            );
        }
    }

    /// Shared body for `begin_blocking_region` (`java_state=1`, WAITING) and
    /// `begin_timed_blocking_region` (`java_state=3`, TIMED_WAITING) — see
    /// `NativeContext::begin_blocking_region`'s doc comment for the GC-safety
    /// contract; only the reported `Thread.getState()` value differs.
    pub(in crate::vm::vm_exec) fn begin_blocking_region_with_state(&mut self, java_state: u8) {
        self.thread
            .gc_block_state
            .java_state
            .store(java_state, std::sync::atomic::Ordering::Release);
        // CRIT (TLAB UAF) — retire this thread's TLAB before entering the
        // blocked region, while the young arena it points into is still valid.
        // While we are GC-blocked a stop-the-world moving collection can run on
        // another thread and `grow()` (realloc) the young arena, freeing the old
        // backing buffer; a retained TLAB `[cursor,end)` into that buffer would
        // then be dangling and the first post-block fast-path bump would write
        // the object header into freed memory → EXCEPTION_ACCESS_VIOLATION in
        // `init_object_header`. Retiring here (arena still mapped) fills the tail
        // for the collector's walk and empties the TLAB so the next allocation,
        // after the block, refills from the current arena. Symmetric to the
        // parked-thread retire in `safepoint_check`; `check_post_block_gc` only
        // remaps existing refs and runs after the buffer may already be freed,
        // so the retire must happen here, before the block.
        self.thread.tlab.retire();
        // JDWP (interpreter round i1 wave 10, lane L1; lazy since wave 12): a
        // thread already suspended by the debugger publishes its frames for
        // `ThreadReference.Frames` before it becomes GC-blocked (still a
        // running mutator here). One load otherwise.
        #[cfg(feature = "experimental-debug")]
        let debugger_published =
            crate::runtime::interpreter::publish_blocked_frames(self.shared, self.thread);
        // T19.H1 — a native about to spin/poll or OS-wait for a long
        // time (e.g. `ReferenceQueue.remove`) must publish its roots and
        // mark itself GC-blocked, otherwise a concurrent stop-the-world
        // collector's `wait_for_all` deadlocks waiting for this thread
        // to reach an interpreter safepoint it will never reach.
        //
        // SBR5 lane ncall: run under the frozen-band memo, as the JNI/FFM
        // in-native deposits do (`conservative_roots::with_frozen_band_memo`).
        // The deposit re-read the whole native stack above the outermost
        // compiled entry, word by word, for a JIT return address on EVERY
        // blocking native: ~20 us per `FileInputStream.read` from a compiled
        // loop against HotSpot's 0.7 us (`Sbr5NcallBench fisRead1k`). That band
        // is frozen while the chain is untouched, which is exactly the memo's
        // argument; a call with no chain entry (an interpreted caller) misses
        // it and scans as before.
        // Interpreter round i4 wave 99, lane L1
        // (`CRATONVM_BLOCKING_LEAVE_KEEPS_SNAPSHOT`): the quiet leave's pause
        // terms, read BEFORE the deposit, so a pause that completes during or
        // after it makes the leave rebuild (`blocking_leave_key`). `None` for
        // a region whose leave must always rebuild: the switch off, foreign
        // code (an FFM downcall: C may upcall and run the lazy in-native
        // protocol inside the region), a terminating thread (no wake).
        let leave_terms = self.blocking_leave_terms(java_state);
        // nio21 wave 17 L3: the blocking-region deposit, whose chain walk may
        // be deferred to the collector (opt-in; see the function).
        crate::jit::conservative_roots::with_frozen_band_memo(None, || {
            self.deposit_root_snapshot_for_blocking_region();
        });
        // Wave 99 (lane L1): the rest of the key, right after the deposit it
        // describes; always written, so no older region's key survives.
        self.record_blocking_leave_key(leave_terms);
        if self.shared.mem.gc_barrier.mark_blocked_region_enter() {
            // A STW was already in progress — arrive at the barrier so
            // the initiator's `wait_for_all` can complete.
            // GCAUDIT-0711-FIX (finding 1a): `_auto` — the deposit above
            // already raised `in_blocked_region` before this check.
            let _ = self
                .shared
                .mem
                .gc_barrier
                .arrive_and_wait_auto(self.thread.thread_id);
        }
        // JDWP (wave 12): after the deposit, whose per-slot record lets a
        // debugger command read this blocked thread's frames if it is
        // suspended while blocked. One load without a debugger.
        #[cfg(feature = "experimental-debug")]
        crate::runtime::interpreter::open_blocked_inspection(
            self.shared,
            self.thread,
            debugger_published,
        );
    }

    /// Try native method first, then full invoke_shared.
    pub(in crate::vm::vm_exec) fn invoke_or_native(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // The NATIVE->JAVA CALLBACK door, labelled separately in
        // `CRATONVM_DBG=dispatch-tally`. Without the label every door's calls
        // arrive under one `invoke_or_native` heading keyed by CALLEE, so the
        // question `composition-native-callback-and-the-promotion-question-20260902.md`
        // item 1 actually asks -- how much of the general resolver's traffic
        // is a native calling back into Java -- cannot be read off it. On
        // `HibfixComposeProbe2` the answer is 1.3 % of it.
        dbg_dispatch_tally("native_callback", class_name, method_name, descriptor);
        invoke_or_native(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }
}

// ---------------------------------------------------------------------------
// CAS helper: compare Values for identity/equality
// ---------------------------------------------------------------------------

/// Compare two Values for CAS purposes. For Int/Long/Float/Double, compares
/// by value. For Object references, compares by pointer identity (reference
/// equality). Returns true if they are "the same" for CAS purposes.
///
/// R1 fix: a primitive field slot that has never been explicitly written
/// decodes as `Value::Object(None)` (the zero-discriminant `Value` variant)
/// when read via `std::ptr::read::<Value>()` on `alloc_zeroed` memory. The
/// canonical fix is [`crate::heap::default_value_for_descriptor`] вЂ” the
/// heap's `alloc_object_with_descriptors` pre-initializes primitive slots
/// with the correctly-tagged zero. This function includes a belt-and-
/// suspenders fallback: when the live slot reads as `Object(None)` and
/// the expected value is a typed primitive zero, treat them as equal so
/// the CAS succeeds and the Java caller (e.g.
/// `ConcurrentHashMap.initTable`) can proceed out of its retry loop even
/// in the rare case where an allocation path skipped descriptor-aware
/// init.
///
/// T19_H6 fix: cross-tag bit-pattern equivalence for primitive Values.
/// The CAS contract operates on raw bits вЂ” a `long` field whose storage
/// tag drifted to `Double` (or vice-versa) must still compare equal when
/// the underlying 64-bit pattern matches. Likewise for the 32-bit pair
/// `(Int, Float)`. We also cover the width-mismatched pairs `(Int, Long)`
/// and `(Int, Double)` because the field-descriptor cache can return `I`
/// for what is actually a `J` slot when an upstream walker (e.g.
/// `Class::field_at_index`) indexes through statics вЂ” in that case the
/// 32-bit-zero-extended Int and the 64-bit Long/Double must match by
/// bit pattern. Equating bits is consistent with the existing
/// `to_bits()` Float/Double equality and is the canonical IEEE-754
/// identity used by `Unsafe.compareAndSet*` in HotSpot.
pub(in crate::vm::vm_exec) fn value_as_u64_bits(v: &Value) -> Option<u64> {
    match v {
        Value::Int(i) => Some(*i as u32 as u64),
        Value::Long(l) => Some(*l as u64),
        Value::Float(f) => Some(f.to_bits() as u64),
        Value::Double(d) => Some(d.to_bits()),
        _ => None,
    }
}

pub(in crate::vm) fn values_equal_for_cas(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Object(x), Value::Object(y)) => match (x, y) {
            (None, None) => true,
            (Some(a), Some(b)) => std::ptr::eq(a.as_ptr(), b.as_ptr()),
            _ => false,
        },
        // Primitive-default coercion: an uninitialized primitive slot
        // reads as Object(None). Accept it as equal to a typed zero so
        // CAS loops don't livelock. Only zero values match вЂ” non-zero
        // primitive expected values still fail (correct mismatch).
        (Value::Object(None), Value::Int(0)) | (Value::Int(0), Value::Object(None)) => true,
        (Value::Object(None), Value::Long(0)) | (Value::Long(0), Value::Object(None)) => true,
        (Value::Object(None), Value::Float(f)) | (Value::Float(f), Value::Object(None))
            if f.to_bits() == 0 =>
        {
            true
        }
        (Value::Object(None), Value::Double(d)) | (Value::Double(d), Value::Object(None))
            if d.to_bits() == 0 =>
        {
            true
        }
        // JIT round 22 wave 2, lane sync (nio21 w7-L1 P2,
        // `CRATONVM_CAS_UNINITIALIZED_IS_DEFAULT`, default ON): a slot read
        // that passes `Uninitialized` through (`get_field_as(.., 'L')` does)
        // is the slot's typed default to every reader HotSpot has -- zero or
        // null -- so it equals a typed default, and itself. Before, it equalled
        // nothing, so a CAS loop whose expected value came from such a read
        // failed forever with no contention. `0|false|off|no` restores that.
        // Cold: only reached with an `Uninitialized` operand.
        (Value::Uninitialized, other) | (other, Value::Uninitialized)
            if true =>
        {
            match other {
                Value::Uninitialized | Value::Object(None) => true,
                Value::Int(0) | Value::Long(0) => true,
                Value::Float(f) => f.to_bits() == 0,
                Value::Double(d) => d.to_bits() == 0,
                _ => false,
            }
        }
        // T19_H6 belt-and-suspenders вЂ” cross-tag primitive bit-pattern
        // equivalence. Same-tag pairs are handled by the matches above; this
        // arm only fires for primitiveГ—primitive cross-tag (e.g. Long vs
        // Double bits), so it can't accidentally match an Object reference.
        (a_v, b_v) => match (value_as_u64_bits(a_v), value_as_u64_bits(b_v)) {
            (Some(ab), Some(bb)) => ab == bb,
            _ => false,
        },
    }
}

/// gci i14/mp: the GC-notification queue that keeps this VM's pool peaks, and
/// its heap pool names (`'static`, for `GcNotificationQueue::note_pool_peak`):
/// the generational heap's own (Serial's three pools), or a G1 / ZGC heap's
/// bean state. `None` where the heap describes no pools.
pub(in crate::vm::vm_exec) fn jmx_peak_queue(
    shared: &SharedVm,
) -> Option<(
    &cratonvm_gc::gc_metrics::GcNotificationQueue,
    &'static [&'static str],
)> {
    match shared.mem.heap.gen_gc_notifications() {
        Some(n) => Some((
            n,
            cratonvm_gc::gc_metrics::JMX_SERIAL_OLD_POOLS,
        )),
        _ => shared
            .mem
            .backend_gc_beans()
            .map(|b| (b.notifications(), b.shape().pools())),
    }
}

/// gci i16/nt: where [`jmx_check_non_heap_thresholds`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "management"), allow(dead_code))]
pub(crate) enum NonHeapThresholdCheck {
    /// `setUsageThreshold0` just armed a non-heap threshold: sample every
    /// armed source now (HotSpot's `jmm_SetPoolThreshold` ->
    /// `detect_low_memory(pool)`).
    Armed,
    /// A class was just defined from `class_file_bytes` bytes
    /// (`define_class_full`): HotSpot's per-expansion metaspace check.
    Defined { class_file_bytes: u64 },
    /// A collection just ended (`record_gc_notification`): class unloading
    /// can have lowered `Metaspace`, and the code heaps may have grown since.
    CollectionEnd,
    /// gci i18/dk: a JIT compile just ran (`jit_bridge.rs`
    /// `CpResolvers::with_compile_request`, the one builder of every compile
    /// request): HotSpot's per-allocation code-cache check. Samples the code
    /// heaps only -- never `Metaspace`, so it takes no class-manager lock and
    /// is callable under any lock a compile door holds.
    CodeAllocated,
}

/// gci i16/nt (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// item 7's residual): HotSpot's low-memory detector for the NON-heap pools
/// (`MemoryService::track_metaspace_memory_usage` /
/// `track_code_cache_memory_usage` -> `LowMemoryDetector::detect_low_memory`):
/// compare `Metaspace` / `Compressed Class Space` and the code heaps against
/// their usage thresholds, in the heap's [`PoolThresholds`] table (the same
/// gauge sensors and delivery the heap pools use), and raise the drain hint
/// when a sensor request is pending, so the next drain point delivers
/// `MEMORY_THRESHOLD_EXCEEDED` (`run_gc_notifications` ->
/// `deliver_pool_sensor`).
///
/// Cost: one `match` and one load (`is_armed`) unless some threshold is set;
/// then a lock per non-heap name to see which sources are armed. The code
/// heaps are two relaxed loads. `Metaspace` is the class-store walk
/// `getUsage()` takes, so outside [`NonHeapThresholdCheck::Armed`] it runs
/// only when [`PoolThresholds::non_heap_walk_due`] says so (class-file growth
/// of `max(64 KiB, used / 64)`, or an unload). Runs no Java and takes the
/// class-manager READ lock only: callable from the define path (its write
/// guard already dropped) and at a collection's end (the world released).
///
/// gci i18/dk: also after every JIT compile
/// ([`NonHeapThresholdCheck::CodeAllocated`], from the one compile-request
/// builder in `jit_bridge.rs`), code heaps only. Code the JIT crate commits
/// outside a compile request (its own stubs and adapters) is seen at the next
/// compile, define or collection end.
///
/// [`PoolThresholds`]: cratonvm_gc::gc_metrics::PoolThresholds
/// [`PoolThresholds::non_heap_walk_due`]: cratonvm_gc::gc_metrics::PoolThresholds::non_heap_walk_due
pub(crate) fn jmx_check_non_heap_thresholds(shared: &SharedVm, at: NonHeapThresholdCheck) {
    let Some((queue, _)) = jmx_peak_queue(shared) else {
        return;
    };
    let thresholds = queue.pool_thresholds();
    if !thresholds.is_armed() {
        return;
    }
    jmx_check_non_heap_thresholds_armed(shared, thresholds, at);
}

/// The armed half of [`jmx_check_non_heap_thresholds`], out of line.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn jmx_check_non_heap_thresholds_armed(
    shared: &SharedVm,
    thresholds: &cratonvm_gc::gc_metrics::PoolThresholds,
    at: NonHeapThresholdCheck,
) {
    #[cfg(feature = "management")]
    {
        use cratonvm_gc::gc_metrics::{JmxPoolUsage, PoolThresholdKind};
        let mut metaspace = false;
        let mut code = false;
        // zt t30/nh: this VM's pools (`CodeCache` when unsegmented).
        let layout = jmx_code_cache_layout(shared);
        let names = cratonvm_native_api::registry::jmx_non_heap_pool_names(layout.as_ref());
        for name in names.iter().copied() {
            if thresholds.threshold(name, PoolThresholdKind::Usage) > 0 {
                if cratonvm_native_builtins_jmx::jmx::non_heap_pool_is_metaspace(name) {
                    metaspace = true;
                } else {
                    code = true;
                }
            }
        }
        if !(metaspace || code) {
            return;
        }
        let unloaded = shared
            .debug
            .diagnostic_counters
            .classes_unloaded
            .load(std::sync::atomic::Ordering::Relaxed);
        if metaspace {
            metaspace = match at {
                NonHeapThresholdCheck::Armed => true,
                NonHeapThresholdCheck::Defined { class_file_bytes } => {
                    thresholds.non_heap_walk_due(class_file_bytes, unloaded)
                }
                NonHeapThresholdCheck::CollectionEnd => thresholds.non_heap_walk_due(0, unloaded),
                // gci i18/dk: the compile path samples the code heaps only.
                NonHeapThresholdCheck::CodeAllocated => false,
            };
        }
        if !(metaspace || code) {
            return;
        }
        // gci i18/dk (`gci-i17rv-i16-default-path-review-findings-RETIRED-UNFIXED-20261018`
        // finding 8): ONE class-manager read guard for the sample, and only
        // the metaspace walk under it (`vm_init::metaspace_used_bytes`, the
        // walk `heap_summary` reports) -- was a `try_read` probe dropped at
        // once, then the whole heap summary and a second read, both blocking.
        // At a collection's end the guard is the `try_read` itself: the
        // class-unloading step after a collection takes the class-manager
        // locks blocking on this same thread, so none is held here; still, a
        // writer now (another thread's define) only postpones the sample --
        // `note_non_heap_walk` is not called, so the walk stays due -- and
        // never makes this thread wait inside a door. The set and the define
        // path hold no class-manager guard (the define's write guard already
        // dropped): they block, as before.
        let metaspace_sample = if metaspace {
            let guard = match at {
                NonHeapThresholdCheck::CollectionEnd => shared.classes.class_manager.try_read(),
                NonHeapThresholdCheck::Armed | NonHeapThresholdCheck::Defined { .. } => {
                    Some(shared.classes.class_manager.read())
                }
                // Never reached (`metaspace` is false for it above); spelled
                // so the compile path can never take a class-manager lock.
                NonHeapThresholdCheck::CodeAllocated => None,
            };
            // gci i33/ms: the same one walk states HotSpot's committed and
            // class-space figures (`vm_init::metaspace_stats`), so the
            // `Compressed Class Space` threshold compares what its
            // `getUsage()` answers.
            guard.map(|cm| {
                let stats = crate::vm::vm_init::metaspace_stats(&cm);
                let split = stats.class_space.map(|(class_used, class_committed, class_max)| {
                    cratonvm_native_builtins_jmx::jmx::MetaspaceSplit {
                        committed: stats.committed,
                        class_used,
                        class_committed,
                        class_max,
                    }
                });
                ((stats.used, cm.max_metaspace_bytes()), split)
            })
        } else {
            None
        };
        if metaspace_sample.is_none() && !code {
            return;
        }
        let code_sample = code.then(jmx_code_cache_bytes);
        let mut pools = cratonvm_native_builtins_jmx::jmx::non_heap_threshold_samples_in(
            names,
            metaspace_sample.map(|(sample, _)| sample),
            metaspace_sample.and_then(|(_, split)| split),
            code_sample,
        );
        // zt t3/ob: the code heaps' `init` as `getUsage()` answers it.
        cratonvm_native_builtins_jmx::jmx::with_code_cache_layout(&mut pools, layout.as_ref());
        let samples: Vec<JmxPoolUsage> = pools.iter().map(non_heap_pool_as_jmx_usage).collect();
        thresholds.check(&samples, &[]);
        if let Some(((used, _), _)) = metaspace_sample {
            thresholds.note_non_heap_walk(used, unloaded);
        }
        if thresholds.has_pending() {
            shared.mem.gc_barrier.note_ref_work_queued();
        }
    }
    #[cfg(not(feature = "management"))]
    {
        // No pool beans without the management natives: nothing can arm one.
        let _ = (shared, thresholds, at);
    }
}

/// gci i16/nt: a non-heap pool's JMX figures as the threshold table's
/// sample (`-1` = undefined -> `None`); `pool_usage_array` maps it back to
/// the same `MemoryUsage` quadruple `getUsage()` answers. Pure.
#[cfg_attr(not(feature = "management"), allow(dead_code))]
pub(in crate::vm::vm_exec) fn non_heap_pool_as_jmx_usage(
    p: &cratonvm_native_api::GcMemoryPoolInfo,
) -> cratonvm_gc::gc_metrics::JmxPoolUsage {
    cratonvm_gc::gc_metrics::JmxPoolUsage {
        name: p.name,
        init: u64::try_from(p.init).ok(),
        used: u64::try_from(p.used).unwrap_or(0),
        committed: u64::try_from(p.committed).unwrap_or(0),
        max: u64::try_from(p.max).ok(),
        collection_used: None,
        managers: p.manager_names,
    }
}

/// gci i15/nh / i16/nt: the code heaps' source -- the JIT's live mapped code
/// and the code-cache cap (`None` unbounded). Process-wide, as HotSpot's code
/// cache is; two relaxed loads.
pub(crate) fn jmx_code_cache_bytes() -> (u64, Option<u64>) {
    let used =
        cratonvm_jit::COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed) as u64;
    let cap = cratonvm_jit::jit_code_cache_cap_bytes();
    (used, (cap != usize::MAX).then_some(cap as u64))
}

/// zt t3/ob: the code heaps' HotSpot `init` on this VM, if it states one
/// (ZGC, `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`); one load of the bean state.
///
/// zt t30/nh (`g1-and-generational-items-carried-from-the-zgc-zt-round-FIXED-20261023.md`
/// item 2): under `CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT` (default ON) the
/// whole HotSpot layout from this VM's options
/// (`config::hotspot_flags::code_cache_layout`) on EVERY collector -- G1 and
/// Generational reported gci i15/nh's 0/0/2 MiB, and no collector followed
/// `ReservedCodeCacheSize`, `SegmentedCodeCache` or `InitialCodeCacheSize`.
/// ZGC keeps its own kills (`None` under either). `=0`: ZGC's fixed default
/// layout, `None` elsewhere, as before.
pub(in crate::vm::vm_exec) fn jmx_code_cache_layout(
    shared: &SharedVm,
) -> Option<cratonvm_types::code_cache_layout::CodeCacheLayout> {
    let zgc_init = || {
        shared
            .mem
            .backend_gc_beans()
            .and_then(|b| b.hotspot_code_heap_init_bytes())
    };
    if !crate::config::hotspot_flags::code_cache_hotspot_layout_enabled() {
        return zgc_init()
            .map(|_| cratonvm_types::code_cache_layout::CodeCacheLayout::DEFAULT_SEGMENTED);
    }
    #[cfg(feature = "zgc")]
    if shared.config.gc_algorithm == crate::config::GcAlgorithm::Zgc {
        zgc_init()?;
    }
    Some(crate::config::hotspot_flags::code_cache_layout(&shared.config))
}


/// gci i42/mx (`gci-i42mx-jfr-gc-configuration-events-are-not-hotspots-FIXED-20261016`):
/// HotSpot's `[MinHeapSize, InitialHeapSize, MaxHeapSize]` from the command
/// line's `-XX:MinHeapSize` / `-Xms` (`min`), `-Xms` (`initial`) and `-Xmx`
/// (`max`), exactly as the `gc,init` block computes them (`vm_init.rs`, gcg
/// g3/x): the initial size clamped to the maximum, the minimum
/// `hotspot_min_heap_size` of it. `MaxHeapSize` is `-Xmx` itself, without the
/// copy reserve `max_heap_bytes` (`Runtime.maxMemory()`) takes off. Pure.
pub(crate) fn gci_i42_mx_hotspot_heap_sizes(
    min: Option<usize>,
    initial: usize,
    max: usize,
) -> [i64; 3] {
    let initial = initial.min(max);
    let min = crate::runtime::unified_logging::hotspot_min_heap_size(min, initial);
    let b = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    [b(min), b(initial), b(max)]
}

/// gci i42/mx: `(DisableExplicitGC, ExplicitGCInvokesConcurrent)` of the
/// folded `VmConfig::explicit_gc`. Both flags given fold to `Disabled`
/// (HotSpot's `JVM_GC` tests that one first).
///
/// gci i43/lf (`gci-i42mx-...` item 8): under `Disabled` the concurrent
/// answer is the flag's own value `concurrent_flag`
/// (`VmConfig::explicit_gc_invokes_concurrent`), as HotSpot's
/// `isExplicitGCConcurrent`, where it was always `false`. `Full` stays
/// `(false, false)` whatever `concurrent_flag` says (only an embedder that
/// sets the two fields inconsistently reaches that), `Concurrent` is
/// `(false, true)`. Pure.
pub(crate) fn gci_i42_mx_explicit_gc_flags(
    policy: crate::config::ExplicitGcPolicy,
    concurrent_flag: bool,
) -> (bool, bool) {
    match policy {
        crate::config::ExplicitGcPolicy::Full => (false, false),
        crate::config::ExplicitGcPolicy::Disabled => (true, concurrent_flag),
        crate::config::ExplicitGcPolicy::Concurrent => (false, true),
    }
}

/// gci i43/lf: `(UseTLAB, MinTLABSize, TLABRefillWasteFraction)` as HotSpot
/// reports them for these flags (its defaults when not given). Pure.
pub(crate) fn gci_i43_lf_tlab_configuration(tlab: &crate::config::TlabFlags) -> (bool, i64, i64) {
    let b = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    (
        tlab.hotspot_use_tlab(),
        b(tlab.hotspot_min_tlab_size()),
        b(tlab.hotspot_refill_waste_fraction()),
    )
}


/// A heap's GC-notification record in the shape the JMX natives take
/// (`cratonvm_native_api::GcNotificationInfo`). Byte counts saturate into
/// `i64`; an unrecorded `init`/`max` is the JMM's `-1`, as in
/// `NativeContextImpl::gc_memory_pools`. gen r4w3/obs2 (2026-09-23).
pub(crate) fn gc_notification_info_of(
    rec: &cratonvm_gc::gc_metrics::GcNotificationRecord,
) -> cratonvm_native_api::GcNotificationInfo {
    let bytes = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let opt = |v: Option<u64>| v.map_or(-1, bytes);
    let pools = |ps: &[cratonvm_gc::gc_metrics::JmxPoolUsage]| {
        ps.iter()
            .map(|p| cratonvm_native_api::GcPoolUsageSnapshot {
                name: p.name,
                init: opt(p.init),
                used: bytes(p.used),
                committed: bytes(p.committed),
                max: opt(p.max),
            })
            .collect::<Vec<_>>()
    };
    cratonvm_native_api::GcNotificationInfo {
        gc_name: rec.collector,
        gc_action: rec.action,
        gc_cause: rec.cause,
        id: rec.id,
        start: rec.start,
        end: rec.end,
        timestamp_ms: bytes(rec.timestamp_ms),
        before: pools(&rec.before[..]),
        after: pools(&rec.after[..]),
    }
}

// ---------------------------------------------------------------------------
// Free functions: invoke_shared, invoke_on_class_shared
// ---------------------------------------------------------------------------

/// Like [`invoke_shared`], but the caller already has an unambiguous,
/// resolved declaring `ClassId` in hand (e.g. from a reflective `Method`
/// object's own `clazz` mirror) and must NOT re-resolve it by name.
///
/// `invoke_shared`'s `shared.load_class_concurrent(class_name)` step goes
/// through the loader-blind global name lookup, which intentionally reports
/// "ambiguous" (not a guess) the moment two or more *different*
/// user-defined loaders each have their OWN distinct class registered under
/// the identical simple name (`ClassManager::get_loaded_class_id`) — a
/// completely ordinary occurrence for any harness that repeatedly mints a
/// fresh `ClassLoader` + identically-named generated class (Spring's
/// `TestCompiler`/`@CompileWithForkedClassLoader`). Skipping that
/// re-resolution when the caller already knows the exact `ClassId` avoids
/// hitting the ambiguity guard for a lookup that was never actually
/// ambiguous — the caller resolved a SPECIFIC class through a SPECIFIC
/// loader already.
pub fn invoke_by_class_id_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // Steady state (wave 15): an initialized class runs no `<clinit>`, so no
    // collection can happen before the dispatch, whose own pinned copy
    // (`PinnedDispatchArgs` in `invoke_on_class_shared_inner`) roots the
    // arguments from its first statement on. The pins below would be a
    // second, redundant rooting pass over the same references.
    if super::is_class_initialized_via_manager(shared, class_id) {
        return invoke_on_class_shared(shared, thread, class_id, method_name, descriptor, args);
    }
    let pin_base = thread.native_pin_roots.len();
    let has_obj_args = push_object_arg_pins(thread, args);

    if let Err(e) = super::ensure_class_initialized_shared(shared, thread, class_id) {
        thread.native_pin_roots.truncate(pin_base);
        return Err(e);
    }

    // gen r4w3/rooting: only pay for the refreshed copy when the init
    // actually moved an argument.
    let fresh = if has_obj_args {
        args_refreshed_if_moved(thread, pin_base, args)
    } else {
        None
    };
    // Released before the dispatch: nothing between here and the dispatch's
    // own pins can collect, and those root the (refreshed) references.
    thread.native_pin_roots.truncate(pin_base);
    invoke_on_class_shared(
        shared,
        thread,
        class_id,
        method_name,
        descriptor,
        fresh.as_deref().unwrap_or(args),
    )
}

/// Push every object reference of `args` onto `thread.native_pin_roots`, in
/// order; `true` when there was one. The caller truncates back to the length
/// it read before the call.
pub(in crate::vm::vm_exec) fn push_object_arg_pins(thread: &mut JvmThread, args: &[Value]) -> bool {
    let mut has_obj_args = false;
    for a in args {
        if let Value::Object(Some(o)) = a {
            thread.native_pin_roots.push(*o);
            has_obj_args = true;
        }
    }
    has_obj_args
}

/// `args` with each object reference re-read from the pin
/// [`push_object_arg_pins`] pushed for it at `pin_base`, or `None` when no
/// reference moved (the caller keeps using `args`).
pub(in crate::vm::vm_exec) fn args_refreshed_if_moved(
    thread: &JvmThread,
    pin_base: usize,
    args: &[Value],
) -> Option<Vec<Value>> {
    let pinned = |k: usize| thread.native_pin_roots.get(pin_base + k).copied();
    let mut k = 0;
    let mut moved = false;
    for a in args {
        if let Value::Object(Some(o)) = a {
            moved |= pinned(k).is_some_and(|p| p != *o);
            k += 1;
        }
    }
    if !moved {
        return None;
    }
    let mut fresh = args.to_vec();
    let mut k = 0;
    for v in fresh.iter_mut() {
        if let Value::Object(Some(o)) = *v {
            *v = Value::Object(Some(pinned(k).unwrap_or(o)));
            k += 1;
        }
    }
    Some(fresh)
}

pub fn invoke_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    // GC-safety: class loading and <clinit> below can run arbitrary Java and
    // trigger a MOVING young GC while `args` still sit in a raw Rust slice —
    // invisible to the collector, so any object arg would be left dangling
    // and the callee would receive a stale (relocated/reclaimed) pointer
    // (native stale-local family, VM-level instance; repro: a native invoking
    // a static on a not-yet-initialized class, e.g. Spliterators.spliterator
    // from the synthetic stream bridges under CRATONVM_DBG_GC_STRESS).
    // Pin object args, then re-read the GC-forwarded addresses after the
    // load+clinit window before dispatching.
    let (class_id, fresh) = load_class_pinning_args(shared, thread, class_name, args);
    let class_id = class_id?;
    // Ensure the class is initialized (runs <clinit>) per JVM spec §5.5 and
    // dispatch: the instance tail, which pins across the initializer only
    // when one can still run.
    invoke_by_class_id_shared(
        shared,
        thread,
        class_id,
        method_name,
        descriptor,
        fresh.as_deref().unwrap_or(args),
    )
}

/// Load `class_name` (thread-safe, per-class-name lock) with the references in
/// `args` pinned across the load, which can run a user loader and collect.
/// The second half is `args` with the forwarded references when the load
/// moved one, `None` when it moved none (the caller keeps `args`); the pins
/// are released on return. Wave 15: the by-name routes copied `args` on every
/// call with an object argument.
pub(in crate::vm::vm_exec) fn load_class_pinning_args(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    args: &[Value],
) -> (Result<ClassId, VmError>, Option<Vec<Value>>) {
    let pin_base = thread.native_pin_roots.len();
    if !push_object_arg_pins(thread, args) {
        return (shared.load_class_concurrent(class_name), None);
    }
    let class_id = shared.load_class_concurrent(class_name);
    let fresh = args_refreshed_if_moved(thread, pin_base, args);
    thread.native_pin_roots.truncate(pin_base);
    (class_id, fresh)
}

/// Whether `NativeContext::invoke` (`NativeContextImpl::invoke`) dispatches an
/// INHERITED static — `C.m` for a static `m` declared in a superclass `P` —
/// through the static tail ([`invoke_static_on_owner`]), which initializes
/// `P` only (JVMS §5.5, JLS §12.4.1), instead of the instance tail, which
/// initializes `C`. The trait method carries no static/instance fact, so the
/// door classifies the resolved method itself (`native_context_invoke`).
///
/// Every mode since wave 16: the `--compatible` census
/// (`CRATONVM_DBG_FIELD_SITE`, one `[native-invoke-init-census]` line per
/// class whose `<clinit>` the static tail would no longer run) read zero over
/// the core suite and a Spring Boot application, JIT on and off. The one
/// policy point. Interpreter round i1 wave 15, lane L5;
/// `docs/feature-designs/real-jdk-mode.md`.
#[inline]
pub fn native_invoke_static_tail_enforced(_shared: &SharedVm) -> bool {
    true
}

/// `NativeContext::invoke`'s by-name route: [`invoke_shared`], except that an
/// inherited static on a not-yet-initialized symbolic owner takes the static
/// tail where [`native_invoke_static_tail_enforced`] says so (and is counted
/// by the census otherwise). With neither the policy nor the census armed
/// this is exactly `invoke_shared`; with them, the classification costs one
/// initialized-memo probe per call once the owner is initialized (an
/// initialized owner has initialized superclasses, so both tails agree).
pub(in crate::vm::vm_exec) fn native_context_invoke(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    preresolved: Option<ClassId>,
) -> MethodCallResult {
    let enforced = native_invoke_static_tail_enforced(shared);
    if preresolved.is_none()
        && !enforced
        && !crate::runtime::interpreter::site_cache::site_stats::on()
    {
        return invoke_shared(shared, thread, class_name, method_name, descriptor, args);
    }
    let (class_id, fresh) = match preresolved {
        Some(class_id) => (Ok(class_id), None),
        None => load_class_pinning_args(shared, thread, class_name, args),
    };
    let class_id = class_id?;
    let args = fresh.as_deref().unwrap_or(args);
    if !super::is_class_initialized_via_manager(shared, class_id) {
        // `invokestatic_init_class` answers the owner itself unless the
        // method resolves to a static declared by a different, non-interface
        // class: an instance method keeps the instance tail.
        let init_class = crate::runtime::interpreter::invokestatic_init_class(
            shared,
            class_id,
            method_name,
            descriptor,
        );
        if init_class != class_id {
            if enforced {
                return invoke_static_on_owner(
                    shared,
                    thread,
                    class_id,
                    class_name,
                    method_name,
                    descriptor,
                    args,
                );
            }
            note_native_invoke_init_census(
                shared,
                class_id,
                init_class,
                class_name,
                method_name,
                descriptor,
            );
        }
    }
    invoke_by_class_id_shared(shared, thread, class_id, method_name, descriptor, args)
}

/// The `--compatible` census line of [`native_context_invoke`]: once per
/// symbolic owner whose `<clinit>` the instance tail runs for an inherited
/// static and the static tail would not.
#[cold]
pub(in crate::vm::vm_exec) fn note_native_invoke_init_census(
    shared: &SharedVm,
    owner: ClassId,
    declaring: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    static SEEN: std::sync::OnceLock<parking_lot::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    let (state, declaring_name) = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(owner) else {
            return;
        };
        let declaring_name = cm
            .get_class(declaring)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        (class.state, declaring_name)
    };
    let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(Default::default()));
    if !seen.lock().insert(class_name.to_string()) {
        return;
    }
    eprintln!(
        "[native-invoke-init-census] {class_name} state={state:?} declared-in={declaring_name} \
         via {class_name}.{method_name}{descriptor}"
    );
}

