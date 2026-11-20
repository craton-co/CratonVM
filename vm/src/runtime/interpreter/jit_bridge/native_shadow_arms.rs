#[allow(unused_imports)]
use super::*;

/// What [`jit_invoke_targets_native_shadow`] found about one call site.
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::runtime::interpreter::jit_bridge) struct NativeShadowArms {
    /// `force_native_over_real_jdk_bytecode` names the CP triple.
    pub(in crate::runtime::interpreter::jit_bridge) forced: bool,
    /// A native that will run is registered on the CP class (or `forced`).
    pub(in crate::runtime::interpreter::jit_bridge) direct: bool,
    /// A native that will run is registered on the resolved declaring class.
    pub(in crate::runtime::interpreter::jit_bridge) inherited: bool,
    /// The resolved method has no `Code` and is not `ACC_NATIVE`.
    pub(in crate::runtime::interpreter::jit_bridge) target_is_abstract_declaration: bool,
    /// The CP class's registration is a reviewed `Intrinsic` (asked only when
    /// `direct` on an abstract declaration).
    pub(in crate::runtime::interpreter::jit_bridge) target_native_is_intrinsic: bool,
    /// Likewise for the declaring class's registration.
    pub(in crate::runtime::interpreter::jit_bridge) declaring_native_is_intrinsic: bool,
    /// `invokeinterface`, the interface-blind arm is on, and the class-blind
    /// `(name, descriptor)` digest probe hit.
    pub(in crate::runtime::interpreter::jit_bridge) interface_blind_class_blind: bool,
}

/// The verdict of [`native_shadow_verdict`], arm by arm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct NativeShadowVerdict {
    pub(in crate::runtime::interpreter::jit_bridge) direct: bool,
    pub(in crate::runtime::interpreter::jit_bridge) inherited: bool,
    pub(in crate::runtime::interpreter::jit_bridge) interface_blind: bool,
    /// The pre-wave-9 rule (no abstract-declaration discount, class-blind
    /// fallback) would have sealed the caller.
    pub(in crate::runtime::interpreter::jit_bridge) sealed_before_wave9: bool,
}

impl NativeShadowVerdict {
    pub(in crate::runtime::interpreter::jit_bridge) fn shadow(self) -> bool {
        self.direct || self.inherited || self.interface_blind
    }

    /// The census label, one per call site that any rule would have sealed.
    pub(in crate::runtime::interpreter::jit_bridge) fn cause(self) -> Option<&'static str> {
        if self.direct {
            Some("direct")
        } else if self.inherited {
            Some("inherited")
        } else if self.interface_blind {
            Some("interface-blind")
        } else if self.sealed_before_wave9 {
            Some("abstract-declaration-admitted")
        } else {
            None
        }
    }
}

/// The native-shadow rule (interpreter round i1 wave 9), as a pure function of
/// the facts about one call site. `owner_may_shadow` is the precise fallback
/// (`abstract_declaration_owner_may_shadow`), asked only when it decides.
///
/// * `direct` / `inherited` count unless the target is an abstract
///   declaration and the registration is neither forced nor an `Intrinsic`.
/// * The interface-blind fallback, reached only when neither counts: for a
///   target with a body it is the class-blind digest probe, exactly as
///   before; for an abstract declaration it also needs `owner_may_shadow`.
pub(in crate::runtime::interpreter::jit_bridge) fn native_shadow_verdict(
    arms: NativeShadowArms,
    owner_may_shadow: impl FnOnce() -> bool,
) -> NativeShadowVerdict {
    let abstract_target = arms.target_is_abstract_declaration;
    let direct =
        arms.direct && (arms.forced || !abstract_target || arms.target_native_is_intrinsic);
    let inherited = arms.inherited && (!abstract_target || arms.declaring_native_is_intrinsic);
    let interface_blind = arms.interface_blind_class_blind
        && !direct
        && !inherited
        && (!abstract_target || owner_may_shadow());
    NativeShadowVerdict {
        direct,
        inherited,
        interface_blind,
        sealed_before_wave9: arms.direct || arms.inherited || arms.interface_blind_class_blind,
    }
}

pub(in crate::runtime::interpreter) fn jit_method_calls_native_shadowed(
    shared: &SharedVm,
    caller_class_id: ClassId,
    code: &[u8],
    code_len: usize,
) -> bool {
    let scan_len = code_len.min(code.len());
    let scan_code = &code[..scan_len];
    let mut pc = 0;
    while pc < scan_len {
        let (instruction, next_pc) = match Instruction::decode(scan_code, pc) {
            Ok(decoded) => decoded,
            Err(_) => return true,
        };
        if let Some(cp_idx) = jit_native_shadow_dynamic_invoke_index(&instruction) {
            if jit_invoke_targets_native_shadow(shared, caller_class_id, cp_idx) {
                return true;
            }
        }
        if next_pc <= pc || next_pc > scan_len {
            return true;
        }
        pc = next_pc;
    }
    false
}

/// Detect forced `Class` generic-metadata bridges in a prospective compiled
/// caller. They must continue through interpreter dispatch, which chooses the
/// Signature-attribute native implementation over the real-JDK bytecode.
///
/// The caller must pass its existing class-manager guard. Do not make this
/// helper acquire the lock itself: `try_jit_compile_callee_slow` already holds
/// a read guard while extracting the method body. If a class-loading writer
/// queues between that guard and a nested `read()`, parking_lot's task-fair
/// `RwLock` parks the nested read behind the writer while the outer read keeps
/// the writer parked forever. Hibernate model/ByteBuddy generation and
/// WildFly parallel extension boot both exercise that ordering.
pub(in crate::runtime::interpreter) fn jit_method_calls_forced_class_generic_metadata(
    cm: &crate::classloading::ClassManager,
    caller_class_id: ClassId,
    code: &[u8],
    code_len: usize,
) -> bool {
    let scan_len = code_len.min(code.len());
    let scan_code = &code[..scan_len];
    let mut pc = 0;
    while pc < scan_len {
        let (instruction, next_pc) = match Instruction::decode(scan_code, pc) {
            Ok(decoded) => decoded,
            Err(_) => return true,
        };
        if let Some(cp_idx) = jit_native_shadow_dynamic_invoke_index(&instruction) {
            let is_forced_generic_metadata = {
                let Some(caller) = cm.get_class(caller_class_id) else {
                    return true;
                };
                let cp = &caller.constant_pool;
                let (class_index, nat_index) = match cp.get(cp_idx) {
                    Some(ConstantPoolEntry::MethodReference {
                        class_index,
                        name_and_type_index,
                    })
                    | Some(ConstantPoolEntry::InterfaceMethodReference {
                        class_index,
                        name_and_type_index,
                    }) => (*class_index, *name_and_type_index),
                    _ => return true,
                };
                matches!(
                    (
                        cp.get_class_name(class_index),
                        cp.get_name_and_type(nat_index)
                    ),
                    (
                        Some("java/lang/Class"),
                        Some(("getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;"))
                            | Some(("getGenericInterfaces", "()[Ljava/lang/reflect/Type;"))
                            | Some(("getGenericSuperclass", "()Ljava/lang/reflect/Type;"))
                    )
                )
            };
            if is_forced_generic_metadata {
                return true;
            }
        }
        if next_pc <= pc || next_pc > scan_len {
            return true;
        }
        pc = next_pc;
    }
    false
}

/// Resolve a constant-pool class NAME for a method that is being COMPILED,
/// through the compiling class's own defining loader.
///
/// # The defect this exists to stop
///
/// The compiler used to answer these with `SharedVm::load_class_concurrent`,
/// which is deliberately loader-BLIND: its own doc says it "can only ever
/// produce a Bootstrap/Extension/Application-loaded class". For a method whose
/// owner was defined by a USER-DEFINED loader that is not a lookup at all --
/// it is a DEFINITION. `resolve_fast_path_class_id` sees the user loader's
/// copy, correctly refuses to hand a built-in-chain caller someone else's
/// namespace, and defines a SECOND copy under `Application`; the compiler then
/// bakes that second `ClassId` into the site.
///
/// Measured on `TestContextAotGeneratorIntegrationTests.processAheadOfTimeWithWebTests`
/// (Spring's `@CompileWithForkedClassLoader`, whose
/// `CompileWithForkedClassLoaderClassLoader` re-defines the whole classpath on
/// purpose): the background compiler, OSR-compiling
/// `com.thoughtworks.qdox.parser.impl.Parser.yyparse` for the FORK loader's
/// copy, resolved the method's `new` sites this way and baked
/// `TypeDef`-under-`Application`. Compiled code then allocated
/// Application-namespace `TypeDef`s inside a fork-namespace parser, and the
/// site's own `checkcast` -- correctly resolved through the compiling class's
/// loader by `intern_typecheck_target` -- refused them:
///
/// ```text
/// [cv-checkcast-fail] typecheck REFUSED: obj_cid=5444 obj_loader=Application
///     obj_cls=com/thoughtworks/qdox/parser/structs/TypeDef
///     site_target=5400 site_target_loader=UserDefined(3)
/// ```
///
/// which Java sees as `ClassCastException: class ...TypeDef cannot be cast to
/// class ...TypeDef` -- the textbook two-copies-one-name message, produced by
/// the compiler rather than by the program.
///
/// # What this does instead
///
/// * Owner defined by a BUILT-IN loader (the overwhelming majority): unchanged
///   -- `load_class_concurrent`, which for such an owner produces exactly the
///   class the interpreter would have resolved.
/// * Owner defined by a USER-DEFINED loader: LOOK UP only, and only what the
///   method-entry doors' `new` sites may bind ([`jit_known_class_constant`]:
///   the loader's own definition, a direct supertype, the loader's initiating
///   memo, then the `--real-jdk` JDK-global-name rule) -- never defining. A
///   miss returns `None`, so the site takes the DEFERRED path it already has
///   for an unresolvable target and the interpreter resolves it correctly at
///   run time. Declining to optimise is always available; defining a second
///   identity is not.
///
/// Interpreter round i1 wave 38, lane L2: that arm was
/// `get_loaded_class_id_for_requester`, which after the loader's own record
/// answers its recorded parents and then the BUILT-IN chain. For a child-first
/// loader that is the wrong guess for every name it has not asked for yet: a
/// cold `new` of a name the class path also has (a site the interpreter never
/// ran) bound the application's class, and a trivial-constructor check judged
/// the application's constructor. The single-pass OSR door, the
/// `cp_elidable_init_resolver` of every `CompileRequest` door, and (since this
/// wave) the eager first-call door ask here; the method-entry doors' own `new`
/// resolver (`resolve_jit_new_site`) already asked `jit_known_class_constant`.
/// `L2W38EagerDoorLoaderNew.java`.
///
/// `CRATONVM_JIT_LOADER_BLIND_CP_RESOLVE=1` restores the old call for a
/// one-binary A/B.
pub(in crate::runtime::interpreter) fn resolve_cp_class_for_owner(
    shared: &SharedVm,
    owner: ClassId,
    name: &str,
) -> Option<ClassId> {
    if jit_loader_blind_cp_resolve() {
        return shared.load_class_concurrent(name).ok();
    }
    let owner_loader = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(owner).map(|c| c.loader_id)
    };
    match owner_loader {
        Some(loader @ cratonvm_types::ClassLoaderId::UserDefined(_)) => {
            let cm = shared.classes.class_manager.read();
            let answer = jit_known_class_constant(shared, &cm, name, owner);
            if answer.is_none() && crate::runtime::env_cache::dbg_jitc() {
                note_cp_class_deferred(&cm, owner, name, loader);
            }
            answer
        }
        _ => shared.load_class_concurrent(name).ok(),
    }
}

