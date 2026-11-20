#[allow(unused_imports)]
use super::*;

/// gc-common w29-e: drop every row this module filed under a weak lock key the
/// registry has just FREED -- its object died (`lib.rs::sweep_lock_keys`,
/// after every collection) or its VM went (`lib.rs::forget_vm_lock_keys`):
/// the `Constructor` side rows and the `AnnotatedType` TYPE_USE rows (gc-common
/// w31-a's defined-`Package` rows moved out in i5 wave 105). `keys`
/// holds every weak key the sweep freed, of every kind; most name no row here.
/// A freed key is never minted again, so nothing could read these rows any
/// more. One table lock at a time, never under the lock-key registry's lock
/// (both callers release it first); nothing here reaches Java. Answers the
/// rows dropped.
pub fn forget_lang_class_keys(keys: &[usize]) -> usize {
    if keys.is_empty() {
        return 0;
    }
    // addr-keyed: a parameter over lock-key registry keys (never re-minted), not addresses.
    fn drop_keys<V>(table: &mut FxHashMap<usize, V>, keys: &[usize]) -> usize {
        if table.is_empty() {
            return 0;
        }
        let before = table.len();
        if table.len() < keys.len() {
            // addr-keyed: not an address -- freed lock-key registry keys, local.
            let set: rustc_hash::FxHashSet<usize> = keys.iter().copied().collect();
            table.retain(|k, _| !set.contains(k));
        } else {
            for k in keys {
                table.remove(k);
            }
        }
        before - table.len()
    }
    let ctors = {
        let mut t = constructor_mirror_side_table()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        drop_keys(&mut *t, keys)
    };
    let anns = {
        let mut t = annotated_parameterized_type_arg_anns_table()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        drop_keys(&mut *t, keys)
    };
    // (gc-common w31-a's defined-`Package` rows of a dead user loader are no
    // longer dropped here: the memo is a field of the VM's
    // `LoaderIdentityTables` since interpreter round i5 wave 105, which this
    // VM-less sweep cannot reach. The VM calls
    // [`forget_dead_loader_defined_packages`] after each of its sweeps.)
    ctors + anns
}

/// Drop every per-VM row this module keeps in a process-wide table (gc-common
/// w21-d): the `Constructor` side rows, the `AnnotatedType` TYPE_USE rows
/// (gc-common w29-e) and the
/// `Method.getTypeParameters()` memo (whose objects stop being reported with
/// their rows, gc-common w30-b / w31-a).
/// Called from `forget_vm_native_root_stores` at VM teardown. The first two
/// are also dropped through their freed weak
/// keys ([`forget_lang_class_keys`], from `lib.rs::forget_vm_lock_keys`); this
/// catches a row whichever runs first.
pub fn forget_vm_lang_class_tables(vm: usize) {
    constructor_mirror_side_table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, row| row.vm != vm);
    annotated_parameterized_type_arg_anns_table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|_, row| row.0 != vm);
    // (The defined-`Package` memo dies with the VM's `LoaderIdentityTables`
    // since interpreter round i5 wave 105; a detached instance is emptied by
    // `classloader::forget_vm_loader_singletons`.)
    method_type_params_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(owner, _, _, _), _| *owner != vm);
    {
        let mut t = ctor_plan_table().lock();
        t.ids.retain(|(owner, _, _), _| *owner != vm);
        t.plans.retain(|_, p| p.vm != vm);
    }
    // JIT round 21 (lane manifest): the class-path `Manifest` handles `jdk_manifest_and_url` keeps.
    JDK_CLASS_PATH_MANIFESTS.forget(vm);
}

/// SBR2 wave 8 (lane ctorplan): the per-`Constructor` plan.
///
/// Everything `Constructor.newInstance` derived on every call from the
/// constructor OBJECT -- the declaring class's id and name, the `<init>`
/// descriptor (through a lock-key registry lookup, a side-table lock and, for a
/// `copy()`, a walk to its root), the parsed parameter list, the
/// abstract/interface refusal -- is a function of (VM, declaring class,
/// descriptor) alone. A plan is that function's value, interned once per
/// (VM, class id, descriptor) when the constructor is minted
/// ([`create_constructor_object`]), and named by the constructor's `slot` field:
/// HotSpot's `slot` is the index of the method in the class, which no Java code
/// reads (only its natives do), and this VM's minted constructors carried a
/// constant `0` there. `Constructor.copy()` (the JDK's, real bytecode) copies
/// `slot`, so the root, every `getConstructor()` copy and `Class.newInstance`'s
/// cached copy share one plan without an identity key, which is what the weak
/// lock-key side table needed a registry lookup (and, on an identity-hash
/// collision, a bug) for. A constructor with no plan (`slot` 0: the lambda
/// proxy's, a serialization constructor, a hand-built one) takes the old path,
/// byte for byte.
///
/// The access decision is NOT part of a plan: it depends on the caller and on
/// the constructor's `override` flag, both read per call.
pub struct CtorPlan {
    pub vm: usize,
    pub class_id: ClassId,
    pub class_name: Arc<str>,
    pub descriptor: Arc<str>,
    pub param_descs: Arc<Vec<String>>,
    pub modifiers: i32,
    /// The declaring class's access flags (`class_access_flags`, a
    /// class-manager read the body made twice per call: the enum refusal and
    /// the abstract / interface refusal). A loaded class's flags never
    /// change. Kept by the per-VM plans only
    /// (`CRATONVM_REFLECT_CTOR_PLAN_PER_VM`, interpreter round i3 wave 75,
    /// lane L3); `None` in the process table, whose plans read them per call
    /// as before.
    pub class_flags: Option<u16>,
}

/// Interpreter round i3 wave 75, lane L3 (`CRATONVM_REFLECT_CTOR_PLAN_PER_VM`;
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// this VM's [`CtorPlan`]s, kept in its reflect-door caches
/// ([`NativeContext::reflect_door_cache_slot`]) beside the `Method.invoke`
/// plans instead of the process table [`ctor_plan_table`]. Interned at mint
/// time under the write lock, read per call under the read lock (the process
/// table took its mutex per call). Ids are `>= 1`, indices + 1, and a row
/// whose class name or modifiers differ from the constructor being minted (a
/// class id reused after an unload) is replaced, never shared -- the process
/// table answered such a row's id.
pub struct CtorPlans {
    pub table: cratonvm_types::lock_order::OrderedPlRwLock<CtorPlansTable>,
}

#[derive(Default)]
pub struct CtorPlansTable {
    /// (declaring class id, descriptor) -> plan id (index + 1).
    pub ids: FxHashMap<(u32, Arc<str>), u32>,
    pub plans: Vec<Arc<CtorPlan>>,
}

impl CtorPlans {
    /// Plans kept; a mint past it gets no plan (the by-name path).
    pub const CAP: usize = 1 << 20;

    pub fn new() -> Self {
        CtorPlans {
            table: cratonvm_types::lock_order::OrderedPlRwLock::new(
                CtorPlansTable::default(),
                cratonvm_types::lock_order::LockLevel::Scratch,
            ),
        }
    }

    /// The id of `(class_id, descriptor)`'s plan when one exists for a class
    /// named `class_name` with constructor `modifiers`.
    pub fn existing(&self, key: &(u32, Arc<str>), class_name: &str, modifiers: i32) -> Option<u32> {
        let t = self.table.read();
        let id = *t.ids.get(key)?;
        t.plans
            .get(id as usize - 1)
            .filter(|p| *p.class_name == *class_name && p.modifiers == modifiers)
            .map(|_| id)
    }

    /// The `slot` value of `meta`'s constructor: its plan's id, made on first
    /// use; `0` when the declaring class cannot be named or the table is
    /// full. Every class-manager read happens before a lock is taken.
    pub fn intern(&self, ctx: &dyn NativeContext, meta: &MethodMetadata) -> i32 {
        let cid = meta.declaring_class_id;
        let Some(class_name) = ctx.class_name_arc_of_id(cid) else {
            return 0;
        };
        // The `Constructor.modifiers` word `create_constructor_object` writes.
        let modifiers = meta.access_flags as i32 & RECOGNIZED_METHOD_MODIFIERS;
        let key: (u32, Arc<str>) = (cid.as_u32(), Arc::from(meta.descriptor.as_str()));
        if let Some(id) = self.existing(&key, &class_name, modifiers) {
            return id as i32;
        }
        let class_flags = ctx.class_access_flags(cid);
        let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
        let plan = Arc::new(CtorPlan {
            vm: ctx.vm_identity(),
            class_id: cid,
            class_name: Arc::clone(&class_name),
            descriptor: Arc::clone(&key.1),
            param_descs: Arc::new(param_descs),
            modifiers,
            class_flags: Some(class_flags),
        });
        let mut t = self.table.write();
        if let Some(&id) = t.ids.get(&key) {
            if t.plans
                .get(id as usize - 1)
                .is_some_and(|p| *p.class_name == *class_name && p.modifiers == modifiers)
            {
                return id as i32;
            }
        }
        if t.plans.len() >= Self::CAP {
            return 0;
        }
        t.plans.push(plan);
        let id = t.plans.len() as u32;
        t.ids.insert(key, id);
        id as i32
    }

    /// The plan named by a `slot` value, if it names one.
    pub fn get(&self, id: i32) -> Option<Arc<CtorPlan>> {
        if id <= 0 {
            return None;
        }
        self.table.read().plans.get(id as usize - 1).cloned()
    }
}

/// This VM's [`CtorPlans`]; `None` for a context with no cache slot.
pub fn ctor_plans(ctx: &dyn NativeContext) -> Option<&CtorPlans> {
    reflect_door_caches(ctx).map(|caches| &caches.ctor_plans)
}

#[derive(Default)]
pub struct CtorPlanTable {
    // addr-keyed: not a heap address -- (VM identity, class id, descriptor); dropped per VM by forget_vm_lang_class_tables.
    pub ids: FxHashMap<(usize, u32, Arc<str>), u32>,
    pub plans: FxHashMap<u32, Arc<CtorPlan>>,
    pub next: u32,
}

pub fn ctor_plan_table() -> &'static cratonvm_types::lock_order::OrderedPlMutex<CtorPlanTable> {
    static TABLE: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<CtorPlanTable>> =
        OnceLock::new();
    TABLE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            CtorPlanTable::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Intern the plan of the constructor `meta` describes and answer its `slot`
/// value (`>= 1`); `0` (no plan) when the declaring class cannot be named.
pub fn ctor_plan_slot(ctx: &dyn NativeContext, meta: &MethodMetadata) -> i32 {
    // Interpreter round i3 wave 75, lane L3: this VM's own plans.
    if ctx.reflect_ctor_plan_per_vm() {
        if let Some(plans) = ctor_plans(ctx) {
            return plans.intern(ctx, meta);
        }
    }
    let vm = ctx.vm_identity();
    let cid = meta.declaring_class_id;
    let key_desc: Arc<str> = Arc::from(meta.descriptor.as_str());
    {
        let t = ctor_plan_table().lock();
        if let Some(id) = t.ids.get(&(vm, cid.as_u32(), Arc::clone(&key_desc))) {
            return *id as i32;
        }
    }
    let Some(class_name) = ctx.class_name_arc_of_id(cid) else {
        return 0;
    };
    let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
    let plan = Arc::new(CtorPlan {
        vm,
        class_id: cid,
        class_name,
        descriptor: Arc::clone(&key_desc),
        param_descs: Arc::new(param_descs),
        modifiers: meta.access_flags as i32 & RECOGNIZED_METHOD_MODIFIERS,
        class_flags: None,
    });
    let mut t = ctor_plan_table().lock();
    if let Some(id) = t.ids.get(&(vm, cid.as_u32(), Arc::clone(&key_desc))) {
        return *id as i32;
    }
    t.next = t.next.wrapping_add(1).max(1);
    let id = t.next;
    t.ids.insert((vm, cid.as_u32(), key_desc), id);
    t.plans.insert(id, plan);
    id as i32
}

/// The plan named by `ctor`'s `slot`, when it is this VM's.
pub fn ctor_plan_of(ctx: &dyn NativeContext, ctor: ObjectRef) -> Option<Arc<CtorPlan>> {
    ctor_plan_of_slot(ctx, ctx.get_field_by_name(ctor, "slot"))
}

/// [`ctor_plan_of`] from an already-read `slot` word (wave 76, lane L3:
/// `CRATONVM_REFLECT_DOOR_WORD_BATCH` reads it in a batch).
pub fn ctor_plan_of_slot(ctx: &dyn NativeContext, slot_word: Value) -> Option<Arc<CtorPlan>> {
    let Value::Int(slot) = slot_word else {
        return None;
    };
    if slot <= 0 {
        return None;
    }
    // Interpreter round i3 wave 75, lane L3: the same choice the mint made
    // (the switch is per VM, read once).
    if ctx.reflect_ctor_plan_per_vm() {
        if let Some(plans) = ctor_plans(ctx) {
            return plans.get(slot);
        }
    }
    let t = ctor_plan_table().lock();
    t.plans
        .get(&(slot as u32))
        .filter(|p| p.vm == ctx.vm_identity())
        .cloned()
}

/// `read_constructor_accessible` for a constructor that has a plan: the
/// accessible flag lives in `override` (what `setAccessible` writes) and the
/// extra slot `write_constructor_accessible` keeps beside it, so the side
/// table (a lock-key registry lookup per call) need not be consulted.
pub fn read_constructor_accessible_planned(ctx: &dyn NativeContext, ctor: ObjectRef) -> bool {
    read_constructor_accessible_planned_word(ctx, ctor, ctx.get_field_by_name(ctor, "override"))
}

/// [`read_constructor_accessible_planned`] from an already-read `override`
/// word (wave 76, lane L3: `CRATONVM_REFLECT_DOOR_WORD_BATCH`).
pub fn read_constructor_accessible_planned_word(
    ctx: &dyn NativeContext,
    ctor: ObjectRef,
    override_word: Value,
) -> bool {
    if let Value::Int(v) = override_word {
        if v != 0 {
            return true;
        }
    }
    let class_id = ctx.class_id_of_object(ctor);
    let idx =
        constructor_extra_base(ctx, class_id).saturating_add(CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE);
    if ctx.object_num_fields(ctor) <= idx {
        return false;
    }
    matches!(ctx.get_field(ctor, idx), Value::Int(v) if v != 0)
}

/// Never mints: a mirror never registered has no row.
pub fn peek_constructor_mirror_side(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Option<ConstructorMirrorSideMeta> {
    let key = crate::existing_weak_lock_key(ctx, obj)?;
    constructor_mirror_side_table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)
        .cloned()
}

pub fn jdk_string_constructor_reflection_rank(descriptor: &str) -> Option<usize> {
    Some(match descriptor {
        "(Ljava/lang/StringBuilder;)V" => 0,
        "([BIILjava/nio/charset/Charset;)V" => 1,
        "([BLjava/lang/String;)V" => 2,
        "([BLjava/nio/charset/Charset;)V" => 3,
        "([BII)V" => 4,
        "([B)V" => 5,
        "(Ljava/lang/StringBuffer;)V" => 6,
        "([CII)V" => 7,
        "([C)V" => 8,
        "(Ljava/lang/String;)V" => 9,
        "()V" => 10,
        "([BIILjava/lang/String;)V" => 11,
        "([BI)V" => 12,
        "([BIII)V" => 13,
        "([III)V" => 14,
        _ => return None,
    })
}

pub fn order_constructors_for_reflection(class_name: &str, constructors: &mut Vec<&MethodMetadata>) {
    if class_name == "java/lang/String" {
        // HotSpot does not expose String constructors in class-file order.
        // Spring SpEL keeps the last conversion-compatible one-arg constructor,
        // so this order makes `new String(3.0d)` choose String(String), not
        // String(byte[])/String(char[]).
        constructors.sort_by_key(|m| {
            jdk_string_constructor_reflection_rank(&m.descriptor).unwrap_or(usize::MAX)
        });
    }
}

/// Number of "extra" slots appended after the JDK Constructor layout to
/// hold CratonVM-specific metadata (not present on real JDK Constructor):
///   +0 в†’ String (raw descriptor, e.g. "(I)V")
///   +1 в†’ Int    (parameter count вЂ” cached)
///   +2 в†’ Int    (accessible flag, 0 or 1)
pub const CONSTRUCTOR_EXTRA_SLOTS: usize = 3;
pub const CONSTRUCTOR_EXTRA_OFFSET_DESC: usize = 0;
pub const CONSTRUCTOR_EXTRA_OFFSET_PARAM_COUNT: usize = 1;
pub const CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE: usize = 2;

/// Legacy synthetic Constructor width вЂ” kept as a floor so the object
/// is always large enough for the synthetic writes and so tests using
/// the MockNativeContext (which returns 0 for `class_num_total_fields`)
/// still have room.
/// Constructor mirrors share the same `mock_jdk_field_slot` synthetic
/// slot map as Method (see `test_utils.rs`), which now runs up through
/// slot 12 (`annotationDefault`) after the G2 additions
/// (`exceptionTypes`/`annotations`/`parameterAnnotations`/`annotationDefault`)
/// -- the floor must stay >= those slots so `constructor_extra_base`'s
/// `CONSTRUCTOR_EXTRA_OFFSET_*` writes don't land on and clobber a
/// still-in-use named field (e.g. floor 6 + `PARAM_COUNT` offset 1 = 7,
/// which collided with `parameterTypes`'s own mock slot 7). Mirrors the
/// identical `METHOD_NUM_FIELDS_LEGACY_FLOOR` fix.
pub const CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR: usize = 13;

pub fn constructor_extra_base(ctx: &dyn NativeContext, class_id: ClassId) -> usize {
    core::cmp::max(
        CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR,
        ctx.class_num_total_fields(class_id),
    )
}

/// C6 fix (mirrors `create_method_object`): populate real JDK-named
/// fields via `set_field_by_name` so Java bytecode `Getfield clazz` /
/// `Getfield modifiers` reads land on the correct inherited slots. Store
/// CratonVM-specific metadata in extra slots appended after the JDK
/// layout.
pub fn create_constructor_object(
    ctx: &mut dyn NativeContext,
    meta: &MethodMetadata,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // Wave 89, lane L3: `CRATONVM_DBG_GDC_PROF` phase marks (see `gdcp`).
    let mut lap = gdcp::Lap::new();
    let class_id = constructor_class_id_for_mint(ctx);

    let jdk_layout_fields = ctx.class_num_total_fields(class_id);
    let base = core::cmp::max(CONSTRUCTOR_NUM_FIELDS_LEGACY_FLOOR, jdk_layout_fields);
    let num_fields = base + CONSTRUCTOR_EXTRA_SLOTS;
    lap.mark(gdcp::CTOR_ALLOC);
    let obj = ctx.alloc_object(class_id, num_fields);
    // GC-safety (root cause of
    // wildfly-standalone-managed-server-boot-fails-under-surefire-fork.md):
    // every reflective mirror/array/string built below can trigger classloading
    // (and therefore GC) via `get_class_mirror` / `descriptor_to_class_mirror[_via_loader]`
    // / `build_mirror_array_comp` / `create_string`. A moving collection between
    // `alloc_object` and the `set_field_by_name` block below left these raw
    // `ObjectRef`s stale — per `pin_native_root`'s contract, a stale ref resolves
    // to whatever now occupies the reused slot (typically a bare
    // `java.lang.Object`, which has no `clazz` field) — so the Constructor
    // silently got a wrong/missing `clazz`, surfacing later as
    // "Constructor.newInstance: no declaring class". This was rare enough to go
    // unnoticed under light allocation pressure, but reproduced deterministically
    // under WildFly's ServiceLoader-based extension bootstrap (500+ module jars
    // loaded on the flat WF32-fix classpath), which skipped several extensions
    // (Elytron, IO, SecurityManager, the clustering ones) via exactly this path,
    // cascading into the ModelController never getting registered and a
    // `NullPointerException` ("this.controller is null") crashing server boot
    // before jboss-logmanager could even open server.log. Pin everything now and
    // re-read the forwarded reference right before use (mirrors
    // `create_method_object`, which has the identical pattern).
    let obj_pin = ctx.pin_native_root(obj);
    lap.mark(gdcp::CTOR_ALLOC_OBJ);

    let class_mirror = ctx.get_class_mirror(meta.declaring_class_id);
    let class_mirror_pin = ctx.pin_native_root(class_mirror);
    lap.mark(gdcp::CTOR_DECL_MIRROR);

    // Parse descriptor for param types
    let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
    // GC-safe: `descriptor_to_class_mirror` allocates (see `build_mirror_array`).
    // Loader-faithful (gated): a constructor on a bytecode-enhanced / child-loader
    // class reports that loader's copy of its parameter types.
    // `CRATONVM_REFLECT_CTOR_MINT_LEAN` (wave 92, lane L3; default off): the
    // `Class[]` component from this VM's reflect-door caches after the first
    // by-name answer.
    let class_comp = lean_component_id(ctx, "java/lang/Class", class_component_slot);
    let ctor_decl_cid = meta.declaring_class_id;
    let param_arr = build_mirror_array_comp(ctx, class_comp, param_descs.len(), |ctx, i| {
        Ok(descriptor_to_class_mirror_via_loader(
            ctx,
            &param_descs[i],
            ctor_decl_cid,
        ))
    })?;
    let param_arr_pin = ctx.pin_native_root(param_arr);
    lap.mark(gdcp::CTOR_PARAMS);
    // `CRATONVM_REFLECT_CTOR_LAZY_DESC_STRING` (wave 90, lane L3; default
    // off): the tail's descriptor `String` is only the fallback of
    // `read_constructor_descriptor`, which asks the side row first. With the
    // switch on it is built below only when that row could not be filed.
    let lazy_desc = reflect_ctor_lazy_desc_string_on(&*ctx);
    let desc_str = if lazy_desc {
        None
    } else {
        // `CRATONVM_REFLECT_CTOR_MINT_LEAN` (wave 92, lane L3; default off):
        // the tail slot is VM-internal (past every JDK field), so its
        // identity is unobservable and the string-table probe and insert of
        // `create_string` buy nothing; `read_constructor_descriptor` reads
        // the contents only.
        let s = if reflect_ctor_mint_lean_on(&*ctx) {
            ctx.create_string_uninterned(&meta.descriptor)
        } else {
            ctx.create_string(&meta.descriptor)
        };
        Some((s, ctx.pin_native_root(s)))
    };
    lap.mark(gdcp::CTOR_DESC);

    // WP2.1 вЂ” populate `exceptionTypes` from the JVMS В§4.7.5 `Exceptions`
    // attribute. The JDK `Constructor.getExceptionTypes()` Java method
    // does `return exceptionTypes.clone();`, which NPEs if null. CGLib's
    // Enhancer.emitConstructors calls this on every superclass constructor
    // during proxy class generation вЂ” see ReflectUtils.getExceptionTypes
    // (ReflectUtils.java:133/605).
    // Already resolved by whoever built this metadata (see
    // `MethodMetadata::exceptions`). Re-deriving it here meant an O(methods)
    // search of the declaring class's method table per mirror.
    let exception_names = meta.exceptions.clone();
    // GC-safe (see `build_mirror_array`): each `descriptor_to_class_mirror`
    // allocates/loads classes, so the array is pinned across the fill loop.
    // Loader-faithful, like the parameter types above (see
    // `create_method_object`).
    let exc_decl_cid = meta.declaring_class_id;
    let exception_arr =
        build_mirror_array_comp(ctx, class_comp, exception_names.len(), |ctx, i| {
            let desc = format!("L{};", exception_names[i]);
            Ok(descriptor_to_class_mirror_via_loader(
                ctx,
                &desc,
                exc_decl_cid,
            ))
        })?;
    let exception_arr_pin = ctx.pin_native_root(exception_arr);
    lap.mark(gdcp::CTOR_EXCEPTIONS);
    // SBR wave 5 (reflmeta): `annotations` / `parameterAnnotations`, as
    // HotSpot fills them (`member_annotation_bytes`; `None` = `null`).
    let ann_arrs = member_annotation_bytes(
        ctx,
        meta.declaring_class_id,
        &meta.name,
        &meta.descriptor,
        true,
        &[
            cratonvm_native_api::RawAnnotationAttr::Annotations,
            cratonvm_native_api::RawAnnotationAttr::ParameterAnnotations,
        ],
    );
    lap.mark(gdcp::CTOR_ANNOTATIONS);

    // Re-read every pinned local's forwarded reference now that all the
    // classloading/allocation above has settled.
    let obj = ctx.read_native_pin(obj_pin, obj);
    let class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
    let param_arr = ctx.read_native_pin(param_arr_pin, param_arr);
    let exception_arr = ctx.read_native_pin(exception_arr_pin, exception_arr);

    // --- Real JDK Constructor layout ---
    ctx.set_field_by_name(obj, "clazz", Value::Object(Some(class_mirror)));
    ctx.set_field_by_name(obj, "parameterTypes", Value::Object(Some(param_arr)));
    ctx.set_field_by_name(obj, "exceptionTypes", Value::Object(Some(exception_arr)));
    // `JVM_RECOGNIZED_METHOD_MODIFIERS` (see `create_field_object`).
    ctx.set_field_by_name(
        obj,
        "modifiers",
        Value::Int(meta.access_flags as i32 & RECOGNIZED_METHOD_MODIFIERS),
    );
    lap.mark(gdcp::CTOR_FIELDS);
    // The plan id (see `CtorPlan`); `0` when no plan could be made.
    let plan_slot = ctor_plan_slot(ctx, meta);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field_by_name(obj, "slot", Value::Int(plan_slot));
    lap.mark(gdcp::CTOR_PLAN);
    for (slot, field) in ["annotations", "parameterAnnotations"]
        .into_iter()
        .enumerate()
    {
        if let Some((a, pin)) = ann_arrs[slot] {
            let a = ctx.read_native_pin(pin, a);
            ctx.set_field_by_name(obj, field, Value::Object(Some(a)));
        }
    }

    // SBR-06: populate the JDK `signature` field from the JVMS В§4.7.9 Signature
    // attribute, mirroring `create_method_object`. `Parameter.getParameterizedType()`
    // runs JDK bytecode (`executable.getAllGenericParameterTypes()`), which checks
    // `hasGenericInformation()` (i.e. `getGenericSignature() != null`) FIRST and
    // short-circuits to the erased `parameterTypes` when the `signature` field is
    // null. The Method path already sets this; Constructors did not, so a generic
    // constructor parameter (e.g. a record's canonical `List<Foo>` component) came
    // back as raw `List` from `Parameter.getParameterizedType()` while
    // `Constructor.getGenericParameterTypes()` (a registered native) was correct.
    if let Some(sig) = meta.signature.clone() {
        let sig_obj = ctx.create_string(&sig);
        // `create_string` above can allocate/classload —
        // re-read `obj` before writing into it.
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field_by_name(obj, "signature", Value::Object(Some(sig_obj)));
    }
    lap.mark(gdcp::CTOR_SIGNATURE);

    // --- CratonVM extra metadata ---
    let obj = ctx.read_native_pin(obj_pin, obj);
    let desc_value = match desc_str {
        Some((s, pin)) => Value::Object(Some(ctx.read_native_pin(pin, s))),
        None => Value::Object(None),
    };
    ctx.set_field(obj, base + CONSTRUCTOR_EXTRA_OFFSET_DESC, desc_value);
    ctx.set_field(
        obj,
        base + CONSTRUCTOR_EXTRA_OFFSET_PARAM_COUNT,
        Value::Int(param_descs.len() as i32),
    );
    ctx.set_field(
        obj,
        base + CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE,
        Value::Int(0),
    );
    lap.mark(gdcp::CTOR_EXTRA);

    let filed =
        register_constructor_mirror_side(ctx, obj, &meta.descriptor, param_descs.len() as i32, false);
    if lazy_desc && !filed {
        // No side row: the tail slot is the descriptor's only home, as it was
        // before the switch. `create_string` may collect, so `obj` is re-read
        // through its pin; nothing allocates between the string and the store.
        // Uninterned: the tail slot is VM-internal, its identity unobservable.
        let s = ctx.create_string_uninterned(&meta.descriptor);
        let cur = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field(cur, base + CONSTRUCTOR_EXTRA_OFFSET_DESC, Value::Object(Some(s)));
    }
    lap.mark(gdcp::CTOR_SIDE);

    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

/// `CRATONVM_REFLECT_CTOR_LAZY_DESC_STRING` (interpreter round i4 wave 90,
/// lane L3; default off). See `create_constructor_object`. Read per VM into
/// its reflect-door caches since wave 92 (was a process `OnceLock`); `false`
/// for a context with no cache slot.
pub fn reflect_ctor_lazy_desc_string_on(ctx: &dyn NativeContext) -> bool {
    reflect_door_caches(ctx).is_some_and(|c| c.ctor_lazy_desc)
}

/// The `ClassId` a `Constructor` mirror is minted as: the by-name
/// `ensure_class_initialized("java/lang/reflect/Constructor")` (0 on failure),
/// as before. With `CRATONVM_REFLECT_CTOR_MIRROR_MEMO` on (the default since
/// the wave-90 triage; read per VM into its reflect-door caches), the first non-zero
/// answer is kept in this VM's reflect-door caches and later mints skip the
/// by-name resolution (the loader-faithful walk, the class-manager reads) and
/// the initialisation check. Both are stable once answered: a `java/` name is
/// never resolved through a caller's loader, and initialisation is permanent.
pub fn constructor_class_id_for_mint(ctx: &mut dyn NativeContext) -> ClassId {
    use std::sync::atomic::Ordering;
    let memo = reflect_door_caches(&*ctx).is_some_and(|c| c.ctor_mirror_memo);
    if memo {
        let cached = reflect_door_caches(&*ctx)
            .map(|c| c.ctor_class_id.load(Ordering::Acquire))
            .unwrap_or(0);
        if cached != 0 {
            return ClassId::new(cached);
        }
    }
    let class_id = ctx
        .ensure_class_initialized("java/lang/reflect/Constructor")
        .unwrap_or(ClassId::new(0));
    if memo && class_id.as_u32() != 0 {
        if let Some(c) = reflect_door_caches(&*ctx) {
            c.ctor_class_id.store(class_id.as_u32(), Ordering::Release);
        }
    }
    class_id
}

pub fn read_constructor_descriptor(
    ctx: &dyn NativeContext,
    ctor_obj: cratonvm_types::ObjectRef,
) -> Option<String> {
    if let Some(m) = peek_constructor_mirror_side(ctx, ctor_obj) {
        return Some((*m.descriptor).to_string());
    }
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    let idx = base.saturating_add(CONSTRUCTOR_EXTRA_OFFSET_DESC);
    if ctx.object_num_fields(ctor_obj) <= idx {
        // SBR wave 5 (reflmeta): a real `copy()` has no tail and no side row;
        // its `root` (the mint) names the same constructor.
        if let Some(root) = reflect_root(ctx, ctor_obj) {
            if let Some(m) = peek_constructor_mirror_side(ctx, root) {
                return Some((*m.descriptor).to_string());
            }
        }
        return None;
    }
    match ctx.get_field(ctor_obj, idx) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Build an `<init>` descriptor `(вЂ¦)V` from the JDK `Constructor.parameterTypes`
/// mirrors. Used when the CratonVM extra-slot descriptor is unreadable (C6 вЂ”
/// `class_num_total_fields` grew after stubв†’real upgrade); falling back to
/// `()V` would run only `Object.<init>` and leave subclass fields unset
/// (Surefire fork: `JUnitPlatformProvider.launcher == null`).
pub fn compose_init_descriptor_from_parameter_types(
    ctx: &dyn NativeContext,
    ctor_obj: cratonvm_types::ObjectRef,
) -> String {
    let params_arr = match ctx.get_field_by_name(ctor_obj, "parameterTypes") {
        Value::Object(Some(arr)) => arr,
        _ => return "()V".to_string(),
    };
    let len = ctx.array_length(params_arr);
    let mut out = String::with_capacity(4 + len.saturating_mul(16));
    out.push('(');
    for i in 0..len {
        if let Value::Object(Some(pm)) = ctx.get_array_element(params_arr, i) {
            out.push_str(&mirror_to_jvm_descriptor_token(ctx, pm));
        }
    }
    out.push(')');
    out.push('V');
    out
}

/// Descriptor for `Constructor.newInstance`: extra slot / side table when
/// trustworthy; otherwise reconstruct from `parameterTypes` (never guess
/// `()V` alone вЂ” see `compose_init_descriptor_from_parameter_types`).
pub fn constructor_descriptor_for_new_instance(
    ctx: &dyn NativeContext,
    ctor_obj: cratonvm_types::ObjectRef,
) -> String {
    fn looks_like_init_descriptor(d: &str) -> bool {
        let d = d.trim();
        d.starts_with('(') && d.ends_with(")V") && d.len() > 3
    }

    /// When `parameterTypes` cannot be read (JDK field layout mismatch),
    /// `compose_init_descriptor_from_parameter_types` yields `()V`. If the
    /// declaring class exposes exactly **one** public `<init>`, that must be
    /// the target of `Class.getConstructor` / Surefire's `instantiateOneArg`
    /// (e.g. `JUnitPlatformProvider(ProviderParameters)`).
    fn unique_public_init_descriptor(ctx: &dyn NativeContext, class_id: ClassId) -> Option<String> {
        let methods = ctx.declared_methods(class_id);
        let mut found: Option<String> = None;
        for m in &methods {
            if m.name != "<init>" || (m.access_flags & 0x0001) == 0 {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(m.descriptor.clone());
        }
        found
    }

    let composed = compose_init_descriptor_from_parameter_types(ctx, ctor_obj);

    if let Some(d) = read_constructor_descriptor(ctx, ctor_obj) {
        let dtrim = d.trim();
        if looks_like_init_descriptor(dtrim) {
            let (slot_params, _) = parse_descriptor_param_and_return(dtrim);
            let (comp_params, _) = parse_descriptor_param_and_return(&composed);
            if slot_params.len() == comp_params.len() {
                return dtrim.to_string();
            }
        }
    }

    if composed == "()V" {
        // `compose_вЂ¦` yields "()V" for BOTH a genuine no-arg constructor
        // (present-but-empty `parameterTypes`) and a read-FAILURE (the field
        // layout mismatch case this fallback targets). Only recover via the
        // unique public `<init>` when `parameterTypes` was actually unreadable
        // вЂ” otherwise `newInstance()` on a class's real no-arg constructor
        // wrongly adopts the parameter list of its (unique public) OTHER
        // constructor. Hibernate instantiates entities via the no-arg ctor, so
        // any entity that also declares a unique public ctor (e.g. the
        // `keymanytoone` `Card(String)`, composite-id `$PK`, enum-mapped
        // entities) failed with "wrong number of arguments: expected 1, got 0".
        let param_types_readable = matches!(
            ctx.get_field_by_name(ctor_obj, "parameterTypes"),
            Value::Object(Some(_))
        );
        if !param_types_readable {
            if let Value::Object(Some(dm)) = ctx.get_field_by_name(ctor_obj, "clazz") {
                if let Some(cid) = mirror_class_id(ctx, dm) {
                    if let Some(d) = unique_public_init_descriptor(ctx, cid) {
                        return d;
                    }
                }
            }
        }
    }

    composed
}

pub fn read_constructor_param_count(
    ctx: &dyn NativeContext,
    ctor_obj: cratonvm_types::ObjectRef,
) -> i32 {
    if let Some(m) = peek_constructor_mirror_side(ctx, ctor_obj) {
        return m.param_count;
    }
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    let idx = base.saturating_add(CONSTRUCTOR_EXTRA_OFFSET_PARAM_COUNT);
    if ctx.object_num_fields(ctor_obj) <= idx {
        // SBR wave 5 (reflmeta): a real `copy()`: the JDK `parameterTypes`.
        return match ctx.get_field_by_name(ctor_obj, "parameterTypes") {
            Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
            _ => 0,
        };
    }
    match ctx.get_field(ctor_obj, idx) {
        Value::Int(v) => v,
        _ => 0,
    }
}

pub fn read_constructor_accessible(
    ctx: &dyn NativeContext,
    ctor_obj: cratonvm_types::ObjectRef,
) -> bool {
    if let Some(m) = peek_constructor_mirror_side(ctx, ctor_obj) {
        if m.accessible {
            return true;
        }
    }
    // Also consult the JDK-standard `override` field вЂ” that's what
    // `AccessibleObject.setAccessible(boolean)` writes (registered in
    // `lib.rs::native_set_accessible_write_override`). Apache Ignite's
    // `GridUnsafe.<clinit>` calls `ctor.setAccessible(true)` on a
    // `java.nio.DirectByteBuffer` constructor and then
    // `ctor.newInstance(...)` вЂ” without this read the extra slot stays at
    // 0, the deep-reflection check denies the access, and Ignite throws
    // `IllegalAccessException: module java.base does not "opens java.nio"`
    // even though setAccessible already paid the check. (`read_method_accessible`
    // above uses the same pattern.)
    if let Value::Int(v) = ctx.get_field_by_name(ctor_obj, "override") {
        if v != 0 {
            return true;
        }
    }
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    let idx = base.saturating_add(CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE);
    if ctx.object_num_fields(ctor_obj) <= idx {
        return false;
    }
    match ctx.get_field(ctor_obj, idx) {
        Value::Int(v) => v != 0,
        _ => false,
    }
}

pub fn write_constructor_accessible(
    ctx: &mut dyn NativeContext,
    ctor_obj: cratonvm_types::ObjectRef,
    value: bool,
) {
    // Never mints: a mirror never registered has no row to update.
    if let Some(key) = crate::existing_weak_lock_key(&*ctx, ctor_obj) {
        let mut g = constructor_mirror_side_table()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(m) = g.get_mut(&key) {
            m.accessible = value;
        }
    }
    let class_id = ctx.class_id_of_object(ctor_obj);
    let base = constructor_extra_base(ctx, class_id);
    let idx = base.saturating_add(CONSTRUCTOR_EXTRA_OFFSET_ACCESSIBLE);
    if ctx.object_num_fields(ctor_obj) > idx {
        ctx.set_field(ctor_obj, idx, Value::Int(if value { 1 } else { 0 }));
    } else if method_class_has_named_layout(ctx, class_id) {
        // SBR wave 5 (reflmeta): a real `copy()` has neither a side row nor a
        // tail; its flag is the JDK `override` field, which
        // `read_constructor_accessible` consults.
        ctx.set_field_by_name(ctor_obj, "override", Value::Int(i32::from(value)));
    }
}

/// Public wrapper used by `lang_reflect::native_constructor_try_set_accessible`.
pub fn write_constructor_accessible_external(
    ctx: &mut dyn NativeContext,
    ctor_obj: cratonvm_types::ObjectRef,
    value: bool,
) {
    write_constructor_accessible(ctx, ctor_obj, value);
}

// --- Constructor getters ---

pub fn native_constructor_get_parameter_types(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `()[Ljava/lang/Class;` — an array-typed return. `get_field_by_name`
    // answers `Int(0)` for an unwritten `[…`-descriptor slot, which then
    // reaches an `arraylength`/`aaload` as a non-reference.
    Ok(Some(crate::field_read::ref_field(
        ctx,
        this,
        "parameterTypes",
    )))
}

pub fn native_constructor_get_modifiers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(ctx.get_field_by_name(this, "modifiers")))}

pub fn native_constructor_get_declaring_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(crate::field_read::ref_field(ctx, this, "clazz")))}

pub fn native_constructor_get_parameter_count(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(read_constructor_param_count(ctx, this))))}

// --- Constructor.newInstance ---

/// `Constructor.newInstance`: [`native_constructor_new_instance_body`], inside
/// a reflective call record, so a stack capture made inside the constructor
/// lists the three JDK frames HotSpot shows for the call
/// (`Constructor.newInstance`, `Constructor.newInstanceWithCaller`,
/// `DirectConstructorHandleAccessor.newInstance`;
/// `NativeExceptionAccess::enter_reflective_call`, interpreter round i1 wave
/// 43, lane L3). The record is left on every exit path.
pub fn native_constructor_new_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i6 wave 120, lane L3 (stage 2 of i46-L3): the record
    // is the row's (`lang_reflect::ReflectiveEntryRow`).
    crate::lang_reflect::reflective_entry::<ConstructorNewInstanceRow>(ctx, args)
}

/// `Constructor.newInstance`'s row (wave 120): its body inside the
/// `ConstructorNewInstance` record, the `VMObjectAlloc` event after it.
pub struct ConstructorNewInstanceRow;

impl crate::lang_reflect::ReflectiveEntryRow for ConstructorNewInstanceRow {
    const KIND: cratonvm_native_api::ReflectiveCallKind =
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance;

    fn serve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        // Interpreter round i3 wave 77, lane L3 (`CRATONVM_REFLECT_DOOR_PHASE_STATS`):
        // the door's phase marks (an off timer marks nothing). Since wave 120
        // they start inside the record and end before it is left.
        let mut ph = PhaseTimer::new(ctx.reflect_door_phase_stats());
        let floor = constructor_pin_floor(ctx, args);
        ph.mark(crate::reflect_door_phases::PH_ENTER);
        // Wave 79, lane L3: as `Method.invoke` (`rebase_compiled_stack_budget`).
        let budget =
            ctx.rebase_compiled_stack_budget(crate::lang_reflect::reflective_stack_bounds());
        let result = native_constructor_new_instance_body(ctx, args, true, &mut ph);
        ctx.restore_compiled_stack_budget(budget);
        // Interpreter round i3 wave 74, lane L3: an access refusal lists
        // `newInstanceWithCaller` -> `AccessibleObject.checkAccess` ->
        // `Reflection.newIllegalAccessException` under `Constructor.newInstance`.
        if let Err(e) = &result {
            note_executable_access_refusal_chain(ctx, true, e);
        }
        release_constructor_pins(ctx, floor);
        if ph.is_on() {
            ph.mark(crate::reflect_door_phases::PH_LEAVE);
            if result.is_ok() {
                record_door_phases(&*ctx, crate::reflect_door_phases::KIND_CTOR, &ph);
            }
        }
        result
    }

    fn after_leave(ctx: &mut dyn NativeContext, result: MethodCallResult) -> MethodCallResult {
        // Interpreter round i2 wave 63, lane L1: the JDK's
        // `DirectConstructorHandleAccessor` allocates through
        // `Unsafe.allocateInstance`, a JVMTI `VMObjectAlloc` (MEASURED on HotSpot
        // 25.0.3: a reflective constructor posts one). After the record, as
        // before wave 120.
        crate::vm_object_alloc_event(ctx, result)
    }
}

/// The pin floor of a `Constructor.newInstance` body: the `Constructor`
/// itself, pinned before the body runs, so every pin the body takes above it
/// is released however the body returns. The body has a dozen exits and
/// released its own pins on some of them only (interpreter round i2 wave 62,
/// lane L4: the `[pin-balance] LEAK` census named this native under
/// `--real-jdk`). A null `Constructor` takes no pin: the body refuses it first.
pub fn constructor_pin_floor(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<usize> {
    match args.first() {
        Some(Value::Object(Some(this))) => Some(ctx.pin_native_root(*this)),
        _ => None,
    }
}

pub fn release_constructor_pins(ctx: &mut dyn NativeContext, floor: Option<usize>) {
    if let Some(floor) = floor {
        ctx.unpin_native_roots(floor);
    }
}

/// `DirectConstructorHandleAccessor$NativeAccessor.newInstance0(Constructor,
/// Object[])`: HotSpot's `JVM_NewInstanceFromConstructor` performs NO access
/// check (the JDK's Java code in front of the accessor did it, with the true
/// caller), so the body runs with its member/module gates off. The accessor may
/// also hold an earlier copy of the constructor (the root caches it), whose
/// `override` flag says nothing about the call being made.
pub fn native_constructor_new_instance0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Wave 120 (stage 2 of i46-L3): the record is the row's.
    crate::lang_reflect::reflective_entry::<ConstructorNewInstance0Row>(ctx, args)
}

/// `DirectConstructorHandleAccessor$NativeAccessor.newInstance0`'s row (wave
/// 120): the unchecked body inside the `ConstructorNewInstance` record.
pub struct ConstructorNewInstance0Row;

impl crate::lang_reflect::ReflectiveEntryRow for ConstructorNewInstance0Row {
    const KIND: cratonvm_native_api::ReflectiveCallKind =
        cratonvm_native_api::ReflectiveCallKind::ConstructorNewInstance;

    fn serve(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let floor = constructor_pin_floor(ctx, args);
        // Wave 79, lane L3: as `Method.invoke` (`rebase_compiled_stack_budget`).
        let budget =
            ctx.rebase_compiled_stack_budget(crate::lang_reflect::reflective_stack_bounds());
        let result =
            native_constructor_new_instance_body(ctx, args, false, &mut PhaseTimer::off());
        ctx.restore_compiled_stack_budget(budget);
        release_constructor_pins(ctx, floor);
        result
    }
}

/// The words a `Constructor.newInstance` reads in one batch under
/// `CRATONVM_REFLECT_DOOR_WORD_BATCH` (interpreter round i3 wave 76, lane L3).
pub static CTOR_DOOR_WORDS: [&str; 3] = ["constructorAccessor", "slot", "override"];