/// `CRATONVM_DBG_JITC`: a user loader's constant-pool class the compile
/// declined to bind ([`resolve_cp_class_for_owner`]), with what the old
/// loader-blind lookup would have bound (`blind=`). The positive control of
/// `L2W38EagerDoorLoaderNew`'s cold rows.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn note_cp_class_deferred(
    cm: &crate::classloading::ClassManager,
    owner: ClassId,
    name: &str,
    loader: cratonvm_types::ClassLoaderId,
) {
    let holder = cm
        .get_class(owner)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    let blind = cm.get_loaded_class_id_for_requester(name, loader);
    eprintln!(
        "[cratonvm-jitc] cp-class deferred holder={holder} name={name} loader={loader:?} blind={:?}",
        blind.map(|id| id.as_u32())
    );
}

/// Kill switch for [`resolve_cp_class_for_owner`].
pub(in crate::runtime::interpreter::jit_bridge) fn jit_loader_blind_cp_resolve() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        false
    })
}

/// Shared body for the JIT `cp_elidable_init_resolver` closures: given the
/// holder class `holder_cid` and an `invokespecial` constant-pool index, return
/// `true` iff it targets a no-arg `<init>()V` whose construction is elidable
/// (see [`is_elidable_construction`]) — for both scalar replacement (IR `new`
/// elision) and the per-allocation ctor-dispatch elision.
///
/// Resolves the target via `load_class_concurrent` so APPLICATION-loaded
/// classes are covered, not only bootstrap/JDK ones: `find_class_by_name` does
/// NOT see app classes in the JIT-compile context (the same gap the
/// `execute`/`try_osr` ctor-elision paths hit). It reads the target
/// class name under a brief `class_manager` lock, DROPS it, resolves via
/// `load_class_concurrent` (already-loaded classes return cached — the common
/// case, since a ctor site's class is loaded — so no `<clinit>`/GC), then
/// re-reads `cm` to check elidability. Lock discipline matches the
/// `field_resolver` precedent (resolve-with-load BEFORE taking the inner `cm`
/// read), so no VM read lock is alive across the load. MUST NOT be called with
/// a `class_manager` lock already held.
pub(in crate::runtime::interpreter) fn resolve_jit_elidable_init_loading(
    shared: &SharedVm,
    holder_cid: ClassId,
    cp_idx: u16,
) -> bool {
    // 1. Extract the target class name + confirm a no-arg `<init>()V` ref
    //    (brief `cm` read, dropped before the load).
    let target_name = {
        let cm = shared.classes.class_manager.read();
        let Some(holder) = cm.get_class(holder_cid) else {
            return false;
        };
        let cp = &holder.constant_pool;
        let (class_index, nat_index) = match cp.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => return false,
        };
        if !matches!(cp.get_name_and_type(nat_index), Some(("<init>", "()V"))) {
            return false;
        }
        match cp.get_class_name(class_index) {
            Some(n) => n.to_string(),
            None => return false,
        }
    };
    // 2. Resolve the target (loading if necessary) with NO `cm` lock held.
    let Some(target_id) = resolve_cp_class_for_owner(shared, holder_cid, &target_name) else {
        return false;
    };
    // 3. Check elidability (brief `cm` read).
    let cm = shared.classes.class_manager.read();
    let elidable = is_elidable_construction(shared, &cm, target_id);
    // DBG (CRATONVM_DBG_CTOR_FIX): when this resolves an elidable ctor whose
    // target `find_class_by_name` could NOT see, it is the app-class gap being
    // closed (the old resolver would have returned false here).
    if elidable && crate::runtime::env_cache::ctor_fix_dbg() {
        let via_find = cm
            .find_class_by_name_for_class(&target_name, holder_cid)
            .is_some();
        eprintln!(
            "[ctor-fix] elidable-resolver: {} elidable=true find_class_by_name={}{}",
            target_name,
            via_find,
            if via_find {
                ""
            } else {
                "  <- APP-CLASS GAP CLOSED"
            },
        );
    }
    elidable
}

/// `CRATONVM_JIT=sync-methods` — admit `ACC_SYNCHRONIZED` methods to the
/// invocation-counter compile path, where the interpreter's call wrapper owns
/// the implicit monitor. Read once and cached; this sits on the hot
/// uncached-invocation path. Default-OFF → behaviour byte-for-byte unchanged.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_sync_methods_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| false)
}

/// Try to JIT-compile a method and return the upgraded cache target.
/// Returns None if the method is not JIT-compatible.
/// Uses the shared JIT cache to avoid re-compiling across threads.
///
/// WP2.4-F1: prefer [`try_jit_upgrade_with_gate`] from invoke-cache call
/// sites that already hold a [`RedefineGate`] from a prior bytecode hit;
/// this entry point synthesizes a fresh gate from the manager.
pub(in crate::runtime::interpreter) fn try_jit_upgrade(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
) -> Option<CachedInvokeTarget> {
    // WP2.4-F1: derive a gate from the manager so JIT-only callers
    // (e.g. tier promotions outside the cache hit path) still get
    // staleness invalidation.
    let gate = RedefineGate::snapshot(
        shared
            .classes
            .class_manager
            .read()
            .class_redefine_generation_handle(cached.declaring_class_id),
    );
    try_jit_upgrade_with_gate(shared, cached, gate)
}

/// How many compiled field sites were refused because the field the resolver
/// FOUND does not have the descriptor the constant pool NAMED.
pub static JIT_FIELD_TAG_DISAGREEMENTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Does the resolved field's own descriptor agree with the one the constant
/// pool's `NameAndType` spells for this site?
///
/// # Why this was asked at all
///
/// **The premise below is now historical.** When this guard was written, this
/// VM's field resolution — `MemberResolver::locate_field`,
/// `ClassFile::find_own_field` and `find_field_recursive` underneath it —
/// matched on the **name alone** and returned the first field of that name it
/// met walking the class, its superinterfaces and its superclass chain, while
/// JVMS §5.4.3.2 resolves by name **and** descriptor.
///
/// Resolution applies the full key since 2026-08-27, and raises
/// `NoSuchFieldError` when the pair is absent since 2026-08-28
/// (`CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1` restores the old answer). So a
/// disagreement can no longer arise from the resolver, and this guard should
/// count zero forever. It is kept as the tripwire for that: a non-zero here
/// now means the descriptor key was NOT applied on some path — the lenient
/// lever is armed, or a caller reached `locate_field` with `descriptor: None`
/// where it should have passed one.
///
/// For the interpreter that is almost always harmless: it reads and writes a
/// dynamically-tagged 16-byte cell, so landing on a same-named field of another
/// type produces a wrong value, not a wrong SHAPE.
///
/// For the JIT it is not harmless, because the two halves of a compiled field
/// site come from two different places. The **slot index** comes from that
/// name-only search; the **type tag** comes from the constant-pool descriptor,
/// which the search never consulted. When they name different fields the
/// compiler emits the tag of one field at the slot of the other — and a `putfield`
/// whose CP descriptor says `I` at a slot whose class declares `[C` writes a
/// `Value::Int` into a reference cell. That is the punned-cell shape exactly:
/// `SQLChar.rawData` (`[C`, slot 1) found holding `Int(1)`, dereferenced by a
/// compiled `arraylength` as the pointer `1`
/// (`known-issues/tomcat/punned-sqlchar-rawdata-cell-writer-localized-…`).
///
/// Refusing the site is the conservative answer: the method falls back to a
/// tier that reads the cell's own tag, so a disagreement costs compilation
/// rather than correctness. That was worth keeping after the resolver was
/// fixed, because it is cheap and it fails in the safe direction.
///
/// A `desc_byte` of 0 means the resolver had no descriptor to report (an empty
/// descriptor string); that is a missing observation, not a disagreement, so it
/// is admitted unchanged.
/// Snapshot of [`JIT_FIELD_TAG_DISAGREEMENTS`], for the end-of-run report.
pub fn jit_field_tag_disagreements() -> u64 {
    JIT_FIELD_TAG_DISAGREEMENTS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(in crate::runtime::interpreter) fn jit_field_tag_agrees(field: &ResolvedField, cp_tag: u8, cp_idx: u16) -> bool {
    if field.desc_byte == 0 || field.desc_byte == cp_tag {
        return true;
    }
    let n = JIT_FIELD_TAG_DISAGREEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 32 {
        tracing::warn!(
            target: "cratonvm::jit",
            cp_index = cp_idx,
            declaring_class_id = field.declaring_class_id.as_u32(),
            field_index = field.field_index,
            resolved_desc = %(field.desc_byte as char),
            cp_desc = %(cp_tag as char),
            "refusing a compiled field site: name-only field resolution found a field whose descriptor differs from the one the constant pool names, so the site's slot index and its type tag describe different fields",
        );
    }
    false
}

/// JVMS §6.5 linking check for a compiled field site: `getfield`/`putfield`
/// must resolve to an instance field and `getstatic`/`putstatic` to a static
/// one (`expect_static`). Field resolution answers the same `ResolvedField`
/// whichever opcode asked, so before this every compile door baked a static
/// field's index as an instance slot (and the reverse) -- a silent wrong-slot
/// access. A mismatch refuses the site; the method then runs in the
/// interpreter, whose `check_field_staticness` throws HotSpot's
/// `IncompatibleClassChangeError`. Only broken bytecode reaches the refusal.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn jit_field_kind_agrees(field: &ResolvedField, expect_static: bool) -> bool {
    field.is_static == expect_static
}