/// `ph`: the door's phase marks (`CRATONVM_REFLECT_DOOR_PHASE_STATS`,
/// interpreter round i3 wave 77, lane L3; an off timer marks nothing).
pub fn native_constructor_new_instance_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    gate_access: bool,
    ph: &mut PhaseTimer,
) -> MethodCallResult {
    use crate::reflect_door_phases::{PH_ARGS, PH_CALL, PH_GATES, PH_INIT, PH_WORDS};
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Constructor.newInstance: null Constructor".to_string()),
            }
            .into())
        }
    };
    // JIT round 21 wave 13 (review rev-native item 4): see
    // `native_method_invoke_body`.
    door_notes_reflection_config_point(&*ctx, gate_access);
    let args_array_early = match args.get(1) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };
    let args_array_pin = args_array_early.map(|arr| (ctx.pin_native_root(arr), arr));

    // Serialization constructor: allocate the real target type.
    //
    // `ReflectionFactory.newConstructorForSerialization(cl)` returns a
    // `Constructor` whose declaring class (`clazz`) is `cl`'s first
    // non-Serializable ancestor (usually `java.lang.Object`), but whose
    // `constructorAccessor` is a `DirectConstructorHandleAccessor` whose
    // MethodHandle `target` is a `DirectMethodHandle$Constructor` that allocates
    // `cl` itself (its `instanceClass` field) and runs only the ancestor `<init>`.
    // Allocating `clazz` here would produce a bare `java.lang.Object`, so the JDK
    // field-setter then throws `cannot assign вЂ¦ in instance of java.lang.Object`
    // (keycloak SkeletonKeyTokenTest.testSerialization). We can't invoke the
    // MethodHandle directly (`DirectMethodHandle$Constructor.invokeExact` is
    // unimplemented), so we read `instanceClass` off the target handle, allocate
    // that type without running its own ctor, and run the ancestor `<init>`
    // (`clazz`) вЂ” exactly the serialization contract. Ordinary CratonVM reflective
    // constructors have no accessor object installed (the field holds a non-object
    // sentinel), so they fall through to the native allocation path below.
    //
    // Interpreter round i3 wave 76, lane L3 (`CRATONVM_REFLECT_DOOR_WORD_BATCH`):
    // `constructorAccessor`, `slot` and `override` in one batch. Nothing
    // between this read and the plan / accessible reads below runs Java or
    // allocates unless it returns (every accessor arm that calls out returns).
    let batched_words: Option<[Value; 3]> = if ctx.reflect_door_word_batch() {
        let mut out = [Value::Object(None); 3];
        ctx.get_fields_by_name(this, &CTOR_DOOR_WORDS, &mut out);
        Some(out)
    } else {
        None
    };
    let accessor_word = match batched_words {
        Some(words) => words[0],
        None => ctx.get_field_by_name(this, "constructorAccessor"),
    };
    if let Value::Object(Some(acc)) = accessor_word {
        // The interned name (a refcount bump), not a fresh `String` per call
        // (wave 76, lane L3; same answer).
        let acc_class = ctx.class_name_arc_of_id(ctx.class_id_of_object(acc));
        if acc_class.as_deref() == Some("jdk/internal/reflect/DirectConstructorHandleAccessor") {
            if let Value::Object(Some(target_mh)) = ctx.get_field_by_name(acc, "target") {
                if let Value::Object(Some(inst_mirror)) =
                    ctx.get_field_by_name(target_mh, "instanceClass")
                {
                    // J1 (round 15 wave 5, lane jdkonly7): a PLAIN constructor
                    // the JDK gave an accessor has this very shape with
                    // `instanceClass == clazz`; with parameters it is not a
                    // serialization constructor and goes to the generic path,
                    // which passes its arguments.
                    let inst_cid = ctx
                        .class_id_from_mirror(inst_mirror)
                        .filter(|&inst| !constructor_is_plain_with_parameters(&*ctx, this, inst));
                    if let Some(inst_cid) = inst_cid {
                        // Interpreter round i4 wave 98, lane L8: the handle's
                        // `DirectMethodHandle.allocateInstance` is
                        // `Unsafe.allocateInstance(instanceClass)`, which
                        // refuses an abstract class or an interface with
                        // `InstantiationException(<external name>)`, and
                        // `DirectConstructorHandleAccessor.newInstance` wraps
                        // that in an `InvocationTargetException`. Objenesis'
                        // `newConstructorForSerialization(Abstract.class,
                        // Object.class.getConstructor()).newInstance()` got an
                        // instance of the abstract class here (`instantiate`
                        // allocates any class id). Probe
                        // `L8W98SerialCtorShapes.java`.
                        // The refusal runs where the handle allocates: after
                        // the accessor's argument checks, inside
                        // `serialization_ctor_with_arguments` for a
                        // constructor with parameters, and below for the
                        // nullary one.
                        // Allocate by the EXACT `inst_cid` the mirror already
                        // resolved to, not by re-resolving `instanceClass`'s
                        // name (`ctx.allocate_instance(name)`, as this used to
                        // do) — the same loader-identity hazard the general
                        // `Constructor.newInstance` path below already guards
                        // against (see its comment / `new_object_initialized_
                        // with_class_id`): a name-only lookup collapses to
                        // whichever same-named class the VM's global class
                        // table associates with that name FIRST, which for a
                        // class loaded by a non-default classloader (e.g.
                        // Hibernate bytecode-enhancement's `EnhancingClassLoader`)
                        // is very often a *different*, non-enhanced definition
                        // than `inst_cid` — deserializing then returns an
                        // instance of the wrong class (same name, different
                        // shape), which silently fails `equals()`/`getClass()`
                        // identity checks against the original object even
                        // though every field value round-tripped correctly.
                        // Round 15 wave 4, lane jdkonly6
                        // (`r15w4-jdkonly5-native-allocation-skips-class-init-FIXED-20260930.md`
                        // item 3; JO5-1): the one native-instantiate funnel.
                        // HotSpot's handle allocates through
                        // `Unsafe.allocateInstance` (the class is initialized
                        // first), runs the ancestor's `<init>` with
                        // `invokespecial` semantics (`Object.<init>` returns,
                        // so a finalizable instance is registered), and
                        // `DirectConstructorHandleAccessor.newInstance` wraps
                        // whatever that threw in an `InvocationTargetException`.
                        // The arm below initialized nothing, never registered,
                        // and DISCARDED the ancestor constructor's throwable,
                        // returning the half-built object.
                        // `CRATONVM_SERIALIZATION_CTOR_FUNNEL=0` restores it;
                        // a class id the funnel cannot resolve falls through
                        // to it too.
                        //
                        // K3 (round 15 wave 6, lane jni6): an ancestor
                        // constructor WITH parameters runs with them.
                        if let Some(result) =
                            serialization_ctor_with_arguments(ctx, this, inst_cid, args_array_pin)
                        {
                            if let Some((pin, _)) = args_array_pin {
                                ctx.unpin_native_roots(pin);
                            }
                            return result;
                        }
                        // Interpreter round i4 wave 98, lane L8: the nullary
                        // serialization constructor, as
                        // `DirectConstructorHandleAccessor.newInstance` runs
                        // it: the argument count first (`wrong number of
                        // arguments: N expected: 0`; this arm ignored the
                        // arguments), then the allocation's refusal of an
                        // abstract class or interface.
                        if let Some(failure) =
                            serialization_ctor_nullary_refusal(ctx, this, inst_cid, args_array_pin)
                        {
                            if let Some((pin, _)) = args_array_pin {
                                ctx.unpin_native_roots(pin);
                            }
                            return Err(failure);
                        }
                        {
                            let anc_cid = match ctx.get_field_by_name(this, "clazz") {
                                Value::Object(Some(anc_mirror)) => {
                                    ctx.class_id_from_mirror(anc_mirror)
                                }
                                _ => None,
                            };
                            let construct = match anc_cid {
                                Some(anc) => {
                                    cratonvm_native_api::registry::NativeConstruct::AncestorInit(
                                        anc,
                                    )
                                }
                                None => {
                                    cratonvm_native_api::registry::NativeConstruct::NoConstructor
                                }
                            };
                            let built = ctx.instantiate(inst_cid, construct);
                            if !matches!(built, Ok(None)) {
                                if let Some((pin, _)) = args_array_pin {
                                    ctx.unpin_native_roots(pin);
                                }
                                return match built {
                                    Ok(obj) => Ok(Some(Value::Object(obj))),
                                    Err(failure) => {
                                        Err(wrap_as_invocation_target_exception(ctx, failure))
                                    }
                                };
                            }
                        }
                        // gc-common w20-f: the ancestor (`clazz`) is resolved
                        // to a name BEFORE the allocation; it was read from the
                        // Constructor at its pre-allocation address.
                        let anc_name: Option<String> = match ctx.get_field_by_name(this, "clazz") {
                            Value::Object(Some(anc_mirror)) => ctx
                                .class_id_from_mirror(anc_mirror)
                                .and_then(|anc_cid| ctx.class_name_of_id(anc_cid)),
                            _ => None,
                        };
                        let num_fields = ctx.class_num_total_fields(inst_cid);
                        let obj = ctx.alloc_object(inst_cid, num_fields);
                        // GC-safety: `invoke_special` below runs the ancestor's
                        // real `<init>` bytecode, which can trigger a moving GC;
                        // `obj` is returned afterward, unpinned otherwise --
                        // exactly the "Constructor.newInstance"-family
                        // stale-ObjectRef pattern documented in
                        // wildfly-parallel-boot-stale-objectref-residual.md.
                        let obj_pin = ctx.pin_native_root(obj);
                        {
                            // Run the ancestor's no-arg `<init>` (`clazz`);
                            // `java.lang.Object.<init>` is a no-op.
                            if let Some(anc_name) = anc_name {
                                let _ = ctx.invoke_special(
                                    &anc_name,
                                    "<init>",
                                    "()V",
                                    &[Value::Object(Some(obj))],
                                );
                            }
                            let obj = ctx.read_native_pin(obj_pin, obj);
                            // gc-common w20-f: release from the FIRST pin this
                            // body took (the args array's, when there is one);
                            // releasing from `obj_pin` left that one behind.
                            let pin_base = args_array_pin.map_or(obj_pin, |(pin, _)| pin);
                            ctx.unpin_native_roots(pin_base);
                            return Ok(Some(Value::Object(Some(obj))));
                        }
                    }
                }
            }
        }
        // JDK 21 and earlier install a DIFFERENT accessor object for this very
        // same contract: a `GeneratedSerializationConstructorAccessorN` under
        // `jdk/internal/reflect/`, which the JDK generates at run time via
        // `MethodAccessorGenerator`. It declares NO instance fields at all --
        // no `target`, therefore no `instanceClass` -- so there is nothing on
        // it to read the target type out of, and the name test above simply
        // does not match. Measured on the two images this VM is gated against,
        // and the choice is not configurable:
        //
        //     JDK 21.0.12+8  ->  GeneratedSerializationConstructorAccessorN
        //     JDK 25.0.4+7   ->  DirectConstructorHandleAccessor
        //
        // Neither `-Djdk.reflect.useDirectMethodHandle` (true or false) nor
        // `-Djdk.reflect.noInflation=true` moves either image, which is also
        // why an earlier attempt to pin the difference on that flag found the
        // output byte-identical: the flag was never the variable.
        //
        // Without this arm the name test failed on 21 and control fell through
        // to the generic path below, which allocates `clazz` -- the ANCESTOR.
        // That single skipped branch was the whole JDK 21 serialization defect,
        // and it wore two faces, which is why it read as two unrelated bugs:
        //
        //   * ancestor CONCRETE (`Integer` -> `Object`): allocating the
        //     ancestor SUCCEEDS and returns a bare `java.lang.Object`. Silent.
        //     The caller gets a plausible object and fails elsewhere, or writes
        //     the wrong thing down and never fails at all.
        //   * ancestor ABSTRACT (`ArrayList` -> `AbstractList`): allocating it
        //     throws `InstantiationException`, which `ObjectStreamClass`
        //     wraps as `InvalidClassException: ... unable to create instance`.
        //     Loud.
        //
        // The generated accessor's own bytecode already does exactly the right
        // thing (`new <target>; invokespecial <ancestor>.<init>()V; areturn`),
        // so the fix is to RUN it rather than to re-derive the target type from
        // an object that does not carry it. That is also the --real-jdk-shaped
        // answer: the real JDK bytecode is authoritative, and we stop pretending
        // to know a JDK-internal layout that turned out to be version-pinned.
        //
        // `_bytecode_only` is required, not a preference: dispatching normally
        // would re-enter this same native and recurse forever.
        else if acc_class.as_deref().is_some_and(|n| {
            n.starts_with("jdk/internal/reflect/GeneratedSerializationConstructorAccessor")
        }) {
            // Refresh through the pin: anything above here may have moved the
            // array (same convention as the generic path below).
            let args_array = args_array_pin.map(|(pin, arr)| ctx.read_native_pin(pin, arr));
            if None::<std::ffi::OsString>.is_some() {
                eprintln!(
                    "[rf-ser] delegating to generated accessor {} for declaring class {:?}",
                    acc_class.as_deref().unwrap_or("?"),
                    ctx.get_field_by_name(this, "clazz")
                );
            }
            // gc-common w21-d: release the args array's pin (the only one this
            // body holds here) instead of leaving it to the funnel's floor.
            if let Some((pin, _)) = args_array_pin {
                ctx.unpin_native_roots(pin);
            }
            return ctx.invoke_virtual_bytecode_only(
                acc,
                "newInstance",
                "([Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(args_array)],
            );
        }
        if None::<std::ffi::OsString>.is_some() {
            eprintln!(
                "[rf-ser] constructorAccessor = {:?} -- no arm matched; falling through to the generic path, which allocates the DECLARING class",
                acc_class.as_deref()
            );
        }
    }

    // Get declaring class name (C6: real-JDK field name)
    // SBR2 wave 8 (lane ctorplan): the constructor's plan, when it has one.
    let plan = match batched_words {
        Some(words) => ctor_plan_of_slot(ctx, words[1]),
        None => ctor_plan_of(ctx, this),
    };
    ph.mark(PH_WORDS);
    let declaring_mirror: Option<ObjectRef> = if plan.is_some() {
        None
    } else {
        match ctx.get_field_by_name(this, "clazz") {
            Value::Object(Some(m)) => Some(m),
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("Constructor.newInstance: no declaring class".to_string()),
                }
                .into())
            }
        }
    };
    // Capture the declaring class's EXACT id from the mirror up front (before
    // any allocation that could relocate the mirror). Allocation below uses
    // this id directly so a loader-private / load-time-weaved class is
    // instantiated as itself (JVMS В§5.3), not as the same-named class some
    // other loader defined first (which the name-based allocation collapses
    // to). See `new_object_initialized_with_class_id`.
    let (declaring_cid, class_name): (Option<ClassId>, Arc<str>) = match (&plan, declaring_mirror) {
        (Some(p), _) => (Some(p.class_id), Arc::clone(&p.class_name)),
        (None, Some(declaring_mirror)) => {
            let declaring_cid = mirror_class_id(ctx, declaring_mirror);
            let class_name = match mirror_class_name(ctx, declaring_mirror) {
                Some(n) if !n.is_empty() => n,
                _ => match ctx.class_id_from_mirror(declaring_mirror) {
                    Some(cid) => ctx.class_name_of_id(cid).unwrap_or_default(),
                    None => String::new(),
                },
            };
            (declaring_cid, Arc::from(class_name))
        }
        (None, None) => (None, Arc::from("")),
    };
    if class_name.is_empty() {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "Constructor.newInstance: declaring class has empty name".to_string(),
        }
        .into());
    }
    if crate::nbflags().iae_trace_ok && dbg_is_entity_name(&class_name) {
        let (cidv, enh) = match declaring_cid {
            Some(cid) => (cid.as_u32() as i64, dbg_class_enhanced(ctx, cid)),
            None => (-1, false),
        };
        eprintln!("CTOR-NEWINSTANCE class={class_name} declaring_cid={cidv} enhanced={enh}");
    }

    // NEW-19: JPMS deep-reflection check. Accessible flag lives in a
    // CratonVM extra slot; when true the check is already paid (JEP 403).
    //
    // JEP 403/261 distinction (same pattern as Method.invoke): a PUBLIC
    // constructor needs only `exports`, not `opens` вЂ” e.g.
    // `ArrayList.class.getConstructor(int.class).newInstance(16)` must
    // succeed without any --add-opens (java.base exports java.util).
    // kafka's ListDeserializer constructs its backing list exactly this
    // way; the previous unconditional deep check denied it with
    // `module java.base does not "opens java.util"`, surfacing as
    // "Could not construct a list instance of java.util.ArrayList".
    // Only a non-public constructor requires the opens/deep check.
    let accessible = !gate_access
        || if plan.is_some() {
            match batched_words {
                Some(words) => read_constructor_accessible_planned_word(ctx, this, words[2]),
                None => read_constructor_accessible_planned(ctx, this),
            }
        } else {
            read_constructor_accessible(ctx, this)
        };
    // L15: kept as an `Option` rather than collapsed straight to 0. An
    // unreadable `modifiers` word decodes as 0, and 0 IS a real access-flag
    // word — package-private — so the member gate added below would otherwise
    // manufacture a refusal out of a field it could not read. The two JPMS arms
    // are unaffected: they only ask `is_public`, for which 0 has always meant
    // "not public".
    let ctor_modifiers_opt = match &plan {
        Some(p) => Some(p.modifiers),
        None => match ctx.get_field_by_name(this, "modifiers") {
            Value::Int(v) => Some(v),
            _ => None,
        },
    };
    let ctor_modifiers = ctor_modifiers_opt.unwrap_or(0);
    let ctor_is_public = (ctor_modifiers & 0x0001) != 0;
    // SBR2 wave 8 (lane ctorplan): a non-public, non-accessible constructor asks
    // two gates (the `opens` gate here, the member gate below) about ONE caller;
    // nothing between them runs Java or allocates, so the frame walk is done once.
    let caller_once: Option<Option<ClassId>> = if !accessible && !ctor_is_public {
        Some(resolve_caller_class_id(ctx))
    } else {
        None
    };
    if !ctor_is_public {
        if let Err(msg) = match caller_once {
            Some(caller) => {
                check_reflection_module_access_for_caller(ctx, &class_name, declaring_cid, caller)
            }
            // `accessible`: the override bypass, as the checked form has it.
            None => Ok(()),
        } {
            // JO6-1 / J9: `Reflection.newIllegalAccessException`'s text; its
            // module form when the package is not exported, else the member
            // form (this gate asks `opens`, which HotSpot never does).
            let legacy = format!("Constructor.newInstance: {class_name}: {msg}");
            return Err(reflective_access_refusal(
                ctx,
                declaring_cid,
                ctor_modifiers,
                legacy,
            ));
        }
    } else if plan.is_some()
        && ctx.reflect_ctor_plan_per_vm()
        && (accessible
            || declaring_cid.is_some_and(|cid| ctx.reflective_export_to_every_accessor(cid)))
    {
        // Interpreter round i3 wave 75, lane L3 (`CRATONVM_REFLECT_CTOR_PLAN_PER_VM`):
        // nothing to refuse -- the override skips the question, and a target
        // every accessor is exported to needs no caller walk (as
        // `Method.invoke` does with a plan). Asked per call: not monotone.
    } else if let Err(msg) =
        check_reflection_export_access_with_target_id(ctx, &class_name, declaring_cid, accessible)
    {
        // JEP 261: `exports`, not `opens`. A public ctor previously got NO
        // module check at all -- the comment above got the exports/opens
        // distinction right and the code then implemented "needs nothing".
        let legacy = format!("Constructor.newInstance: {class_name}: {msg}");
        return Err(reflective_access_refusal(
            ctx,
            declaring_cid,
            ctor_modifiers,
            legacy,
        ));
    }

    // L15: the MEMBER-MODIFIER gate. Everything above this point is JPMS; until
    // now there was nothing else, so a `private` constructor was reflectively
    // reachable from anywhere without `setAccessible(true)` — CratonVM was MORE
    // permissive than HotSpot on this path, not less.
    //
    // HotSpot's `Constructor.newInstanceWithCaller` calls
    //
    //     checkAccess(caller, clazz, clazz, modifiers)
    //
    // -> `AccessibleObject.verifyAccess` -> `Reflection.verifyMemberAccess`,
    // the SAME funnel `native_method_invoke` is routed through above. Only the
    // `targetClass` argument differs: a constructor has no receiver, so HotSpot
    // passes the declaring class itself, and `verifyMemberAccess`'s `protected`
    // sub-rule then reduces to `isSubclassOf(clazz, caller)`. That is exactly
    // what `caller_may_access_member` computes from a `receiver` of
    // `Some(declaring)`, which is why the argument is spelled that way rather
    // than as `None`.
    //
    // Ordering is HotSpot's too: `verifyMemberAccess` runs `verifyModuleAccess`
    // FIRST, so the JPMS refusals above still outrank this one and a
    // cross-module non-public constructor keeps the module message it has today.
    //
    // The `ReflectionFactory.newConstructorForSerialization` path returns far
    // above this point and is deliberately left untouched — HotSpot's
    // serialization constructor bypasses the access check as well.
    //
    // NARROWING. Two fail-OPEN valves, and the second is a DELIBERATE
    // divergence from `native_method_invoke`'s `_ => false`: there the
    // fail-closed leg was the behaviour that already shipped, whereas here
    // every refusal this block can emit is new, so an input it cannot read must
    // not become the first one it ever emits.
    //
    //   * `modifiers` unreadable — see `ctor_modifiers_opt` above;
    //   * no resolvable caller frame, or a declaring mirror that did not
    //     resolve to a `ClassId`. `resolve_caller_class_id` answers `None`
    //     during VM bootstrap and on an all-reflection frame stack, and
    //     reflective construction runs on both.
    //
    // Deliberately NOT implemented: `verifyMemberAccess`'s class-accessibility
    // half (a member of a non-public class is reachable only from that class's
    // own runtime package, however public the member). `enforce_module_check_
    // on_field`'s `public_member_class_is_reachable` is that rule for fields;
    // adding it here would refuse the reflective instantiation of every
    // package-private implementation type from a foreign package, which is a
    // much larger blast radius than this record was filed with and wants its
    // own measurement.
    //
    // `docs/feature-designs/native-dispatch-architecture.md`
    //
    // Round 15 wave 5 (lane jdkonly7, J6): that half now exists behind the
    // opt-in `CRATONVM_REFLECT_CTOR_CLASS_ACCESS` (see
    // `reflective_ctor_class_is_reachable`), in HotSpot's place between the
    // module test and the member modifiers. Interpreter round i2 wave 59, lane
    // L3: the per-VM `CRATONVM_REFLECT_CLASS_ACCESS` asks it by RUNTIME package,
    // as the `Method.invoke`, `Field` and `canAccess` doors do.
    if let Some(cid) = declaring_cid.filter(|_| !accessible) {
        if !reflective_ctor_class_is_reachable(ctx, cid) {
            let legacy = format!(
                "Constructor.newInstance: {class_name}: class is not public and the caller is \
                 not in its runtime package"
            );
            return Err(reflective_access_refusal(
                ctx,
                Some(cid),
                ctor_modifiers,
                legacy,
            ));
        }
    }
    let mut this = this;
    if !accessible && !ctor_is_public && ctor_modifiers_opt.is_some() {
        // Metadata-only, exactly as on the field path, EXCEPT the nest hosts
        // of a private constructor (JIT round 20 wave 16, lane nest;
        // `settle_reflective_nest_hosts`): loaded first, as `getNestHost`
        // does, with `this` pinned and refreshed (the argument array keeps
        // its own pin above). `declaring_mirror` and `args` are not read
        // after this point.
        let caller_cid = caller_once.unwrap_or_else(|| resolve_caller_class_id(ctx));
        if let (Some(caller), Some(declaring)) = (caller_cid, declaring_cid) {
            settle_reflective_nest_hosts(
                ctx,
                caller,
                declaring,
                ctor_modifiers,
                &mut [&mut this],
                &mut [],
                &[],
            );
        }
        let caller_entitled = match (caller_cid, declaring_cid) {
            (Some(caller), Some(declaring)) => crate::lang_reflect::caller_may_access_member(
                ctx,
                caller,
                declaring,
                ctor_modifiers,
                Some(declaring),
            ),
            _ => true,
        };
        if !caller_entitled {
            // Same helper, same `IllegalAccessException`, same message shape as
            // the `Method.invoke` refusal — the two paths must not drift.
            check_access(
                ctx,
                ctor_modifiers,
                false,
                declaring_cid,
                &format!("Constructor.newInstance: {class_name}"),
            )?;
        }
    }

    // Round 15 wave 5 (lane jdkonly7): JDK 25's `newInstanceWithCaller`, right
    // after `checkAccess` and whatever `setAccessible` said:
    //
    //     if ((clazz.getModifiers() & Modifier.ENUM) != 0)
    //         throw new IllegalArgumentException("Cannot reflectively create enum objects");
    //
    // This native had no such test: `E.class.getDeclaredConstructor(String.class,
    // int.class)` + `setAccessible(true)` + `newInstance("X", 9)` built a new
    // enum constant (the reflection-proof enum singleton was not).
    // `CRATONVM_REFLECT_ENUM_CTOR_REFUSAL=0` restores that (read for an enum
    // class only).
    // Interpreter round i3 wave 75, lane L3: a per-VM plan keeps the declaring
    // class's flags (`CtorPlan::class_flags`); otherwise read per call.
    let planned_class_flags = plan.as_ref().and_then(|p| p.class_flags);
    if let Some(cid) = declaring_cid {
        if planned_class_flags.unwrap_or_else(|| ctx.class_access_flags(cid)) & ACC_ENUM != 0
        {
            // JDK 25 raises it in `acquireConstructorAccessor`, two hops below
            // `newInstance` (interpreter round i2 wave 54, lane L3; probe
            // `L3W54ReflectiveRaiseChains`, row `enum-ctor`).
            note_enum_constructor_chain(ctx);
            return Err(illegal_arg_exc(
                "Cannot reflectively create enum objects".to_string(),
            ));
        }
    }

    // Descriptor: extra slot / side table, or rebuild from `parameterTypes`
    // (C6 / Surefire fork вЂ” never default to bare `()V`; see
    // `constructor_descriptor_for_new_instance`).
    let descriptor: Arc<str> = match &plan {
        Some(p) => Arc::clone(&p.descriptor),
        None => Arc::from(constructor_descriptor_for_new_instance(ctx, this)),
    };

    // Reject abstract classes / interfaces before allocating вЂ” matches
    // `java.lang.reflect.Constructor.newInstance` which throws
    // InstantiationException on abstract targets.
    if let Some(cid) = declaring_cid {
        let flags = planned_class_flags.unwrap_or_else(|| ctx.class_access_flags(cid));
        let abstract_bit = cratonvm_types::access_flags::ACC_ABSTRACT;
        let iface_bit = cratonvm_types::access_flags::ACC_INTERFACE;
        if flags & (abstract_bit | iface_bit) != 0 {
            // `java.lang.reflect.Constructor.newInstance` throws a real
            // `java.lang.InstantiationException` (NOT IllegalStateException) on
            // an abstract/interface target. The exact type matters: Spring's
            // `BeanUtils.instantiateClass` catches `InstantiationException`
            // specifically to rethrow `BeanInstantiationException("Is it an
            // abstract class?")` (DefaultListableBeanFactoryTests
            // .beanDefinitionWithAbstractClass). With the wrong type that catch
            // is missed and the bean appears to instantiate.
            // MEASURED: `Constructor.newInstance()` on an abstract class or an
            // abstract JDK type throws `InstantiationException` with a NULL
            // message on HotSpot — `java.io.InputStream` and `PB$A` both — while
            // `Class.newInstance()` on an INTERFACE names it. The two entry
            // points differ, and this one is the null-message form; passing the
            // class name here was the other form's rule applied to this one.
            //
            // Spring's `beanDefinitionWithAbstractClass` depends on the TYPE
            // (see the note above), which is unchanged.
            //
            // Round 15 wave 5 (lane jdkonly7): built through `construct_throwable`,
            // so a constructor that throws is the answer (JO3-2); this
            // discarded that throwable and fell back to the flat error below.
            // Interpreter round i2 wave 55, lane L3: its trace starts in the
            // JDK accessor HotSpot raises it in, not the call's
            // `DirectConstructorHandleAccessor` (`ReflectiveCheck::AbstractClass`;
            // probe `L3W55AbstractConstructorFrames`).
            ctx.note_reflective_check(cratonvm_native_api::ReflectiveCheck::AbstractClass);
            match cratonvm_native_api::throwable_builder::construct_new(
                ctx,
                "java/lang/InstantiationException",
                "()V",
                &[],
            ) {
                Ok(Some(exc)) => {
                    return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                        exc,
                    ))
                }
                Err(thrown) => return Err(thrown),
                Ok(None) => {}
            }
            // Fallback if the exception class can't be constructed.
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: format!(
                    "InstantiationException: cannot instantiate abstract/interface type {class_name}"
                ),
            }
            .into());
        }
    }
    ph.mark(PH_GATES);

    // `Constructor.newInstance` must trigger class initialization before it
    // allocates and invokes `<init>`. This is observable for BeanShell/ASM
    // generated classes: their constructor reads static state populated by
    // `<clinit>`, and reflective construction otherwise throws
    // `InterpreterError: Unititialized class: no static`.
    // HIB-CV-26 fix (2026-07-16): propagate the real `<clinit>` failure
    // (already a catchable `ExceptionInInitializerError`/
    // `NoClassDefFoundError` per JVMS §5.5, or a genuine internal error)
    // instead of re-wrapping it as an unrecoverable `VmError::Internal`.
    // Matches real JDK `Constructor.newInstance` semantics, which
    // throws `ExceptionInInitializerError` for a failed initializer.
    // Interpreter round i2 wave 58, lane L3: listed in the accessor factory,
    // as HotSpot raises it (`reflective_door_initialize`); the native
    // accessor's `newInstance0` (`!gate_access`) raises it in its own frame.
    if gate_access {
        reflective_door_initialize(ctx, declaring_cid)?;
    } else if let Some(cid) = declaring_cid {
        ctx.initialize_class(cid)?;
    }
    ph.mark(PH_INIT);

    // Parse parameter types
    let param_descs: Arc<Vec<String>> = match &plan {
        Some(p) => Arc::clone(&p.param_descs),
        None => Arc::new(parse_descriptor_param_and_return(&descriptor).0),
    };

    // Extract arguments from Object[] (args[1])
    let args_array = args_array_pin.map(|(pin, arr)| ctx.read_native_pin(pin, arr));
    let actual_arg_count = match args_array {
        Some(arr) => ctx.array_length(arr),
        None => 0,
    };

    // JIT round 21 wave 8, lane reflect2 (`CRATONVM_REFLECT_NATIVE_ACCESSOR_ARG_TEXTS`,
    // correctness, default ON): `NativeAccessor.newInstance0` (`!gate_access`)
    // is HotSpot's `JVM_NewInstanceFromConstructor`, whose `Reflection::invoke`
    // raises "wrong number of arguments" and the argument refusals of
    // [`native_accessor_argument_refusal`] from the native frame, not the Java
    // accessor's texts. The door (`gate_access`) gives them where JDK 25's
    // factory would pick `NativeAccessor`: the members
    // [`method_takes_native_accessor`] names (a constructor counts a slot for
    // the object it builds, as a receiver) and, under
    // `-Djdk.reflect.useNativeAccessorOnly=true`, every constructor
    // ([`door_honours_native_accessor_only`], asked on a refusal only).
    let ctor_vm_texts = native_accessor_arg_texts_enabled()
        && (!gate_access
            || (door_native_accessor_texts_enabled()
                && method_takes_native_accessor(&class_name, ctor_modifiers, false, &param_descs)));
    let ctor_vm_texts_now = move |ctx: &dyn NativeContext| {
        ctor_vm_texts
            || (native_accessor_arg_texts_enabled()
                && door_honours_native_accessor_only(ctx, gate_access))
    };

    if actual_arg_count != param_descs.len() {
        if ctor_vm_texts_now(&*ctx) {
            return Err(illegal_arg_exc(INVOKE0_ARITY_TEXT.to_string()));
        }
        // Identical wording to `Method.invoke`'s -- HotSpot does not
        // distinguish the two here, and naming the class was ours. Raised
        // inside the call (wave 46, lane L3).
        let expected = param_descs.len();
        return Err(raise_reflective_check(
            ctx,
            cratonvm_native_api::ReflectiveCheck::ArgumentCount,
            |_| {
                illegal_arg_exc(format!(
                    "wrong number of arguments: {actual_arg_count} expected: {expected}"
                ))
            },
        ));
    }

    // Coerce the constructor arguments BEFORE allocating, so the new object is
    // never held by native code (unrooted) across an allocation. The `<init>`
    // arguments follow the implicit `this` that `new_object_initialized`
    // prepends.
    let mut init_args: Vec<Value> = Vec::with_capacity(param_descs.len());
    for (i, pdesc) in param_descs.iter().enumerate() {
        let arg_val = if let Some(arr) = args_array {
            ctx.get_array_element(arr, i)
        } else {
            Value::Object(None)
        };
        let coerced = match coerce_arg_strict(
            ctx,
            arg_val,
            pdesc,
            "Constructor.newInstance argument",
            declaring_cid,
        ) {
            Ok(v) => v,
            // The bare `?` here is what dropped the cause: `Method.invoke` a
            // few hundred lines up attached one and this path did not, so the
            // same wrong argument produced a different exception through the
            // two entry points.
            // Raised inside the call (wave 46, lane L3).
            // `newInstance0`'s own refusals (JIT round 21 wave 8, lane reflect2).
            Err(e) if ctor_vm_texts_now(&*ctx) => {
                return Err(native_accessor_argument_refusal(ctx, arg_val, pdesc, e))
            }
            Err(e) => {
                return Err(raise_reflective_coercion_failure(
                    ctx, arg_val, pdesc, e, true,
                ))
            }
        };
        init_args.push(coerced);
    }
    ph.mark(PH_ARGS);

    // GC-safe allocate + `<init>`: pins the new object as a GC root across the
    // constructor under the moving collector, so a heavy `<init>` that triggers
    // a relocation doesn't leave us returning a stale pointer (which would
    // resolve to a reused `java.lang.Object`). Wrap any Java exception in
    // InvocationTargetException per the `Constructor.newInstance` javadoc.
    let alloc_result = match declaring_cid {
        Some(cid) => ctx.new_object_initialized_with_class_id(cid, &descriptor, &init_args),
        None => ctx.new_object_initialized(&class_name, &descriptor, &init_args),
    };
    ph.mark(PH_CALL);
    match alloc_result {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        Ok(_) => Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: format!("Constructor.newInstance: failed to allocate {class_name}"),
        }
        .into()),
        Err(failure) => Err(wrap_reflective_callee_failure(
            ctx,
            failure,
            ReflectiveAccessor::Constructor,
            [declaring_cid, None],
        )),
    }
}

// ---------------------------------------------------------------------------
// Class.getDeclaredConstructors / getDeclaredConstructor
// ---------------------------------------------------------------------------

/// The private constructor of the lambda proxy `class_id`, over its captures
/// (the `arg$N` fields' types, [`lambda_proxy_captured_fields`]). `None` when
/// `class_id` has no lambda metadata.
pub fn lambda_proxy_constructor(ctx: &dyn NativeContext, class_id: ClassId) -> Option<MethodMetadata> {
    ctx.lambda_proxy_serial_metadata(class_id)?;
    let params: String = lambda_proxy_captured_fields(ctx, class_id)
        .into_iter()
        .map(|f| f.descriptor)
        .collect();
    Some(MethodMetadata {
        name: "<init>".to_string(),
        descriptor: format!("({params})V"),
        access_flags: 0x0002, // ACC_PRIVATE
        declaring_class_id: class_id,
        exceptions: Vec::new(),
        signature: None,
    })
}

pub fn native_class_get_declared_constructors(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredConstructors on null".to_string()),
            }
            .into())
        }
    };
    // JDK `Class.getDeclaredConstructors0(boolean publicOnly)` вЂ” when true,
    // returns only ACC_PUBLIC <init> (used by `Class.getConstructors()`).
    // When this native is invoked from the synthetic `getDeclaredConstructors()`
    // wrapper (args.len()==1), publicOnly is false.
    let public_only = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = empty_listing_array(ctx, "java/lang/reflect/Constructor");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // Wave 89, lane L3: `CRATONVM_DBG_GDC_PROF` (see `gdcp`).
    let mut lap = gdcp::Lap::new();
    // HotSpot `JVM_GetClassDeclaredConstructors` links the class first (the
    // VM's policy decides; interpreter round i1 wave 17).
    ctx.link_class_for_reflection(class_id)?;
    // 2026-10-07 (item 3 of the i47-L5 page): called with no argument, this
    // serves the JDK's `getDeclaredConstructors()` (`--compatible`), whose
    // `privateGetDeclaredConstructors` frames the missing-type raise is marked by
    // (`served_listing_prepass`); `getDeclaredConstructors0(publicOnly)` takes two.
    if args.len() == 1 {
        served_listing_prepass(
            ctx,
            class_id,
            ListingDoor::DeclaredConstructors,
            ListingLookup::All,
        )?;
    }
    lap.mark(gdcp::LINK);
    // `Reflection::new_constructor` resolves each constructor's parameter and
    // exception types through the class's loader (interpreter round i1 wave
    // 47, lane L5; see `note_reflected_members`).
    if ctx.jdk_names_initiated_armed() {
        ctx.note_reflected_members(class_id, 1, public_only);
    }
    lap.mark(gdcp::NOTE);

    let methods = ctx.declared_methods(class_id);
    lap.mark(gdcp::DECLARED);
    // A lambda proxy declares one private constructor over its captures, as
    // HotSpot's spun class does (interpreter round i1 wave 35, item 3 of
    // `i29-L4-native-indy-linkage-skips-the-jdks-validation`); its parameters
    // are the `arg$N` fields' types.
    let lambda_ctor = (!public_only && is_lambda_proxy_id(class_id))
        .then(|| lambda_proxy_constructor(ctx, class_id))
        .flatten();
    let mut constructors: Vec<&MethodMetadata> = methods
        .iter()
        .filter(|m| m.name == "<init>" && (!public_only || (m.access_flags & 0x0001) != 0))
        .collect();
    if constructors.is_empty() {
        constructors.extend(lambda_ctor.as_ref());
    }
    if let Some(class_name) = ctx.class_name_of_id(class_id) {
        if class_name != "java/lang/String"
            && constructors.len() > 1
            && annotation_cds_member_order()
        {
            // Interpreter round i4 wave 98, lane L3 (`L3W98ReflectAnnotations`
            // row `En ctor`): HotSpot lists constructors in its sorted
            // `methods` array order, where its unstable quicksort leaves the
            // overloads of `<init>` (one `Symbol`) in an order of its own
            // (`hotspot_method_array_order`). Only with a CDS index, which
            // ranks the names around them as HotSpot's addresses do; the
            // same `CRATONVM_ANNOTATION_CDS_MEMBER_ORDER=0` lever turns it off.
            let index = cds_symbol_index(ctx);
            if index.len() > 0 {
                let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
                let order = hotspot_method_array_order(&index, &names);
                let reordered: Vec<&MethodMetadata> = order
                    .into_iter()
                    .map(|i| &methods[i])
                    .filter(|m| constructors.iter().any(|c| std::ptr::eq(*c, *m)))
                    .collect();
                if reordered.len() == constructors.len() {
                    constructors = reordered;
                }
            }
        }
        order_constructors_for_reflection(&class_name, &mut constructors);
    }
    lap.mark(gdcp::ORDER);
    // `Reflection::new_constructor` loads the parameter and exception types
    // (interpreter round i2 wave 54, lane L3; `resolve_reflected_signature_types`).
    if ctx.reflection_loads_signature_types() {
        resolve_reflected_signature_types(
            ctx,
            class_id,
            constructors
                .iter()
                .map(|m| (m.descriptor.as_str(), m.exceptions.as_slice())),
        )?;
    }
    lap.mark(gdcp::SIGNATURE_TYPES);

    // GC-safe: `create_constructor_object` allocates (see `build_mirror_array`).
    // `CRATONVM_REFLECT_CTOR_MINT_LEAN` (wave 92, lane L3; default off): from
    // this VM's reflect-door caches after the first by-name answer.
    let constructor_component =
        lean_component_id(ctx, "java/lang/reflect/Constructor", ctor_component_slot);
    lap.mark(gdcp::COMPONENT);
    let arr = build_mirror_array_comp(ctx, constructor_component, constructors.len(), |ctx, i| {
        Ok(create_constructor_object(ctx, constructors[i])?)
    })?;
    lap.finish_call(constructors.len());
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_class_get_declared_constructor(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getDeclaredConstructor on null".to_string()),
            }
            .into())
        }
    };

    // Parameter types array (args[1])
    let param_types_arr = match args.get(1) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchMethodException {
                message: "<init>".to_string(),
            }
            .into())
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    served_listing_prepass(
        ctx,
        class_id,
        ListingDoor::DeclaredConstructor,
        ListingLookup::All,
    )?;

    let methods = ctx.declared_methods(class_id);

    // gen r4w3/rooting: `param_type_matches` may load through a user loader
    // (GC point); pin the parameter-types array and re-read it after each.
    let pt_pin = param_types_arr.map(|arr| ctx.pin_native_root(arr));

    for meta in &methods {
        if meta.name != "<init>" {
            continue;
        }

        let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);

        if let (Some(pt_arr), Some(pt_pin)) = (param_types_arr, pt_pin) {
            let cur_arr = ctx.read_native_pin(pt_pin, pt_arr);
            let expected_count = ctx.array_length(cur_arr);
            if param_descs.len() != expected_count {
                continue;
            }
            let mut matched = true;
            for (i, pdesc) in param_descs.iter().enumerate() {
                // gen r4w3/rooting: current address after the last GC point.
                let pt_arr = ctx.read_native_pin(pt_pin, pt_arr);
                let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        matched = false;
                        break;
                    }
                };
                if !param_type_matches(ctx, expected_mirror, pdesc, meta.declaring_class_id) {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
            }
        } else {
            // No param types specified вЂ” match no-arg constructor
            if !param_descs.is_empty() {
                continue;
            }
        }

        if let Some(pin) = pt_pin {
            ctx.unpin_native_roots(pin);
        }
        let ctor_obj = create_constructor_object(ctx, meta)?;
        return Ok(Some(Value::Object(Some(ctor_obj))));
    }

    // gen r4w3/rooting: current address for the diagnostic message.
    let param_types_arr = match (param_types_arr, pt_pin) {
        (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
        _ => None,
    };
    let message = no_such_method_message(&*ctx, class_id, "<init>", param_types_arr);
    if let Some(pin) = pt_pin {
        ctx.unpin_native_roots(pin);
    }
    Err(cratonvm_types::error::RuntimeError::NoSuchMethodException { message }.into())
}

// ---------------------------------------------------------------------------
// Inherited enumeration вЂ” getFields/getMethods/getConstructors
// ---------------------------------------------------------------------------

/// Collect all public fields from the class hierarchy (this class +
/// superclasses + interfaces) and materialize them as a `Field[]` array.
/// Mirrors `Class.privateGetPublicFields()`.
///
/// Per JDK semantics: for an interface, the superclass walk is skipped
/// (matches `collect_public_methods`).
///
/// GC-safety (2026-07-16): the hierarchy walk only accumulates plain
/// `FieldMetadata` values (no GC objects, nothing a moving collector can
/// invalidate). Object materialization happens in one final pass through
/// `build_mirror_array`, which pins the destination array across every
/// allocating `create_field_object` call. The previous version pushed
/// freshly-created `ObjectRef`s straight into an ordinary (unrooted) `Vec`
/// *during* the walk -- on a class with many public fields/properties (e.g.
/// JAXB's `ClassInfoImpl` walking a Hibernate entity hierarchy), a GC
/// triggered by the Nth `create_field_object` call could relocate/reclaim
/// the first N-1 already-created Field mirrors, since that Vec was not a
/// GC root. This is the exact residual gap flagged (but never swept) in
/// `jit-junit-discovery-reflection-corruption.md`
/// ("collect_public_fields/methods (getFields/getMethods Vec-build)").
pub fn collect_public_fields(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let mut metas: Vec<FieldMetadata> = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![class_id];

    while let Some(cid) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        resolve_public_listing_rung(ctx, cid, true)?;
        let fields = ctx.declared_fields(cid);
        for meta in fields {
            if (meta.access_flags & 0x0001) != 0 {
                // PUBLIC
                metas.push(meta);
            }
        }
        // Walk superclass вЂ” skipped for interfaces (see
        // `collect_public_methods` rationale).
        if !ctx.is_interface_class(cid) {
            if let Some(parent) = ctx.superclass_of(cid) {
                stack.push(parent);
            }
        }
        // Walk interfaces
        for iface_id in ctx.class_interfaces(cid) {
            stack.push(iface_id);
        }
    }
    let field_component = reflection_component_id(ctx, "java/lang/reflect/Field");
    Ok(build_mirror_array_comp(
        ctx,
        field_component,
        metas.len(),
        |ctx, i| Ok(create_field_object(ctx, &metas[i])),
    )?)
}

/// One public method a JDK-shaped walk found: its metadata and its index in
/// `declared_methods_with_synthetic(meta.declaring_class_id)` (the
/// link-resolver cache's value). Shared, because `getMethods`' walk hands a
/// superclass's or superinterface's list to every class below it.
pub type PublicMethodRow = std::rc::Rc<(MethodMetadata, u32)>;

/// The parameter half of a method descriptor, `(...)`.
pub fn method_descriptor_params(descriptor: &str) -> &str {
    &descriptor[..descriptor.find(')').map_or(descriptor.len(), |i| i + 1)]
}

/// The return half of a method descriptor (after `)`).
pub fn method_descriptor_return(descriptor: &str) -> &str {
    descriptor.find(')').map_or("", |i| &descriptor[i + 1..])
}

/// `d.isAssignableFrom(x)` for two declaring classes (`is_subclass` covers
/// interfaces and the class itself).
pub fn declaring_class_assignable_from(ctx: &dyn NativeContext, d: ClassId, x: ClassId) -> bool {
    d == x || ctx.is_subclass(x, d)
}

/// `to.isAssignableFrom(from)` for two return descriptors, `to` read in
/// `to_owner`'s loader and `from` in `from_owner`'s, without loading: a class
/// the VM cannot place answers `false` (HotSpot's `getReturnType()` has
/// resolved both; an unresolvable one keeps the earlier method).
pub fn return_descriptor_assignable_from(
    ctx: &dyn NativeContext,
    to: &str,
    to_owner: ClassId,
    from: &str,
    from_owner: ClassId,
) -> bool {
    if to == from {
        return true;
    }
    if let (Some(to_elem), Some(from_elem)) = (to.strip_prefix('['), from.strip_prefix('[')) {
        let reference = |d: &str| d.starts_with('L') || d.starts_with('[');
        return reference(to_elem)
            && reference(from_elem)
            && return_descriptor_assignable_from(ctx, to_elem, to_owner, from_elem, from_owner);
    }
    let Some(to_name) = to.strip_prefix('L').and_then(|s| s.strip_suffix(';')) else {
        return false;
    };
    if to_name == "java/lang/Object" {
        return from.starts_with('L') || from.starts_with('[');
    }
    if from.starts_with('[') {
        return matches!(to_name, "java/lang/Cloneable" | "java/io/Serializable");
    }
    let Some(from_name) = from.strip_prefix('L').and_then(|s| s.strip_suffix(';')) else {
        return false;
    };
    match (
        ctx.class_id_seen_by_class_without_loading(to_name, to_owner),
        ctx.class_id_seen_by_class_without_loading(from_name, from_owner),
    ) {
        (Some(to_id), Some(from_id)) => to_id == from_id || ctx.is_subclass(from_id, to_id),
        _ => false,
    }
}