/// Round 15 wave 3 (lane chain3, CHN15-2 producer): the declaring class id
/// the IR alias model may take as `field`'s identity (`field` resolved from
/// a constant-pool entry of `referencing`), or `None` when that answer is not
/// certain to be the one every other ask of the same entry gets.
///
/// Two different declaring classes let `ir::Graph::may_alias` call two field
/// accesses disjoint, so a class a later ask of the SAME entry could answer
/// differently is a miscompile, not a lost optimization. Only a class of a
/// BUILT-IN loader is answered: its field owners come from the flat store
/// (built-in classes only) and then the per-site resolution cache, which is
/// sticky. A user-defined loader's owner comes from its loader-local lookup
/// (`lookup_loader_initiated`), which can move while a compile runs: from the
/// flat store's class to the loader's own for a JDK-global name it defines
/// (`is_user_loader_global_name`), from an initiating memo's copy to the
/// loader's private definition for an isolated URL loader (the interpreter
/// asks `lookup_loader_defined_exact` there). Such a site keeps the
/// index-only rule. A static field never has an identity.
///
/// Round 15 wave 4 (lane fieldowner, CH3-2, default on
/// `CRATONVM_JIT_FIELD_IDENTITY_USER_LOADERS`): a user loader's site is
/// answered when the owner its `Fieldref` names is a class that loader
/// DEFINED (`lookup_loader_defined_exact`: a loader defines a name once, and
/// every lookup of the name through it answers that definition first) and the
/// resolved field is that owner's own or an ancestor's.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_field_decl_identity(
    shared: &SharedVm,
    referencing: ClassId,
    cp_idx: u16,
    field: &ResolvedField,
) -> Option<u32> {
    if field.is_static {
        return None;
    }
    let loader = shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing)?;
    if loader.is_builtin() {
        return Some(field.declaring_class_id.as_u32());
    }
    let owner_name = jit_field_owner_name(shared, referencing, cp_idx)?;
    let owner = super::constants::lookup_loader_defined_exact(shared, referencing, &owner_name)?;
    let cm = shared.classes.class_manager.read();
    (cm.get_loader_id(owner) == Some(loader) && jit_field_owner_covers(&cm, owner, field))
        .then_some(field.declaring_class_id.as_u32())
}



/// `CRATONVM_JIT_FIELD_SITE_FAITHFUL` (default on; `0` / `false` / `off` /
/// `no` turns it off): [`jit_field_owner_refusal`] and the spliced bodies'
/// descriptor-tag check (round 15 wave 4, lane fieldowner). Read at compile
/// time, and only for a site the check can concern.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_field_site_faithful_enabled() -> bool {
    true
}





/// The class name a `Fieldref` of `referencing`'s pool names, or `None` for
/// an entry that is not one.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_field_owner_name(shared: &SharedVm, referencing: ClassId, cp_idx: u16) -> Option<String> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(referencing)?;
    match class.constant_pool.get(cp_idx) {
        Some(ConstantPoolEntry::FieldReference { class_index, .. }) => class
            .constant_pool
            .get_class_name(*class_index)
            .map(|name| name.to_string()),
        _ => None,
    }
}

/// Does a field site whose owner resolves to `owner` get `field`? The
/// interpreter's own test of a cached answer (`cache_matches_owner` in
/// `resolve_field_ref_loader_aware`): `field` is declared by `owner` or by one
/// of its supertypes.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_field_owner_covers(
    cm: &crate::classloading::ClassManager,
    owner: ClassId,
    field: &ResolvedField,
) -> bool {
    field.declaring_class_id == owner || cm.is_subclass_of(owner, field.declaring_class_id)
}

/// Why a compiled field site of `referencing`'s pool entry `cp_idx`, whose
/// threadless resolution answered `field`, must not be compiled, or `None`
/// when the interpreter resolves the same entry to the same field (round 15
/// wave 4, lane fieldowner; `r15w3-chain3-jit-field-owner-skips-loader-
/// faithful-answer-20260930.md`). A refused site is left to the interpreter,
/// whose loader-aware resolver drives the loader; a baked index or static
/// storage of another owner is a silent wrong-slot read or write.
///
/// Only a user-defined referencing loader can differ (a built-in loader's
/// owners come from the flat store on both sides):
///
/// * A JDK-global name the loader may define (`javax/`, `jdk/`, `sun/`,
///   `com/sun/`), `--real-jdk`: the interpreter asks the loader
///   (`drive_loader_for_global_name`), the threadless core answers the flat
///   store's JDK class until the loader has answered. Wave 37's rule
///   ([`loader_faithful_global_name_answer`], the one `new` and statically
///   bound call owners take): `None` defers, and a recommended owner must be
///   the one the field was found through.
/// * An isolated-shaped loader (its recorded delegation chain ends at the
///   platform or bootstrap loader, a superset of
///   `is_isolated_url_loader_definition`, which needs a thread): the
///   interpreter takes only the loader's own DEFINITION
///   (`lookup_loader_defined_exact`) and otherwise drives it; the threadless
///   core also takes the initiating memo, a cached entry or the flat store.
///   The owner must be that definition; with none yet, a field of an
///   application-loader class is refused (the stale application copy the
///   interpreter's rule exists for; such a loader cannot delegate to it).
/// * Any other user loader outside loader-initiated resolution: the
///   interpreter asks `lookup_loader_initiated` first, the threadless core the
///   cached entry or the flat store. Inside it both ask the same lookup.
pub(in crate::runtime::interpreter) fn jit_field_owner_refusal(
    shared: &SharedVm,
    referencing: ClassId,
    cp_idx: u16,
    field: &ResolvedField,
) -> Option<&'static str> {
    use cratonvm_types::ClassLoaderId;
    let loader = shared
        .classes
        .class_manager
        .read()
        .get_loader_id(referencing);
    let Some(ClassLoaderId::UserDefined(ns)) = loader else {
        return None;
    };
    if !jit_field_site_faithful_enabled() {
        return None;
    }
    let owner_name = jit_field_owner_name(shared, referencing, cp_idx)?;
    if owner_name.starts_with('[') || owner_name.starts_with("java/") {
        return None;
    }
    if super::constants::is_user_loader_global_name(&owner_name) {
        // The interpreter's own predicate for asking the loader
        // (`drive_loader_for_global_name`): `--real-jdk`, and `--compatible`
        // with the `global-name-ask` row, which is on by default there since
        // interpreter round i1 wave 49. This used to ask `is_jdk_only()`
        // ("`--compatible`'s interpreter answers the global route too", true
        // only before wave 49), so under `--compatible` a cold `getstatic` of
        // a child-first loader's own `javax/` class compiled against the
        // JDK's field while the interpreter asked the loader (interpreter
        // round i4 wave 93, lane L2; `L2W93JdkNameColdGetstatic`). The `new`
        // and call-owner doors already asked this predicate
        // (`jit_user_loader_global_name_change`).
        // `CRATONVM_JIT_FIELD_OWNER_ASK_PREDICATE=0` restores the old gate.
        let asks = {
            super::constants::loader_global_name_ask_enabled(shared, &owner_name)
        };
        if !asks {
            return None;
        }
        let cm = shared.classes.class_manager.read();
        let Some(flat) = jit_known_class(shared, &cm, &owner_name, referencing) else {
            return Some("field-owner-deferred");
        };
        let answer =
            loader_faithful_global_name_answer(shared, &cm, referencing, &owner_name, flat);
        return match answer {
            None => Some("field-owner-deferred"),
            Some(owner) if jit_field_owner_covers(&cm, owner, field) => None,
            Some(_) => Some("field-owner-rebound"),
        };
    }
    let isolated_shape = matches!(
        shared.loader_identity.user_loader_parents().builtin_parent(ns),
        Some(terminal) if terminal < ClassLoaderId::NATIVE_APPLICATION
    );
    // No class-manager guard is held across these lookups: each takes its own.
    let owner = if isolated_shape {
        super::constants::lookup_loader_defined_exact(shared, referencing, &owner_name)
    } else if super::constants::should_use_loader_initiated_resolution(shared, referencing) {
        return None;
    } else {
        super::constants::lookup_loader_initiated(shared, referencing, &owner_name)
    };
    // Round 15 wave 5 (lane fieldowner2), FO4-2's admission: the site's own
    // resolution-cache entry is what the interpreter's isolated path answers
    // (`resolve_field_ref_loader_aware`), and only the loader-aware resolver
    // (or the loader's own definition) fills it. Computed before the
    // class-manager guard: the probe takes the resolution cache's lock.
    let sticky_entry = isolated_shape
        && owner.is_none()
        && jit_field_site_is_sticky(shared, referencing, cp_idx, field);
    let cm = shared.classes.class_manager.read();
    match owner {
        Some(owner) if jit_field_owner_covers(&cm, owner, field) => None,
        Some(_) => Some("field-owner-not-the-loaders"),
        None if isolated_shape
            && !sticky_entry
            && cm.get_loader_id(field.declaring_class_id) == Some(ClassLoaderId::Application) =>
        {
            Some("field-owner-isolated-app-copy")
        }
        None => None,
    }
}

/// Does `referencing`'s pool entry `cp_idx` have a resolution-cache entry
/// naming `field` (same declaring class and slot), with the sticky isolated
/// field sites on? Round 15 wave 5, lane fieldowner2
/// (`r15w5-fieldowner2-jit-field-owner-sticky-admission-patch-FIXED-20260930.md`).
pub(in crate::runtime::interpreter::jit_bridge) fn jit_field_site_is_sticky(
    shared: &SharedVm,
    referencing: ClassId,
    cp_idx: u16,
    field: &ResolvedField,
) -> bool {
    if !super::field_access::isolated_field_site_sticky_enabled() {
        return false;
    }
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    match resolver.probe_field_ref(resolver.scope(referencing), cp_idx) {
        crate::runtime::resolve::CacheProbe::Hit(hit) => resolver.adopt(hit).is_ok_and(|hit| {
            hit.declaring_class_id == field.declaring_class_id
                && hit.field_index == field.field_index
                && hit.is_static == field.is_static
        }),
        crate::runtime::resolve::CacheProbe::Miss => false,
    }
}

/// Is this `getfield` (`op` is `0xb4`) a load of `java.lang.ref.Reference.referent`
/// (SBR4 satb)? HotSpot's `Reference.get()` is an intrinsic that loads the
/// referent through an SATB keep-alive, and compiled code's INLINE field loads
/// have none: the compact arm, and the legacy-cell arm that serves a receiver
/// allocated without `GC_FLAG_COMPACT` (the TLAB fast path allocates every
/// object that way, so most `Reference`s). The only compiled load that carries
/// the keep-alive is the checked `jit_getfield` helper, so a compile door
/// treats such a site as one it cannot describe: the resolver answers the
/// marker tag [`REFERENT_SITE_TAG`], the compiler emits no field row for the
/// pc (single-pass arm 4: a plain `jit_getfield(vm, obj, 0)` call; the
/// optimizing tier declines the method to single-pass; a splice or an OSR body
/// is refused). The slot is 0 for every `Reference`, which is the index arm 4
/// passes.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_load_of_referent(shared: &SharedVm, op: Option<u8>, field: &ResolvedField) -> bool {
    op == Some(0xb4)
        && field.field_index == 0
        && crate::memory::ref_discovery::is_referent_field(
            shared,
            field.declaring_class_id,
            field.field_index,
        )
}