/// JDK 25 `PublicMethods.MethodList.merge(head, method)` over `list`, one
/// (name, parameter types) key's methods in order: of two methods with the
/// same return type, a class's beats an interface's, and of two of the same
/// kind the one whose declaring class is the subtype wins; a method with
/// another return type is kept beside them.
pub fn merge_public_method_row(
    ctx: &dyn NativeContext,
    list: &mut Vec<PublicMethodRow>,
    row: PublicMethodRow,
) {
    let dclass = row.0.declaring_class_id;
    let d_iface = ctx.is_interface_class(dclass);
    let mut i = 0;
    while i < list.len() {
        if method_descriptor_return(&list[i].0.descriptor)
            == method_descriptor_return(&row.0.descriptor)
        {
            let xdclass = list[i].0.declaring_class_id;
            let x_iface = ctx.is_interface_class(xdclass);
            if d_iface == x_iface {
                if declaring_class_assignable_from(ctx, dclass, xdclass) {
                    // The existing method is the same or overrides the new one.
                    return;
                }
                if declaring_class_assignable_from(ctx, xdclass, dclass) {
                    // The new method overrides the existing one.
                    list.remove(i);
                    continue;
                }
            } else if d_iface {
                // An interface's method never displaces a class's.
                return;
            } else {
                list.remove(i);
                continue;
            }
        }
        i += 1;
    }
    list.push(row);
}

/// JDK 25 `PublicMethods.MethodList.getMostSpecific()`: the first method,
/// replaced by any later one whose return type is a proper subtype of the
/// current one's.
pub fn most_specific_public_method(
    ctx: &dyn NativeContext,
    list: &[PublicMethodRow],
) -> Option<PublicMethodRow> {
    let mut best = list.first()?.clone();
    for row in &list[1..] {
        let rt = method_descriptor_return(&best.0.descriptor);
        let rt2 = method_descriptor_return(&row.0.descriptor);
        if rt != rt2
            && return_descriptor_assignable_from(
                ctx,
                rt,
                best.0.declaring_class_id,
                rt2,
                row.0.declaring_class_id,
            )
        {
            best = row.clone();
        }
    }
    Some(best)
}

/// JDK 25 `Class.getMethodsRecursive(name, ptypes, includeStatic, true)`: the
/// public methods named `name` that `matches` (the parameter types) declared
/// by `cid` itself, if any (they override everything above); else the
/// superclass's answer (none for an interface) merged with each direct
/// superinterface's, asked with `include_static = false` (a static interface
/// method is not inherited). Interpreter round i4 wave 94, lane L3
/// (`CRATONVM_PUBLIC_METHOD_JDK_WALK`).
pub fn jdk_get_methods_recursive(
    ctx: &mut dyn NativeContext,
    cid: ClassId,
    name: &str,
    include_static: bool,
    matches: &mut dyn FnMut(&mut dyn NativeContext, &MethodMetadata) -> bool,
) -> Result<Vec<PublicMethodRow>, MethodCallFailed> {
    resolve_public_listing_rung(ctx, cid, false)?;
    let mut res: Vec<PublicMethodRow> = Vec::new();
    for (idx, meta) in declared_methods_with_synthetic(&*ctx, cid).into_iter().enumerate() {
        if meta.name != name
            || meta.name == "<init>"
            || meta.name == "<clinit>"
            || (meta.access_flags & 0x0001) == 0
            || (!include_static && (meta.access_flags & 0x0008) != 0)
        {
            continue;
        }
        if !matches(ctx, &meta) {
            continue;
        }
        res.push(std::rc::Rc::new((meta, idx as u32)));
    }
    if !res.is_empty() {
        return Ok(res);
    }
    if !ctx.is_interface_class(cid) {
        if let Some(sc) = ctx.superclass_of(cid) {
            res = jdk_get_methods_recursive(ctx, sc, name, include_static, matches)?;
        }
    }
    for intf in ctx.class_interfaces(cid) {
        for row in jdk_get_methods_recursive(ctx, intf, name, false, matches)? {
            merge_public_method_row(&*ctx, &mut res, row);
        }
    }
    Ok(res)
}

/// JDK 25 `PublicMethods`: methods keyed by (name, parameter types) in first-
/// insertion order, each key holding its `MethodList`.
#[derive(Default)]
pub struct JdkPublicMethods {
    pub keys: Vec<Vec<PublicMethodRow>>,
    pub index: HashMap<(String, String), usize>,
}

impl JdkPublicMethods {
    pub fn merge(&mut self, ctx: &dyn NativeContext, row: PublicMethodRow) {
        let key = (
            row.0.name.clone(),
            method_descriptor_params(&row.0.descriptor).to_string(),
        );
        match self.index.get(&key) {
            Some(&i) => merge_public_method_row(ctx, &mut self.keys[i], row),
            None => {
                self.index.insert(key, self.keys.len());
                self.keys.push(vec![row]);
            }
        }
    }

    pub fn into_rows(self) -> Vec<PublicMethodRow> {
        self.keys.into_iter().flatten().collect()
    }
}

/// JDK 25 `Class.privateGetPublicMethods()`: the class's own public methods
/// (statics included), then its superclass's public methods (none for an
/// interface), then each direct superinterface's non-static public methods,
/// each merged by [`JdkPublicMethods::merge`]. `memo` holds each class's
/// answer for the one `getMethods` call (the JDK caches it per class).
pub fn jdk_private_get_public_methods(
    ctx: &mut dyn NativeContext,
    cid: ClassId,
    memo: &mut HashMap<ClassId, std::rc::Rc<Vec<PublicMethodRow>>>,
) -> Result<std::rc::Rc<Vec<PublicMethodRow>>, MethodCallFailed> {
    if let Some(hit) = memo.get(&cid) {
        return Ok(hit.clone());
    }
    resolve_public_listing_rung(ctx, cid, false)?;
    let mut pms = JdkPublicMethods::default();
    for (idx, meta) in declared_methods_with_synthetic(&*ctx, cid).into_iter().enumerate() {
        if meta.name == "<init>" || meta.name == "<clinit>" || (meta.access_flags & 0x0001) == 0 {
            continue;
        }
        pms.merge(&*ctx, std::rc::Rc::new((meta, idx as u32)));
    }
    if !ctx.is_interface_class(cid) {
        if let Some(sc) = ctx.superclass_of(cid) {
            let rows = jdk_private_get_public_methods(ctx, sc, memo)?;
            for row in rows.iter() {
                pms.merge(&*ctx, row.clone());
            }
        }
    }
    for intf in ctx.class_interfaces(cid) {
        let rows = jdk_private_get_public_methods(ctx, intf, memo)?;
        for row in rows.iter() {
            if (row.0.access_flags & 0x0008) == 0 {
                pms.merge(&*ctx, row.clone());
            }
        }
    }
    let rows = std::rc::Rc::new(pms.into_rows());
    memo.insert(cid, rows.clone());
    Ok(rows)
}

/// `getMethods` by [`jdk_private_get_public_methods`]
/// (`CRATONVM_PUBLIC_METHOD_JDK_WALK`; interpreter round i4 wave 94, lane L3).
pub fn collect_public_methods_jdk(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let mut memo = HashMap::new();
    let rows = jdk_private_get_public_methods(ctx, class_id, &mut memo)?;
    drop(memo);
    let method_component = reflection_component_id(ctx, "java/lang/reflect/Method");
    build_mirror_array_comp(ctx, method_component, rows.len(), |ctx, i| {
        create_method_object(ctx, &rows[i].0)
    })
}

/// Collect all public methods from the class hierarchy.
///
/// Per JDK 25 `Class.privateGetPublicMethods()` semantics:
/// * For a **class**: walk declared public methods + super-interfaces +
///   superclass chain.
/// * For an **interface**: walk declared public methods + super-interfaces
///   only вЂ” **NEVER** the superclass (which is `java/lang/Object` per
///   JVMS but is intentionally skipped вЂ” see `Class.java`:
///   `Class<?> sc = isInterface() ? null : getSuperclass();`).
///
/// G2-fix (NegativeArraySizeException): walking the superclass chain for
/// an interface pulled in `java/lang/Object`'s public 0-arg methods
/// (`toString`, `hashCode`, `getClass`, `notify`, `notifyAll`). ByteBuddy's
/// `JavaDispatcher$DynamicClassLoader.invoker()` then calls
/// `parameterTypes.length - 1` and `anewarray` on the result вЂ”
/// underflowing to `-1` and triggering `NegativeArraySizeException`.
/// Real JDK 25 returns only the 2 declared `Invoker` methods; this fix
/// matches.
///
/// (`collect_public_methods_from` and its `merge_public_method`, a third
/// public-method walk that only its own recursion called, were deleted in
/// interpreter round i4 wave 92, lane L3;
/// `i52-L3-proposal-one-public-method-walk-for-getmethod-and-getmethods`.)
pub fn collect_public_methods(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    if ctx.public_method_jdk_walk() {
        return collect_public_methods_jdk(ctx, class_id);
    }
    let mut metas: Vec<MethodMetadata> = Vec::new();
    let mut visited = std::collections::HashSet::new();
    // Class.getMethods() retains all public declarations of the most-specific
    // class (including compiler bridges) but must suppress any same-signature
    // declaration inherited from a superclass. Track signatures contributed by
    // more-specific classes separately from the emitted method list so a bridge
    // and its bridged method declared by the same class both remain visible.
    let mut inherited_class_method_keys = std::collections::HashSet::new();
    // Each rung carries whether its static methods are members: the queried
    // class's and its superclasses' are, a superinterface's never are
    // (JLS 8.4.8; JDK 25's `getMethodsRecursive` / `PublicMethods`), so
    // `ArrayList.class.getMethods()` holds no `List.of` (interpreter round i1
    // wave 52, lane L3; `L3W52ClassLookupAnswers`, row `methods-of-impl`).
    // An interface is reached only through interface edges (or is the
    // queried class), so keying `visited` by class alone stays exact.
    let mut stack = vec![(class_id, true)];

    while let Some((cid, include_static)) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        resolve_public_listing_rung(ctx, cid, false)?;
        // F2: include synthetic JDK declarations so frameworks that
        // walk the public method table on a synthetic-stub class (e.g.
        // java/lang/ClassLoader) still see the JDK-contracted methods.
        let methods = declared_methods_with_synthetic(ctx, cid);
        let is_interface = ctx.is_interface_class(cid);
        let mut declared_class_method_keys = std::collections::HashSet::new();
        for meta in methods {
            if meta.name == "<init>" || meta.name == "<clinit>" {
                continue;
            }
            if !include_static && (meta.access_flags & 0x0008) != 0 {
                continue;
            }
            if (meta.access_flags & 0x0001) != 0 {
                // An overriding method suppresses inherited methods by name and
                // parameter list, irrespective of a covariant return type. The
                // return descriptor is deliberately omitted from this key: a
                // covariant override has a compiler bridge with a different
                // return type, and that bridge must suppress the ancestor too.
                let parameter_end = meta
                    .descriptor
                    .find(')')
                    .map(|index| index + 1)
                    .unwrap_or(meta.descriptor.len());
                let override_key = (
                    meta.name.clone(),
                    meta.descriptor[..parameter_end].to_string(),
                );
                if inherited_class_method_keys.contains(&override_key) {
                    continue;
                }
                metas.push(meta);
                declared_class_method_keys.insert(override_key);
            }
        }

        // A more-specific interface default method suppresses the matching
        // abstract/default declaration inherited from its parent interface just
        // as a class override suppresses a superclass declaration. Keeping this
        // set class-only leaked InterfaceMethods.getValue() alongside
        // DefaultMethods.getValue(), confusing bridge resolution.
        inherited_class_method_keys.extend(declared_class_method_keys);

        // Process a class's superclass before its interfaces. Apart from
        // matching the JDK's class-over-interface precedence, this makes the
        // complete class override set available before any interface method is
        // considered for inclusion.
        for iface_id in ctx.class_interfaces(cid) {
            stack.push((iface_id, false));
        }
        // G2-fix: interfaces intentionally skip their superclass
        // (java/lang/Object) per JDK semantics.
        if !is_interface {
            if let Some(parent) = ctx.superclass_of(cid) {
                stack.push((parent, include_static));
            }
        }
    }
    // GC-safety (2026-07-16): see `collect_public_fields`'s doc comment --
    // same fix, same residual-gap doc reference.
    let method_component = reflection_component_id(ctx, "java/lang/reflect/Method");
    Ok(build_mirror_array_comp(
        ctx,
        method_component,
        metas.len(),
        |ctx, i| Ok(create_method_object(ctx, &metas[i])?),
    )?)
}

pub fn native_class_get_fields(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getFields on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/Field");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    served_listing_prepass(ctx, class_id, ListingDoor::Fields, ListingLookup::All)?;
    let arr = collect_public_fields(ctx, class_id)?;
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_class_get_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getField on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // JDK 25's `Objects.requireNonNull(name)`: no message (wave 53,
            // lane L3; see `native_class_get_declared_field`).
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchFieldException {
                field_name: target_name,
            }
            .into())
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    served_listing_prepass(
        ctx,
        class_id,
        ListingDoor::Field,
        ListingLookup::Field(&target_name),
    )?;

    if crate::vmflags().loader.dbg_fbcglib && target_name.starts_with("CGLIB$") {
        let cname = ctx.class_name_of_id(class_id);
        eprintln!(
            "[FBCGLIB-DBG] Class.getField({target_name}) on class_id={class_id:?} name={cname:?}"
        );
    }

    // Round 9 audit fix (HIGH #7): probe the LinkResolver for the
    // resolved hierarchy walk. `getField` keys on `(class_id, name, "")`
    // вЂ” same shape as JNI `GetFieldID` with a null signature. On a cache
    // hit we still re-fetch the metadata from the declaring class so
    // `create_field_object` produces a faithful mirror.
    if let Some((decl, abs_idx, is_static)) =
        ctx.link_resolver_get_field(class_id, &target_name, "")
    {
        let fields = ctx.declared_fields(decl);
        for meta in &fields {
            if meta.name == target_name
                && meta.slot_index == abs_idx as usize
                && meta.is_static == is_static
                && (meta.access_flags & 0x0001) != 0
            {
                let field_obj = create_field_object(ctx, meta);
                return Ok(Some(Value::Object(Some(field_obj))));
            }
        }
        // Cached entry no longer applicable; fall through to the walk.
    }

    // Walk class hierarchy for public field
    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![class_id];
    while let Some(cid) = stack.pop() {
        if !visited.insert(cid) {
            continue;
        }
        resolve_public_listing_rung(ctx, cid, true)?;
        let fields = ctx.declared_fields(cid);
        for meta in &fields {
            if meta.name == target_name && (meta.access_flags & 0x0001) != 0 {
                ctx.link_resolver_insert_field(
                    class_id,
                    &target_name,
                    "",
                    meta.declaring_class_id,
                    meta.slot_index as u32,
                    meta.is_static,
                );
                let field_obj = create_field_object(ctx, meta);
                return Ok(Some(Value::Object(Some(field_obj))));
            }
        }
        // JDK 25 `getField0`: the direct superinterfaces in declaration order,
        // each searched to the end, then the superclass (none for an
        // interface). Pushed in reverse so the LIFO stack pops them in that
        // order; the old order popped the LAST interface first, so of two
        // superinterfaces declaring one constant name the second answered
        // (interpreter round i4 wave 94, lane L3; code review).
        if !ctx.is_interface_class(cid) {
            if let Some(parent) = ctx.superclass_of(cid) {
                stack.push(parent);
            }
        }
        for iface_id in ctx.class_interfaces(cid).into_iter().rev() {
            stack.push(iface_id);
        }
    }

    Err(cratonvm_types::error::RuntimeError::NoSuchFieldException {
        field_name: target_name,
    }
    .into())
}

pub fn native_class_get_methods(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // bytebuddy_probe stack-overflow guard (agent15) вЂ” see comment on
    // GET_DECLARED_METHODS_DEPTH above. `getMethods` walks the super/iface
    // chain (collect_public_methods) so it can recurse even more deeply
    // than `getDeclaredMethods` once any superclass mirror's `<clinit>`
    // calls back into reflection.
    let prev_depth = GET_METHODS_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if prev_depth > 50 {
        GET_METHODS_DEPTH.with(|d| d.set(prev_depth));
        let arr = empty_reflection_array(ctx, "java/lang/reflect/Method");
        return Ok(Some(Value::Object(Some(arr))));
    }
    // zr z11/nd: restored on the UNWIND too. An allocation refusal inside
    // this native unwinds (`NativeAllocOom`) past the restore at the end, so
    // every `OutOfMemoryError` raised in here leaked one level; after 51 of
    // them on a thread this answered an empty array for every class.
    let _depth_restore = DepthRestore(&GET_METHODS_DEPTH, prev_depth);
    let result = (|| -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(obj))) => *obj,
            _ => {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("Class.getMethods on null".to_string()),
                }
                .into())
            }
        };
        let class_id = match mirror_class_id(ctx, this) {
            Some(id) => id,
            None => {
                let arr = empty_reflection_array(ctx, "java/lang/reflect/Method");
                return Ok(Some(Value::Object(Some(arr))));
            }
        };
        // 2026-10-07 (item 3 of the i47-L5 page): see `served_listing_prepass`.
        served_listing_prepass(ctx, class_id, ListingDoor::Methods, ListingLookup::All)?;
        let arr = collect_public_methods(ctx, class_id)?;
        Ok(Some(Value::Object(Some(arr))))
    })();
    GET_METHODS_DEPTH.with(|d| d.set(prev_depth));
    result
}

pub fn native_class_get_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getMethod on null".to_string()),
            }
            .into())
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // JDK 25's `Objects.requireNonNull(name)`: no message (wave 53,
            // lane L3; see `native_class_get_declared_field`).
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let target_name = ctx.read_string(name_obj).unwrap_or_default();
    let param_types_arr = match args.get(2) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            // WF6 вЂ” same short-circuit as getDeclaredMethod: synth a no-op
            // `main(String[])` Method for jboss-modules / WildFly /
            // Keycloak entry-class lookups so the launcher progresses past
            // the NoSuchMethod wall instead of fataling.
            if let Ok(Some(method_obj)) =
                wf_shim_synth_main_method(ctx, this, &target_name, param_types_arr)
            {
                return Ok(Some(Value::Object(Some(method_obj))));
            }
            // HotSpot's `methodToString`: `int.x()`, not `x` (interpreter
            // round i1 wave 52, lane L3; `L3W52ClassLookupAnswers`).
            let owner = mirror_type_name(ctx, this);
            let message = if owner.is_empty() {
                target_name
            } else {
                no_such_method_message_of(ctx, &owner, &target_name, param_types_arr)
            };
            return Err(
                cratonvm_types::error::RuntimeError::NoSuchMethodException { message }.into(),
            );
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    if ctx.reflection_loads_signature_types() {
        if let Some(params) = parameter_types_descriptor(ctx, param_types_arr) {
            served_listing_prepass(
                ctx,
                class_id,
                ListingDoor::Method,
                ListingLookup::Method(&target_name, &params),
            )?;
        }
    }

    // Round 9 audit fix (HIGH #7): build the same synthetic descriptor
    // key as `getDeclaredMethod` and probe the LinkResolver before
    // walking. Public-method lookups can also straddle several
    // superclass rungs, but the cache key uses the *origin* class_id
    // (where the user queried from) вЂ” that's the natural dedupe axis.
    let mut cache_key_ok = true;
    let cache_key_desc: String = match param_types_arr {
        None => String::new(),
        Some(pt_arr) => {
            let n = ctx.array_length(pt_arr);
            let mut s = String::with_capacity(n * 16);
            for i in 0..n {
                let mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        cache_key_ok = false;
                        break;
                    }
                };
                let name = mirror_class_name(ctx, mirror).unwrap_or_default();
                s.push_str(&name);
                // Strict mode: two same-named classes are two parameter types, so the
                // cached answer for one must not serve the other.
                if ctx.is_jdk_only() {
                    if let Some(id) = mirror_class_id(ctx, mirror) {
                        s.push('#');
                        s.push_str(&id.as_u32().to_string());
                    }
                }
                s.push(';');
            }
            s
        }
    };

    if cache_key_ok {
        if let Some((decl, idx)) =
            ctx.link_resolver_get_method(class_id, &target_name, &cache_key_desc)
        {
            // Refetch the cached entry's metadata vec on the declaring
            // class and verify the (name, public) constraints вЂ” if the
            // entry still applies we can short-circuit the hierarchy walk.
            let methods = declared_methods_with_synthetic(ctx, decl);
            if let Some(meta) = methods.get(idx as usize) {
                if meta.name == target_name
                    && meta.name != "<init>"
                    && meta.name != "<clinit>"
                    && (meta.access_flags & 0x0001) != 0
                {
                    let method_obj = create_method_object(ctx, meta)?;
                    return Ok(Some(Value::Object(Some(method_obj))));
                }
            }
            // Fall through on stale cache entry.
        }
    }

    // gen r4w3/rooting: `param_type_matches` may load through a user loader
    // (GC point); pin `this` and the parameter-types array for the walk and
    // re-read them after each GC point.
    let this_pin = ctx.pin_native_root(this);
    let pt_pin = param_types_arr.map(|arr| ctx.pin_native_root(arr));

    // Interpreter round i4 wave 94, lane L3 (`CRATONVM_PUBLIC_METHOD_JDK_WALK`,
    // per VM, default off; the i52-L3 one-walk proposal): JDK 25's
    // `getMethod0` -- `getMethodsRecursive`, then `getMostSpecific` -- instead
    // of the depth-first walk below, which it then skips. Differs where
    // several rungs answer: two superinterfaces in either order (the more
    // specific interface's method), a covariant return among a rung's own
    // matches (the most specific return type), an interface queried for an
    // `Object` method (none: an interface has no superclass here).
    let jdk_walk = ctx.public_method_jdk_walk();
    if jdk_walk {
        let mut matches = |ctx: &mut dyn NativeContext, meta: &MethodMetadata| -> bool {
            let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
            let param_types_arr = match (param_types_arr, pt_pin) {
                (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
                _ => None,
            };
            let expected_count = param_types_arr.map_or(0, |arr| ctx.array_length(arr));
            if param_descs.len() != expected_count {
                return false;
            }
            let (Some(pt_arr), Some(pt_pin)) = (param_types_arr, pt_pin) else {
                return true;
            };
            for (i, pdesc) in param_descs.iter().enumerate() {
                // gen r4w3/rooting: re-read after the previous element's
                // `param_type_matches`.
                let pt_arr = ctx.read_native_pin(pt_pin, pt_arr);
                let Value::Object(Some(expected_mirror)) = ctx.get_array_element(pt_arr, i) else {
                    return false;
                };
                if !param_type_matches(ctx, expected_mirror, pdesc, meta.declaring_class_id) {
                    return false;
                }
            }
            true
        };
        let found = match jdk_get_methods_recursive(ctx, class_id, &target_name, true, &mut matches)
        {
            Ok(found) => found,
            Err(failed) => {
                ctx.unpin_native_roots(this_pin);
                return Err(failed);
            }
        };
        if let Some(best) = most_specific_public_method(&*ctx, &found) {
            let (meta, meta_idx) = &*best;
            if cache_key_ok {
                ctx.link_resolver_insert_method(
                    class_id,
                    &target_name,
                    &cache_key_desc,
                    meta.declaring_class_id,
                    *meta_idx,
                );
            }
            ctx.unpin_native_roots(this_pin);
            let method_obj = create_method_object(ctx, meta)?;
            return Ok(Some(Value::Object(Some(method_obj))));
        }
    }

    // Walk class hierarchy for public method. Each rung carries whether its
    // static methods are members: JDK 25's `getMethodsRecursive` passes
    // `includeStatic` down the superclass chain but `false` to every
    // superinterface (JLS 8.4.8: a static interface method is not inherited),
    // so `ArrayList.class.getMethod("of")` is a `NoSuchMethodException` while
    // `List.class.getMethod("of")` finds it (interpreter round i1 wave 52,
    // lane L3; `L3W52ClassLookupAnswers`).
    let mut visited = std::collections::HashSet::new();
    // With the JDK walk (above) nothing is left to walk here.
    let mut stack = if jdk_walk {
        Vec::new()
    } else {
        vec![(class_id, true)]
    };
    while let Some((cid, include_static)) = stack.pop() {
        if !visited.insert((cid, include_static)) {
            continue;
        }
        if let Err(failed) = resolve_public_listing_rung(ctx, cid, false) {
            ctx.unpin_native_roots(this_pin);
            return Err(failed);
        }
        // F2: include synthetic JDK declarations on each rung of the hierarchy
        // walk so a public method declared on a synthetic-stub superclass
        // (e.g. java/lang/ClassLoader) is still reachable from getMethod.
        let methods = declared_methods_with_synthetic(ctx, cid);
        for (meta_idx, meta) in methods.iter().enumerate() {
            if meta.name != target_name
                || meta.name == "<init>"
                || meta.name == "<clinit>"
                || (meta.access_flags & 0x0001) == 0
                || (!include_static && (meta.access_flags & 0x0008) != 0)
            {
                continue;
            }
            // Check parameter types. A `null` array is treated as a
            // ZERO-length one (JDK `Class.searchMethods` semantics вЂ” see
            // `getDeclaredMethod` above / HIB-CV-36): it matches ONLY a
            // no-arg method, never the first same-named overload.
            let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);
            // gen r4w3/rooting: current address after any earlier GC point.
            let param_types_arr = match (param_types_arr, pt_pin) {
                (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
                _ => None,
            };
            let expected_count = match param_types_arr {
                Some(pt_arr) => ctx.array_length(pt_arr),
                None => 0,
            };
            if param_descs.len() != expected_count {
                continue;
            }
            if let (Some(pt_arr), Some(pt_pin)) = (param_types_arr, pt_pin) {
                let mut matched = true;
                for (i, pdesc) in param_descs.iter().enumerate() {
                    // gen r4w3/rooting: re-read after the previous element's
                    // `param_type_matches`.
                    let pt_arr = ctx.read_native_pin(pt_pin, pt_arr);
                    let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                        Value::Object(Some(m)) => m,
                        _ => {
                            matched = false;
                            break;
                        }
                    };
                    if !param_type_matches(ctx, expected_mirror, pdesc, meta.declaring_class_id) {
                        matched = false;
                        break;
                    }
                }
                if !matched {
                    continue;
                }
            }
            // Round 9 audit fix (HIGH #7): cache the cold-miss result.
            // Cache key uses the user-queried `class_id`; declaring is
            // the rung where the method was actually found. Skip if the
            // cache key could not be built (None mirror in param array).
            if cache_key_ok {
                ctx.link_resolver_insert_method(
                    class_id,
                    &target_name,
                    &cache_key_desc,
                    cid,
                    meta_idx as u32,
                );
            }
            // gen r4w3/rooting: `this`/array unused past here.
            ctx.unpin_native_roots(this_pin);
            let method_obj = create_method_object(ctx, meta)?;
            return Ok(Some(Value::Object(Some(method_obj))));
        }
        // Depth-first class lookup precedes interfaces: for a package-private
        // subclass that overrides an interface method, Class.getMethod must
        // retain that non-public declaring class so callers can subsequently
        // locate the public interface method themselves. Push interfaces first
        // so the LIFO stack visits the superclass next.
        for iface_id in ctx.class_interfaces(cid) {
            stack.push((iface_id, false));
        }
        if let Some(parent) = ctx.superclass_of(cid) {
            stack.push((parent, include_static));
        }
    }

    // WF6 вЂ” same last-resort synthesis as `getDeclaredMethod`. Keycloak's
    // launcher path occasionally hits `getMethod` instead of
    // `getDeclaredMethod`; both must produce a usable Method mirror for the
    // boot to continue.
    // gen r4w3/rooting: the walk's `param_type_matches` calls may have
    // collected; take current addresses for the fallbacks below.
    let this = ctx.read_native_pin(this_pin, this);
    let param_types_arr = match (param_types_arr, pt_pin) {
        (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
        _ => None,
    };
    if let Ok(Some(method_obj)) =
        wf_shim_synth_main_method(ctx, this, &target_name, param_types_arr)
    {
        ctx.unpin_native_roots(this_pin);
        return Ok(Some(Value::Object(Some(method_obj))));
    }

    let message = no_such_method_message(&*ctx, class_id, &target_name, param_types_arr);
    ctx.unpin_native_roots(this_pin);
    Err(cratonvm_types::error::RuntimeError::NoSuchMethodException { message }.into())
}