/// `CRATONVM_JIT_IR_REFERENT_SPLICE` (JIT r20 wave 18, lane xLib, THROW20-4 of
/// `jit-r20-throw-proposals.md`; perf, default ON, kill "0"). Opt-in during w20, when
/// `R20Rev16ReferentSpin` never saw the referent cleared with the splice on: the
/// referent load's dead home stayed published at the loop's back-edge poll. Fixed by
/// the back-edge dead-referent clears in `ir_lower` (refspin, 803d036de2) and re-enabled
/// after the w21p host run (ZGC and Generational `cleared true` in both modes). Page:
/// `docs/feature-designs/deopt-osr.md`.
/// an OPTIMIZING splice may carry a [`jit_load_of_referent`] site -- the body
/// of `Reference.get()` -- as an `Op::Load` that always takes the checked
/// `jit_getfield` helper, the load that carries the SATB keep-alive
/// (`InlineSite::referent_load_pcs`: no compact row, no declaring-class
/// identity, and `ir_lower` declines every inline arm at such a pc). The
/// single-pass inliner keeps refusing the body. Read per resolution; no static.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_ir_referent_splice_enabled() -> bool {
    true
}

/// The descriptor tag a field resolver answers for a site that must stay on the
/// checked `jit_getfield` helper (see [`jit_load_of_referent`]). `0` is no real
/// descriptor; `cratonvm_jit` reads it as "no field row for this pc".
pub(in crate::runtime::interpreter::jit_bridge) const REFERENT_SITE_TAG: u8 = 0;

/// Does `code` hold a `getfield` (`0xb4`) INSTRUCTION naming constant-pool
/// entry `cp_idx`? An instruction walk, not a byte scan.
pub(in crate::runtime::interpreter::jit_bridge) fn code_has_getfield_of(code: &[u8], cp_idx: u16) -> bool {
    let mut pc = 0usize;
    while pc < code.len() {
        if code[pc] == 0xb4 {
            if let (Some(&hi), Some(&lo)) = (code.get(pc + 1), code.get(pc + 2)) {
                if u16::from_be_bytes([hi, lo]) == cp_idx {
                    return true;
                }
            }
        }
        let len = cratonvm_jit::bytecode_insn_len(code, pc);
        if len == 0 {
            return true; // cannot walk it: refuse rather than miss a site
        }
        pc += len;
    }
    false
}

/// A compiled field site's resolution of `referencing`'s pool entry `cp_idx`:
/// the resolution core through `MemberResolver` (the entry point the resolve
/// guard enforces), then [`jit_field_owner_refusal`]. `Err(None)`: the entry
/// does not resolve without the interpreter; `Err(Some(why))`: it resolves to
/// a field the interpreter would not use. Every compile door's field peek in
/// this file goes through here (round 15 wave 4, lane fieldowner).
pub(in crate::runtime::interpreter::jit_bridge) fn jit_resolve_field_site(
    shared: &SharedVm,
    referencing: ClassId,
    cp_idx: u16,
) -> Result<ResolvedField, Option<&'static str>> {
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let Ok(field) = resolver
        .field_ref(resolver.scope(referencing), cp_idx)
        .and_then(|field| resolver.adopt(field))
    else {
        return Err(None);
    };
    if let Some(why) = jit_field_owner_refusal(shared, referencing, cp_idx, &field) {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] field site refused class_id={} cp={cp_idx}: {why}",
                referencing.as_u32()
            );
        }
        return Err(Some(why));
    }
    Ok(field)
}

/// JIT round 21 wave 16 (lane perf-cf-lean, `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`):
/// the constant-pool index of the ONE reference field of `this` whose being
/// null makes `code`'s entry return at once, or `None`.
///
/// The entry is executed abstractly from pc 0 with local 0 = `this` and that
/// field assumed null, so every branch is decided: `aload*` / `astore*` /
/// `dup` / `pop` / `aconst_null`, `getfield` of `this` (always the same pool
/// index), `ifnull` / `ifnonnull` / `if_acmpeq` / `if_acmpne` / `goto` on
/// values whose answer is known (`this` is non-null and equals itself; the
/// field and `aconst_null` are null), ending in a `void` `return`. Any other
/// instruction, an unknown value at a branch, a second field, or more than
/// [`ENTRY_NULL_GUARD_MAX_STEPS`] steps answers `None`. Nothing on that path
/// can throw, call, store to the heap, lock or allocate, so skipping the call
/// when the field reads null is the callee's own execution.
///
/// `CompletableFuture.postComplete()`: `f = this; (h = f.stack) != null ||
/// (f != this && ...)` -- with `stack` null and `f == this` it returns.
pub(crate) fn entry_null_guard_field(code: &[u8], max_locals: usize) -> Option<u16> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum V {
        This,
        Null,
        Unknown,
    }
    let read_u16 = |at: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
    };
    let target = |pc: usize| -> Option<usize> {
        let off = i16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
        let t = i64::try_from(pc).ok()? + i64::from(off);
        usize::try_from(t).ok().filter(|&t| t < code.len())
    };
    let mut locals = vec![V::Unknown; max_locals.max(1)];
    locals[0] = V::This;
    let mut stack: Vec<V> = Vec::new();
    let mut field: Option<u16> = None;
    let mut pc = 0usize;
    for _ in 0..ENTRY_NULL_GUARD_MAX_STEPS {
        let op = *code.get(pc)?;
        if stack.len() > 8 {
            return None;
        }
        match op {
            0x01 => {
                stack.push(V::Null);
                pc += 1;
            }
            0x2a..=0x2d => {
                stack.push(*locals.get(usize::from(op - 0x2a))?);
                pc += 1;
            }
            0x19 => {
                stack.push(*locals.get(usize::from(*code.get(pc + 1)?))?);
                pc += 2;
            }
            0x4b..=0x4e => {
                let v = stack.pop()?;
                *locals.get_mut(usize::from(op - 0x4b))? = v;
                pc += 1;
            }
            0x3a => {
                let v = stack.pop()?;
                *locals.get_mut(usize::from(*code.get(pc + 1)?))? = v;
                pc += 2;
            }
            0x59 => {
                let v = *stack.last()?;
                stack.push(v);
                pc += 1;
            }
            0x57 => {
                stack.pop()?;
                pc += 1;
            }
            0xb4 => {
                if stack.pop()? != V::This {
                    return None;
                }
                let idx = read_u16(pc + 1)?;
                if field.is_some_and(|f| f != idx) {
                    return None;
                }
                field = Some(idx);
                stack.push(V::Null);
                pc += 3;
            }
            0xc6 | 0xc7 => {
                let is_null = match stack.pop()? {
                    V::Null => true,
                    V::This => false,
                    V::Unknown => return None,
                };
                let taken = is_null == (op == 0xc6);
                pc = if taken { target(pc)? } else { pc + 3 };
            }
            0xa5 | 0xa6 => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                if a == V::Unknown || b == V::Unknown {
                    return None;
                }
                let taken = (a == b) == (op == 0xa5);
                pc = if taken { target(pc)? } else { pc + 3 };
            }
            0xa7 => pc = target(pc)?,
            // `return` with the field read: the guard.
            0xb1 => return field,
            _ => return None,
        }
    }
    None
}

/// The step bound of [`entry_null_guard_field`]'s abstract execution.
pub(in crate::runtime::interpreter::jit_bridge) const ENTRY_NULL_GUARD_MAX_STEPS: usize = 64;

/// JIT round 21 wave 16 (lane perf-cf-lean, `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`):
/// the VM's answer to `cratonvm_jit::ir_lower::WELL_KNOWN_CALL_ENTRY_NULL_GUARD_TAG`
/// for the call `class_name.method_name descriptor` spelled by `caller`'s pool
/// with `invoke_kind` (`0` virtual, `1` special): the
/// `call_entry_null_guard_answer_word` of the target's guard field, or `0`.
///
/// Admitted only for a target every receiver at the site dispatches to, the
/// same monotone rule the optimizing tier's unguarded splice uses (private or
/// final method, final declaring class, or a final constant-pool class; an
/// `invokespecial` only of a private method of the constant-pool class
/// itself), that is not static, native, abstract, synchronized or an
/// initializer, that a registered native does not shadow (real class bytes
/// stay authoritative: a shadowed triple keeps its call), with no breakpoint
/// in it, whose entry is [`entry_null_guard_field`]'s shape, and whose field
/// resolves (loader-faithfully, `jit_resolve_field_site`) to a reference
/// instance field with a compact displacement. On an answer the target class
/// is recorded as copied bytecode (`JitCache::note_bytecode_copied`), so a
/// redefinition of it withdraws the compiled body. Never loads a class; the
/// class manager is a `try_read`.
pub(crate) fn call_entry_null_guard_answer(
    shared: &SharedVm,
    caller: ClassId,
    invoke_kind: u8,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> i64 {
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
    if !cratonvm_jit::ir_lower::ir_call_entry_null_guard_enabled()
        || caller == ClassId::new(0)
        || !matches!(invoke_kind, 0 | 1)
        || !descriptor.ends_with(")V")
        || method_name.starts_with('<')
    {
        return 0;
    }
    let (declaring_id, cp_idx) = {
        let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
            return 0;
        };
        let Some(cp_class) = jit_known_class_constant(shared, &cm, class_name, caller) else {
            return 0;
        };
        let store = cm.class_store();
        // JVMS 5.4.3.3 through the resolve facade (no raw hierarchy walk:
        // `runtime::resolve::guard`), then the declaring class's own entry.
        // Every target admitted below is receiver-bound, so the resolved
        // method is the selected one.
        let Some(declaring_id) = crate::runtime::resolve::selection::resolve_declaring(
            store,
            cp_class,
            method_name,
            descriptor,
        ) else {
            return 0;
        };
        let Some(method) = store
            .get(declaring_id)
            .and_then(|c| c.find_method(method_name, descriptor))
        else {
            return 0;
        };
        if method.is_static() || method.is_native() || method.is_abstract() || method.is_synchronized()
        {
            return 0;
        }
        let Some(declaring) = store.get(declaring_id) else {
            return 0;
        };
        if declaring.is_interface() {
            return 0;
        }
        let class_is_final = |id: ClassId| {
            store
                .get(id)
                .is_some_and(|c| c.access_flags.contains(ClassAccessFlags::FINAL))
        };
        let bound = if invoke_kind == 1 {
            method.access_flags.contains(MethodAccessFlags::PRIVATE) && declaring_id == cp_class
        } else {
            method
                .access_flags
                .intersects(MethodAccessFlags::PRIVATE | MethodAccessFlags::FINAL)
                || class_is_final(declaring_id)
                || class_is_final(cp_class)
        };
        // A private method is only ever the selected one in its own class
        // (an ancestor's private namesake is a linkage error, not a target).
        if !bound
            || (method.access_flags.contains(MethodAccessFlags::PRIVATE) && declaring_id != cp_class)
        {
            return 0;
        }
        if inline_screen_native_shadows(shared, Some(&*cm), &declaring.name, method_name, descriptor)
        {
            return 0;
        }
        let Some(code) = method.code() else {
            return 0;
        };
        let Some(cp_idx) =
            entry_null_guard_field(code.code.as_bytes(), usize::from(code.max_locals))
        else {
            return 0;
        };
        // Marked under the class-manager guard, which a redefinition's writer
        // excludes (as the elided-constructor mark above); a later refusal
        // only leaves a conservative mark (r21 wave 23, opt-in flip review).
        shared.jit.jit_cache.note_bytecode_copied(declaring_id);
        (declaring_id, cp_idx)
    };
    if super::breakpoint_bars_compiling(shared, declaring_id, method_name, descriptor) {
        return 0;
    }
    let Ok(field) = jit_resolve_field_site(shared, declaring_id, cp_idx) else {
        return 0;
    };
    if field.is_static || !matches!(field.desc_byte, b'L' | b'[') {
        return 0;
    }
    if !cratonvm_types::compact_ref_fields_enabled() {
        return 0;
    }
    let Some(layout) = cratonvm_types::class_layout(field.declaring_class_id.as_u32()) else {
        return 0;
    };
    let (Some(off), Some(true)) = (
        layout.field_disp(field.field_index),
        layout.field_is_ref(field.field_index),
    ) else {
        return 0;
    };
    let Some(disp) = u32::try_from(off).ok().filter(|&d| d > 0) else {
        return 0;
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] call-entry-null-guard ADMITTED {class_name}.{method_name}{descriptor} \
             declaring={} cp={cp_idx} disp={disp} volatile={}",
            declaring_id.as_u32(),
            field.is_volatile
        );
    }
    cratonvm_jit::ir_lower::call_entry_null_guard_answer_word(disp, field.is_volatile)
}