pub fn native_class_get_constructors(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getConstructors on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            let arr = empty_reflection_array(ctx, "java/lang/reflect/Constructor");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    served_listing_prepass(ctx, class_id, ListingDoor::Constructors, ListingLookup::All)?;

    let methods = ctx.declared_methods(class_id);
    let mut public_ctors: Vec<&MethodMetadata> = methods
        .iter()
        .filter(|m| m.name == "<init>" && (m.access_flags & 0x0001) != 0)
        .collect();
    if let Some(class_name) = ctx.class_name_of_id(class_id) {
        order_constructors_for_reflection(&class_name, &mut public_ctors);
    }

    let constructor_component = reflection_component_id(ctx, "java/lang/reflect/Constructor");
    let arr = build_mirror_array_comp(ctx, constructor_component, public_ctors.len(), |ctx, i| {
        Ok(create_constructor_object(ctx, public_ctors[i])?)
    })?;
    Ok(Some(Value::Object(Some(arr))))
}

pub fn native_class_get_constructor(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getConstructor on null".to_string()),
            }
            .into())
        }
    };
    let param_types_arr = match args.get(1) {
        Some(Value::Object(Some(arr))) => Some(*arr),
        _ => None,
    };

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchMethodException {
                message: "<init>".to_string(),
            }
            .into())
        }
    };
    // 2026-10-07 (item 3 of the i47-L5 page): the listings this door's JDK
    // method makes, in its order, marked by its JDK frames on a missing
    // signature type (`served_listing_prepass`).
    served_listing_prepass(ctx, class_id, ListingDoor::Constructor, ListingLookup::All)?;

    let methods = ctx.declared_methods(class_id);
    // gen r4w3/rooting: `param_type_matches` may load through a user loader
    // (GC point); pin the parameter-types array and re-read it after each.
    let pt_pin = param_types_arr.map(|arr| ctx.pin_native_root(arr));
    for meta in &methods {
        if meta.name != "<init>" || (meta.access_flags & 0x0001) == 0 {
            continue;
        }
        let (param_descs, _) = parse_descriptor_param_and_return(&meta.descriptor);

        if let (Some(pt_arr), Some(pt_pin)) = (param_types_arr, pt_pin) {
            let cur_arr = ctx.read_native_pin(pt_pin, pt_arr);
            let expected_count = ctx.array_length(cur_arr);
            if param_descs.len() != expected_count {
                continue;
            }
            let mut matched = true;
            for (i, pdesc) in param_descs.iter().enumerate() {
                // gen r4w3/rooting: current address after the last GC point.
                let pt_arr = ctx.read_native_pin(pt_pin, pt_arr);
                let expected_mirror = match ctx.get_array_element(pt_arr, i) {
                    Value::Object(Some(m)) => m,
                    _ => {
                        matched = false;
                        break;
                    }
                };
                if !param_type_matches(ctx, expected_mirror, pdesc, meta.declaring_class_id) {
                    matched = false;
                    break;
                }
            }
            if !matched {
                continue;
            }
        } else if !param_descs.is_empty() {
            continue;
        }

        if let Some(pin) = pt_pin {
            ctx.unpin_native_roots(pin);
        }
        let ctor_obj = create_constructor_object(ctx, meta)?;
        return Ok(Some(Value::Object(Some(ctor_obj))));
    }

    // gen r4w3/rooting: current address for the diagnostic message.
    let param_types_arr = match (param_types_arr, pt_pin) {
        (Some(arr), Some(pin)) => Some(ctx.read_native_pin(pin, arr)),
        _ => None,
    };
    let message = no_such_method_message(&*ctx, class_id, "<init>", param_types_arr);
    if let Some(pin) = pt_pin {
        ctx.unpin_native_roots(pin);
    }
    Err(cratonvm_types::error::RuntimeError::NoSuchMethodException { message }.into())
}

// --- Class.getInterfaces / Class.getModifiers ---

/// Resolve a lambda proxy's functional interface to a `ClassId`, preserving
/// the bootstrap's exact resolved identity before any host-scoped fallback.
///
/// Residual 4 follow-up (2026-07-20, core-spring-boot-test-config-data-and-classpath-scan-cluster-FIXED.md): both
/// `native_class_get_interfaces` and `native_class_get_generic_interfaces`
/// used a bare `ctx.class_id_by_name(&iface_name)` here — loader-blind.
/// Scoping `typesig_to_real_type`'s LATER resolution (inside the generic
/// branch) closed one race, but isolated runs could still occasionally hit
/// the original failure under heavy host contention: `getInterfaces()` is
/// commonly called before `getGenericInterfaces()` in the same reflective
/// walk, and *this* lookup — for the exact same interface name — was never
/// scoped at all. Whichever of these two call sites is the FIRST thing in
/// the whole process to touch `iface_name` decides which loader's copy gets
/// used everywhere downstream (both mint-or-reuse via the same flat global
/// store); under different execution timing a different one can win the
/// race. Scoping both to the lambda's own host loader removes the race
/// instead of just moving it.
pub fn lambda_functional_interface_id_loader_aware(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    iface_name: &str,
) -> Option<ClassId> {
    // The invokedynamic bootstrap resolves the SAM through the defining
    // class's loader and retains that ClassId on the call site.  Use that
    // authoritative identity first: host names are not unique under forked
    // loaders, so resolving the host by name can lose the exact loader and
    // make both the scoped and global fallbacks ambiguous.
    if let Some(iface_id) = ctx.lambda_functional_interface_id(class_id) {
        return Some(iface_id);
    }
    let host_id = lambda_proxy_host_class(&*ctx, class_id);
    host_id
        .and_then(|host_id| ctx.class_id_by_name_near(iface_name, host_id))
        .or_else(|| ctx.class_id_by_name(iface_name))
}

/// A lambda proxy's direct superinterfaces as HotSpot's spun class lists them
/// (`InnerClassLambdaMetafactory`, a de-duplicated `LinkedHashSet`): the
/// functional interface, then each `altMetafactory` `FLAG_MARKERS` interface
/// in bootstrap-argument order, then `java.io.Serializable` when
/// `FLAG_SERIALIZABLE` added it. The ONE list `getInterfaces()` and
/// `getGenericInterfaces()` both return (a lambda class has no `Signature`
/// attribute, so the generic form is the raw one).
///
/// Markers are resolved like the functional interface's by-name fallback:
/// through the host's loader first, then globally; a marker that resolves to
/// nothing is left out rather than reported as a wrong class.
pub fn lambda_proxy_interface_ids(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    iface_id: ClassId,
) -> Vec<ClassId> {
    // Interpreter round i6 wave 118, lane L3 (`CRATONVM_REFLECT_LAMBDA_PROXY_FACTS`):
    // the record's list, built by the same rule (functional interface,
    // markers, then `Serializable` for a flagged lambda none extends).
    if crate::nbflags().reflect_lambda_proxy_facts {
        if let Some(record) = ctx.lambda_proxy_record(class_id) {
            if !record.interfaces.is_empty() {
                return record.interfaces;
            }
        }
    }
    let mut iface_ids = vec![iface_id];
    let markers = ctx
        .lambda_proxy_serial_metadata(class_id)
        .map(|meta| meta.marker_interfaces)
        .unwrap_or_default();
    if !markers.is_empty() {
        let host_id = lambda_proxy_host_class(&*ctx, class_id);
        for name in &markers {
            let marker_id = host_id
                .and_then(|host_id| ctx.class_id_by_name_near(name, host_id))
                .or_else(|| ctx.class_id_by_name(name));
            if let Some(marker_id) = marker_id {
                if !iface_ids.contains(&marker_id) {
                    iface_ids.push(marker_id);
                }
            }
        }
    }
    // FLAG_SERIALIZABLE appends `java.io.Serializable`
    // (`isSerializable && !foundSerializableSupertype`); a lambda serializable
    // only because its functional interface extends it gets nothing added.
    if ctx.lambda_proxy_serializability(class_id)
        == cratonvm_native_api::LambdaSerializability::ByFlag
    {
        if let Some(ser_id) = ctx.class_id_by_name("java/io/Serializable") {
            if !iface_ids.contains(&ser_id) {
                iface_ids.push(ser_id);
            }
        }
    }
    iface_ids
}

pub fn native_class_get_interfaces(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.getInterfaces on null".to_string()),
            }
            .into())
        }
    };

    // WP2.5: if this is the synthetic `Proxy$Instance` class mirror,
    // return the most recently created proxy's interfaces array
    // (stored by `native_proxy_new_instance`). All proxies currently
    // share this class id, so we can't return per-instance interfaces
    // off the class itself вЂ” see [`proxy_last_interfaces`] for the
    // last-wins limitation (generated `$ProxyN` classes declare their
    // interfaces and never reach this fallback).
    //
    // Wave 95 (lane L3, `CRATONVM_REFLECT_GET_INTERFACES_LEAN`): the stored
    // name read here is kept for the strict-name step below, which read it
    // again.
    let lean = reflect_get_interfaces_lean_on(&*ctx);
    let mut slot1_name: Option<String> = None;
    if let Value::Object(Some(name_ref)) = ctx.get_field(this, 1) {
        if let Some(name) = ctx.read_string(name_ref) {
            let is_proxy_instance = name == "java/lang/reflect/Proxy$Instance";
            if lean {
                slot1_name = Some(name);
            }
            if is_proxy_instance {
                // bug nb-lib-gckeys В§2: read the GC-tracked last-proxy
                // interfaces array (rooted + remapped by the annotation-proxy
                // GC hooks) instead of the old raw `AtomicU64` pointer that
                // was never rooted and could dangle after a moving GC.
                if let Some(arr_ref) = proxy_last_interfaces(ctx.vm_identity()) {
                    return Ok(Some(Value::Object(Some(arr_ref))));
                }
                // No proxy has been created yet вЂ” fall through and
                // return an empty array.
                let empty = empty_reflection_array(ctx, "java/lang/Class");
                return Ok(Some(Value::Object(Some(empty))));
            }
        }
    }

    // bytebuddy_probe (agent-bb3) вЂ” Object short-circuit. Real JDK returns
    // an empty Class[] for java/lang/Object. If our class manager ever
    // hands back interfaces for Object (e.g. due to a synthetic-mirror
    // mixup where the reverse-map points at the wrong ClassId), the
    // ByteBuddy hierarchy walker treats them as super-types of Object
    // and the IllegalStateException reasserts. Force-empty here.
    //
    // bytebuddy_probe (agent-bb4) вЂ” read name via the STRICT helper so a
    // reverse-map alias of non-Object в†’ Object does NOT spuriously
    // return an empty interfaces array for a non-Object class C (which
    // legitimately implements interfaces). See `mirror_class_name_strict`.
    //
    // Lean (wave 95): `mirror_class_name_strict` answers the stored name when
    // it is non-empty, which is the string read above; only an empty or
    // missing one takes the strict reader's fallbacks, whose reverse-map
    // answer (`class_id_from_mirror`, the first step of `mirror_class_id`
    // too) is kept for the class-id step below. Nothing allocates in between.
    let mut lean_mirror_cid: Option<Option<ClassId>> = None;
    let strict_name = match slot1_name.filter(|s| !s.is_empty()) {
        Some(stored) => stored,
        None if lean => {
            let cid = ctx.class_id_from_mirror(this);
            lean_mirror_cid = Some(cid);
            let name = match cid {
                Some(cid) => ctx.class_name_of_id(cid),
                None => mirror_class_id(ctx, this).and_then(|cid| ctx.class_name_of_id(cid)),
            };
            name.unwrap_or_default()
        }
        None => mirror_class_name_strict(ctx, this).unwrap_or_default(),
    };
    let dbg_bb = dbg_bb_enabled();
    // `this_name` is read only by the `CRATONVM_DBG_BB` lines.
    let this_name = if lean && !dbg_bb {
        String::new()
    } else if !strict_name.is_empty() {
        strict_name.clone()
    } else {
        mirror_class_name(ctx, this).unwrap_or_default()
    };
    if strict_name == "java/lang/Object" || strict_name == "java.lang.Object" {
        if dbg_bb {
            eprintln!(
                "[bb-dbg] getInterfaces({}) -> [] [object-early-strict]",
                this_name
            );
        }
        let empty = empty_reflection_array(ctx, "java/lang/Class");
        return Ok(Some(Value::Object(Some(empty))));
    }

    let class_id = match lean_mirror_cid {
        Some(Some(cid)) => Some(cid),
        _ => mirror_class_id(ctx, this),
    };
    let class_id = match class_id {
        Some(id) => id,
        None => {
            if dbg_bb {
                eprintln!("[bb-dbg] getInterfaces({}) -> [] [no-class-id]", this_name);
            }
            let arr = empty_reflection_array(ctx, "java/lang/Class");
            return Ok(Some(Value::Object(Some(arr))));
        }
    };

    // Synthetic lambda proxies aren't in the class store (so `class_interfaces`
    // is empty), but a lambda's concrete class implements exactly its functional
    // (SAM) interface. Return `[SAM]` so reflective type/listener matching that
    // walks `getInterfaces()` finds it (matches HotSpot's `$$Lambda` class).
    if let Some(iface_name) = ctx.lambda_functional_interface(class_id) {
        if let Some(iface_id) =
            lambda_functional_interface_id_loader_aware(ctx, class_id, &iface_name)
        {
            // A lambda whose call site passed FLAG_SERIALIZABLE gets
            // `java.io.Serializable` APPENDED to the spun class's interfaces by the
            // real metafactory (`isSerializable && !foundSerializableSupertype`).
            // One that is serializable only because its functional interface already
            // extends Serializable does NOT -- it is reachable transitively.
            // `altMetafactory` marker interfaces sit between the two.
            let iface_ids = lambda_proxy_interface_ids(ctx, class_id, iface_id);
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getInterfaces({}) -> [{}] [lambda, {} iface(s)]",
                    this_name,
                    iface_name,
                    iface_ids.len()
                );
            }
            // GC-safe mirror materialisation, as in the non-lambda path below.
            let class_comp = if lean {
                interfaces_class_component_id(ctx)
            } else {
                class_component_id(ctx)
            };
            let arr = build_mirror_array_comp(ctx, class_comp, iface_ids.len(), |ctx, i| {
                Ok(ctx.get_class_mirror(iface_ids[i]))
            })?;
            return Ok(Some(Value::Object(Some(arr))));
        }
    }

    let iface_ids = ctx.class_interfaces(class_id);
    if dbg_bb {
        let names: Vec<String> = iface_ids
            .iter()
            .map(|id| ctx.class_name_of_id(*id).unwrap_or_default())
            .collect();
        eprintln!("[bb-dbg] getInterfaces({}) -> {:?}", this_name, names);
    }
    // GC-safe: `get_class_mirror` may allocate a mirror (see `build_mirror_array`).
    // Component type is `Class` (HotSpot returns `[Ljava/lang/Class;`), not Object.
    let class_comp = if lean {
        interfaces_class_component_id(ctx)
    } else {
        class_component_id(ctx)
    };
    let arr = build_mirror_array_comp(ctx, class_comp, iface_ids.len(), |ctx, i| {
        Ok(ctx.get_class_mirror(iface_ids[i]))
    })?;
    Ok(Some(Value::Object(Some(arr))))
}

/// True if `mirror` is a primitive-type or `void` Class mirror -- exactly
/// [`mirror_is_primitive`], i.e. `Class.isPrimitive()`'s answer.
///
/// JIT round 20 (lane strings, r19-class-mirror-is-primitive-reads-a-hard-coded-slot):
/// this used to read instance slot `7` and treat any non-zero `Int` there as
/// "primitive", a second, independently derived answer next to the by-name
/// `primitive` slot `Class.isPrimitive` reads. Slot 7 is only the synthetic
/// stub's `LEGACY_PRIMITIVE_SLOT`; on a real image it is whatever field the
/// layout put there (JDK 21 has no `primitive` field at all, and an `int` such as
/// `classRedefinedCount` at 7 would have made `getModifiers()` answer 0x411 for an
/// ordinary class). The synthetic class library is still covered: its stub has
/// no named `primitive` field, so `mirror_is_primitive` falls to the name check,
/// as `Class.isPrimitive` already does there.
pub fn class_mirror_is_primitive(ctx: &mut dyn NativeContext, mirror: ObjectRef) -> bool {
    mirror_is_primitive(&*ctx, mirror)
}

/// The PUBLIC/PROTECTED/PRIVATE access bits an array class inherits from its
/// element type (per JVM_GetClassModifiers). `name` is an array class name in
/// descriptor form, e.g. `[I`, `[[Ljava/lang/String;`. Primitive element types
/// are PUBLIC; reference element types contribute their own access bits.
pub fn array_element_access_bits(ctx: &mut dyn NativeContext, name: &str) -> i32 {
    const ACCESS_MASK: u16 = 0x0001 | 0x0002 | 0x0004; // PUBLIC|PRIVATE|PROTECTED
    let elem = name.trim_start_matches('[');
    if elem.starts_with('L') && elem.ends_with(';') {
        // Reference element: L<internal-name>; вЂ” look up its access flags.
        let internal = elem[1..elem.len() - 1].replace('.', "/");
        if let Some(cid) = ctx.class_id_by_name(&internal) {
            return (ctx.class_access_flags(cid) & ACCESS_MASK) as i32;
        }
        // Unresolved element type: default to PUBLIC (matches the common case
        // and HotSpot for the public-element arrays that dominate).
        0x0001
    } else {
        // Primitive element (I, Z, J, ...) вЂ” primitives are PUBLIC.
        0x0001
    }
}

/// Do the reflection / security Bridges that stood over JDK 25 bytecode and
/// answered other than it yield to that bytecode (are not registered) over a
/// real JDK, in both modes? `Class.desiredAssertionStatus()` (the JDK body
/// consults the loader's `setDefaultAssertionStatus` /
/// `setClassAssertionStatus` / `setPackageAssertionStatus` table, which the
/// Bridge ignored) and `AccessControlContext.<init>(ProtectionDomain[])` (the
/// JDK body keeps the domains and throws `NullPointerException` for a null
/// array; the Bridge dropped them). Under the same switch the
/// `ClassLoader.setDefaultAssertionStatus` Bridge also runs the JDK body's
/// `initializeJavaAssertionMaps()` step, without which the loader's table the
/// first body reads never exists. Interpreter round i1 wave 51, lane L3
/// (`docs/feature-designs/native-dispatch-architecture.md`;
/// probe `L3W51ReflectionBridgesYield.java`).
/// `CRATONVM_REFLECTION_JDK_BODIES`, read once per registry (per VM), at
/// registration; default on since the lane's last commit (`=0` keeps the
/// Bridges). The synthetic class library (`real_jdk == false`, the unit
/// tests' registries) keeps them always.
pub fn reflection_jdk_bodies(r: &cratonvm_native_api::NativeMethodRegistry) -> bool {
    r.real_jdk()
}