/// `CRATONVM_JIT_DEFERRED_FIELD_SITES` (default ON since round i4 wave 94
/// triage, x86-64 only; `=0` reverts; token
/// `deferred-field-sites`): a field site a compile door cannot resolve is
/// compiled as a deferred site (`DirectHelperTable::field_site_cp`) instead of
/// refusing the method. Read per compile. Interpreter round i4 wave 92 (the
/// method-entry door) and wave 93 (the OSR door), lane L2.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_deferred_field_sites_enabled() -> bool {
    cfg!(target_arch = "x86_64")
}

/// Bound on [`JitBridgeState::deferred_field_retier`]: an armed body whose
/// deferred site never runs keeps its row, so the map stops growing here.
pub(crate) const DEFERRED_FIELD_RETIER_CAP: usize = 1024;

/// `CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER` (default ON since the w100 triage; token
/// `deferred-field-site-retier`; interpreter round i5 wave 100, lane L2,
/// proposal A of `i94-L2-proposal-deferred-field-site-retier-and-self-patch`):
/// one more optimizing compile of a method whose optimizing task fell through
/// to a single-pass body, asked when a deferred field site of that body first
/// links (its owner is loaded by then, so the recompile's field peek finds the
/// row the first build bailed on). Performance only. Read per fell-through
/// optimizing task.
/// Default ON since interpreter round i5 w100 triage (cold deferred site
/// 19 -> 10 ns/call, control unchanged); `=0` off.
pub(crate) fn deferred_field_site_retier_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_DEFERRED_FIELD_SITE_RETIER_ROWLESS_ONLY` (default OFF; token
/// `deferred-field-site-retier-rowless-only`; interpreter round i6 wave 115,
/// lane L2, `i100-L2-proposal-fall-through-carries-its-bail-site` item 2):
/// arm [`deferred_field_site_retier_enabled`]'s re-tier only for a
/// fell-through task whose optimizing build bailed on a row-less field site
/// of the method's own code (`cratonvm_jit::IrFallThroughCause::RowlessFieldSite`),
/// so a body that fell through on anything else (an IR-refused shape, a
/// lowering refusal) no longer spends a late-bind retry on a recompile that
/// bails again. Performance only. Read per fell-through optimizing task.
pub(crate) fn deferred_field_site_retier_rowless_only_enabled() -> bool {
    false
}

/// May a fell-through optimizing task with `cause` arm the deferred-field-site
/// re-tier? Always, unless
/// [`deferred_field_site_retier_rowless_only_enabled`]; then only for a
/// row-less field-site bail. Asked only for a fell-through task (the switch
/// is read after the cheaper conjuncts).
pub(in crate::runtime::interpreter) fn deferred_field_site_retier_cause_admits(
    cause: Option<cratonvm_jit::IrFallThroughCause>,
) -> bool {
    retier_cause_admits(cause, deferred_field_site_retier_rowless_only_enabled)
}

/// [`deferred_field_site_retier_cause_admits`] with the switch read through
/// `rowless_only` (the unit tests pass a constant). The switch is read only
/// when a cause is not already a row-less site.
pub(in crate::runtime::interpreter::jit_bridge) fn retier_cause_admits(
    cause: Option<cratonvm_jit::IrFallThroughCause>,
    rowless_only: impl FnOnce() -> bool,
) -> bool {
    matches!(
        cause,
        Some(cratonvm_jit::IrFallThroughCause::RowlessFieldSite { .. })
    ) || !rowless_only()
}


/// Bit of `compile_id` in [`JitBridgeState::deferred_field_retier_filter`]:
/// `(word, mask)`.
pub(crate) fn deferred_field_retier_filter_bit(compile_id: u32) -> (usize, u64) {
    let bit = compile_id & 0xFF;
    ((bit >> 6) as usize, 1u64 << (bit & 63))
}

/// Rewrite [`JitBridgeState::deferred_field_retier_filter`] from the map's
/// keys; the caller holds the map's lock.
pub(crate) fn publish_deferred_field_retier_filter<V>(
    bridge: &JitBridgeState,
    map: &std::collections::HashMap<u32, V>,
) {
    let mut words = [0u64; 4];
    for &id in map.keys() {
        let (word, mask) = deferred_field_retier_filter_bit(id);
        words[word] |= mask;
    }
    for (cell, word) in bridge.deferred_field_retier_filter.iter().zip(words) {
        cell.store(word, std::sync::atomic::Ordering::Release);
    }
}

/// Arm the deferred-field-site re-tier (proposal A of
/// `i94-L2-proposal-deferred-field-site-retier-and-self-patch`) for `key`,
/// whose optimizing task just fell through to a single-pass body: remember
/// the PUBLISHED body's compile id, so the first deferred field site of that
/// body to link asks one more optimizing compile
/// (`jit::helpers::deferred_field_site_retier_take`). A body that is not a
/// single-pass one, or carries no compile id, is not armed. No-op while
/// [`deferred_field_site_retier_enabled`] is off. Cold: once per fell-through
/// optimizing task.
pub(in crate::runtime::interpreter) fn arm_deferred_field_site_retier(
    shared: &SharedVm,
    key: &crate::jit::tiered::MethodKey,
) {
    if !deferred_field_site_retier_enabled() {
        return;
    }
    // The class id first, the cache after: `fetch_osr_compile_inputs` takes
    // `class_manager.read()`, never under `jit_cache` (the order the
    // supersede census below the call site keeps).
    let Some((class_id, _, _)) = fetch_osr_compile_inputs(
        shared,
        key.class_id,
        &key.class_name,
        &key.method_name,
        &key.descriptor,
    ) else {
        return;
    };
    let body = {
        let jit_cache = shared.jit.jit_cache.read();
        jit_cache.get(&key.class_name, &key.method_name, &key.descriptor, class_id)
    };
    let Some(body) = body else {
        return;
    };
    if body.used_ir_backend || body.compile_id == 0 {
        return;
    }
    let bridge = &shared.jit.bridge;
    let Ok(mut map) = bridge.deferred_field_retier.lock() else {
        return;
    };
    if map.len() >= DEFERRED_FIELD_RETIER_CAP && !map.contains_key(&body.compile_id) {
        return;
    }
    map.insert(
        body.compile_id,
        (
            crate::jit::tiered::MethodKey::with_class_id(
                class_id,
                Arc::clone(&key.class_name),
                Arc::clone(&key.method_name),
                Arc::clone(&key.descriptor),
            ),
            None,
        ),
    );
    publish_deferred_field_retier_filter(bridge, &map);
    drop(map);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] deferred-field-site retier armed {}.{}{} compile_id={}",
            key.class_name, key.method_name, key.descriptor, body.compile_id
        );
    }
}

/// `CRATONVM_JIT_OSR_DEFERRED_FIELD_SITE_RETIER` (default ON, as its method-entry twin; token
/// `osr-deferred-field-site-retier`; interpreter round i5 wave 102, lane L2,
/// page `i92-L2-deferred-field-sites-only-at-the-method-entry-door`): the
/// OSR door's twin of [`deferred_field_site_retier_enabled`]. An OSR body
/// that deferred a field site ([`compile_osr_body`], wave 93) is armed under
/// its compile id; when one of its deferred sites links, its cached OSR body is
/// marked stale for the background OSR compile and one OSR compile at its
/// entry bci is asked (the EAGER19-4 late-bind path,
/// [`late_bind_osr_key_served`]'s single-pass arm), so the NEXT activation that
/// enters the loop runs a body with the field row instead of the helper call.
/// The running activation keeps its body. Needs the background compiler.
/// Performance only. Read per OSR compile that deferred a site.
pub(crate) fn osr_deferred_field_site_retier_enabled() -> bool {
    true
}

/// Arm [`osr_deferred_field_site_retier_enabled`]'s re-tier for the OSR body
/// `body` that [`compile_osr_body`] just published for `class_id`'s method at
/// `entry_pc`, having deferred at least one field site. A body of the
/// optimizing tier, or one without a compile id, is not armed. Cold: once per
/// such OSR compile.
pub(in crate::runtime::interpreter::jit_bridge) fn arm_osr_deferred_field_site_retier(
    shared: &SharedVm,
    class_id: ClassId,
    key: &crate::jit::tiered::MethodKey,
    body: &crate::jit::CompiledMethod,
    entry_pc: usize,
) {
    if !osr_deferred_field_site_retier_enabled() || !crate::runtime::env_cache::bg_compile() {
        return;
    }
    if body.used_ir_backend || body.compile_id == 0 {
        return;
    }
    let Ok(entry_bci) = u32::try_from(entry_pc) else {
        return;
    };
    let bridge = &shared.jit.bridge;
    let Ok(mut map) = bridge.deferred_field_retier.lock() else {
        return;
    };
    if map.len() >= DEFERRED_FIELD_RETIER_CAP && !map.contains_key(&body.compile_id) {
        return;
    }
    map.insert(
        body.compile_id,
        (
            crate::jit::tiered::MethodKey::with_class_id(
                class_id,
                Arc::clone(&key.class_name),
                Arc::clone(&key.method_name),
                Arc::clone(&key.descriptor),
            ),
            Some(entry_bci),
        ),
    );
    publish_deferred_field_retier_filter(bridge, &map);
    drop(map);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr deferred-field-site retier armed {}.{}{} entry_bci={entry_bci} compile_id={}",
            key.class_name, key.method_name, key.descriptor, body.compile_id
        );
    }
}

/// The take half of [`arm_osr_deferred_field_site_retier`], called by
/// `jit::helpers::deferred_field_site_retier_take` with the row it removed:
/// mark the method's cached OSR body stale for the background OSR compile
/// ([`take_late_bind_osr_stale`]) and ask one OSR compile at `entry_bci`.
/// Answers the request's verdict, `None` when nothing was asked (no
/// background compiler).
pub(crate) fn request_osr_deferred_field_site_retier(
    shared: &SharedVm,
    key: &crate::jit::tiered::MethodKey,
    entry_bci: u32,
) -> Option<crate::jit::tiered::OsrRequest> {
    if !crate::runtime::env_cache::bg_compile() {
        return None;
    }
    let bridge = &shared.jit.bridge;
    if let Ok(mut stale) = bridge.late_bind_osr_stale.lock() {
        if stale.len() < LATE_BIND_CALLEE_CAP {
            stale.insert((
                key.class_id.as_u32(),
                Arc::clone(&key.method_name),
                Arc::clone(&key.descriptor),
            ));
        }
        bridge
            .late_bind_osr_stale_count
            .store(stale.len(), std::sync::atomic::Ordering::Release);
    }
    ensure_bg_compiler_started(shared);
    let request = shared.jit.tiered_manager.request_osr_classified(key, entry_bci);
    resolve_admitted_compile_strings(shared);
    Some(request)
}



/// j27 wave 5 (lane RECORDS, RECORDS21-3): per component of a record fold
/// (`(field index, tag)`, each a field `record` declares, as
/// `CpResolvers::indy_record_fold` checked), is the field typed
/// `Ljava/lang/String;`? Empty when none is. The JIT reads a marked
/// component's cached `String.hash` before calling `Objects.hashCode`
/// (`jit/src/ir/record_fold_shortcuts.rs`).
pub(in crate::runtime::interpreter::jit_bridge) fn record_fold_string_components(
    shared: &SharedVm,
    record: ClassId,
    components: &[(u32, u8)],
) -> Vec<bool> {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(record) else {
        return Vec::new();
    };
    let marks: Vec<bool> = components
        .iter()
        .map(|&(index, _)| {
            usize::try_from(index)
                .ok()
                .and_then(|index| class.field_at_index(index))
                .is_some_and(|field| &*field.descriptor == "Ljava/lang/String;")
        })
        .collect();
    if marks.contains(&true) {
        marks
    } else {
        Vec::new()
    }
}

/// The descriptor tag of the `Fieldref` at `holder`'s pool entry `cp_idx`,
/// read from that pool alone -- nothing is resolved, loaded or recorded, which
/// is the point: a site the door's field resolution declined is compiled as a
/// deferred site and resolved at its first execution
/// (`jit::helpers::jit_field_site_cp`). `None` for an entry that is not a
/// `Fieldref`, or whose descriptor does not start with a field type's tag
/// (the refusal stays). Interpreter round i4 wave 92/93, lane L2.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_deferred_field_tag(shared: &SharedVm, holder: ClassId, cp_idx: u16) -> Option<u8> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(holder)?;
    let nat_idx = match class.constant_pool.get(cp_idx) {
        Some(ConstantPoolEntry::FieldReference {
            name_and_type_index,
            ..
        }) => *name_and_type_index,
        _ => return None,
    };
    let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
    descriptor
        .as_bytes()
        .first()
        .copied()
        .filter(|tag| b"BCDFIJSZL[".contains(tag))
}

/// Round 15 wave 4 (lane fieldowner, CH3-1): one [`jit_resolve_field_site`]
/// answer per constant-pool index for the four field asks one
/// `CpResolvers::with_compile_request` hands a compile (index / tag / compact
/// slot, static row, volatility, declaring class). They used to resolve three
/// times for three answers that agreed only because resolution is sticky; a
/// user loader's owner can move between two asks.
#[derive(Default)]
pub(in crate::runtime::interpreter::jit_bridge) struct JitFieldSiteMemo {
    pub(in crate::runtime::interpreter::jit_bridge) rows: std::cell::RefCell<std::collections::HashMap<u16, Option<ResolvedField>>>,
}

/// Resolve one `ldc` / `ldc_w` constant-pool entry for the JIT, in the pool of
/// **`holder`**.
///
/// # Why this is one function and not three copies
///
/// A `String` or `Class` constant is baked into compiled code as a SITE — the
/// pair `(holder class id, cp index)` — which `jit_ldc_string_cp` /
/// `jit_ldc_class_cp` re-read at run time. The two halves of that pair must come
/// from the same constant pool, and nothing in the types says so: the index is a
/// `u16` and the holder a `u32`, and either can be filled in from whichever
/// `ClassId` happens to be in scope.
///
/// Three copies of this resolver existed — one for the compiling method, one for
/// a callee compiled as its own artifact, one for OSR — each spelling the holder
/// out by hand a few lines below its own `cm.get_class(...)`. The callee copy
/// spelled the CALLER's `class_id` in its `String` arm while reading the
/// CALLEE's pool; its `ClassReference` arm three lines further down used the
/// callee's. Compiling `SqlClientPool.newConnection` (which inlines
/// `SqlClientConnection.<init>`) then baked `(SqlClientPool, cp#47)` for a
/// literal that lives at cp#47 of `SqlClientConnection`. cp#47 of
/// `SqlClientPool` is a `ClassReference`, so the helper raised `InternalError`
/// and `TechEmpowerTest` answered HTTP 500 — 3/3 runs under
/// `CRATONVM_BG_COMPILE=0`.
///
/// **That was the loud half.** Had the caller held a `String` at that index,
/// compiled code would have pushed the **wrong literal** and reported nothing.
///
/// One function takes one `holder` and uses it for the lookup AND for both
/// baked sites, so the halves cannot disagree again. Behaviour is otherwise
/// identical to the three copies it replaces.
///
/// `get_utf8_wide(..).is_none()` is the representability test, unchanged: a
/// lone-surrogate literal cannot be pooled on a Rust `String` and stays
/// uncompilable.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_ldc_constant_for(
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    cp_idx: u16,
) -> Option<cratonvm_jit::JitLdcConstant> {
    let class = cm.get_class(holder)?;
    match class.constant_pool.get(cp_idx)? {
        ConstantPoolEntry::Integer(v) => Some(cratonvm_jit::JitLdcConstant::Immediate {
            bits: *v as i64,
            is_float: false,
        }),
        ConstantPoolEntry::Float(v) => Some(cratonvm_jit::JitLdcConstant::Immediate {
            bits: v.to_bits() as i64,
            is_float: true,
        }),
        ConstantPoolEntry::StringReference { string_index }
            if class.constant_pool.get_utf8_wide(*string_index).is_none() =>
        {
            // The SITE, not the text: the record JVMS 5.4.3 keeps is keyed
            // `(class, cp index)`.
            class.constant_pool.get_utf8(*string_index).map(|_| {
                cratonvm_jit::JitLdcConstant::String {
                    holder_class_id: holder.as_u32(),
                    cp_idx,
                }
            })
        }
        // `ldc <Class>`: the mirror is a heap object and the target class may
        // not be loaded yet, so report the SITE and let `helpers.ldc_class_cp`
        // resolve it and fetch the mirror at run time, the way the
        // interpreter's own `ldc` handler does.
        ConstantPoolEntry::ClassReference { .. } => {
            Some(cratonvm_jit::JitLdcConstant::ClassMirror {
                holder_class_id: holder.as_u32(),
                cp_idx,
            })
        }
        // JIT r23 wave 4b, lane L2 (`CRATONVM_JIT_LDC_CONDY_SLOT`): a
        // reference-typed condy, reported as the SITE; `with_ldc_slot` turns it
        // into `DynamicSlot` once the entry is resolved and slotted. A
        // primitive condy (`ldc` of an `I`/`F` condy) keeps `None`.
        ConstantPoolEntry::Dynamic {
            name_and_type_index,
            ..
        } if ldc_condy_slot_enabled()
            && ldc_condy_is_reference(&class.constant_pool, *name_and_type_index) =>
        {
            Some(cratonvm_jit::JitLdcConstant::Dynamic {
                holder_class_id: holder.as_u32(),
                cp_idx,
            })
        }
        _ => None,
    }
}