/// Does `Class.getModifiers()` yield to JDK 25's body (`return modifiers;`)
/// over a real JDK, in both modes? Since interpreter round i1 wave 52 (lane
/// L3) the VM writes HotSpot's `modifier_flags` into every mirror's
/// `modifiers` field when it makes the mirror (`vm_object.rs`,
/// `hotspot_class_modifiers`: the rule [`native_class_get_modifiers`]
/// implements -- primitive, array, lambda proxy, InnerClasses, no
/// `ACC_SUPER`), so the Bridge no longer has to stand over the bytecode
/// (`docs/feature-designs/native-dispatch-architecture.md`;
/// probe `L3W52ClassMirrorModifiers.java`).
/// `CRATONVM_CLASS_MODIFIERS_JDK_BODY`, read once per registry (per VM), at
/// registration; default on since the lane's last commit (`=0` keeps the
/// Bridge). The synthetic class library keeps the Bridge always (its
/// `Class` stub has no `modifiers` field), and so does `isSynthetic`, which
/// calls the Bridge's function directly.
///
/// Never on a JDK 17/21 image: there `Class.getModifiers` is still `native`
/// (JDK 25 is the first image whose `Class` has the `modifiers` field and
/// the body), so yielding left it with no implementation at all, and the
/// default `--real-jdk` boot died in `EnumSet` (`UnsatisfiedLinkError`).
pub fn class_modifiers_jdk_body(r: &cratonvm_native_api::NativeMethodRegistry) -> bool {
    r.real_jdk()
        && !r.real_jdk_older_than(25)
}

/// The switch behind [`class_shape_jdk_body`] (JIT round 19 wave 1, lane
/// classshape; proposal BUF18-6). Perf, default ON (`=0` restores the three
/// natives): `R19ClassshapePredicates` `is-array` 355 -> 4 ns/op,
/// `mirror-facts` 436 -> 13 ns/op, every `shape` / `fact` line as HotSpot 25
/// in both modes.
pub const CLASS_SHAPE_JDK_BODY_ENV: &str = "CRATONVM_CLASS_SHAPE_JDK_BODY";

/// Do `Class.isArray()`, `isInterface()` and `isPrimitive()` yield to JDK 25's
/// bodies over a real JDK, in both modes?
///
/// JDK 25 answers the three from fields of the mirror (`componentType != null`,
/// `Modifier.isInterface(modifiers)`, `primitive`), which HotSpot inlines into
/// a getfield and a compare. CratonVM registered a native in front of each, so
/// every call was a native crossing (the dispatch funnel, then
/// `class_id_from_mirror`), and a compiled caller cannot splice a native:
/// `docs/feature-designs/native-dispatch-architecture.md`. The
/// mirror populator (`vm/src/vm/vm_object.rs`, `class_mirror_impl` /
/// `get_or_create_primitive_mirror`) writes all three fields HotSpot-true for
/// every mirror it mints: `componentType` for every array class (primitive and
/// reference components), `modifiers` = HotSpot's `modifier_flags` (the value
/// [`class_modifiers_jdk_body`] already trusts in both modes, so ACC_INTERFACE
/// is exact), `primitive` only for the nine real primitive mirrors. The
/// descriptor-backed stand-in mirrors ([`synthetic_class_mirror`]) get the
/// same fields written under this switch (see there).
///
/// `CRATONVM_CLASS_SHAPE_JDK_BODY` (default ON), read once per registry (per
/// VM), at registration. Never on a JDK 17/21 image (`Class.isInterface` /
/// `isPrimitive` / `isArray` are `native` there) and never on the synthetic
/// class library (`real_jdk == false`).
pub fn class_shape_jdk_body(r: &cratonvm_native_api::NativeMethodRegistry) -> bool {
    r.real_jdk()
        && !r.real_jdk_older_than(25)
        && cratonvm_types::flags::runtime_flag_default_on(CLASS_SHAPE_JDK_BODY_ENV)
}

/// The switch behind [`class_component_jdk_body`] (lane classshape's
/// CLASSSHAPE19-1, landed 2026-10-07). Perf, default ON (`=0` restores the
/// two natives).
pub const CLASS_COMPONENT_JDK_BODY_ENV: &str = "CRATONVM_CLASS_COMPONENT_JDK_BODY";

/// Do `Class.getComponentType()` and `componentType()` yield to JDK 25's
/// bodies over a real JDK, in both modes?
///
/// JDK 25 answers both from the mirror's `componentType` field
/// (`isArray() ? componentType : null`), so they splice once `isArray` does
/// ([`class_shape_jdk_body`]): `R19ClassComponentCensus get-component-type`
/// 250-490 -> 10-27 ns/op in both modes (HotSpot 10). `descriptorString()` stays
/// native under `--compatible` on cost (see its registration). The
/// registrations' premise, "CratonVM array
/// mirrors leave `componentType` null", is stale: the populator writes it for
/// every array class from the array class's own component (`array_info`, so a
/// child loader's `X[]` carries that loader's `X`), and stand-in mirrors carry
/// their loader's component ([`synthetic_class_mirror_for_loader`]).
/// `R19ClassComponentCensus` (35 lines: primitive, multi-dimensional, nested,
/// local, anonymous, lambda, hidden and child-loader arrays, an uninitialized
/// `Class.forName("[L..;")` component, `MethodType` and `ObjectStreamField`
/// descriptors) equals HotSpot 25 in both modes with the bodies.
///
/// `CRATONVM_CLASS_COMPONENT_JDK_BODY` (default ON), read once per registry
/// (per VM), at registration. Never on a JDK 17/21 image and never on the
/// synthetic class library.
pub fn class_component_jdk_body(r: &cratonvm_native_api::NativeMethodRegistry) -> bool {
    r.real_jdk()
        && !r.real_jdk_older_than(25)
        && cratonvm_types::flags::runtime_flag_default_on(CLASS_COMPONENT_JDK_BODY_ENV)
}

/// Write the three shape fields JDK 25's `Class.isArray` / `isPrimitive` /
/// `isInterface` bodies read into a descriptor-backed stand-in mirror, so the
/// bodies answer what the natives answered from the name: `componentType` for
/// an array name (the component's mirror, resolved the way
/// [`descriptor_to_class_mirror`] resolves any descriptor; a stand-in again
/// when the component cannot be loaded, so the recursion ends at the leaf),
/// `primitive` + HotSpot's `PUBLIC|FINAL|ABSTRACT` for one of the nine
/// primitive names. `modifiers` of any other stand-in stays 0, which is what
/// `native_class_is_interface` answered (no class id: `false`).
///
/// Only under [`CLASS_SHAPE_JDK_BODY_ENV`] (lane classshape, round 19), and only
/// when `java/lang/Class` declares `primitive` (JDK 25's field-reading bodies;
/// a JDK 21 image or the synthetic class library keeps the natives and has no
/// such field, so nothing is written and no store is dropped). GC: the component lookup can
/// allocate, so `mirror` is pinned across it and the forwarded reference is
/// returned.
pub fn synthetic_mirror_shape_fields(
    ctx: &mut dyn NativeContext,
    mirror: cratonvm_types::ObjectRef,
    name: &str,
    loader: Option<cratonvm_types::ObjectRef>,
) -> cratonvm_types::ObjectRef {
    if matches!(
        name,
        "int" | "long" | "float" | "double" | "boolean" | "char" | "byte" | "short" | "void"
    ) {
        ctx.set_field_by_name(mirror, "primitive", Value::Int(1));
        ctx.set_field_by_name(mirror, "modifiers", Value::Int(0x0411));
        return mirror;
    }
    let Some(component_desc) = name.strip_prefix('[').filter(|d| !d.is_empty()) else {
        return mirror;
    };
    let pin = ctx.pin_native_root(mirror);
    let component = match loader {
        Some(l) => stand_in_component_for_loader(ctx, component_desc, l),
        None => descriptor_to_class_mirror(ctx, component_desc),
    };
    let mirror = ctx.read_native_pin(pin, mirror);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(mirror, "componentType", Value::Object(Some(component)));
    mirror
}

/// The component mirror of a stand-in array that belongs to `loader`
/// (CLASSSHAPE19-2), resolved the way `native_class_get_component_type`
/// resolves a stand-in's component: in `loader`'s own namespace first (the
/// loader-keyed array class for a nested array, else another stand-in of that
/// loader); only a name `loader` did not define goes through
/// [`descriptor_to_class_mirror`] (a delegated, built-in-loader class). The
/// caller keeps `loader` pinned across this call.
pub fn stand_in_component_for_loader(
    ctx: &mut dyn NativeContext,
    component_desc: &str,
    loader: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    let Some(ns) = crate::classloader::peek_loader_namespace_id(ctx, loader) else {
        return descriptor_to_class_mirror(ctx, component_desc);
    };
    let leaf = component_desc
        .trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'));
    // A primitive component (`I`) or an array of one (`[I`) has no loader; a
    // leaf `loader` did not define is a delegated class.
    let Some(leaf_cid) = leaf.and_then(|l| ctx.class_id_defined_by_loader_exact(l, ns)) else {
        return descriptor_to_class_mirror(ctx, component_desc);
    };
    if !component_desc.starts_with('[') {
        return ctx.get_class_mirror(leaf_cid);
    }
    if let Some(arr) = ctx.array_class_id_for_loader(component_desc, ns) {
        return ctx.get_class_mirror(arr);
    }
    synthetic_class_mirror_for_loader(ctx, component_desc, loader)
}

pub fn native_class_get_modifiers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };

    // Primitive types and `void`: HotSpot's JVM_GetClassModifiers returns
    // PUBLIC|FINAL|ABSTRACT (0x411). CratonVM's primitive mirrors carry no
    // class access_flags, so without this they read back as 0 вЂ” i.e.
    // "package-private". That breaks reflective callers that test the access
    // of a method's primitive *return* or *parameter* types. Concretely,
    // ByteBuddy's `isPackagePrivate()` element-matcher, which Mockito uses in
    // `ignoreAlso(returns(isPackagePrivate()).or(hasParameters(whereAny(
    // hasType(isPackagePrivate())))))`, was matching every primitive-signature
    // method, so Mockito left them unimplemented on the generated mock в†’
    // `AbstractMethodError` at first use (kafka suite bug-09).
    const ACC_PUBLIC: i32 = 0x0001;
    const ACC_FINAL: i32 = 0x0010;
    const ACC_ABSTRACT: i32 = 0x0400;
    if class_mirror_is_primitive(ctx, this) {
        return Ok(Some(Value::Int(ACC_PUBLIC | ACC_FINAL | ACC_ABSTRACT)));
    }

    // Array classes: JVM_GetClassModifiers returns FINAL|ABSTRACT plus the
    // element type's PUBLIC/PROTECTED/PRIVATE bits. (CratonVM previously
    // returned the bare array flags 0x011 = public|final, missing ABSTRACT and
    // not deriving access from the element type.)
    //
    // The element's bits are ITS `getModifiers()` bits -- the InnerClasses
    // attribute's flags for a nested element class, which is where `private`
    // and `protected` live (javac writes a `protected` nested class's own
    // `access_flags` as PUBLIC and a `private` one's as package-private).
    // Reading the class file's own `access_flags` answered
    // `PrivNested[].class` 0x410 where HotSpot answers 0x412 (and 0x411 for
    // 0x414 on a protected element), and `computeDefaultSUID` hashes these
    // (interpreter round i1 wave 29, lane L4;
    // `L4W29ClassReflectionShapes.java`).
    //
    // The name is resolved ONCE (SBR2 W9 SMALL2: it was resolved a second time,
    // for an array test the branch below had already answered, which was a
    // `String` built per call on the ordinary-class path).
    //
    // NIO2-17-5 rung 3 (JIT round 19 wave 3, lane funnel3;
    // `CRATONVM_CLASS_FACTS_MEMO_MIRROR`): the name is used only by the two
    // array arms below, so a mirror the class-facts memo says is not an
    // array skips rendering it.
    let this_name = match mirror_rung_facts(ctx, this) {
        Some(facts) if !facts.is_some_and(crate::class_facts_memo::NameFacts::is_array) => None,
        _ => mirror_class_name(ctx, this),
    };
    if let Some(name) = this_name.as_deref() {
        // An array mirror has a `componentType`; an ordinary class NAMED like
        // an array (`[Lp/X;`, which a user loader defines under `-Xverify:none`
        // with no `Bad class name` check) is not one and answers its own flags
        // below, as on HotSpot (t1 wave 13, lane I24W, probe
        // `T1W13I24WStructuralFormat.java` row S05: 0x401).
        if name.starts_with('[')
            && (matches!(ctx.get_field_by_name(this, "componentType"), Value::Object(Some(_)))
                || mirror_class_id(ctx, this).and_then(|id| ctx.array_component_class_id(id)).is_some())
        {
            let mut element = mirror_class_id(ctx, this);
            let dims = name.bytes().take_while(|b| *b == b'[').count();
            let mut descents = 0;
            while let Some(id) = element {
                match ctx.array_component_class_id(id) {
                    Some(component) => {
                        element = Some(component);
                        descents += 1;
                    }
                    None => break,
                }
            }
            // A chain that stopped short is still sitting on an ARRAY class,
            // whose own flags are not the element's.
            if descents < dims {
                element = None;
            }
            // Interpreter round i5, build w102b: an array of a HIDDEN element
            // (`X$$Lambda/0x...[]`) has no id-linked component in the class
            // table and its element name is not loadable, so the name lookup
            // below defaulted it PUBLIC (0x411 where HotSpot answers 0x410).
            // Walk the mirrors' `componentType` chain to the element mirror
            // instead (probe `L3W102ClassNameShapes.java`).
            let elem_by_mirror = || {
                let mut node = this;
                let mut steps = 0;
                while let Value::Object(Some(c)) = ctx.get_field_by_name(node, "componentType") {
                    node = c;
                    steps += 1;
                    if steps > 255 {
                        break;
                    }
                }
                if steps == 0 {
                    None
                } else {
                    mirror_class_id(ctx, node)
                }
            };
            let is_ref = name.trim_start_matches('[').starts_with('L');
            let element = if is_ref {
                element.or_else(elem_by_mirror)
            } else {
                None
            };
            let elem_access = match element {
                // A reference element class the class table knows by id.
                Some(id) => effective_class_modifiers(ctx, id) & 0x0007,
                None => array_element_access_bits(ctx, &name),
            };
            return Ok(Some(Value::Int(ACC_FINAL | ACC_ABSTRACT | elem_access)));
        }
    }

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };

    // Lambda proxies are JVM hidden classes вЂ” HotSpot reports them as
    // `final synthetic` (0x1010, verified on JDK 25). Without this they carry no
    // class-store access flags and read back 0, so `Class.isSynthetic()` is false
    // and Spring's `ClassUtils.isLambdaClass()` (which requires `isSynthetic()`)
    // misclassifies every lambda. bug-06 fam5 #1.
    const ACC_SYNTHETIC: i32 = 0x1000;
    if is_lambda_proxy_id(class_id) && ctx.lambda_proxy_host(class_id).is_some() {
        return Ok(Some(Value::Int(ACC_FINAL | ACC_SYNTHETIC)));
    }

    // Array classes: HotSpot's JVM_GetClassModifiers returns the element type's
    // accessibility (PUBLIC/PROTECTED/PRIVATE) OR'd with FINAL|ABSTRACT вЂ” every
    // array class is `final abstract`. Our array Class mirrors carry only the
    // PUBLIC|FINAL bits in their access_flags (ABSTRACT 0x400 is dropped), so
    // `int[].class.getModifiers()` returned 0x11 vs HotSpot's 0x411. Because
    // `getModifiers()` feeds `ObjectStreamClass.computeDefaultSUID`, the missing
    // ABSTRACT bit produced a WRONG serialVersionUID for every array class вЂ”
    // e.g. a worker writing a `byte[]`/`int[]`/`String[]` message field gets a
    // SUID the peer rejects with InvalidClassException, desyncing the Gradle
    // workerв†”daemon stream. Synthesize the JVM-faithful value here.
    if let Some(name) = this_name.as_deref() {
        if name.starts_with('[') {
            let comp = name.trim_start_matches('[');
            // Object element в†’ use the element class's accessibility bits;
            // primitive element (B/I/J/вЂ¦) в†’ public.
            let access =
                if let Some(stripped) = comp.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
                    ctx.class_id_by_name(stripped)
                        .map(|cid| ctx.class_access_flags(cid) & 0x0007)
                        .unwrap_or(0x0001)
                } else {
                    0x0001
                };
            // | ACC_FINAL (0x10) | ACC_ABSTRACT (0x400)
            return Ok(Some(Value::Int((access | 0x0410) as i32)));
        }
    }

    // JVMS В§4.7.6: for nested classes, Class.getModifiers() returns the
    // `inner_class_access_flags` from the InnerClasses attribute entry whose
    // `inner_class_info_index` points at this class вЂ” NOT the class's own
    // `access_flags`. The class's own access_flags does not carry ACC_STATIC
    // (our `ClassAccessFlags` bitflag set does not even contain a STATIC
    // variant, since STATIC is not legal in the top-level ClassFile flags),
    // whereas the InnerClasses entry does.  This matters for reflection code
    // such as Jackson's check `!Modifier.isStatic(cls.getModifiers())` on a
    // `public static class` nested inside another class.
    //
    // The canonical location for the InnerClasses entry is the class itself
    // (javac emits an InnerClasses attribute on every class that references
    // a nested class, including the nested class's own class file referring
    // to itself).  We probe the class's inner_classes list for an entry
    // whose `inner_class` name equals this class's name; if found we return
    // those flags, otherwise we fall back to the class's own access_flags.
    Ok(Some(Value::Int(effective_class_modifiers(ctx, class_id))))
}

/// `Class.getClassAccessFlagsRaw0()`: the class file's OWN `access_flags`,
/// `ACC_SUPER` included and no InnerClasses substitution -- the value
/// `Class.accessFlags()` parses for a top-level class, and the one place the
/// JDK exposes `SUPER`. It used to be `getModifiers`, which masks `0x20`, so
/// `String.class.accessFlags()` lost `SUPER` (interpreter round i1 wave 29,
/// lane L4; `L4W29ClassReflectionShapes.java`).
/// Primitives are `PUBLIC | ABSTRACT | FINAL`; a lambda proxy, which has no
/// class file here, reads as HotSpot's hidden lambda class does,
/// `FINAL | SUPER | SYNTHETIC`. Arrays never arrive: the JDK body asks the
/// element type.
pub fn native_class_get_class_access_flags_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if class_mirror_is_primitive(ctx, this) {
        return Ok(Some(Value::Int(0x0411)));
    }
    let Some(class_id) = mirror_class_id(ctx, this) else {
        return Ok(Some(Value::Int(0)));
    };
    if is_lambda_proxy_id(class_id) && ctx.lambda_proxy_host(class_id).is_some() {
        return Ok(Some(Value::Int(0x1030)));
    }
    Ok(Some(Value::Int(i32::from(
        ctx.class_access_flags(class_id),
    ))))
}

/// `Class.getModifiers()` of an ordinary (non-array, non-primitive,
/// non-lambda) class: the InnerClasses entry's flags when it is nested, else
/// its own `access_flags`, with `ACC_SUPER` masked out (both rules are
/// explained inline). An array class takes its element's access bits from
/// here too.
pub fn effective_class_modifiers(ctx: &dyn NativeContext, class_id: ClassId) -> i32 {
    // Memoised per class (SBR2 W9 SMALL2): the answer is a pure function of the
    // class's own `access_flags` and InnerClasses entry, which cannot change
    // once the class is defined (class ids are never reused; a redefinition may
    // not change a class's modifiers). Computing it cloned the whole
    // InnerClasses attribute and built the class name `String` on every call --
    // `Class.getModifiers` was 0.7-1.2 us against a ~0.2 us native crossing.
    let key = (ctx.vm_identity(), class_id.as_u32());
    if let Some(m) = CLASS_MODIFIERS_CACHE
        .get_or_init(Default::default)
        .read()
        .get(&key)
    {
        return *m;
    }
    // A redefinition can swap the `InnerClasses` row this reads (wave 17); a
    // fill that raced it is not kept (wave 19, [`inner_tables_fill_guard`]).
    let fill = inner_tables_fill_guard(&*ctx);
    let modifiers = effective_class_modifiers_uncached(ctx, class_id);
    // A class the table does not name (yet) answers from absent data: not a fact.
    if ctx.class_name_arc_of_id(class_id).is_some() && !fill.moved(&*ctx) {
        CLASS_MODIFIERS_CACHE
            .get_or_init(Default::default)
            .write()
            .insert(key, modifiers);
        if fill.moved(&*ctx) {
            CLASS_MODIFIERS_CACHE
                .get_or_init(Default::default)
                .write()
                .remove(&key);
        }
    }
    modifiers
}

// [`effective_class_modifiers`]'s memo: `(vm identity, class id)` to the answer.
// addr-keyed: not a heap address -- (VM identity, class id); class ids are never reused within a VM.
pub static CLASS_MODIFIERS_CACHE: OnceLock<RwLock<FxHashMap<(usize, u32), i32>>> = OnceLock::new();

pub fn effective_class_modifiers_uncached(ctx: &dyn NativeContext, class_id: ClassId) -> i32 {
    let own_flags = ctx.class_access_flags(class_id);
    let effective_flags = if let Some(this_name) = ctx.class_name_of_id(class_id) {
        ctx.inner_classes(class_id)
            .into_iter()
            .find(|(inner, _outer, _name, _flags)| *inner == this_name)
            .map(|(_inner, _outer, _name, flags)| flags)
            .unwrap_or(own_flags)
    } else {
        own_flags
    };
    // Strip ACC_SUPER (0x0020): it is a JVM-internal class-file flag (legacy
    // invokespecial semantics), set on virtually every modern class, but it is
    // NOT a Java language modifier. HotSpot's JVM_GetClassModifiers masks it out
    // (JVM_RECOGNIZED_CLASS_MODIFIERS excludes 0x0020), so `getModifiers()`
    // returns e.g. 0x1 (public) not 0x21. ByteBuddy validates generated-subclass
    // modifiers against its recognized set and throws "Illegal modifiers 33" when
    // the 0x20 bit leaks through `Note.class.getModifiers()` вЂ” breaking
    // Hibernate's ByteBuddy lazy-proxy generation. (ACC_SUPER never appears in
    // InnerClasses access flags, so masking is safe on the nested-class path too.)
    let masked = effective_flags & !0x0020u16;
    i32::from(masked)
}

// ---------------------------------------------------------------------------
// Annotation support (Phase 20)
// ---------------------------------------------------------------------------

/// Annotation proxy: 4-field synthetic object
///   field 0 = String (annotation type descriptor, e.g. "Ljava/lang/Override;")
///   field 1 = Class mirror (annotation type class, or null)
///   field 2 = String[] (element names)
///   field 3 = Object[] (element values вЂ” boxed primitives, Strings, Class mirrors, etc.)
pub const ANN_PROXY_FIELDS: usize = 4;
pub const ANN_PROXY_TYPE_DESC: usize = 0;
pub const ANN_PROXY_TYPE_MIRROR: usize = 1;
pub const ANN_PROXY_ELEM_NAMES: usize = 2;
pub const ANN_PROXY_ELEM_VALUES: usize = 3;

/// Convert an annotation type descriptor to an internal class name.
/// E.g. "Ljava/lang/Override;" -> "java/lang/Override"
pub fn annotation_desc_to_class_name(desc: &str) -> Option<&str> {
    if desc.starts_with('L') && desc.ends_with(';') {
        Some(&desc[1..desc.len() - 1])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Reflection annotation-proxy identity cache
//
// HotSpot caches annotation instances per class (`Class.annotationData`), so
// repeated `getAnnotation(X)` / `getDeclaredAnnotations()` on the SAME class
// return the SAME instance (`a1 == a2`). CratonVM previously rebuilt a fresh
// proxy on every call, so `getAnnotation(X) != getAnnotation(X)` вЂ” breaking
// identity-sensitive callers (annotations used as `IdentityHashMap` keys, or
// caches keyed on the annotation instance).
//
// We cache the proxy keyed by a reflection holder identity plus annotation type.
// Class entries mirror HotSpot's per-class `annotationData`; method entries add
// the exact (name, descriptor) because two methods on one class can carry
// different values of the same annotation type. The cached `ObjectRef`s live
// only in this process-global side-table, invisible to the heap field scan, so
// they MUST be GC-rooted and remapped вЂ” see `gc_scan_annotation_proxy_roots`
// (roots.rs) and `gc_update_annotation_proxy_refs` (gc.rs). Without that, a
// moving young GC would relocate a cached proxy and the next read would return
// a stale `ObjectRef` (use-after-free).
// ---------------------------------------------------------------------------

/// Keyed by `vm_identity` because the rest of the key is a `ClassId`, and
/// every VM mints those from zero: without the partition VM 2 looked up
/// `(42, "Lfoo/Inherited;")`, hit VM 1's entry, and returned a proxy allocated
/// in a heap that no longer existed. Sequentially that is a freed object;
/// concurrently it is a live address in the *wrong* heap, and
/// `safe_native_call`'s return-value `load_and_forward` segfaulted reading its
/// header (`test_s50_ann_inheritedValue`, 2 runs in 3).
pub static ANNOTATION_PROXY_CACHE: VmScoped<FxHashMap<(u32, String), ObjectRef>> = VmScoped::new();

// Per-proxy child roots, keyed the same way. The inner key is a raw heap
// address, which is likewise only meaningful inside one heap.
// addr-keyed: REMAPPED by gc_update_annotation_proxy_refs, PRUNED by prune_dead_annotation_proxies / forget_unloaded_annotation_proxies.
pub static ANNOTATION_PROXY_CHILD_ROOTS: VmScoped<FxHashMap<usize, Vec<ObjectRef>>> = VmScoped::new();

/// The most recently created proxy's interfaces array — one cell per VM, since
/// the array itself is a heap object.
pub static PROXY_LAST_INTERFACES: VmScoped<Option<ObjectRef>> = VmScoped::new();

/// Per-VM teardown for the three side-tables above. Called from
/// `release_vm_native_state`.
///
/// Also drops the VM's `Class$Atomic` reflection side store
/// (`crate::forget_vm_class_atomic_side_store`), made per-VM in gc-common
/// w3-b: the same per-`Class` reflection state, the same teardown point, and
/// no second call site to add in `vm_init.rs`.
pub fn forget_vm_annotation_proxies(vm_identity: usize) {
    ANNOTATION_PROXY_CACHE.forget(vm_identity);
    ANNOTATION_PROXY_CHILD_ROOTS.forget(vm_identity);
    PROXY_LAST_INTERFACES.forget(vm_identity);
    crate::forget_vm_class_atomic_side_store(vm_identity);
    // gc-common w20-f: the annotation-element `TypeNotPresentException`
    // sentinel, the same annotation-parsing state.
    UNRESOLVABLE_CLASS_SENTINEL.forget(vm_identity);
    // Round 16 wave 13 (lane annorder): the CDS symbol index of this VM's
    // `java.home` (`annotation_cds_member_order`).
    CDS_SYMBOL_INDEX.forget(vm_identity);
    // Round 16 wave 16 (lane proxy6): the handed-out
    // `AnnotationInvocationHandler`s and the per-type member-order memo.
    ANNOTATION_JDK_HANDLERS.forget(vm_identity);
    ANNOTATION_MEMBER_ORDER_MEMO.forget(vm_identity);
}

/// Drop every cached annotation proxy this VM's class unloading invalidated.
///
/// A row is `(holder_class_id, annotation_descriptor) -> proxy_instance`, and
/// **class unloading can kill either side** — the same shape
/// `forget_unloaded_proxy_classes` exists to handle for the generated-`$ProxyN`
/// *class* cache, one level down at the *instance*.
///
/// The VALUE is the half that bites. A cached proxy is an instance of a
/// generated `jdk/proxyN/$ProxyM`, and that class's defining loader is the
/// annotation "container" loader — for Spring, one of the per-context
/// isolation loaders (`OverridingClassLoader`, the cglib enhancer's). When a
/// context is discarded its loader becomes unreachable and
/// `ClassManager::unload_user_classes` retires the generated class with it.
/// The cached INSTANCE survives that: it is a GC root in its own right (see
/// [`gc_scan_annotation_proxy_roots`]), so the collector keeps the object and
/// nothing keeps its class. Every later `getDeclaredAnnotations()` on the same
/// holder then hits this cache, returns that instance, and resolving its class
/// throws `NoClassDefFoundError: jdk/proxyN/$ProxyM`.
///
/// That is not hypothetical. `WebMvcEndpointIntegrationTests` failed exactly
/// so in the 2026-08-29 full Spring Boot suite run: from the SECOND context
/// refresh onward, every one of nine autoconfiguration classes logged
/// `MergedAnnotation -- Failed to introspect annotations on class …:
/// java.lang.NoClassDefFoundError: jdk/proxy1/$Proxy26`. Spring's
/// `AnnotationsScanner` catches that Throwable and returns NO annotations, so
/// `@AutoConfiguration`/`@Conditional`/`@Bean` metadata silently vanished,
/// `WebMvcEndpointHandlerMapping` was never defined, and the class failed
/// twice — a `NoSuchBeanDefinitionException` and a 404. Nothing recovers on
/// its own, because the poisoned row is never evicted: the log shows the same
/// nine classes failing on every subsequent refresh.
///
/// The KEY is purged on the same evidence: a row keyed on a holder class id
/// that no longer exists can only be matched again through id reuse — which
/// `class_manager`'s own array-cache comment records as real ("a loader id is
/// never recycled, but a *class* id under a live loader is") — and matching by
/// reuse would hand a fresh class the retired one's annotations.
///
/// `class_id_of` resolves a cached proxy's class; `None` means the ref no
/// longer names a valid object, which is itself a reason to drop the row.
/// Dropping is always safe: the next `getDeclaredAnnotations()` rebuilds the
/// proxy. Returns the number of rows dropped.
pub fn forget_unloaded_annotation_proxies(
    vm_identity: usize,
    dead: &rustc_hash::FxHashSet<u32>,
    class_id_of: &dyn Fn(ObjectRef) -> Option<u32>,
) -> usize {
    if dead.is_empty() {
        return 0;
    }
    let mut dropped: Vec<ObjectRef> = Vec::new();
    ANNOTATION_PROXY_CACHE.with(vm_identity, |table| {
        table.retain(|(holder, _), proxy| {
            let keep = !dead.contains(holder)
                && class_id_of(*proxy).is_some_and(|cid| !dead.contains(&cid));
            if !keep {
                dropped.push(*proxy);
            }
            keep
        });
    });
    // The child-root table is keyed by proxy address and is what keeps a
    // proxy's member values (Class/String/array members) rooted. A row dropped
    // above has no reader left, so leaving its children behind would root
    // garbage for the life of the process.
    for proxy in &dropped {
        forget_annotation_proxy_child_roots(vm_identity, *proxy);
    }
    // Round 16 wave 16 (lane proxy6): a handed-out handler goes with its
    // proxy's row or its annotation type, and a memoized member order with
    // its type (a class id is reused once its class is gone).
    forget_unloaded_annotation_jdk_handlers(vm_identity, dead, &dropped);
    forget_annotation_member_orders(vm_identity, dead);
    dropped.len()
}

/// Drop every cached annotation proxy whose holder is one of `redefined`: a
/// JVMTI redefinition replaced those classes' annotations (interpreter round
/// i1 wave 27, lane L3). HotSpot's per-class `annotationData` is rebuilt once
/// `classRedefinedCount` moves; this cache is keyed by the holder alone, so
/// without the purge `getAnnotation` kept answering the previous version's
/// values (`L3/L3W26RedefinedReflection`). Returns the number of rows dropped.
///
/// `redefined` must hold the redefined class and its subclasses (a
/// subclass's row can hold an `@Inherited` annotation of the class).
///
/// Unlike [`forget_unloaded_annotation_proxies`] this keeps the dropped
/// proxies' child roots: a proxy the application still holds is alive and
/// usable, and its rows go when it dies, as an uncached proxy's do
/// ([`prune_dead_annotation_proxies`]).
pub fn forget_redefined_annotation_proxies(
    vm_identity: usize,
    redefined: &rustc_hash::FxHashSet<u32>,
) -> usize {
    // Round 16 wave 16 (lane proxy6): a redefined annotation type may have
    // new members or defaults; its memoized member order goes.
    forget_annotation_member_orders(vm_identity, redefined);
    if redefined.is_empty() || !ANNOTATION_PROXY_CACHE.has_row(vm_identity) {
        return 0;
    }
    ANNOTATION_PROXY_CACHE.with(vm_identity, |table| {
        let before = table.len();
        table.retain(|(holder, _), _| !redefined.contains(holder));
        before - table.len()
    })
}

pub fn remember_annotation_proxy_child_roots(vm: usize, proxy: ObjectRef, roots: Vec<ObjectRef>) {
    ANNOTATION_PROXY_CHILD_ROOTS.with(vm, |table| {
        table.insert(proxy.as_ptr() as usize, roots);
    });
}

pub fn forget_annotation_proxy_child_roots(vm: usize, proxy: ObjectRef) {
    ANNOTATION_PROXY_CHILD_ROOTS.with(vm, |table| {
        table.remove(&(proxy.as_ptr() as usize));
    });
}

// ---------------------------------------------------------------------------
// Last-created-proxy interfaces array (Pattern-A fix, bug nb-lib-gckeys В§2)
//
// `Class.getInterfaces()` on the shared `Proxy$Instance` class mirror returns
// the most-recently-created proxy's interfaces array (the "last-wins"
// limitation documented in `native_class_get_interfaces`). The array used to
// be cached in a raw `AtomicU64` in `lib.rs` (`PROXY_LAST_INTERFACES_BITS`)
// holding `arr.as_ptr()`, then read back via `ObjectRef::from_raw` here. That
// raw pointer was NEVER a GC root and NEVER remapped: a moving young GC would
// relocate (or, if the proxy became unreachable, reclaim) the array, and the
// next `getInterfaces()` would dereference a stale/dangling address.
//
// Fix: hold the array as a real `ObjectRef` in this process-global side-table
// and fold it into the SAME GC scan / remap hooks that already keep the
// annotation-proxy cache live (`gc_scan_annotation_proxy_roots` /
// `gc_update_annotation_proxy_refs`, wired into roots.rs + gc.rs). No new GC
// wiring is needed вЂ” the array is now a tracked root and is repointed after
// every relocation, so the read below is always valid.
/// Record the interfaces array of the most recently created proxy so a later
/// `Class.getInterfaces()` on the shared `Proxy$Instance` mirror can return it
/// (GC-tracked вЂ” see [`proxy_last_interfaces`]). Called from
/// `lib.rs::native_proxy_new_instance`.
pub fn set_proxy_last_interfaces(vm_identity: usize, arr: ObjectRef) {
    PROXY_LAST_INTERFACES.with(vm_identity, |cell| *cell = Some(arr));
}

/// Fetch the GC-tracked last-proxy interfaces array, if any. Returns the
/// CURRENT (post-relocation) `ObjectRef` because the cell is remapped by
/// [`gc_update_annotation_proxy_refs`].
pub fn proxy_last_interfaces(vm_identity: usize) -> Option<ObjectRef> {
    PROXY_LAST_INTERFACES
        .peek(vm_identity, |cell| *cell)
        .flatten()
}

/// Build-or-fetch the cached annotation proxy for `ann` as seen on
/// `queried_class_id`. The cache lock is NEVER held across
/// `create_annotation_proxy` (which allocates and may trigger a GC whose root
/// scan re-locks this cache вЂ” that would deadlock). On a concurrent first-build
/// race the loser's proxy is dropped (still reachable from the caller's stack
/// until the next GC), exactly as `OscCache` documents.
/// Find the initiating loader that must resolve annotation member values for a
/// declaring class.  Real-JDK class-definition hooks can omit a direct object
/// record for generated Ehcache JAXB model classes; their sibling `ConfigType`
/// in the same namespace retains the authoritative loader identity.
pub fn annotation_container_loader(
    ctx: &mut dyn NativeContext,
    holder_class_id: ClassId,
) -> Option<ObjectRef> {
    let holder_name = ctx.class_name_of_id(holder_class_id).unwrap_or_default();
    crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), holder_class_id.as_u32()).or_else(
        || {
            holder_name
                .starts_with("org/ehcache/xml/model/")
                .then(|| {
                    ctx.class_id_by_name_near("org/ehcache/xml/model/ConfigType", holder_class_id)
                        .and_then(|sibling| {
                            crate::classloader::defining_loader_for(
                                crate::classloader::loader_tables(ctx),
                                sibling.as_u32(),
                            )
                        })
                        .or_else(|| {
                            let id = ctx.loader_id_of_class(holder_class_id);
                            (id >= 3)
                                .then(|| {
                                    crate::classloader::loader_object_for_namespace_id_in(
                                        crate::classloader::loader_tables(ctx),
                                        id as u32,
                                    )
                                })
                                .flatten()
                        })
                })
                .flatten()
        },
    )
}

pub fn cached_annotation_proxy_for_key(
    ctx: &mut dyn NativeContext,
    holder_class_id: ClassId,
    key: String,
    ann: &cratonvm_native_api::AnnotationData,
    ann_class_id: ClassId,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    let key = (holder_class_id.as_u32(), key);
    if let Some(cached) = ANNOTATION_PROXY_CACHE
        .peek(vm, |table| table.get(&key).copied())
        .flatten()
    {
        return Ok(cached);
    }
    // Resolve Class-valued members through the declaring class's defining loader
    // (HotSpot's `AnnotationParser` "container"), so classloader-isolation loaders
    // (e.g. Spring's OverridingClassLoader / FilteringClassLoader) yield a
    // deferred `TypeNotPresentException` for filtered types. `None` for built-in
    // loaders keeps the global resolution.
    let holder_name = ctx.class_name_of_id(holder_class_id).unwrap_or_default();
    let container_loader = annotation_container_loader(ctx, holder_class_id);
    if crate::nbflags().iae_trace_ok {
        eprintln!(
            "ANN-HOLDER cid={} holder={holder_name} ann={} container_loader={}",
            holder_class_id.as_u32(),
            ann.type_descriptor,
            if container_loader.is_some() {
                "SOME"
            } else {
                "none"
            }
        );
    }
    let proxy = create_annotation_proxy_with_type(
        ctx,
        ann,
        ann_class_id,
        Some(holder_class_id),
        container_loader,
    )?;
    let cached = ANNOTATION_PROXY_CACHE.with(vm, |table| *table.entry(key).or_insert(proxy));
    if cached.as_ptr() != proxy.as_ptr() {
        forget_annotation_proxy_child_roots(vm, proxy);
    }
    Ok(cached)
}

/// [`cached_annotation_proxy`] for the `getAnnotation`-style call sites, which
/// hold only the holder class. `None` when the annotation's own type does not
/// resolve — the same omission `getDeclaredAnnotations()` makes, rather than a
/// proxy whose `annotationType()` is null.
pub fn cached_annotation_proxy_resolving(
    ctx: &mut dyn NativeContext,
    queried_class_id: ClassId,
    ann: &cratonvm_native_api::AnnotationData,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Cache FIRST. `resolve_annotation_type_near` can call `loadClass` on a
    // user-defined loader, and `getAnnotation(X)` is hot enough that paying
    // that on every hit would be a real cost — the point of this cache is that
    // a repeat lookup touches nothing.
    if let Some(cached) = peek_annotation_proxy_cache(ctx, queried_class_id, &ann.type_descriptor) {
        return Ok(Some(cached));
    }
    let Some(ann_class_id) = resolve_annotation_type_near(ctx, ann, Some(queried_class_id)) else {
        return Ok(None);
    };
    Ok(Some(cached_annotation_proxy(
        ctx,
        queried_class_id,
        ann,
        ann_class_id,
    )?))
}

pub fn peek_annotation_proxy_cache(
    ctx: &mut dyn NativeContext,
    holder_class_id: ClassId,
    key: &str,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    ANNOTATION_PROXY_CACHE
        .peek(vm, |table| {
            table
                .get(&(holder_class_id.as_u32(), key.to_string()))
                .copied()
        })
        .flatten()
}

/// Build-or-fetch the cached annotation proxy for a class annotation. Keep the
/// historical key shape for class entries so existing class annotation identity
/// remains unchanged.
pub fn cached_annotation_proxy(
    ctx: &mut dyn NativeContext,
    queried_class_id: ClassId,
    ann: &cratonvm_native_api::AnnotationData,
    ann_class_id: ClassId,
) -> Result<ObjectRef, MethodCallFailed> {
    cached_annotation_proxy_for_key(
        ctx,
        queried_class_id,
        ann.type_descriptor.clone(),
        ann,
        ann_class_id,
    )
}

/// Cache key for an annotation declared directly on a reflected method or
/// constructor. NUL separators make the component boundaries unambiguous, and
/// the leading `M` keeps this namespace disjoint from class-annotation keys.
pub fn method_annotation_proxy_key(method_name: &str, method_desc: &str, ann_type: &str) -> String {
    format!("M\0{method_name}\0{method_desc}\0{ann_type}")
}

pub fn cached_method_annotation_proxy(
    ctx: &mut dyn NativeContext,
    declaring_class_id: ClassId,
    method_name: &str,
    method_desc: &str,
    ann: &cratonvm_native_api::AnnotationData,
    ann_class_id: ClassId,
) -> Result<ObjectRef, MethodCallFailed> {
    cached_annotation_proxy_for_key(
        ctx,
        declaring_class_id,
        method_annotation_proxy_key(method_name, method_desc, &ann.type_descriptor),
        ann,
        ann_class_id,
    )
}


/// GC root scan for the annotation-proxy side tables: the cached proxies and
/// their child roots, each reported WITH the `ClassId` of the holder it was
/// cached for (companion to [`gc_update_annotation_proxy_refs`]).
///
/// gc-common w3-b (`handoff-w2b-loader-owned-native-side-tables.md` part B):
/// these used to be pushed as unconditional roots, which made every holder
/// whose annotation TYPE (or a `Class`-valued member) lives in the holder's
/// own user loader immortal — the proxy names the annotation type, the type
/// names its loader, and `forget_unloaded_annotation_proxies` could never run
/// for a loader this very table retained. With the holder id the VM's root
/// source pins a user-loader holder's proxies to that loader
/// (`metadata_pin`) whenever the cycle's marker follows it, exactly like
/// statics and `ClassValue` results; everything else is rooted as before.
///
/// A child root whose proxy is not (or no longer) in the cache is reported
/// with owner `None`, i.e. rooted — the previous behaviour — until
/// [`prune_dead_annotation_proxies`] drops it with its dead proxy.
pub fn gc_scan_annotation_proxy_entries(
    vm_identity: usize,
    visit: &mut dyn FnMut(Option<u32>, ObjectRef),
) {
    // Holder of each cached proxy, by address, so a proxy's children are
    // reported under the same owner as the proxy. The two tables are read
    // under separate (never nested) locks, per `VmScoped`'s discipline.
    // addr-keyed: local to this root scan (one pause).
    let mut holder_of: FxHashMap<usize, u32> = FxHashMap::default();
    ANNOTATION_PROXY_CACHE.peek(vm_identity, |table| {
        for (&(holder, _), proxy) in table.iter() {
            holder_of.insert(proxy.as_ptr() as usize, holder);
            visit(Some(holder), *proxy);
        }
    });
    ANNOTATION_PROXY_CHILD_ROOTS.peek(vm_identity, |table| {
        for (proxy_addr, roots) in table.iter() {
            let owner = holder_of.get(proxy_addr).copied();
            for root in roots {
                visit(owner, *root);
            }
        }
    });
    gc_scan_annotation_jdk_handlers(vm_identity, visit);
}

/// GC root scan for the last-created-proxy interfaces array
/// (`PROXY_LAST_INTERFACES`, bug nb-lib-gckeys §2): rooted so the
/// shared-mirror `Class.getInterfaces()` fallback never dereferences a
/// reclaimed/relocated array. It pins the interfaces of ONE proxy — at most one
/// user loader — until the next proxy is created; the cached proxies
/// themselves are [`gc_scan_annotation_proxy_entries`].
pub fn gc_scan_annotation_proxy_roots(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    if let Some(arr) = proxy_last_interfaces(vm_identity) {
        out.push(arr);
    }
}

/// Drop the annotation-proxy rows whose proxy did not survive this collection
/// (`is_live`, over PRE-remap addresses), and child-root rows keyed by a dead
/// proxy. Returns the number of rows dropped from both tables.
///
/// Two reasons, both new with the conditional rooting above:
///
/// * a proxy pinned to its holder's loader dies with that loader, and a row
///   must never outlive its value even on a cycle that unloads nothing — the
///   post-move fix-up would otherwise "remap" a reclaimed address, and the
///   next `getAnnotation` would hand an unrelated object back as the proxy;
/// * a child-root row is created for EVERY bare proxy
///   (`create_annotation_proxy_with_type`), cached or not, and nothing removed
///   the rows of uncached ones: each was a permanent root for its descriptor
///   string and element arrays after the proxy itself was gone.
///
/// Always safe: the cache is a memoizer, the next query rebuilds.
pub fn prune_dead_annotation_proxies(vm_identity: usize, is_live: &dyn Fn(usize) -> bool) -> usize {
    let mut dropped = 0usize;
    if ANNOTATION_PROXY_CACHE.has_row(vm_identity) {
        ANNOTATION_PROXY_CACHE.with(vm_identity, |table| {
            let before = table.len();
            table.retain(|_, proxy| is_live(proxy.as_ptr() as usize));
            dropped += before - table.len();
        });
    }
    if ANNOTATION_PROXY_CHILD_ROOTS.has_row(vm_identity) {
        ANNOTATION_PROXY_CHILD_ROOTS.with(vm_identity, |table| {
            let before = table.len();
            table.retain(|&proxy_addr, _| is_live(proxy_addr));
            dropped += before - table.len();
        });
    }
    dropped += prune_dead_annotation_jdk_handlers(vm_identity, is_live);
    dropped
}