/// JIT r23 wave 4b, lane L2 (`CRATONVM_JIT_LDC_CONDY_SLOT`, **default ON**;
/// page `r23-w2-boxing-method-ref-call-costs-microseconds-FIXED-20261021.md`): a
/// method whose `ldc` names a reference-typed `CONSTANT_Dynamic` compiles once
/// the entry is resolved, reading the recorded result from its GC-maintained
/// slot (`JitLdcConstant::DynamicSlot`). The population is the JDK-spun lambda
/// proxy whose forwarder loads its implementation handle as class data
/// (`InnerClassLambdaMetafactory.useImplMethodHandle`: a protected member of
/// another package, an `invokespecial` member, a hidden implementation
/// class): its `apply` bailed permanently with `ldc-constant-unsupported` and
/// ran `invokeExact` interpreted on every call. `0` restores that bail.
pub(crate) fn ldc_condy_slot_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_VH_THIN_RELEASE_PLAIN` -- perf, **default ON**, `0` the kill
/// switch (JIT r23 wave 6, lane L2; page
/// `r21-completablefuture-stage-cost-is-allocation-and-calls-20261018-RETIRED-20261024.md`): the
/// thin `VarHandle` write helper serves `setRelease` / `setOpaque` with the
/// plain barriered store, as both tiers' inline prefixes do (x86-64 TSO), not
/// the volatile stripe-locked, double-fenced one. Read once per VM: the
/// helper is a per-call path.
pub(crate) fn vh_thin_release_plain_enabled(shared: &SharedVm) -> bool {
    *shared.jit.bridge.vh_thin_release_plain_switch.get_or_init(|| {
        true
    })
}

/// The per-VM memo of the `Integer` (`long == false`) or `Long` class id the
/// compile-time `jit_well_known_class_id` answers, or `None` with
/// `CRATONVM_JIT_WELL_KNOWN_BOX_CLASS_MEMO=0` (correctness of the generated
/// code is unaffected either way; the memo removes a compile-timing
/// dependence: a `try_read` that lost to a class definition answered `0`,
/// and the body then baked no inline box). JIT r23 wave 6, lane L2.
pub(crate) fn well_known_box_class_memo(
    shared: &SharedVm,
    long: bool,
) -> Option<&std::sync::atomic::AtomicU32> {
    true
        .then(|| &shared.jit.bridge.well_known_box_class_ids[usize::from(long)])
}



/// `CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER` -- perf, **default ON** since JIT
/// r23 wave 6 (`0` the kill switch; opt-in in wave 5), lane L2, same page:
/// the optimizing planner does not
/// splice a body at depth 0 whose SAM call has a one-proxy receiver profile
/// and would stay a call inside the splice, when the body's own compile
/// splices that forwarder; the caller then calls the body's optimizing
/// artifact instead of paying the compiled lambda arm per execution
/// (`resolve_inline_site_from`). Read per resolution.
pub(in crate::runtime::interpreter::jit_bridge) fn keep_lambda_site_caller_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_KEEP_LOOP_SPLICE_CALLER` -- perf,
/// **default ON since j27 2026-10-28**, `=0` kills (JIT r23 wave 12, lane L5;
/// it was opt-in after it lost on the n23-w12a A/B: `W8L6SplitParts cached`
/// 4066-4481 ns on against 3216-3337 off, `full` 5237-5516 against 4402-4512;
/// page
/// `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`). The optimizing
/// planner does not splice a body at depth 0 whose in-loop virtual or
/// interface call its own artifact splices behind a class guard, when the
/// nested resolution cannot reproduce that splice
/// ([`keep_loop_splice_caller_lost_class`]). The caller then calls the
/// body's artifact once, instead of paying a compiled call per loop
/// iteration. The loop-site twin of `CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER`.
/// Read per resolution.
pub(in crate::runtime::interpreter::jit_bridge) fn keep_loop_splice_caller_enabled() -> bool {
    cratonvm_jit::r25_w7b_chain_splice::keep_loop_splice_caller_enabled()
}



/// Whether the keep-loop-splice-caller row at `ipc` is answered in the copy
/// by the inline String prefix: the switch, `ipc` among the copy's
/// `chain-prefix-kept` pcs (`prefix_pcs`), and the row's class
/// `java/lang/String` (`class_is_string`, asked last: it takes a lock).
pub(in crate::runtime::interpreter::jit_bridge) fn keep_loop_row_answered_by_string_prefix(
    prefix_pcs: &[usize],
    ipc: usize,
    class_is_string: impl FnOnce() -> bool,
) -> bool {
    prefix_pcs.contains(&ipc) && class_is_string()
}

/// `CRATONVM_JIT_IR_KEEP_LOOP_RARE_ROW` -- perf, **default ON**, `=0` kills
/// (rx27 lane bimodal; same page). The keep-loop-splice-caller rule does not
/// count as lost an artifact row at a site that runs on fewer than C2's
/// `InlineFrequencyRatio` of the body's entries
/// (`r25_w7b_chain_splice::chain_frequency_verdict`, the nested chain rule's
/// own window): the kept call is paid once per entry at most, never once per
/// loop iteration, which is the cost the rule exists to avoid. Without it
/// `BmpCharPropertyGreedy.match`'s pc 70 (`next.match` after the greedy
/// run, ~190 per mille) refused the splice whenever the callee's C2 body had
/// published first. Read per resolution.
pub(in crate::runtime::interpreter::jit_bridge) fn keep_loop_rare_row_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_NESTED_EXACT_YIELDS_TO_PROFILE` -- perf, default ON since 2026-11-09 (`=0` kills it)
/// (rx27 wave 3, lane bimodal; page
/// `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`). A nested
/// exact-receiver row (`nest-exact`: spliced only where the receiver is proven
/// exactly the constant-pool class) whose receiver evidence names another
/// class is dropped at every site, not only at a dispatch-chain site (W7-B) or
/// under the nested trap switch, so the profiled route plans the class the
/// evidence names. `regex-matches`: `Matcher.match` pc 121
/// (`parentPattern.matchRoot.match`) spliced `Pattern$Node.match` for a
/// receiver it can never prove, and every subject paid a full call into
/// `BmpCharPropertyGreedy.match`. Sound as the profiled row is: an exact class
/// test whose miss edge is the call. Read per nested row.
pub(in crate::runtime::interpreter::jit_bridge) fn nested_exact_yields_to_profile_enabled() -> bool {
    true
}

/// Whether a keep-loop-splice-caller row whose site saw `samples` receivers in
/// `invocations` entries of its body is rare (the switch above, and the chain
/// rule's frequency verdict; no invocations is no evidence, so not rare).
pub(in crate::runtime::interpreter::jit_bridge) fn keep_loop_row_is_rare(samples: u32, invocations: u32) -> bool {
    keep_loop_rare_row_enabled()
        && cratonvm_jit::r25_w7b_chain_splice::chain_frequency_verdict(samples, invocations)
            .is_err()
}

/// Whether `class_id` names the boot-loader class `name`. Takes the class
/// manager for reading, with nothing else of this subsystem held.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_class_is_builtin_named(shared: &SharedVm, class_id: u32, name: &str) -> bool {
    if class_id == 0 {
        return false;
    }
    let id = ClassId::new(class_id);
    let cm = shared.classes.class_manager.read();
    cm.get_loader_id(id).is_some_and(|l| l.is_builtin())
        && cm.class_store().get(id).is_some_and(|c| &*c.name == name)
}

/// Predict the callee's standalone guarded splice from profile and the common
/// planner. A metadata-only resolution applies the normal body screens without
/// expanding ordinary nested calls, binding compiled entries, or reading the
/// current C1/C2 artifact. A lambda forwarder's implementation is resolved only
/// for its body admission and combined size, using the same metadata-only mode.
pub(in crate::runtime::interpreter::jit_bridge) fn planned_guarded_splice_classes(
    shared: &SharedVm,
    declaring_id: ClassId,
    ipc: usize,
    target: &cratonvm_jit::InlineInvokeTarget,
    profile: Option<&cratonvm_jit::profile::MethodProfile>,
    in_loop: bool,
) -> Vec<u32> {
    let resolve = |id, class: &str, method: &str, desc: &str| {
        let receiver = ClassId::new(id);
        if shared.classes.is_lambda_proxy_class(receiver) {
            return resolve_ir_lambda_sam_site(
                shared, declaring_id, receiver, class, method, desc, None, 0, true, None,
            );
        }
        resolve_inline_site_from(
            shared,
            declaring_id,
            Some(ClassId::new(id)),
            class,
            method,
            desc,
            0,
            None,
            true,
            None,
            true,
        )
    };
    cratonvm_jit::standalone_profile_guarded_splice(target, ipc, profile, in_loop, &resolve)
        .into_iter()
        .collect()
}

/// The pure rule of [`keep_loop_splice_caller_enabled`] for one call row of
/// a depth-0 body. It returns the class the splice would lose, or `None`.
/// The standalone profile/body plan supplies `standalone_guard`. It is lost
/// when all of these hold:
/// - the call is virtual or interface (`invoke_kind` 0 or 2);
/// - it lies inside a loop of the body (`in_loop`);
/// - no nested row at that pc is guarded on that class (`nested_guards`;
///   a nest-exact row for another class, such as the constant-pool class,
///   does not count).
pub(in crate::runtime::interpreter::jit_bridge) fn keep_loop_splice_caller_lost_class(
    invoke_kind: u8,
    standalone_guard: Option<u32>,
    in_loop: bool,
    nested_guards: &[u32],
) -> Option<u32> {
    if !matches!(invoke_kind, 0 | 2) || !in_loop {
        return None;
    }
    let guard = standalone_guard.filter(|&c| c != 0)?;
    (!nested_guards.contains(&guard)).then_some(guard)
}

/// `CRATONVM_JIT_IR_NESTED_LAMBDA_SAM_GUARDED` -- perf, **default ON**, `0`
/// the kill switch (JIT r23 wave 9, lane L5; page
/// `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`). A body spliced at
/// depth 0 whose SAM call names one lambda proxy
/// ([`one_lambda_proxy_receiver`]) gets that call's forwarder spliced one
/// level down behind a class test on the proxy (a nested guarded row whose
/// miss edge is the call), instead of the whole body being kept as a call
/// (`CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER`, which still decides when the
/// nested forwarder is refused). Sound as every nested guarded row is: the
/// test is exact, the body is the proxy's own forwarder, and any other
/// receiver takes the call. Read per resolution.
pub(in crate::runtime::interpreter::jit_bridge) fn nested_lambda_sam_guarded_enabled() -> bool {
    true
}

/// The one lambda proxy a SAM call site's receiver row names: its dominant
/// class at 80 % or more (the seed-a-guarded-cache policy, with its evidence
/// floor), when that class id is in the proxy range (`0x8000_0000` and up).
/// `None` for no row, a thin or split row, or an ordinary class. JIT r23
/// wave 5 (lane L2) computed this inline for the keep-the-caller rule; wave 9
/// (lane L5) shares it with the nested guarded forwarder row.
pub(in crate::runtime::interpreter::jit_bridge) fn one_lambda_proxy_receiver(
    counts: Option<&cratonvm_jit::profile::ReceiverCounts>,
) -> Option<u32> {
    counts
        .and_then(|c| cratonvm_jit::profile::dominant_receiver(c, 80))
        .filter(|&id| id >= 0x8000_0000)
}

/// Is the `CONSTANT_Dynamic` whose `NameAndType` is `name_and_type_index`
/// reference-typed (its field descriptor a class or array type)? Only those
/// can have a GC-maintained slot; a primitive condy keeps the old refusal.
pub(in crate::runtime::interpreter::jit_bridge) fn ldc_condy_is_reference(
    pool: &cratonvm_reader::constant_pool::ConstantPool,
    name_and_type_index: u16,
) -> bool {
    pool.get_name_and_type(name_and_type_index)
        .is_some_and(|(_, desc)| desc.starts_with('L') || desc.starts_with('['))
}

/// Attach the VM's GC-maintained constant slot to a String/Class `ldc` site
/// (`crate::jit::helpers::compiled_ldc_slot_for`), so the single-pass backend
/// can read the resolved constant instead of calling its helper on every
/// execution. The helper stays the miss arm. A `0` slot (kill switch
/// `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0`, a class redefinition, an
/// unresolved site, the background worker with no slot yet) leaves the
/// plain variant, i.e. today's helper CALL.
///
/// Call with NO class-manager lock held: the slot lookup takes the
/// resolution-cache, slot-table and JNI global-ref locks.
pub(in crate::runtime::interpreter::jit_bridge) fn with_ldc_slot(
    shared: &SharedVm,
    c: Option<cratonvm_jit::JitLdcConstant>,
) -> Option<cratonvm_jit::JitLdcConstant> {
    use cratonvm_jit::JitLdcConstant as L;
    match c {
        Some(L::String {
            holder_class_id,
            cp_idx,
        }) => {
            let slot_addr = crate::jit::helpers::compiled_ldc_slot_for(
                shared,
                ClassId::new(holder_class_id),
                cp_idx,
            );
            Some(if slot_addr != 0 {
                L::StringSlot {
                    holder_class_id,
                    cp_idx,
                    slot_addr,
                }
            } else {
                L::String {
                    holder_class_id,
                    cp_idx,
                }
            })
        }
        Some(L::ClassMirror {
            holder_class_id,
            cp_idx,
        }) => {
            let slot_addr = crate::jit::helpers::compiled_ldc_slot_for(
                shared,
                ClassId::new(holder_class_id),
                cp_idx,
            );
            Some(if slot_addr != 0 {
                L::ClassMirrorSlot {
                    holder_class_id,
                    cp_idx,
                    slot_addr,
                }
            } else {
                L::ClassMirror {
                    holder_class_id,
                    cp_idx,
                }
            })
        }
        // JIT r23 wave 4b, lane L2: a slot exists only for a RESOLVED condy
        // whose result is a non-null reference (`compiled_ldc_slot_for` slots
        // `Value::Object(Some(_))` records only), so a `null` result stays
        // `Dynamic` and the method keeps declining, as before this switch.
        Some(L::Dynamic {
            holder_class_id,
            cp_idx,
        }) => {
            let slot_addr = crate::jit::helpers::compiled_ldc_slot_for(
                shared,
                ClassId::new(holder_class_id),
                cp_idx,
            );
            Some(if slot_addr != 0 {
                L::DynamicSlot {
                    holder_class_id,
                    cp_idx,
                    slot_addr,
                }
            } else {
                L::Dynamic {
                    holder_class_id,
                    cp_idx,
                }
            })
        }
        other => other,
    }
}

/// The owners [`CpResolvers::invokespecial_owner`] substituted for a site's
/// constant-pool class during one compile, keyed by the class whose constant
/// pool holds the site and the `(class, name, descriptor)` triple the jit
/// crate then hands the door's direct binder (i12-L2).
///
/// The substituted class is the DECLARING class of a private or `final`
/// method, or the JVMS §6.5 selection start of a super call. It is not the
/// class the constant pool names, so the binder must neither access-check it
/// against the caller (JVMS §5.4.4 checks the REFERENCED class, which
/// `invokespecial_owner` already resolved through [`jit_known_class`]) nor
/// look it up again by name in the caller's loader: the id recorded here is
/// the one the hierarchy walk found, which is the loader-faithful answer.
#[derive(Default)]
pub(in crate::runtime::interpreter) struct SubstitutedOwners(pub(in crate::runtime::interpreter::jit_bridge) std::cell::RefCell<Vec<SubstitutedOwner>>);

pub(in crate::runtime::interpreter::jit_bridge) struct SubstitutedOwner {
    pub(in crate::runtime::interpreter::jit_bridge) holder: ClassId,
    pub(in crate::runtime::interpreter::jit_bridge) class_name: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) method_name: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) descriptor: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) id: ClassId,
}

impl SubstitutedOwners {
    pub(in crate::runtime::interpreter::jit_bridge) fn note(&self, holder: ClassId, class_name: &str, method: &str, desc: &str, id: ClassId) {
        let mut owners = self.0.borrow_mut();
        if owners.iter().any(|o| {
            o.holder == holder
                && &*o.class_name == class_name
                && &*o.method_name == method
                && &*o.descriptor == desc
        }) {
            return;
        }
        owners.push(SubstitutedOwner {
            holder,
            class_name: class_name.into(),
            method_name: method.into(),
            descriptor: desc.into(),
            id,
        });
    }

    pub(in crate::runtime::interpreter::jit_bridge) fn owner_id(
        &self,
        holder: ClassId,
        class_name: &str,
        method: &str,
        desc: &str,
    ) -> Option<ClassId> {
        self.0
            .borrow()
            .iter()
            .find(|o| {
                o.holder == holder
                    && &*o.class_name == class_name
                    && &*o.method_name == method
                    && &*o.descriptor == desc
            })
            .map(|o| o.id)
    }
}

/// The class a direct binder binds a statically bound call to — see
/// [`direct_bind_callee_class`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct DirectBindClass {
    pub(in crate::runtime::interpreter::jit_bridge) id: ClassId,
    /// A substituted owner (see [`SubstitutedOwners`]) that `holder` could
    /// not name itself: the package half of JVMS §5.4.4 refuses it (a
    /// package-private declaring class of another package).
    pub(in crate::runtime::interpreter::jit_bridge) inaccessible_owner: bool,
}

impl DirectBindClass {
    /// Whether the resolved `method` may be bound through this class.
    ///
    /// An owner the caller may access is bound exactly as before. An
    /// inaccessible substituted owner is bound only for a `public` or
    /// `private` instance method: a public member is accessible whatever
    /// declares it (JVMS §5.4.4 checks the REFERENCED class, and
    /// `invokespecial_owner` did), and a private one is the caller's own or a
    /// nestmate's. A `protected` or package-private member of such a class
    /// keeps the dispatch helper, whose resolution applies the member rules;
    /// no substitution selects a `static` method, so a static match is not a
    /// substituted site's.
    pub(in crate::runtime::interpreter::jit_bridge) fn admits(self, method: &cratonvm_reader::method::ClassFileMethod) -> bool {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        !self.inaccessible_owner
            || (!method.is_static()
                && method
                    .access_flags
                    .intersects(MethodAccessFlags::PUBLIC | MethodAccessFlags::PRIVATE))
    }
}

/// Which loaded class a direct binder binds `callee_class.callee_method
/// callee_desc`, named for a site of `holder`, to (i12-L2).
///
/// `callee_class` is normally the constant pool's own name, resolved through
/// [`jit_known_class`] — `holder`'s loader, `holder`'s §5.4.4 access,
/// JPMS export clause included. When the jit crate substituted a declaring
/// class or selection start for it (`cp_invokespecial_owner_resolver`),
/// `substituted` holds the id that substitution found, and that id is the
/// answer: no second lookup by name (which could find another loader's copy,
/// or nothing through a user-defined loader that never initiated the
/// ancestor), and no class-constant access check, whose export trace would
/// name a class the caller never referenced. Only the package half of
/// §5.4.4 is asked, to tell the binder whether the member's own access is
/// in question ([`DirectBindClass::admits`]). `StringBuilder.length()` from a
/// class-path class is the motivating case: `StringBuilder` is `final`, the
/// body is `java.lang.AbstractStringBuilder`'s, and that class is
/// package-private.
pub(in crate::runtime::interpreter::jit_bridge) fn direct_bind_callee_class(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    substituted: &SubstitutedOwners,
    holder: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<DirectBindClass> {
    let Some(id) = substituted.owner_id(holder, callee_class, callee_method, callee_desc) else {
        return jit_known_class_constant(shared, cm, callee_class, holder).map(|id| {
            DirectBindClass {
                id,
                inaccessible_owner: false,
            }
        });
    };
    Some(DirectBindClass {
        id,
        inaccessible_owner: !substituted_owner_accessible(cm, holder, id),
    })
}

/// The package half of JVMS §5.4.4 for a substituted owner `owner` of a site
/// in `holder` (public, or the same runtime package). `false` when either
/// class is gone from the store.
pub(in crate::runtime::interpreter::jit_bridge) fn substituted_owner_accessible(
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    owner: ClassId,
) -> bool {
    if owner == holder {
        return true;
    }
    match (cm.get_class(holder), cm.get_class(owner)) {
        (Some(accessor), Some(target)) => {
            crate::runtime::resolve::MemberResolver::class_access_denial(
                accessor,
                target,
                &cm.module_registry,
            )
            .is_none()
        }
        _ => false,
    }
}

/// JIT round j27 (lane MHINDY): the source line of `bci` in `method`'s
/// `LineNumberTable` (every such attribute of its `Code`, the entry with the
/// largest `start_pc <= bci`, as `stackwalker::line_number_for_bci` reads
/// it), or `None` without one. For
/// [`CpResolvers::mh_constant_filter_window_is_one_line`].
pub(in crate::runtime::interpreter::jit_bridge) fn mh_filter_line_at(method: &cratonvm_reader::method::ClassFileMethod, bci: usize) -> Option<u16> {
    use cratonvm_reader::attribute::Attribute;
    let mut best: Option<(u16, u16)> = None;
    for attribute in method
        .code()
        .map(|code| &code.attributes[..])
        .unwrap_or_default()
    {
        if let Attribute::LineNumberTable(entries) = attribute {
            for e in entries {
                if usize::from(e.start_pc) <= bci && best.is_none_or(|(start, _)| e.start_pc >= start)
                {
                    best = Some((e.start_pc, e.line_number));
                }
            }
        }
    }
    best.map(|(_, line)| line)
}

/// The constant-pool resolvers a compile request carries, for ONE class's pool.
///
/// The three compile doors — [`compile_optimizing_artifact`], its nested
/// callee compile, and [`try_jit_compile_callee_slow`] — each used to spell
/// out these thirteen closures, ~350 lines apiece, and they had started to drift
/// (only the slow path recorded a refusal for a non-`Fieldref` field site, and
/// a JVMS field-kind change had to be made six times in wave 2). Each door now
/// builds one of these and hands `try_compile` one-line closures over it; the
/// closures that genuinely differ per door (receiver inlining, CHA, the
/// class-id namer, the direct binders) stay with their doors.
///
/// Every method is the body its closures had, with the class id, method name
/// and bytecode taken from here (interpreter round i1, wave 3, L7).
pub(in crate::runtime::interpreter::jit_bridge) struct CpResolvers<'a> {
    pub(in crate::runtime::interpreter::jit_bridge) shared: &'a SharedVm,
    /// The class whose constant pool — and whose bytecode — is being compiled:
    /// the calling class for every site the scan finds.
    pub(in crate::runtime::interpreter::jit_bridge) class_id: ClassId,
    /// Every owner [`Self::invokespecial_owner`] substituted for a site's
    /// constant-pool class during this compile, for the door's direct binder
    /// ([`direct_bind_callee_class`]).
    pub(in crate::runtime::interpreter::jit_bridge) substituted_owners: SubstitutedOwners,
    /// The compiled method's name and (padded) bytecode, for the
    /// illegal-`final`-put check.
    pub(in crate::runtime::interpreter::jit_bridge) method_name: &'a str,
    /// The compiled method's descriptor: with `method_name` and a bci it
    /// names an `invokedynamic` instruction for [`Self::indy`].
    pub(in crate::runtime::interpreter::jit_bridge) method_descriptor: &'a str,
    pub(in crate::runtime::interpreter::jit_bridge) code: &'a [u8],
    /// `CRATONVM_DBG_JIT_LDC` tag for `ldc2_w` answers; `None` for the door
    /// that never printed them.
    pub(in crate::runtime::interpreter::jit_bridge) ldc2w_trace: Option<&'static str>,
    /// The constant-pool operands of `code`'s `invokevirtual`,
    /// `invokespecial`, `invokestatic` and `invokeinterface` instructions, as
    /// sorted `(operand, opcode)` pairs,
    /// collected on the first question ([`Self::names_an_invoke_of`]).
    pub(in crate::runtime::interpreter::jit_bridge) invoke_operands: std::cell::OnceCell<Box<[(u16, u8)]>>,
    /// al16 (2026-10-05): every armed first-call cell [`Self::first_call`]
    /// handed this compile, for [`note_first_call_waiters`].
    pub(in crate::runtime::interpreter::jit_bridge) first_calls_seen: std::cell::RefCell<Vec<usize>>,
    /// JIT round 21, lane tcshape: every constant-pool index
    /// [`Self::deferred_field_tag`] compiled as a deferred field site, for
    /// [`note_deferred_field_waiters`].
    pub(in crate::runtime::interpreter::jit_bridge) deferred_fields_seen: std::cell::RefCell<Vec<u16>>,
}

