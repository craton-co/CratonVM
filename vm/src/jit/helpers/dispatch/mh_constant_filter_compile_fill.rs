#[allow(unused_imports)]
use super::*;

/// MHBIND2-1 (JIT round 21 wave 20, lane mhfilter;
/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER`): the cold fill's record of a
/// REAL filter species at `obj` (validated by the fill's `is_object_address`,
/// word `raw`, collection epoch `epoch`) that every bind refused, for the
/// compile doors (`invoke_kind 5`,
/// `jit_bridge::note_mh_constant_target_filter`). Admitted by
/// `lang_invoke::mh_real_filter_compiled_target`; then, as the bind's own
/// loader rule (`mh_constant_bind_loaders_agree`), the caller must see every
/// class the site names as the target's class does, and every class the
/// filter's own descriptor names as the filter's class does. The row's member
/// is the MAIN call (the target of an argument filter, the filter of a return
/// filter), the other the PRE call. Holds the JIT thread only for the
/// admission's reads.
pub fn mh_constant_filter_compile_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    obj: ObjectRef,
    raw: u64,
    epoch: u64,
) {
    use crate::runtime::interpreter::jit_bridge as memo;
    let parts = {
        // SAFETY: called only from `mh_constant_bind_fill` on the JIT helper's
        // own thread, after the fill's admission block dropped its guard, so no
        // other `&mut JvmThread` is live; the guard is held for this block.
        let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
            return;
        };
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        cratonvm_native_builtins::lang_invoke::mh_real_filter_compiled_target(
            &mut ctx,
            obj,
            info.descriptor,
        )
    };
    let Some(parts) = parts else {
        return;
    };
    if !memo::mh_constant_bind_loaders_agree(
        vm,
        info.declaring_class_id,
        parts.target.class_id,
        info.descriptor,
    ) || !memo::mh_constant_bind_loaders_agree(
        vm,
        info.declaring_class_id,
        parts.filter.class_id,
        &parts.filter.desc,
    ) {
        return;
    }
    // The species' `argL0` slot, holding the target's direct handle: the
    // frame a filtered receiver's null guard resumes on. `0`: unknown.
    let heap = &vm.mem.heap;
    let target_word = parts.target_handle.as_ptr() as u64;
    let target_slot = Some(heap.class_id_of(obj))
        .filter(|c| c.as_u32() != 0)
        .and_then(|c| crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, c, "argL0"))
        .filter(|s| *s < heap.num_fields(obj))
        .filter(|&s| mh_bind_slot_key(heap.get_field(obj, s)) == target_word)
        .and_then(|s| u32::try_from(s).ok())
        .unwrap_or(0);
    let (main, pre) = match parts.position {
        Some(_) => (&parts.target, &parts.filter),
        None => (&parts.filter, &parts.target),
    };
    if mh_constant_bind_dbg() {
        eprintln!(
            "[mh-bind] compile row {} -> {}.{}{} filter ({}), pre {}.{}{}, argL0 slot {target_slot}",
            info.descriptor,
            main.class,
            main.name,
            main.desc,
            match parts.position {
                Some(p) => format!("argument {p}"),
                None => "return".to_string(),
            },
            pre.class,
            pre.name,
            pre.desc,
        );
    }
    memo::note_mh_constant_target_filter(
        vm,
        raw,
        epoch,
        info.descriptor,
        (&main.class, &main.name, &main.desc),
        main.class_id,
        memo::MhConstantFilter::new(
            parts.position,
            main.invoke_kind,
            (&pre.class, &pre.name, &pre.desc),
            pre.invoke_kind,
            pre.class_id,
            target_slot,
        ),
    );
}

/// mh21: the `[mh-bind] census` line of a REAL species at `obj` (validated by
/// the fill's `is_object_address`) that every bind refused, under
/// `CRATONVM_DBG_JIT_MH_BIND=1` only
/// (`lang_invoke::mh_real_species_census_line`). The census the proposals
/// MHBIND21-3 / MHFILTER21-3 / MHFILTER21-4 name as their first step: count the
/// lines by `species=` and `kind=` on a Spring / Groovy run.
pub fn mh_constant_real_species_census(vm: &SharedVm, info: &JitInvokeInfo, obj: ObjectRef) {
    let line = {
        // SAFETY: as `mh_constant_filter_compile_fill`: called only from
        // `mh_constant_bind_fill` on the JIT helper's own thread, after the
        // fill's admission block dropped its guard.
        let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
            return;
        };
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        cratonvm_native_builtins::lang_invoke::mh_real_species_census_line(&mut ctx, obj)    };
    if let Some(line) = line {
        eprintln!("[mh-bind] census {} {line}", info.descriptor);
    }
}

/// The bound value's word of the real `bindTo` result at `obj` -- at another
/// address than the site's bound one, in the same VM and collection epoch --
/// when it is of the site's [`MhRealBoundShape`] (MHB-3, JIT round 21 wave
/// 17, lane mhbind): the same species class (read through `class_id_of`, which
/// validates the address as an object start, as [`mh_bind_shape_matches`]
/// does), the same `argL0`, `form` and `type` objects, and a non-null bound
/// value of the admitted value's exact class. `None` otherwise (the fill
/// decides the handle). Reads only; no allocation, no GC point.
pub fn mh_real_bound_shape_receiver(
    vm: &SharedVm,
    obj: ObjectRef,
    shape: &MhRealBoundShape,
) -> Option<u64> {
    let heap = &vm.mem.heap;
    if heap.class_id_of(obj).as_u32() != shape.class_id
        || mh_bind_slot_key(heap.get_field(obj, shape.target_slot)) != shape.target_word
        || mh_bind_slot_key(heap.get_field(obj, shape.form_slot)) != shape.form_word
        || mh_bind_slot_key(heap.get_field(obj, shape.type_slot)) != shape.type_word
    {
        return None;
    }
    let Value::Object(Some(receiver)) = heap.get_field(obj, shape.receiver_slot) else {
        return None;
    };
    // A field's reference, not an address just proved by `is_object_address`:
    // the validating header reads.
    let class = jit_dispatch_class(vm, receiver).class_id()?.as_u32();
    // JIT round 21 wave 19 (lane mhbind2): or the shape's second class.
    (class == shape.receiver_class
        || (shape.receiver_class_alt != 0 && class == shape.receiver_class_alt))
        .then_some(receiver.as_ptr() as u64)
}
/// Count one native dispatch on a JIT fast-path edge (§4 census).
///
/// One `Option` test plus one relaxed `fetch_add` on an id resolved at
/// cache-fill time. No allocation, no hashing, no lock, no string comparison —
/// all of that was hoisted to the cold admission above. The acceptance
/// criterion for `--real-jdk` is *zero* `SyntheticStub` invocations through
/// **any** path, and an uncounted path is an unverifiable one.
#[inline]
pub fn count_jit_native_dispatch(vm: &SharedVm, id: Option<cratonvm_native_api::NativeMethodId>) {
    if let Some(id) = id {
        vm.natives.native_methods.record_invocation(id);
    }
}

// ---------------------------------------------------------------------------
// Invoke dispatch helpers
// ---------------------------------------------------------------------------

/// Per-info-pointer JIT dispatch state: caches a compiled callee's entry point
/// so that repeated calls from the same JIT call site skip the JIT cache lookup.
pub struct DispatchCache {
    pub entry: usize,
    pub needs_context: bool,
    /// Owns JIT code while this thread-local raw entry remains published.
    ///
    /// [`cratonvm_jit::RetainedCode`], not a bare `Arc`, because this map is
    /// evicted by the very thread that dispatches through it: a generation
    /// flush (`flush_raw_entry_dispatch_caches`), a class-identity flush, a
    /// replacement `insert`, or thread exit. Any of those can run while this
    /// thread is *inside* the body it names — `try_mic_rust_cached_entry` reads
    /// only `entry`/`needs_context` out of the map and calls the raw pointer, so
    /// the map entry is the sole owner across the call, and a nested dispatch
    /// from the callee re-enters the flush. Dropping a bare `Arc` there unmaps
    /// the code under this thread's own return address; measured on
    /// `BasicErrorControllerIntegrationTests` at
    /// `active_jit_executions` = 1. The wrapper releases through
    /// `defer_jit_owner`, which retains until no thread is in compiled code.
    ///
    /// Read as well as held since 2026-08-17: the dispatch sites clone it and
    /// hand it to `try_call_compiled_entry_reentrant_owned` instead of making
    /// that function re-derive the same `Arc` out of the code-range registry.
    ///
    /// Behind an `Rc` since round 9 wave 3 (vmhelpers3). The per-call clone
    /// that keeps the body alive across the call (the map entry may be evicted
    /// by a nested dispatch while this thread is inside it) used to be a
    /// `RetainedCode` clone — at the time an `Arc` increment plus a holder-count
    /// increment, and the matching two decrements plus the last-holder test on
    /// drop: four locked RMWs per cached dispatch, 6-7% of the mutual-recursion
    /// probe (`perf-mutual-recursion-forces-every-call-through-invoke-dispatch`).
    /// The maps are thread-local, so a non-atomic `Rc` gives the same
    /// guarantee: the ONE `RetainedCode` lives until the map entry AND every
    /// in-flight clone are gone, and its own drop still routes through the
    /// retirement queue exactly as before.
    pub owner: Option<std::rc::Rc<cratonvm_jit::RetainedCode>>,
    /// The callee declares an exception table, so the sentinel it returns can
    /// RE-RUN it with this call's arguments, and the call roots its reference
    /// arguments across the compiled call ([`call_compiled_then_route`]).
    /// Answered once, when the entry is filled; `false` costs a call nothing.
    pub rerun_pins: bool,
}

impl DispatchCache {
    /// The body this entry names was replaced by a later publication of its
    /// key (`CompiledMethod::superseded`) or invalidated (`retired`). Either
    /// way the site must re-resolve, and the entry is dropped at the hit.
    ///
    /// This is what lets `JitCache::put` move only the publish half of the
    /// generation word on a replacement instead of flushing every thread's
    /// positive memos (round 11 wave 11). A replaced body is still correct
    /// code, so a hit that raced the flag ran correct code; the entry's own
    /// `RetainedCode` kept it mapped. `retired` is checked as well although
    /// every invalidation also moves the retire half: a memo must not wait for
    /// its thread's next flush to stop entering withdrawn code.
    ///
    /// An entry with no owner names nothing this can ask about. Every
    /// production insertion carries one (an unowned raw entry is never
    /// cached); the unowned form exists only in tests.
    #[inline]
    pub fn names_a_stale_body(&self) -> bool {
        use std::sync::atomic::Ordering::Acquire;
        self.owner.as_ref().is_some_and(|o| {
            let cm = o.arc();
            cm.superseded.load(Acquire) || cm.retired.load(Acquire)
        })
    }
}


#[derive(Clone, Copy)]
pub struct NativeDispatchCache {
    pub receiver_class_id: u32,
    pub callback: cratonvm_native_api::NativeCallback,
    pub kind: ObjectNativeKind,
    /// §4 census handle for the registry slot this entry dispatches, resolved
    /// once by [`admit_jit_fast_native`] at cache-fill time. `None` only when
    /// the registry has no slot for the triple, which under `JdkOnly` cannot
    /// happen (admission refuses that case) and under `Compatible` just means
    /// the dispatch is uncountable, exactly as it was before.
    pub native_id: Option<cratonvm_native_api::NativeMethodId>,
}

#[derive(Clone, Copy)]
pub enum ObjectNativeKind {
    HashMap,
    Matcher,
    /// Registered `java/lang/StringBuilder` natives (the `append(I)/(C)/
    /// (String)` family + `toString`/`length`). Exact-receiver-guarded like
    /// the other kinds; resolution consults the native registry ONCE at
    /// cache-fill time instead of a 3-string hash per call.
    StringBuilder,
}

#[derive(Clone, Copy)]
pub enum IntegerNativeKind {
    ValueOf,
    IntValue,
}

#[derive(Clone, Copy)]
pub struct IntegerNativeDispatchCache {
    pub kind: IntegerNativeKind,
    pub callback: cratonvm_native_api::NativeCallback,
    /// §4 census handle — see [`NativeDispatchCache::native_id`].
    pub native_id: Option<cratonvm_native_api::NativeMethodId>,
}

/// A JIT call site resolved to a **leaf** native — see
/// [`cratonvm_native_api::NativeMethodRegistry::set_leaf`].
///
/// This is the compiled-code half of the funnel bypass the interpreter already
/// has for `Thread.onSpinWait` / `Thread.currentThread`. Measured on
/// `probes/NativeShapeProbe.java`, a no-argument static native from compiled
/// code cost ~560 ns against ~8 ns for an ordinary Java call, and essentially
/// none of it was the native's own body: it was `jit_invoke_dispatch`'s tail
/// (`invoke_or_native`, a ~27-gate string cascade ending in a three-string
/// registry hash) plus `safe_native_call`'s GC bookkeeping. This entry removes
/// both — the cascade at cache-fill time, the funnel via
/// `safe_native_call_leaf`.
/// What a resolved leaf site actually does on a hit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LeafNativeKind {
    /// Call the registered callback through `safe_native_call_leaf`.
    Callback,
    /// `Thread.currentThread()` — answered from `thread.java_thread_obj`
    /// WITHOUT calling the native at all, exactly as the interpreter's inline
    /// cache does (`dispatch_static.rs`, `InterpIntrinsic::ThreadCurrentThread`).
    ///
    /// It cannot be an ordinary leaf, because its registered body
    /// (`native_thread_current_thread` → `NativeContext::current_thread_object`)
    /// **allocates** when this thread's mirror has not been built yet — it
    /// loads `java/lang/Thread`, allocates the mirror, builds the field holder
    /// and the thread group. That is a real violation of leaf contract item 1,
    /// so the claim is not made at registration; the site instead serves only
    /// the warm case and falls through to the ordinary dispatcher (which runs
    /// that allocating slow path) when the mirror is absent.
    ///
    /// Worth its own arm because of how often the JDK calls it: **twice per
    /// uncontended `ReentrantLock.lock()`/`unlock()` pair**, censused with
    /// `--dump-native-registry` in `probes/LockNativeCensusProbe.java`, plus
    /// every AQS ownership check and every thread-local lookup. The interpreter
    /// took it from 306 ns to 136 ns; compiled code was still paying 564 ns
    /// because none of the compiled loop's calls reach the interpreter's cache.
    ThreadCurrentThread,
    /// `ArrayList.get(int)` — the site resolved to
    /// `cratonvm_native_collections::native_al_get`. A hit on an ordinary
    /// `ArrayList` with an in-range index is answered by
    /// `jit_arraylist_get` (one fused field read and one array-element read)
    /// WITHOUT the `safe_native_call_*` wrapper or the argument decode. Every
    /// other receiver shape — views, wrappers, an out-of-range index, the
    /// `CRATONVM_DBG_ALTRACE` trace — falls through to the ordinary
    /// [`LeafNativeKind::Callback`] path of the same entry, which calls the
    /// same native. Classified once in `resolve_native_site`
    /// (`collection_leaf_kind`); r9w4 vm4, per `NOTES-w3-collections3.md`.
    ArrayListGet,
    /// `ArrayList.size()` — the site resolved to `native_al_size`; served by
    /// `jit_arraylist_size` on the same terms as [`LeafNativeKind::ArrayListGet`].
    ArrayListSize,
    /// `ConcurrentHashMap.get(Object)` / `ConcurrentMap.get` -- the site resolved to
    /// `cratonvm_native_collections::native_chm_get`. A primitive-wrapper or `String` key is
    /// answered by `jit_chm_get` (in-place, allocation-free, never-blocking chain walk)
    /// WITHOUT the argument decode or the `safe_native_call_*` wrapper; every other key, a
    /// null key (the native's NPE) and any decline fall through to the ordinary callback
    /// path, which runs the same native. r9w11 chm11.
    ChmGet,
    /// `--compatible` side-table `ThreadLocal.get()` (`phases_early::native_tl_get`):
    /// served on table hit without `JitDecodedArgs` or `flush_thread_satb`; misses
    /// fall back to [`LeafNativeKind::Callback`] to run `initialValue` / supplier.
    CompatThreadLocalGet,
    /// `Integer.hashCode()I` (and the `Character`/`Byte`/`Short` twins, which
    /// register the same body) -- the site resolved to
    /// `lang_math::native_wrapper_int_hash_code`. Answered as that body does
    /// (field 0 as an `int`, else 0) WITHOUT the argument decode or the funnel.
    ///
    /// Round 11 wave 11, lane `hashmap`: `HashMap.hash` calls `key.hashCode()`
    /// on every `put` and `get`, and an inline cache can never hold a native,
    /// so with `Integer` keys every one of those calls missed into
    /// `jit_invoke_virtual_mic` and then paid `safe_native_call_prevalidated_objects`
    /// (these wrapper natives are not registered leaf). `CratonBench hashmap`
    /// measured 29 999 550 such `mic_site_native` calls in one phase.
    IntWrapperHashCode,
    /// `Integer.equals(Object)Z` (and its int-stored twins) -- the site resolved
    /// to `lang_math::native_wrapper_int_equals`. Only the same-class,
    /// both-plain-object case is answered here; every other shape (a null or
    /// differently-classed argument, an array, a wrapper whose field did not
    /// decode as an `int`) declines to the ordinary callback path, which runs
    /// the same native. `HashMap.getNode` calls it once per `get`.
    IntWrapperEquals,
    /// `Object.getClass()` on a plain object whose class already has its mirror
    /// and a memoised "report the stamped class" display verdict. SBR round 2,
    /// lane CALLER: every `Field.get*` crosses this native once through the
    /// funnel (`Field.checkAccess`'s `obj.getClass()`); answered here WITHOUT
    /// the argument decode or the funnel. Any other shape (array, unmemoised or
    /// aliased display, no mirror yet) declines to the callback, which runs the
    /// same native. `CRATONVM_CALLER_LEAF=0` turns the arm off.
    ObjectGetClass,
    /// `java/lang/Object.hashCode()I` (native identity hash).
    ObjectHashCode,
    /// `Reference.refersTo(Object)Z` (and the three subclasses'): the referent
    /// slot compared with the argument, as `native_ref_refers_to` does. Crossed
    /// once per `Field` access by `AccessibleObject.isAccessChecked`.
    /// `CRATONVM_CALLER_LEAF=0` turns the arm off.
    ReferenceRefersTo,
    /// `Class.isInstance(Object)Z` (JIT round 20 wave 17, lane pAtom2;
    /// `CRATONVM_JIT_CLASS_IS_INSTANCE_INLINE`): a null argument is `false`,
    /// and a plain object whose class's mirror IS the receiver is `true`
    /// ([`class_is_instance_exact_hit`], which also publishes that class's
    /// compiled-`getClass()` word for the optimizing tier's prefix). Every
    /// other shape (an array, a subclass or interface instance) declines to the
    /// callback, which runs the same native. The field updaters'
    /// `accessCheck` crosses it on every access.
    ClassIsInstance,
    /// `ZoneInfo.getOffsets(J[I)I` (`tzdb::native_zone_info_get_offsets`).
    ZoneInfoGetOffsets,
    /// `ZoneInfo.getOffsetsByWall(J[I)I` (`tzdb::native_zone_info_get_offsets_by_wall`).
    ZoneInfoGetOffsetsByWall,
}

/// `CRATONVM_CALLER_LEAF` (default on): the `ObjectGetClass` and
/// `ReferenceRefersTo` site arms. `0`/`off` restores the funnel for both.
pub fn caller_leaf_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("off") | Ok("false")
        )
    })
}

/// Which collection fast path, if any, a resolved native site may serve
/// before its ordinary callback. Keyed on the RESOLVED callback's identity —
/// not on names — so a triple re-registered to a different body (or a
/// receiver class whose override is some other native) is never answered by
/// the ArrayList probe; and on the CALL SITE descriptor, so the raw argument
/// slot and return encoding are the ones the probe assumes. Only virtual and
/// interface sites qualify: a static site has no receiver, and an
/// `invokespecial` (`super.get(i)` from an `ArrayList` subclass) keeps the
/// plain callback path.
pub fn collection_leaf_kind(
    info: &JitInvokeInfo,
    callback: cratonvm_native_api::NativeCallback,
    poly: bool,
) -> LeafNativeKind {
    if !matches!(info.invoke_kind, 0 | 2) || poly {
        return LeafNativeKind::Callback;
    }
    let cb = callback as usize;
    if cb == cratonvm_native_collections::native_al_get as usize
        && info.descriptor == "(I)Ljava/lang/Object;"
    {
        LeafNativeKind::ArrayListGet
    } else if cb == cratonvm_native_collections::native_al_size as usize && info.descriptor == "()I"
    {
        LeafNativeKind::ArrayListSize
    } else if cb == cratonvm_native_collections::native_chm_get as usize
        && info.descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
    {
        LeafNativeKind::ChmGet
    } else if info.descriptor == "()I"
        && cratonvm_native_builtins::intrinsics::integer::is_int_wrapper_hash_code_native(callback)
    {
        LeafNativeKind::IntWrapperHashCode
    } else if info.descriptor == "(Ljava/lang/Object;)Z"
        && cratonvm_native_builtins::intrinsics::integer::is_int_wrapper_equals_native(callback)
    {
        LeafNativeKind::IntWrapperEquals
    } else if info.descriptor == "()Ljava/lang/Class;"
        && caller_leaf_enabled()
        && cratonvm_native_builtins::is_object_get_class_native(callback)
    {
        LeafNativeKind::ObjectGetClass
    } else if info.descriptor == "()I"
        && caller_leaf_enabled()
        && cratonvm_native_builtins::is_object_hash_code_native(callback)
    {
        LeafNativeKind::ObjectHashCode
    } else if info.descriptor == "(Ljava/lang/Object;)Z"
        && caller_leaf_enabled()
        && cratonvm_native_builtins::reference::is_refers_to_native(callback)
    {
        LeafNativeKind::ReferenceRefersTo
    } else if info.descriptor == "(Ljava/lang/Object;)Z"
        && cratonvm_jit::class_is_instance_inline_enabled()
        && cratonvm_native_builtins::is_class_is_instance_native(callback)
    {
        LeafNativeKind::ClassIsInstance
    } else if info.descriptor == "()Ljava/lang/Object;"
        && cratonvm_native_builtins::phases_early::is_tl_compat_get_native(callback)
    {
        LeafNativeKind::CompatThreadLocalGet
    } else if info.descriptor == "(J[I)I"
        && cratonvm_native_builtins::tzdb::is_zone_info_get_offsets_native(callback)
    {
        LeafNativeKind::ZoneInfoGetOffsets
    } else if info.descriptor == "(J[I)I"
        && cratonvm_native_builtins::tzdb::is_zone_info_get_offsets_by_wall_native(callback)
    {
        LeafNativeKind::ZoneInfoGetOffsetsByWall
    } else {
        LeafNativeKind::Callback
    }
}

/// FUNNEL3-19-2 (JIT round 19 wave 6, lane mega6;
/// `CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS`, default on): the site-cache
/// door's (Route S) validated reference argument for a leaf-kind probe, from
/// the forwarding pass's mask (`forward_jit_reference_args_validated`, under
/// `CRATONVM_JIT_NATIVE_WALK_ONCE`).
///
/// Only on a `warm` call: the mask's contract is "no GC point since the
/// forwarding pass", and a resolution in this call (a cold fill, a refill)
/// can load and initialize classes, which runs Java. Only for the kinds that
/// take a reference argument (`lean_leaf_kind_takes_reference`). `None`
/// otherwise, and the probe walks the word as before.
#[inline]
/// Leaf kinds whose third argument word (`args[2]`) is a reference (`int[] offsets`).
pub fn lean_leaf_kind_takes_ref_arg2(kind: LeafNativeKind) -> bool {
    matches!(
        kind,
        LeafNativeKind::ZoneInfoGetOffsets | LeafNativeKind::ZoneInfoGetOffsetsByWall
    )
}

pub fn site_leaf_probe_validated_arg(
    vm: &SharedVm,
    kind: LeafNativeKind,
    warm: bool,
    prevalidated: u64,
    args_slice: &[i64],
) -> Option<ObjectRef> {
    if !warm || prevalidated == 0 || !vm.natives.native_methods.jit_leaf_probe_validated_args() {
        return None;
    }
    if lean_leaf_kind_takes_reference(kind) {
        return prevalidated_jit_ref(prevalidated, 1, *args_slice.get(1)?);
    }
    if lean_leaf_kind_takes_ref_arg2(kind) {
        return prevalidated_jit_ref(prevalidated, 2, *args_slice.get(2)?);
    }
    None
}

/// Serve an [`LeafNativeKind::ArrayListGet`] / [`LeafNativeKind::ArrayListSize`]
/// site hit from the `native-collections` no-wrapper probes (and, since round 11
/// wave 11, the `ChmGet` probe's siblings [`LeafNativeKind::IntWrapperHashCode`]
/// and [`LeafNativeKind::IntWrapperEquals`], which re-state their natives'
/// read-only bodies).
///
/// `Some(bits)` is exactly what the site's native would have returned, in the
/// JIT return encoding (`size()` sign-extended; `get` an object pointer or 0);
/// `None` means "run the entry's ordinary callback path", which is also what
/// happens for every receiver or index the probe does not recognise. The
/// probes have the same no-allocation / no-Java / no-throw contract as
/// `jit_overlay_hashmap_get`, which `jit_hashmap_get_direct` already calls
/// without the funnel.
///
/// `validated_arg` (JIT round 19 wave 6, lane mega6; FUNNEL3-19-2,
/// `CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS`): the object `args_slice[1]` names,
/// already validated (and forwarded) by the calling door, or `None`. When
/// `Some`, the `ChmGet` / `IntWrapperEquals` / `ReferenceRefersTo` arms use it
/// instead of walking the word again; every other check is unchanged (a null
/// word still answers before it). The callers pass `None` with the switch off.
pub fn try_jit_arraylist_leaf(
    vm: &SharedVm,
    thread: &mut JvmThread,
    kind: LeafNativeKind,
    receiver: ObjectRef,
    args_slice: &[i64],
    validated_arg: Option<ObjectRef>,
) -> Option<i64> {
    match kind {
        LeafNativeKind::ArrayListGet => {
            // The `int` argument travels as the raw 64-bit slot; the low 32
            // bits are the Java value, exactly as the argument decode reads it.
            let index = *args_slice.get(1)? as i32;
            let probe = {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::jit_arraylist_get(&ctx, receiver, index)
            };
            match probe? {
                Value::Object(Some(obj)) => {
                    // Object-return handoff root, same contract as every other
                    // JIT native fast path (see `jit_integer_value_of_direct`).
                    thread.native_pending_return = Some(obj);
                    Some(obj.as_ptr() as i64)
                }
                Value::Object(None) => Some(0),
                // `elementData` is a reference array; anything else is not a
                // shape the probe should have answered — let the native decide.
                _ => None,
            }
        }
        LeafNativeKind::ArrayListSize => {
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_arraylist_size(&ctx, receiver).map(|n| n as i64)
        }
        LeafNativeKind::ChmGet => {
            // The key travels as a raw reference slot. Screen it exactly as
            // `jit_concurrent_hashmap_get_direct` does; a null key is the
            // native's NPE, so decline and let the callback raise it.
            let raw = *args_slice.get(1)?;
            if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let key = match validated_arg {
                Some(key) => key,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let probe = {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::jit_chm_get(
                    &mut ctx,
                    receiver,
                    Value::Object(Some(key)),
                )
            };
            match probe? {
                Value::Object(Some(obj)) => {
                    thread.native_pending_return = Some(obj);
                    Some(obj.as_ptr() as i64)
                }
                Value::Object(None) => Some(0),
                _ => None,
            }
        }
        LeafNativeKind::IntWrapperHashCode => {
            // `native_wrapper_int_hash_code` line for line: field 0 when it
            // decodes as an `int`, else 0. `get_field` forwards the receiver
            // exactly as it does for the native.
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            match ctx.get_field(receiver, 0) {
                Value::Int(v) => Some(i64::from(v)),
                _ => Some(0),
            }
        }
        LeafNativeKind::IntWrapperEquals => {
            // `native_wrapper_int_equals`: a null argument is `false`.
            let raw = *args_slice.get(1)?;
            if raw == 0 {
                return Some(0);
            }
            if (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let other = match validated_arg {
                Some(other) => other,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let receiver = vm.mem.heap.load_and_forward(receiver);
            let other = vm.mem.heap.load_and_forward(other);
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            // Only the same-class, both-plain-object case. The native also
            // compares class NAMES when the ids differ (a second loader's copy)
            // and refuses arrays; both go to the callback path, which runs it.
            if ctx.heap_kind_of(receiver) != cratonvm_types::ObjectKind::Object
                || ctx.heap_kind_of(other) != cratonvm_types::ObjectKind::Object
                || ctx.class_id_of_object(receiver) != ctx.class_id_of_object(other)
            {
                return None;
            }
            let a = match ctx.get_field(receiver, 0) {
                Value::Int(v) => v,
                _ => return Some(0),
            };
            let b = match ctx.get_field(other, 0) {
                Value::Int(v) => v,
                _ => return Some(0),
            };
            Some(i64::from(a == b))        }
        LeafNativeKind::ObjectHashCode => {
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            let hash = cratonvm_native_builtins::object_hash_code_fast(&ctx, receiver)?;
            Some(hash as i64)
        }
        LeafNativeKind::ObjectGetClass => {
            let (plain, cid) = {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                let plain = ctx.heap_kind_of(receiver) == cratonvm_types::ObjectKind::Object;
                (plain, ctx.class_id_of_object(receiver))
            };
            if !plain
                || !cratonvm_native_builtins::getclass_display_is_cached_stamp(vm.vm_identity, cid)
            {
                return None;
            }
            // nio20 L3: publishes the compiled-`getClass()` word too.
            let mirror = stamped_class_mirror_publishing_word(vm, cid)?;
            thread.native_pending_return = Some(mirror);
            Some(mirror.as_ptr() as i64)
        }
        LeafNativeKind::ReferenceRefersTo => {
            let raw = *args_slice.get(1)?;
            let receiver = vm.mem.heap.load_and_forward(receiver);
            let referent = {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // zt t23/zb8: the non-keep-alive read, as the native.
                ctx.get_field_no_keepalive(
                    receiver,
                    cratonvm_native_builtins::reference::REFERENT_SLOT,
                )
            };
            let Value::Object(referent) = referent else {
                return None;
            };
            if raw == 0 {
                return Some(i64::from(referent.is_none()));
            }
            if (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let other = match validated_arg {
                Some(other) => other,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let other = vm.mem.heap.load_and_forward(other);
            Some(i64::from(referent == Some(other)))
        }
        LeafNativeKind::ClassIsInstance => {
            // `native_class_is_instance`: a null argument is `false` (the
            // receiver, the mirror, is non-null at a virtual site).
            let raw = *args_slice.get(1)?;
            if raw == 0 {
                return Some(0);
            }
            if (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let obj = match validated_arg {
                Some(obj) => obj,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let obj = vm.mem.heap.load_and_forward(obj);
            // An array's header holds its COMPONENT's id: the native's array
            // arms decide it.
            if vm.mem.heap.kind_of(obj) != cratonvm_types::ObjectKind::Object {
                return None;
            }
            let mirror = vm.mem.heap.load_and_forward(receiver);
            let cid = vm.mem.heap.class_id_of(obj);
            class_is_instance_exact_hit(vm, cid, mirror).then_some(1)
        }
        LeafNativeKind::CompatThreadLocalGet => {
            let receiver = vm.mem.heap.load_and_forward(receiver);
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            match cratonvm_native_builtins::phases_early::tl_compat_get_hit_fast(&mut ctx, receiver)? {
                Value::Object(Some(obj)) => {
                    thread.native_pending_return = Some(obj);
                    Some(obj.as_ptr() as i64)
                }
                Value::Object(None) => Some(0),
                _ => None,
            }
        }
        LeafNativeKind::ZoneInfoGetOffsets | LeafNativeKind::ZoneInfoGetOffsetsByWall => {
            let millis = *args_slice.get(1)?;
            let raw_arr = *args_slice.get(2)?;
            let offsets = if raw_arr == 0 {
                None
            } else {
                let arr = match validated_arg {
                    Some(a) => a,
                    None => {
                        let bits = raw_arr as u64;
                        if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
                            return None;
                        }
                        let found = vm.mem.heap.is_object_address(bits as usize)?;
                        vm.mem.heap.load_and_forward_validated(found)
                    }
                };
                if vm.mem.heap.array_element_type(arr) != Some(ArrayElementType::Int) {
                    return None;
                }
                Some(arr)
            };
            let by_wall = kind == LeafNativeKind::ZoneInfoGetOffsetsByWall;
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_builtins::tzdb::jit_zone_info_get_offsets(
                &mut ctx,
                receiver,
                millis,
                offsets,
                by_wall,
            )
            .map(i64::from)
        }
        LeafNativeKind::Callback | LeafNativeKind::ThreadCurrentThread => None,
    }
}

/// JIT round 20 wave 17, lane pAtom2 (`LeafNativeKind::ClassIsInstance`): is
/// `mirror` the mirror of `class_id` (`class_mirrors[class_id]`)? Then
/// `mirror.isInstance(obj)` is `true` for a plain object of that class, which
/// is what `native_class_is_instance` answers by its id compare (and HotSpot
/// by `obj.getClass() == mirror`). No allocation, no Java, no GC point.
///
/// On a hit, while the `class_mirrors` read lock is held, the class's
/// compiled-`getClass()` word is published as `stamped_class_mirror_publishing_word`
/// publishes it, so the optimizing tier's `Class.isInstance` prefix (and its
/// `getClass()` prefix) answer the next call with no CALL. The word's contract
/// is that `getClass()` of every instance of the class answers this mirror,
/// so it is published only when that holds: the class's display verdict is
/// memoised as its stamp, or no `getClass()` remap arm names it
/// (`getclass_reports_only_itself`, by name: conservative). Only when the
/// table exists and the word is still unpublished; a busy class manager (the
/// non-blocking read) skips the publish, never the answer. The class manager
/// is asked with no `class_mirrors` guard held, and the mirror is read again
/// under the guard the publish happens under.
pub fn class_is_instance_exact_hit(
    vm: &SharedVm,
    class_id: crate::classloading::ClassId,
    mirror: ObjectRef,
) -> bool {
    if vm.classes.class_mirrors.read().get(&class_id).copied() != Some(mirror) {
        return false;
    }
    let words = &vm.classes.getclass_mirror_words;
    if words.is_allocated() && words.get(class_id) == 0 {
        let stamp =
            cratonvm_native_builtins::getclass_display_is_cached_stamp(vm.vm_identity, class_id)
                || vm.classes.class_manager.try_read().is_some_and(|cm| {
                    cm.get_class(class_id).is_some_and(|class| {
                        cratonvm_native_builtins::getclass_reports_only_itself(&class.name)
                    })
                });
        if stamp {
            let mirrors = vm.classes.class_mirrors.read();
            if let Some(current) = mirrors.get(&class_id).copied() {
                words.publish(class_id, current);
            }
        }
    }
    true
}

/// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`;
/// `NativeClassAccess::note_class_mirror_id`, called by the
/// `Class.isAssignableFrom` / `Class.isInstance` natives with a pair they just
/// resolved through `class_id_from_mirror`): publish the mirror hint the
/// optimizing tier's arms read (`cratonvm_jit::ir_lower::
/// class_mirror_hint_resolve`), so the next compiled call of the pair needs
/// no call. Never changes an answer: a hint is only a guess that compiled
/// code proves against the class word before using it.
///
/// * Steady state (this mirror's hint names `class_id` and its class word is
///   the mirror): two loads, nothing else.
/// * Only while the compiled-`getClass()` table exists (a compile asked for
///   it), for a class-band id (no lambda proxy), with the switch on, and for a
///   class `kind` gives a hint (`0` for an array or primitive-named class:
///   their pairs keep the native's array / primitive arms).
/// * The class word is published first if it is not, under exactly
///   [`class_is_instance_exact_hit`]'s rule -- the class's display verdict is
///   memoised as its stamp, or no `getClass()` remap arm names it -- since a
///   word also answers compiled `getClass()` for the class's instances. (An
///   interface's or abstract class's word is never read by a `getClass()`
///   prefix: no instance's header names it.) A remapped class gets no word,
///   hence no hint, and its pairs keep the call.
/// * A refused class (an array, a primitive name, a remapped `getClass()`) is
///   remembered in the slot (`MIRROR_HINT_DECLINED`, no present bit), so its
///   later calls stop at the steady-state test.
/// * The hint is written under the `class_mirrors` read lock, after reading
///   `class_mirrors[class_id] == mirror` and the class word equal to it there
///   (both writers' lock; the GC's re-key / forget hold the WRITE lock).
/// * On a hint's first fill, the class's supers display is published if it
///   has none (non-blocking: a busy class manager skips it), so a class used
///   as a SOURCE before its `<clinit>` completed is served too.
///
/// No allocation, no Java, no GC point: the natives that call it are leaf.
pub fn class_mirror_id_hint_fill(
    vm: &SharedVm,
    mirror: ObjectRef,
    class_id: ClassId,
    kind: &dyn Fn() -> u8,
) {
    let words = &vm.classes.getclass_mirror_words;
    if !words.is_allocated() {
        return;
    }
    let raw = class_id.as_u32();
    if usize::try_from(raw).map_or(true, |i| {
        i >= cratonvm_jit::ir_lower::GETCLASS_WORDS_CLASS_CAPACITY
    }) {
        return;
    }
    // Cast: the mirror's address, as compiled code compares it.
    let addr = mirror.as_ptr() as usize;
    let present = usize::try_from(cratonvm_jit::ir_lower::MIRROR_HINT_PRESENT).unwrap_or(0);
    let declined = usize::try_from(cratonvm_jit::ir_lower::MIRROR_HINT_DECLINED).unwrap_or(0);
    let hint = words.mirror_hint(addr);
    // Cast: the low 32 bits of a hint word are its class id.
    let same_class = (hint as u32) == raw;
    let names_it = hint & present != 0 && same_class;
    if names_it && words.get(class_id) == addr {
        return;
    }
    // A class this filler already refused (below): nothing to do. Only a
    // remembered refusal -- compiled code reads no present bit there.
    if hint & declined != 0 && same_class {
        return;
    }
    if !words.mirror_hints_enabled() {
        return;
    }
    // Remember a refusal in the slot (no present bit: compiled code declines
    // as for an empty slot), so the next call of this mirror stops above.
    let refuse = || {
        let word = cratonvm_jit::ir_lower::MIRROR_HINT_DECLINED | u64::from(raw);
        words.publish_mirror_hint(addr, usize::try_from(word).unwrap_or(0));
    };
    let k = kind();
    if k == 0 {
        refuse();
        return;
    }
    if words.get(class_id) == 0 {
        let faithful =
            if cratonvm_native_builtins::getclass_display_is_cached_stamp(vm.vm_identity, class_id)
            {
                Some(true)
            } else {
                // `None`: a busy class manager -- ask again on a later call.
                vm.classes.class_manager.try_read().map(|cm| {
                    cm.get_class(class_id).is_some_and(|class| {
                        cratonvm_native_builtins::getclass_reports_only_itself(&class.name)
                    })
                })
            };
        match faithful {
            Some(true) => {}
            Some(false) => {
                refuse();
                return;
            }
            None => return,
        }
        let mirrors = vm.classes.class_mirrors.read();
        if mirrors.get(&class_id).copied() == Some(mirror) {
            words.publish(class_id, mirror);
        }
    }
    {
        let mirrors = vm.classes.class_mirrors.read();
        if mirrors.get(&class_id).copied() != Some(mirror) || words.get(class_id) != addr {
            return;
        }
        let word = cratonvm_jit::ir_lower::mirror_hint_word(raw, k == 2);
        words.publish_mirror_hint(addr, usize::try_from(word).unwrap_or(0));
    }
    if !names_it && vm.jit.supers_displays.enabled() && !vm.jit.supers_displays.is_published(raw) {
        vh_const_publish_display(vm, class_id, false);
    }
}


#[derive(Clone, Copy)]
pub struct NativeSiteCache {
    pub kind: LeafNativeKind,
    /// `poly_call_site::vh_value_site_classes(info.descriptor)` for a
    /// signature-polymorphic (`poly`) entry, `0` otherwise. Published before
    /// the native runs, so a `VarHandle` store-mode native can apply
    /// `asType`'s static refusals (round 11 wave 16,
    /// `r11w16-vh-jit-site-cache-arm-vh-value-site-patch-FIXED-20260925`).
    /// Since round 15 wave 3 (lane vh3; VH1-3) the whole
    /// `poly_call_site::vh_param_site_classes` word (coordinates included,
    /// read modes too), armed with `arm_vh_param_site`.
    pub vh_value_site: u64,
    /// Whether the resolved slot claims the leaf contract, and may therefore go
    /// through `safe_native_call_leaf` instead of the full funnel.
    ///
    /// `false` entries are the majority, and they are the reason this cache is
    /// worth having for more than leaves: **skipping `invoke_or_native` is a
    /// bigger win than skipping the funnel.** Measured on a quiet host with
    /// `probes/AqsAttributionProbe.java`:
    ///
    /// | | ns/op |
    /// |---|---:|
    /// | ordinary Java call | 7.9 |
    /// | `AtomicInteger.get` — leaf, site-cached | 233 |
    /// | `AtomicInteger.compareAndSet` — non-leaf | 806 |
    /// | `setExclusiveOwnerThread` — non-leaf, 2 object args | 821 |
    ///
    /// The ~570 ns between the leaf and non-leaf rungs is not the funnel — it
    /// is the dispatcher's tail re-running `invoke_or_native`'s ~27-gate string
    /// cascade and its three-string registry hash **on every call**. Resolving
    /// the site once removes that for both flavours; the leaf claim then
    /// decides only which call wrapper runs.
    pub leaf: bool,
    pub callback: cratonvm_native_api::NativeCallback,
    /// §4 census handle — see [`NativeDispatchCache::native_id`].
    pub native_id: Option<cratonvm_native_api::NativeMethodId>,
    /// Exact receiver class id this entry is valid for, or `None` for an
    /// `invokestatic` site (no receiver to guard).
    ///
    /// A virtual/interface site is resolved on the RECEIVER's runtime class,
    /// not on `info.class_name` (the constant-pool owner), for the same reason
    /// [`NativeDispatchCache`] is keyed that way: an interface method resolves
    /// to whatever the receiver overrides. The guard is re-tested on every hit
    /// so a site that goes polymorphic falls out to the generic dispatcher
    /// rather than calling the wrong body.
    pub receiver_class_id: Option<u32>,
    /// The entry was resolved through rule 4 — a signature-polymorphic
    /// `VarHandle` access mode, whose native is registered with an erased
    /// `Object[]`/`Object` signature that has nothing to do with the call
    /// site's own descriptor.
    ///
    /// The dispatch side must then unbox the erased result against the CALL
    /// SITE descriptor (`unbox_poly_return_checked`), exactly as
    /// `invoke_on_class_shared_inner` does. `coerce_native_return` — what
    /// every other entry uses — would hand a boxed `Integer` back into an
    /// `int` return slot.
    pub poly: bool,
    /// `capability::classify_native`'s answer for this site's RECEIVER class,
    /// resolved once with the rest of the entry.
    ///
    /// `Some` means the dispatch must run
    /// `vm_exec::check_native_dispatch_capability_for_slot` before calling the
    /// callback — the same gate, at the same point in the call, as
    /// `invoke_or_native`'s "dispatch site 1 of 3". `None` for every native
    /// that is not process spawn, library load, `Unsafe`, Panama, or
    /// file/socket I/O, which is nearly all of them; those pay one
    /// predicted-not-taken branch.
    ///
    /// It is the KIND and not a flag because the dispatch must not re-derive
    /// it: only this resolution saw the receiver's runtime class. See the long
    /// note at the classification site in `resolve_native_site` for why this is
    /// carried at all and no longer a refusal.
    pub capability_kind: Option<cratonvm_native_api::CapabilityKind>,
    /// This site's descriptor, decoded once — see [`NativeSiteDescriptor`].
    pub desc: NativeSiteDescriptor,
    /// `Some((is_static, declaring class))` when the native answers an
    /// `ACC_SYNCHRONIZED` method under `native_sync_enabled`
    /// (`interpreter::native_sync_for_id`, the funnel's own memo): the
    /// dispatch then takes the full funnel under that monitor
    /// (`safe_native_call_synchronized`) and none of the leaf / collection /
    /// `VarHandle` shortcuts. `None` — nearly every entry — costs one
    /// not-taken branch.
    pub sync: Option<(bool, ClassId)>,
    /// The ANCESTOR of the receiver's class whose registered native this
    /// entry serves (`resolve_native_owner_for_receiver`'s walk), with that
    /// class's redefine generation at fill; `None` for the receiver's own
    /// native, a static or `invokespecial` site (their class is the one the
    /// hit's other redefine tests ask), a carrier entry and the
    /// signature-polymorphic tail. Round 15 wave 4, lane walk2
    /// (`r15w3-walk-helpers-site-cache-owner-redefine-patch-FIXED-20260930.md`): the
    /// hit asked the site's class and the receiver's, never this one, so a
    /// JVMTI redefinition of the owner after the fill kept its native over
    /// the woven body wherever no retire flushed this memo.
    ///
    /// Round 15 wave 4 (lane walk3): with
    /// `NativeMethodRegistry::jit_site_cache_owner_redefine` on, a rule-1
    /// entry (the receiver's own native, or a static site's) names its owner
    /// here too (`site_native_owner_scope`).
    pub native_owner: Option<(ClassId, u32)>,
    /// The VM's redefine epoch (`JitCache::redefine_epoch`) read before
    /// `native_owner`'s generation. While it holds, nothing was redefined
    /// since the fill and the hit asks nothing more.
    pub fill_redefine_epoch: u32,
    /// `native_owner` is the ONE class whose redefinition can stale this
    /// entry, and the fill applied the redefine rule to it
    /// (`site_native_owner_scope`): the hit skips its permanent "was the
    /// receiver / the constant-pool class ever redefined" tests and asks the
    /// owner once per redefine epoch. `false` (a carrier, rule-4 or
    /// unscoped entry, and every entry under
    /// `CRATONVM_JIT_SITE_CACHE_OWNER_REDEFINE=0`) keeps those tests. Round 15
    /// wave 4, lane walk3
    /// (`r15w4-walk2-jit-site-cache-declines-forever-after-a-redefinition-FIXED-20261001.md`);
    /// default ON since round 16 (lane redefine).
    pub owner_scoped: bool,
    /// A redefined-owner refusal (`site_refusal` slot 12) for the ONE receiver
    /// class in `receiver_class_id`, not a site-wide one: every call with that
    /// receiver declines (the woven body, or the funnel's per-call walk,
    /// decides), and the first call with another receiver re-resolves the site
    /// once (`native_site_refill_for_receiver`). The rest of the entry is a
    /// placeholder that is never dispatched (`NativeSiteCache::refusal_for_receiver`).
    /// Round 16 (lane redefine; backlog sweepB S4, WK4-6): a site-wide
    /// `(generation, None)` refused every later receiver of a polymorphic site
    /// whose FIRST receiver's owner was woven. Only with
    /// `NativeMethodRegistry::jit_site_cache_owner_redefine`.
    pub refused_for_receiver: bool,
}

impl NativeSiteCache {
    /// The receiver-keyed slot-12 refusal (see `refused_for_receiver`): a
    /// virtual/interface site whose receiver `guard` resolved to a redefined
    /// owner. Its callback is never called (the hit returns before any
    /// dispatch for a `refused_for_receiver` entry). Fill time only.
    pub fn refusal_for_receiver(guard: u32, info: &JitInvokeInfo, fill_redefine_epoch: u32) -> Self {
        fn never_dispatched(
            _ctx: &mut dyn cratonvm_native_api::NativeContext,
            _args: &[Value],
        ) -> crate::error::MethodCallResult {
            Ok(None)
        }
        NativeSiteCache {
            kind: LeafNativeKind::Callback,
            leaf: false,
            callback: never_dispatched,
            native_id: None,
            receiver_class_id: Some(guard),
            capability_kind: None,
            desc: NativeSiteDescriptor::decode(info.descriptor),
            poly: false,
            vh_value_site: 0,
            sync: None,
            native_owner: None,
            fill_redefine_epoch,
            owner_scoped: false,
            refused_for_receiver: true,
        }
    }
}

/// Round 16 (lane redefine; backlog sweepB S4): must the cached `entry` be
/// re-resolved for a call whose receiver class is `receiver`? Only a
/// receiver-keyed refusal (`NativeSiteCache::refused_for_receiver`) for ANOTHER
/// receiver: a positive entry keeps the monomorphic contract (another receiver
/// declines without re-resolving, as before), and a site-wide refusal is
/// answered before the receiver is known. Pure; the hit asks it.
pub fn native_site_refill_for_receiver(entry: Option<&NativeSiteCache>, receiver: Option<u32>) -> bool {
    entry.is_some_and(|e| e.refused_for_receiver && e.receiver_class_id != receiver)
}

/// Round 16 (lane redefine): what a re-resolution for a second receiver
/// (`native_site_refill_for_receiver`) caches. A second receiver-keyed refusal
/// becomes the site-wide refusal (`None`): two receivers with woven owners
/// would otherwise re-resolve the site on every alternation, so a site pays
/// at most one extra resolution per registry generation.
pub fn native_site_refill_outcome(resolved: Option<NativeSiteCache>) -> Option<NativeSiteCache> {
    resolved.filter(|e| !e.refused_for_receiver)
}

/// `CRATONVM_JIT_SITE_CACHE_CAPABILITY` — default-ON, `=0` restores the
/// blanket refusal of capability-classified triples in `resolve_native_site`.
///
/// One binary, two arms, so "serve `Unsafe` from the site cache and run the
/// gate on dispatch" can be priced against "refuse the site and pay
/// `invoke_or_native` for every call" without a rebuild — and switched off
/// on the spot if a capability question is ever raised against it.
pub fn site_cache_capability_gate_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map_or(true, |v| v != "0" && !v.eq_ignore_ascii_case("false"))
    })
}

/// A call site's descriptor, decoded once and carried on the cache entry the
/// dispatch already probes.
///
/// A site-cached native dispatch used to parse `info.descriptor` twice per
/// call: `decode_dispatch_values_into` walked it to type each argument, and
/// `coerce_native_return` scanned it again for the return tag. Measured on
/// this file's `jit_native_dispatch_profile`, that argument decode cost
/// 12.4-14.7 ns for a receiver-plus-one-object site against 9.5-11.9 ns with
/// the tags already in hand.
///
/// It rides on [`NativeSiteCache`] rather than in a memo of its own because
/// that entry is already fetched — the probe is 0.8 ns and is paid whether or
/// not this is here. A separate `JitSiteKey`-keyed map was tried first and
/// measured 14 ns per lookup, i.e. more than the parsing it removed.
///
/// Validity needs no argument beyond the one the entry already carries: the
/// descriptor belongs to the `JitInvokeInfo` this entry was resolved against,
/// and every path that can invalidate the entry (registry generation, receiver
/// class, class redefinition, the site-memo flush on a JIT generation change)
/// invalidates the decode with it.
#[derive(Clone, Copy)]
pub struct NativeSiteDescriptor {
    /// One descriptor tag per PARAMETER, receiver excluded.
    pub params: [u8; INLINE_JIT_NATIVE_ARGS],
    /// Parameters decoded into `params`, or `u8::MAX` when the site declares
    /// more than `INLINE_JIT_NATIVE_ARGS` of them — then `params` is unusable
    /// and the caller parses the descriptor exactly as before.
    pub params_len: u8,
    /// What `cratonvm_jit::return_type` answers for this descriptor.
    pub ret: u8,
}

impl NativeSiteDescriptor {
    pub fn decode(descriptor: &str) -> Self {
        let mut params = [0u8; INLINE_JIT_NATIVE_ARGS];
        let mut count = 0usize;
        for tag in DescriptorParamIter::new(descriptor) {
            if count < INLINE_JIT_NATIVE_ARGS {
                params[count] = tag;
            }
            count += 1;
        }
        Self {
            params,
            params_len: if count > INLINE_JIT_NATIVE_ARGS {
                u8::MAX
            } else {
                count as u8
            },
            ret: cratonvm_jit::return_type(descriptor),
        }
    }

    /// The decoded tags, or `None` for a site with more parameters than this
    /// can hold.
    pub fn params(&self) -> Option<&[u8]> {
        if self.params_len == u8::MAX {
            None
        } else {
            Some(&self.params[..self.params_len as usize])
        }
    }
}

// Thread-local map from [`JitSiteKey`] -> cached JIT entry.
// Using a thread-local avoids synchronization on the hot path.
// T10.9.B: FxHashMap — pointer values are internal; this is touched on every
// JIT-dispatched invoke.
//
// The key is `(vm_identity, JitInvokeInfo pointer)`, NOT the pointer alone —
// see [`JitSiteKey`] for why the pointer alone aliases across VMs and what a
// cross-VM hit would execute. A thread reaches two VMs via JNI
// `AttachCurrentThread`, or by being reused across `SharedVm`s in one test
// process; thread-local is not per-VM.
site_keyed_memos! {
    retire_only {
        /// Warm-up counts for compiled-call bytecode templates. A template can
        /// be rebuilt on each publication before it reaches its profile-fold
        /// stride; the VM, site, and selected-method key keeps its count alive
        /// until code retirement without sharing it across VMs or methods.
        COMPILED_TEMPLATE_WARMUP: (JitSiteKey, u128) => std::sync::Arc<std::sync::atomic::AtomicU32>;
        /// Statically bound site -> the published callee body it enters. The
        /// body is found by exact key (`JitCache::get`), so the only event
        /// that can change the answer is a replacement or withdrawal of that
        /// key. A withdrawal is a retire; a replacement is publish-only and
        /// marks the old body `superseded`, which the hit checks
        /// (`DispatchCache::names_a_stale_body`). `VIRTUAL_DISPATCH_CACHE`
        /// below is held to the same check at both of its hits.
        DISPATCH_CACHE: JitSiteKey => DispatchCache;
        /// Virtual/interface call sites are keyed by their JIT site key AND the
        /// receiver's actual class id. A static CP owner is not sound here:
        /// an interface method may resolve to a receiver override.
        VIRTUAL_DISPATCH_CACHE: (JitSiteKey, u32) => DispatchCache;
        /// `(JitSiteKey, receiver ClassId or 0) -> ` the compiled callee whose own
        /// exception handler a dispatch helper resumes (`resolve_callee_cached`).
        ///
        /// Every serviced callee exception used to re-resolve it under the class
        /// manager lock and rebuild the template — a copy of the whole method body,
        /// its exception table and three `Arc<str>`s — per THROW, so a loop whose
        /// compiled callee throws and catches paid that on every iteration. Only
        /// successful resolutions are stored (a callee with a non-empty table that
        /// is not `synchronized`), so a hit also answers
        /// `callee_has_exception_table` without the lock. Keyed on the receiver
        /// class for kinds 0/2 (it selects the override), on the site alone
        /// otherwise; the `RedefineGate` is re-tested per hit, and the memo is on
        /// the class-identity flush because the static kinds resolve by NAME.
        CALLEE_HANDLER_TEMPLATE_CACHE: (JitSiteKey, u32) => (std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate);
        /// `(JitSiteKey, receiver ClassId) -> CachedDispatchTarget`.
        ///
        /// Keyed exactly like `VIRTUAL_DISPATCH_CACHE`. Array receivers and
        /// `ClassId(0)` never reach it — an array header carries its COMPONENT
        /// class id, so `(site, class id)` does not identify one.
        VIRTUAL_TARGET_CACHE: (JitSiteKey, u32) => CachedDispatchTarget;
        /// `JitSiteKey -> (redefine epoch, verdict)`: whether the class this
        /// site names was ever redefined, as of that epoch of the site's VM
        /// ([`site_class_was_redefined`]). No publication can change which
        /// class a site names.
        QUIESCE_VERDICT: JitSiteKey => (u32, bool);
        /// Leaf-native resolution per JIT call site — see
        /// [`NativeSiteCache`]. `None` is a cached refusal ("this site is
        /// not a leaf native"), which is what keeps the ~27-gate `invoke_or_native`
        /// probe off every OTHER site's steady state; it is keyed on the registry
        /// generation stored beside it so a lazy `register_*` pass that appears
        /// later is still seen.
        ///
        /// W4B: `retire_only` (was `publication_sensitive`). A resolution is a
        /// function of (site, receiver class, registry generation, redefine
        /// epoch): the first two are in the key/guard, the last two are checked
        /// per hit, and the door serves a positive entry before any compiled-callee
        /// machinery runs, so a NEW body can neither falsify nor be preferred
        /// over it. Every compile used to empty this memo on every thread, so
        /// under javac's continuous publication each native call re-resolved its
        /// site (class-manager read, name copy, registry hash, admission).
        /// Address reuse is closed by the retire bump in `Drop for CompiledMethod`.
        /// A refusal that is an artefact of one resolution (class manager
        /// write-locked) is never stored (`resolve_native_site_tracked`).
        NATIVE_SITE_CACHE: JitSiteKey => (u32, Option<NativeSiteCache>);
        /// `(JitSiteKey, receiver ClassId) -> ` the native-site resolution of
        /// a virtual/interface site's second receiver class, stamped with the
        /// registry generation like `NATIVE_SITE_CACHE` (JIT round 19 wave 3,
        /// lane funnel3; FUNNEL2-19-1, `native_site_second_receiver_entry`).
        /// Empty unless `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY` is on.
        /// `retire_only` for the reason `NATIVE_SITE_CACHE` gives.
        NATIVE_SITE_CACHE_ALT: (JitSiteKey, u32) => (u32, Option<NativeSiteCache>);
    }
    publication_sensitive {
        DISPATCH_COUNTER: JitSiteKey => u32;
        OBJECT_NATIVE_DISPATCH_CACHE: JitSiteKey => NativeDispatchCache;
        INTEGER_NATIVE_DISPATCH_CACHE: JitSiteKey => Option<IntegerNativeDispatchCache>;
        VIRTUAL_DISPATCH_COUNTER: (JitSiteKey, u32) => u32;
        /// `(JitSiteKey, receiver ClassId) -> declined-probe count`.
        ///
        /// A compile probe that declined this callee will decline it again. On
        /// netty's `AdaptiveByteBufAllocatorTest` `probe_returned_none` read
        /// **6 592 571** — every single entryless MIC hit in the run — against
        /// `cyc_compile_probe` of 9.16e9 cycles, i.e. ~3.5 s of a ~430 s run spent
        /// re-asking a question that had already been answered on the first call.
        ///
        /// Throttled rather than permanent: a refusal can be transient (another
        /// thread holding the compile, a tier budget), and a never-retry memo would
        /// pin such a site to the helper for the life of the process. One probe per
        /// [`MIC_COMPILE_PROBE_RETRY`] calls keeps the retry while removing 1023 of
        /// every 1024 probes. Being a `site_keyed_memos!` member is what makes it
        /// safe: the same generation and class-identity flushes that drop every
        /// other dispatch memo drop this one too.
        MIC_COMPILE_DECLINED: (JitSiteKey, u32) => u32;
        /// `JitSiteKey -> ` the interpreter frame template for an `invokestatic`
        /// callee the JIT did not compile, or `None` as a cached refusal.
        ///
        /// Keyed by the call site alone: a static call has no receiver, so the
        /// target is a pure function of the site. Being a `site_keyed_memos!`
        /// member is what makes it safe — the same generation and class-identity
        /// flushes that drop every other dispatch memo drop this one too.
        STATIC_BYTECODE_CALLEE_CACHE: JitSiteKey => Option<(std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate)>;
        /// `(JitSiteKey, receiver ClassId) -> ` the interpreter frame template for
        /// an `invokevirtual`/`invokeinterface` callee the JIT did not compile, or
        /// `None` as a cached refusal.
        ///
        /// Keyed by the RECEIVER CLASS as well as the site, because that pair is
        /// what selects the override: the same site reached with a different
        /// receiver class gets its own entry rather than reusing one resolved for
        /// another class. That is also why no separate receiver guard is re-tested
        /// per hit — the key IS the guard. Array receivers and `ClassId(0)` never
        /// reach it, for the reason `VIRTUAL_TARGET_CACHE` records.
        VIRTUAL_BYTECODE_CALLEE_CACHE: (JitSiteKey, u32) => Option<(std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate)>;
        /// `JitSiteKey -> ` the interpreter frame template for an `invokespecial`
        /// callee the JIT did not compile, or `None` as a cached refusal.
        ///
        /// Keyed by the call site ALONE, unlike its virtual twin, and that is the
        /// definition of `invokespecial`: the target is chosen by the resolved
        /// constant-pool class, never by the receiver's runtime class. A
        /// receiver-keyed memo here would be modelling a retarget the opcode
        /// explicitly does not perform — the one that turns a super-call into a
        /// self-call and recurses forever (see the `invoke_kind == 1` arm).
        SPECIAL_BYTECODE_CALLEE_CACHE: JitSiteKey => Option<(std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate)>;
        /// `JitSiteKey -> ()`: this statically bound site's published callee
        /// needs a wrapped entry (`requires_wrapped_entry`), so the by-name
        /// `jit_cache` probe in `jit_invoke_dispatch` can only refuse it again.
        /// Without this memo nothing was cached for such a site and every call
        /// repeated `class_manager.read()`, `jit_cache.read()` and a
        /// string-keyed lookup (round 11).
        ///
        /// A refusal, so it is publication-sensitive (round 13 wave 6, lane
        /// sync3; it was `retire_only` until then): since round 13 wave 4 a
        /// synchronized INSTANCE method can publish a SELF-LOCKING body
        /// (`CompiledMethod::self_locks_monitor`, not wrapped) over a wrapped
        /// one it was compiled after -- a replacement, which is publish-only --
        /// and a `retire_only` memo kept an `invokespecial` of it (private or
        /// `super.`) on the interpreter route for the life of the thread.
        WRAPPED_STATIC_CALLEE: JitSiteKey => ();
    }
}

thread_local! {
    /// Real `java/lang/Integer` class discovered from the first ordinary
    /// `valueOf` result in each VM, as `(vm_identity, class id)`. A different
    /// VM identity invalidates the entry.
    pub static INTEGER_WRAPPER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
    /// Exact real-JDK `java/util/regex/Matcher` class id, discovered once per
    /// VM for the virtual-MIC native fast path. `(vm_identity, class id)`.
    pub static MATCHER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Preserve this compiled call site's method count when its frame template
/// is invalidated. Code retirement and VM teardown clear the site-keyed memo.
pub fn compiled_template_warmup(
    site: JitSiteKey,
    method: u128,
) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
    COMPILED_TEMPLATE_WARMUP.with(|map| {
        std::sync::Arc::clone(map.borrow_mut().entry((site, method)).or_insert_with(|| {
            std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0))
        }))
    })
}

// ===========================================================================
// BUG-1 fix: native-stack recursion guard for the JIT→JIT dispatch path.
//
// A recursive Java method that has been JIT-compiled (e.g. `binaryTrees(18)`'s
// deep `make(int)` self-recursion) never re-enters `interpreter::execute`, so
// the interpreter's `EXEC_DEPTH` / `EXEC_DEPTH_CEILING` guard (which throws a
// *catchable* `StackOverflowError`) never fires. Each compiled recursion level
// instead stacks a large native Rust frame through
// `jit_invoke_dispatch` / `jit_invoke_virtual_mic` → compiled entry → … with
// nothing checking remaining OS stack, so deep recursion overflows the guard
// page → uncatchable rc=127 abort.
//
// We mirror the interpreter's guard with a dedicated thread-local JIT-dispatch
// depth counter, incremented at the top of BOTH dispatch helpers under an RAII
// Drop-decrement, compared against the per-thread ceiling
// `interpreter::jit_dispatch_depth_ceiling()` (derived from the thread's REAL
// native stack size, using a generous per-level budget because the JIT
// dispatch frame is much larger than an interpreter level). On overflow we do
// NOT recurse: we stash a catchable `java/lang/StackOverflowError` in
// `JIT_PENDING_EXCEPTION` and return the `i64::MIN` deopt sentinel, exactly
// like the array-NPE / AIOOBE helpers, so the interpreter's post-JIT drain
// routes it through the method's exception table.
// ===========================================================================
thread_local! {
    /// Re-entrant JIT-dispatch depth for the current thread. Incremented on
    /// entry to `jit_invoke_dispatch` / `jit_invoke_virtual_mic` and
    /// decremented (RAII) on return. Compared against
    /// `interpreter::jit_dispatch_depth_ceiling()`.
    pub static JIT_DISPATCH_DEPTH: Cell<u32> = const { Cell::new(0) };

    pub static DISPATCH_CACHE_JIT_GENERATION: Cell<u64> = const { Cell::new(0) };
}

/// RAII guard that decrements [`JIT_DISPATCH_DEPTH`] when dropped. Constructed
/// by [`enter_jit_dispatch`] only AFTER the depth has been incremented and the
/// ceiling check passed, so every successful entry has exactly one matching
/// decrement on every return path (including the compiled callee unwinding).
pub struct JitDispatchDepthGuard;
impl Drop for JitDispatchDepthGuard {
    fn drop(&mut self) {
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// t1 wave 5 lane E2 (stage 2 of
/// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`):
/// replace this OS thread's re-entrant JIT-dispatch depth, answering the old
/// one. A continuation slice run on its carrier's OS thread below carrier
/// compiled frames starts from 0, as on a runner, so its `StackOverflowError`
/// budget is not shrunk by the carrier's levels (the real-stack floor,
/// `jit_native_stack_exhausted`, still sees every carrier frame). Every guard
/// the slice takes is dropped inside the slice, so restoring cannot unbalance
/// the carrier's count.
pub fn replace_jit_dispatch_depth(v: u32) -> u32 {
    JIT_DISPATCH_DEPTH.try_with(|d| d.replace(v)).unwrap_or(0)
}

/// Only an instance constructor's first argument is an uninitialized receiver.
pub fn constructor_receiver(info: &JitInvokeInfo, args: &[i64]) -> i64 {
    if info.invoke_kind == 1 && info.method_name == "<init>" {
        args.first().copied().unwrap_or(0)
    } else {
        0
    }
}

/// Enter a JIT dispatch level: bump the thread-local depth and check it against
/// the per-thread native-stack ceiling.
///
/// On success returns `Ok(guard)` — the caller binds it (e.g. `let _g = ...`)
/// so the level is released on return. On overflow returns `Err(sentinel)`:
/// the depth has already been rolled back, a catchable
/// `java/lang/StackOverflowError` has been stashed in `JIT_PENDING_EXCEPTION`
/// (when constructible), and the caller must immediately `return` the contained
/// `i64::MIN` deopt sentinel WITHOUT recursing further. If the throwable cannot
/// be constructed (heap too exhausted) the sentinel is still `i64::MIN` so the
/// JIT caller deopts rather than continuing with corrupt state.
#[must_use]
pub fn enter_jit_dispatch(vm: &SharedVm, ctor_receiver: i64) -> Result<JitDispatchDepthGuard, i64> {
    let ceiling = crate::runtime::interpreter::jit_dispatch_depth_ceiling();
    let depth = JIT_DISPATCH_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v + 1
    });
    if depth > ceiling && !dispatch_ceiling_yields_to_stack_floor() {
        // Roll back the increment we just made — we are NOT entering a level.
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        // Interpreter round i1 wave 40, lane L2: name the bound that tripped
        // (the depth ceiling here, the stack floor below), so a probe's `soe`
        // row says which one to scale. Cold: once per raised error.
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] jit-dispatch depth ceiling TRIP: depth={depth} ceiling={ceiling}"
            );
        }
        crate::runtime::interpreter::withdraw_compiled_constructor_at_entry(vm, ctor_receiver);
        return Err(raise_jit_stack_overflow(vm));
    }
    // Round 13 wave 2 (lane mega, `r13w1-crash-ic-recursion-stack-guard-patch`
    // patch 2): the depth above counts only helper levels, so a recursion
    // whose other levels are raw inline-cache hits arrives with a small count
    // and a nearly exhausted native stack. Both dispatch helpers enter here --
    // `jit_invoke_dispatch` is also the slow path of a PIC-only or cache-less
    // site -- so one check covers every inline-cache guard's low-stack edge.
    if jit_native_stack_exhausted() {
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        if crate::runtime::env_cache::dbg_jitc() {
            // Interpreter round i1 wave 41, lane L2: the stack the helper
            // levels above the first compiled entry used, per level -- the
            // floor sits `SELF_CALL_STACK_BUDGET` (plus a requested stack's
            // growth) below that entry when that term binds, so this is an
            // estimate of the real per-level cost the ceiling's 8 KiB
            // guesses at (an over-estimate when the thread's own stack low
            // end bound the floor instead).
            let probe = 0u8;
            let sp_now = &probe as *const u8 as usize;
            let floor = JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
            let above = floor
                .saturating_add(current_self_call_budget().bytes())
                .saturating_sub(sp_now);
            eprintln!(
                "[cratonvm-jitc] jit-dispatch native stack floor TRIP: depth={depth} ceiling={ceiling} \
                 approx_bytes_per_level={}",
                above / usize::try_from(depth.max(1)).unwrap_or(1)
            );
        }
        crate::runtime::interpreter::withdraw_compiled_constructor_at_entry(vm, ctor_receiver);
        return Err(raise_jit_stack_overflow(vm));
    }
    Ok(JitDispatchDepthGuard)
}

/// Kill switch for [`dispatch_ceiling_yields_to_stack_floor`] (interpreter
/// round i1 wave 41, lane L2;
/// `docs/feature-designs/native-dispatch-architecture.md`).
/// `false` restores the depth ceiling as a hard bound.
pub const DISPATCH_CEILING_YIELDS_TO_STACK_FLOOR: bool = true;

/// Past the per-thread JIT-dispatch depth ceiling, does the real stack floor
/// decide instead? The ceiling is an ESTIMATE of how many helper levels fit
/// (half the carrier at 8 KiB a level: 512 on a default 8 MiB thread, where
/// the interpreter admits 8192 frames and HotSpot thousands of compiled ones),
/// while [`jit_native_stack_exhausted`], asked at the same helper entry right
/// after it, compares the real stack pointer with the compiled-recursion
/// floor, which keeps at least the self-call margin (1 MiB on a default
/// carrier) above the thread's own stack end -- the bound a direct compiled
/// self call already lives by. So while that check is on
/// (`CRATONVM_JIT_IC_STACK_GUARD`, default on) the estimate only ever threw
/// early: a recursion through the helpers stopped at 512 levels that the
/// stack held. With the guard switched off the ceiling binds as before.
///
/// Reached only past the ceiling, so the flag read is off the common path.
#[cold]
pub fn dispatch_ceiling_yields_to_stack_floor() -> bool {
    DISPATCH_CEILING_YIELDS_TO_STACK_FLOOR
}

/// Stash a catchable `java/lang/StackOverflowError` for the JIT caller to route
/// through its exception table, and return the `i64::MIN` deopt sentinel.
///
/// Mirrors `jit_newarray_oom`: obtain the current `&mut JvmThread`, construct
/// the throwable via `create_exception_object`, and `set_jit_pending_exception`.
/// Cold path — only hit at pathological recursion depth.
#[cold]
pub fn raise_jit_stack_overflow(vm: &SharedVm) -> i64 {
    // SAFETY: called from inside a JIT dispatch helper, on the thread that set
    // the JIT thread pointer; no other `&mut JvmThread` is live at this point
    // (we are above any compiled-callee invocation).
    let mut stashed = false;
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        // Round 13 wave 2 (lane mega): the constructor below runs Java
        // (`Throwable.<init>` -> `fillInStackTrace()`, an inline-cache call)
        // at a stack pointer that is BELOW the floor by definition, so every
        // compiled stack check it passes trips again and raises a nested
        // `StackOverflowError`, whose constructor trips again... each level one
        // more `execute`, until the interpreter's re-entry cap fails the whole
        // chain into a bare deopt sentinel that callers answer by re-running
        // the callee interpreted: the C2-arm hang of `R13CrashIcRecursionSoe`.
        // Suspend the floor while the error is built (HotSpot disables its
        // yellow zone for the same reason); the margin below it is at least
        // three 64 KiB strides, far more than one constructor needs.
        let _suspended = SuspendedStackFloor::enter();
        if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/StackOverflowError",
            None,
        ) {
            set_jit_pending_exception(thread, exc);
            stashed = true;
        }
    }
    // Round 12 wave 4: nothing constructible -- raise the out-of-band signal
    // (the `jit_monitor_enter_body` idiom) so the bare sentinel is never taken
    // as a value with no exception pending.
    if !stashed && helper_failure_conversion_on() {
        set_jit_deopt_pending();
    }
    i64::MIN
}

/// The compiled-recursion stack floor, suspended for the life of the value
/// (round 13 wave 2, lane mega): [`raise_jit_stack_overflow`] builds its error
/// with every compiled stack check disarmed, then puts the floor back.
///
/// Suspension stores `0` in the thread's floor cell, which every check reads
/// as "far above": the self-call guard and the IR inline-cache sample
/// (`jit_self_call_stack_guard`), [`jit_native_stack_exhausted`], and the
/// prologue fetch that single-pass frames entered meanwhile cache
/// (`jit_native_stack_floor`). Those frames all belong to the construction
/// and return before the restore. `CRATONVM_JIT_SOE_BUILD_SUSPENDS_FLOOR=0`
/// leaves the floor armed (the pre-fix behaviour).
pub struct SuspendedStackFloor(pub Option<usize>);

impl SuspendedStackFloor {
    pub fn enter() -> Self {
        let saved = JIT_SELF_CALL_STACK_FLOOR.with(|f| {
            let v = f.get();
            // Unset (`usize::MAX`) or already suspended (`0`, a nested raise
            // under a construction): nothing to suspend or restore here.
            if v == usize::MAX || v == 0 {
                None
            } else {
                f.set(0);
                Some(v)
            }
        });
        Self(saved)
    }
}

impl Drop for SuspendedStackFloor {
    fn drop(&mut self) {
        if let Some(v) = self.0 {
            JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(v));
        }
    }
}

/// Interpreter round i3 wave 79, lane L3: re-anchor this thread's
/// compiled-recursion budget ([`SELF_CALL_STACK_BUDGET`] below the first
/// observed stack pointer) for a native that re-enters Java -- a served
/// `Method.invoke` / `Constructor.newInstance` -- and return the value to hand
/// back to [`restore_self_call_stack_floor`] when the native returns.
///
/// The budget bounds how much stack COMPILED recursion may burn (the JIT's
/// `-Xss` analogue for the interpreter's frame cap). A reflective level's Rust
/// frames (the door, `execute`, the helpers) are not compiled Java frames, but
/// were charged to it: nested reflective calls under the JIT stopped at about
/// 300 levels on the 128 MiB main thread with a `StackOverflowError`
/// (`L3W79NestedReflectDepth` `depth-400`; HotSpot and `--nojit` run it).
///
/// The common case is free (wave-79 follow-up: unsetting the floor on every
/// call made each nested call's first compiled query ask the OS for the
/// stack bounds, +35-70% on `L3W77ReflectFactsBench` under the JIT): only a
/// call whose stack pointer is already within [`REFLECTIVE_REBASE_MARGIN`] of
/// the cached floor moves it, to [`SELF_CALL_STACK_BUDGET`] below that
/// pointer, never below the thread's own guard floor
/// ([`compute_self_call_stack_floor_in`] over `bounds`, the stack bounds the
/// caller caches; `None` asks the OS, once per rebase). A shallow call is one
/// TLS read and a compare and returns [`FLOOR_NOT_REBASED`]. An unset floor
/// (`usize::MAX`, nothing compiled asked yet) or a suspended one (`0`, a
/// `StackOverflowError` under construction) is left as it is.
pub fn rebase_self_call_stack_floor(bounds: Option<(usize, usize)>) -> usize {
    let probe = 0u8;
    let sp_now = std::hint::black_box(&probe) as *const u8 as usize;
    let floor = JIT_SELF_CALL_STACK_FLOOR.with(std::cell::Cell::get);
    if !floor_wants_rebase(sp_now, floor) {
        return FLOOR_NOT_REBASED;
    }
    rebase_self_call_stack_floor_cold(sp_now, floor, bounds)
}

/// [`rebase_self_call_stack_floor`]'s move, off the common path.
#[cold]
#[inline(never)]
pub fn rebase_self_call_stack_floor_cold(
    sp_now: usize,
    floor: usize,
    bounds: Option<(usize, usize)>,
) -> usize {
    let bounds = bounds.or_else(thread_stack_bounds);
    let budget = current_self_call_budget();
    let rebased = compute_self_call_stack_floor_in(sp_now, budget, bounds);
    if rebased >= floor {
        // The guard floor binds already: nothing lower to move to.
        return FLOOR_NOT_REBASED;
    }
    JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(rebased));
    floor
}

/// What [`rebase_self_call_stack_floor`] returns when it moved nothing;
/// [`restore_self_call_stack_floor`] then does nothing. Never a floor: a
/// floor is `0` (suspended) or an address `SELF_CALL_STACK_BUDGET` below a
/// stack pointer.
pub const FLOOR_NOT_REBASED: usize = usize::MAX;

/// How close to the cached floor a reflective door's stack pointer must be
/// before the door moves it: half the budget, so a level re-anchors once per
/// ~2 MiB of nested native frames and a shallow call never.
pub const REFLECTIVE_REBASE_MARGIN: usize = SELF_CALL_STACK_BUDGET / 2;

/// Does a reflective door at `sp_now` move the cached `floor`? Only an armed
/// floor (not unset, not suspended) within [`REFLECTIVE_REBASE_MARGIN`].
pub fn floor_wants_rebase(sp_now: usize, floor: usize) -> bool {
    floor != 0 && floor != usize::MAX && sp_now <= floor.saturating_add(REFLECTIVE_REBASE_MARGIN)
}

/// Undo [`rebase_self_call_stack_floor`]: the floor the native's caller had
/// (nothing when it moved none).
pub fn restore_self_call_stack_floor(saved: usize) {
    if saved != FLOOR_NOT_REBASED {
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(saved));
    }
}




#[cold]
pub fn throwable_class_name(vm: &SharedVm, obj: ObjectRef) -> Option<String> {
    let cid = vm.mem.heap.class_id_of(obj);
    vm.classes
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| c.name.to_string())
}

/// The S111r12 rescues' trigger: did the by-name dispatch on `dispatch_class`
/// fail with ITS OWN untranslated linkage miss (`VmError::Linkage`
/// `NoSuchMethodError` naming exactly that class and method), so that the
/// rescue may resend the call through the site's constant-pool class?
///
/// Structured only. Until interpreter round i5 wave 100 (lane L2) a second
/// arm also accepted a THROWN `NoSuchMethodError` whose detail message read
/// `C.m(desc)`; the by-name terminal has written HotSpot's `'ret C.m(params)'`
/// since 2026-09-23, so that arm had matched nothing since then and was
/// removed rather than re-keyed on the new text. The miss that arm was for --
/// a by-name dispatch on the receiver's class failing where the site's own
/// reference does not resolve -- is the site's resolution failure, which
/// HotSpot raises naming the SITE's class without running anything:
/// [`jit_site_reference_failure`] answers it in
/// [`handle_jit_dispatch_error`], for every door, from the class store rather
/// than from message text. The raw arm stays live for the dispatch classes the
/// by-name route refuses without building a throwable (the VM's `<...>`
/// placeholder names) and for a throwable that could not be materialized.
/// The rescue is a no-op for a correctly typed receiver: by-name lookup from a
/// subclass of the site's class finds every method the site's reference
/// resolves to.
#[cold]
pub fn is_dispatch_no_such_method_miss(
    err: &crate::error::MethodCallFailed,
    dispatch_class: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    use crate::error::{LinkageError, MethodCallFailed, VmError};

    match err {
        MethodCallFailed::InternalError(VmError::Linkage(LinkageError::NoSuchMethodError {
            class_name,
            method_name: err_method,
            method_descriptor,
        })) => {
            class_name == dispatch_class
                && err_method == method_name
                && method_descriptor == descriptor
        }
        _ => false,
    }
}

/// The class a compiled `invokevirtual` / `invokeinterface` site NAMED, when
/// its constant-pool reference itself provably does not resolve (JVMS 5.4.3.3
/// / 5.4.3.4 find nothing), so HotSpot's `LinkResolver` throws
/// `NoSuchMethodError` naming that class (`'int HJ.m()'`, `'int UGS.gone()'`)
/// before any selection, on every execution, and no method runs. Two shapes:
///
/// * the only declaration is a STATIC method of a superinterface (static
///   interface methods are not inherited;
///   [`crate::runtime::resolve::selection::reference_names_only_an_uninherited_static_interface_method`];
///   interpreter round i4 wave 99, lane L7, `L7W98StaticShadowDispatch` s4);
/// * nothing at all declares it -- separate compilation removed it
///   ([`crate::runtime::resolve::selection::member_provably_absent`], real class
///   bytes only) and no native is registered under the reference's own triple
///   (interpreter round i5 wave 100, lane L2, `L2W100UnresolvedSiteWarm`;
///   behind `CRATONVM_UNRESOLVABLE_METHOD_REF_THROWS`, default ON, as the
///   interpreter's check in `resolve_method_metadata_noting`).
///
/// `None` unless the site is kind 0 or 2 with an owner loaded in the site's
/// namespace ([`jit_site_owner`]) whose kind matches the invoke (a `Methodref`
/// naming an interface or an `InterfaceMethodref` naming a class is the
/// `IncompatibleClassChangeError` of step 1, which comes first). Under the
/// caller's class-manager guard; asked on a failed dispatch
/// ([`jit_site_reference_failure`]) and where a (site, receiver class) target
/// is resolved and memoized ([`virtual_dispatch_target_cached`],
/// [`jit_selection_errs`]), never per call on a cached path.
pub fn jit_site_unresolvable_owner(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    info: &JitInvokeInfo,
) -> Option<String> {
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    let owner = jit_site_owner(cm, info)?;
    let owner_class = cm.get_class(owner)?;
    if owner_class.is_interface() != (info.invoke_kind == 2) {
        return None;
    }
    let store = cm.class_store();
    // The common answer first: the reference resolves (one walk up the
    // owner's hierarchy, on a memo miss).
    if !crate::runtime::resolve::selection::member_provably_absent(
        store,
        owner,
        info.method_name,
        info.descriptor,
    ) {
        return None;
    }
    let unresolvable = crate::runtime::resolve::selection::reference_names_only_an_uninherited_static_interface_method(
        store,
        owner,
        info.method_name,
        info.descriptor,
    ) || (vm
        .natives
        .native_methods
        .find(info.class_name, info.method_name, info.descriptor)
        .is_none()
        && crate::runtime::interpreter::invoke::unresolvable_method_ref_throws_enabled());
    unresolvable.then(|| owner_class.name.to_string())
}

/// The error a compiled `invokevirtual` / `invokeinterface` site raises when
/// its constant-pool reference itself does not resolve
/// ([`jit_site_unresolvable_owner`]): HotSpot's `NoSuchMethodError` naming the
/// class the SITE named, never the receiver's class.
///
/// Every compiled dispatch door retargets onto the receiver's class before it
/// calls by name, so the by-name terminal in `invoke_on_class_shared_inner`
/// names the receiver (`'int HC1.m()'`). The S111r12 rescue that would have
/// resent the call through the site's class matched the old `C.m(desc)`
/// message text only, and its thrown arm was removed in wave 100
/// ([`is_dispatch_no_such_method_miss`]). Rewriting here, in the one sink every
/// door's failure passes through, keeps the answer the site's whatever door the
/// call took (the by-name tails, the MIC hit/miss tails, the register-overflow
/// bail, a selection failure, the bytecode-callee doors). Interpreter round i4
/// wave 99, lane L7 (the static-interface shape); wave 100, lane L2 (every
/// unresolvable reference, and the kind guard).
///
/// `None` (the error stays as it is) unless the failure is a linkage error
/// (`VmError::Linkage`, or a thrown `NoSuchMethodError` /
/// `AbstractMethodError` / `IncompatibleClassChangeError`) and
/// [`jit_site_unresolvable_owner`] names the site's class. Then no callee can
/// have been selected at all, so the failure can only be the site's own.
/// Cold: asked only on a failed dispatch.
#[cold]
pub fn jit_site_reference_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: &crate::error::MethodCallFailed,
    info: &JitInvokeInfo,
) -> Option<crate::error::MethodCallFailed> {
    use crate::error::{MethodCallFailed, VmError};
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    let is_linkage = match err {
        MethodCallFailed::InternalError(VmError::Linkage(_)) => true,
        MethodCallFailed::ExceptionThrown(exc) => matches!(
            throwable_class_name(vm, *exc).as_deref(),
            Some(
                "java/lang/NoSuchMethodError"
                    | "java/lang/AbstractMethodError"
                    | "java/lang/IncompatibleClassChangeError"
            )
        ),
        _ => false,
    };
    if !is_linkage {
        return None;
    }
    let owner_name = {
        let cm = vm.classes.class_manager.read();
        jit_site_unresolvable_owner(vm, &cm, info)?
    };
    Some(jit_site_unresolvable_error(vm, thread, owner_name, info))
}

/// The `NoSuchMethodError` an unresolvable compiled site raises, naming
/// `owner_name` (from [`jit_site_unresolvable_owner`]).
#[cold]
pub fn jit_site_unresolvable_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    owner_name: String,
    info: &JitInvokeInfo,
) -> crate::error::MethodCallFailed {

    crate::runtime::exceptions::throw_linkage_error(
        vm,
        thread,
        crate::error::LinkageError::NoSuchMethodError {
            class_name: owner_name,
            method_name: info.method_name.to_string(),
            method_descriptor: info.descriptor.to_string(),
        },
    )
}

/// S112r9 — JIT dispatch error handler. When a JIT-dispatched callee returns
/// an error, route it through `JIT_PENDING_EXCEPTION` so the interpreter's
/// post-JIT exception-routing path can find a handler (or propagate to the
/// top of the JVM with a printable message).
///
/// Previously only `MethodCallFailed::ExceptionThrown` was captured, and
/// `MethodCallFailed::InternalError` was silently dropped — the JIT helper
/// returned 0/null to the JIT caller, which would proceed as if the call
/// returned a benign null. That was the root cause of Spring Boot 3 fat-jars
/// exiting silently with rc=0 between `prepareEnvironment` and `printBanner`:
/// some downstream invoke produced an `InternalError` ("method has no Code
/// attribute" or similar linkage gap), the JIT swallowed it, the JIT'd
/// `prepareEnvironment` continued with corrupt state and returned, then the
/// caller `run()` returned cleanly without ever reaching `printBanner`.
///
/// Wrapping the InternalError in a Java `java/lang/InternalError` gives the
/// VM a real Throwable to walk through exception tables. If the heap is
/// exhausted or the class can't be loaded, we fall back to leaving the
/// error unstored — the original "swallow and return 0" behaviour. That
/// keeps this purely additive: it never makes a previously-working scenario
/// worse, only converts silent rc=0 into a visible stack trace.
/// Route a failed JIT dispatch into the thread-local pending-exception
/// slot and return the value the dispatch helper should hand back to its
/// JIT caller: `i64::MIN` (the deopt sentinel) when a pending Java
/// exception was successfully stashed, or `0` if the failure could not be
/// turned into a throwable (legacy silent-drop fallback).
///
/// Returning `i64::MIN` makes the JIT caller's post-invoke exception guard
/// (`emit_post_invoke_exception_check` in `jit/src/x64.rs`) fire and deopt
/// out, so the interpreter routes the real exception through the method's
/// exception table — instead of the JIT running on with a bogus `0` and
/// masking the true failure with a downstream secondary error.
#[must_use]
pub fn handle_jit_dispatch_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    info: &JitInvokeInfo,
) -> i64 {
    use crate::error::{ClassFileError, MethodCallFailed, RuntimeError, VmError};
    // The SITE's resolution failure, whichever door the call took (the
    // by-name tails, the MIC hit/miss tails, the register-overflow bail, a
    // selection failure): see `jit_site_reference_failure`.
    let site_err = jit_site_reference_failure(vm, thread, &err, info);
    let err = match site_err {
        Some(site_err) => site_err,
        None => err,
    };
    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            set_jit_pending_exception(thread, exc);
        }
        // A native callee that returns `Err(RuntimeError::X)` is, by the
        // exception model, asking the VM to throw the Java exception that
        // `X` maps to (e.g. `NoSuchMethodException`, `NullPointerException`,
        // `ClassCastException`). The `From<RuntimeError>` conversion wraps
        // these as `InternalError(VmError::Runtime(..))`, which is *not*
        // an internal VM bug — it is a catchable Java throwable.
        //
        // The interpreter's per-instruction post-processing already does
        // this conversion (`interpreter.rs`: "Convert RuntimeErrors from
        // native methods into catchable Java exceptions"), but the JIT
        // dispatch path previously skipped it and wrapped the runtime
        // error in a *fatal* `java/lang/InternalError`. That turned an
        // ordinary catchable exception into an uncatchable abort — e.g.
        // Netty's `Class.getDeclaredConstructor(...)` probe for the
        // legacy `DirectByteBuffer(long,int)` constructor (absent on
        // JDK 25) threw `NoSuchMethodException`, which Netty catches and
        // falls back from; under the JIT it surfaced as a fatal
        // `InternalError: ... NoSuchMethodException: <init>`.
        //
        // Mirror the interpreter: route `VmError::Runtime` through
        // `throw_runtime_error` so the proper Java exception object is
        // built and caught by the caller's exception table. `NotImplemented`
        // stays a hard error; the stack-overflow shapes take the next arm.
        MethodCallFailed::InternalError(VmError::Runtime(rt_err))
            if !matches!(
                rt_err,
                RuntimeError::NotImplemented { .. } | RuntimeError::StackOverflowError
            ) =>
        {
            match crate::runtime::exceptions::throw_runtime_error(vm, thread, rt_err) {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(thread, exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    // Exception-object construction failed — fall back to
                    // the legacy `InternalError` wrap so the failure is
                    // still visible rather than silently dropped.
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        // Round 12 wave 3 (hfix): a stack overflow detected under this
        // dispatch (the interpreter's `EXEC_DEPTH_CEILING`, a value-stack
        // push) is a catchable `java.lang.StackOverflowError`. The
        // interpreter excludes it from its FIRST conversion only because its
        // catch-all `VmError::Runtime` arm converts it a few lines later; the
        // arm above copied the exclusion without that second arm, so the
        // overflow fell to the catch-all below and surfaced as an
        // `InternalError` ("JIT dispatch into ... failed: runtime error:
        // StackOverflowError") that no `catch (StackOverflowError)` sees
        // (`R12HuntSoeRecover` phase 5).
        MethodCallFailed::InternalError(VmError::Runtime(rt_err))
            if dispatch_error_is_stack_overflow(&rt_err) =>
        {
            match crate::runtime::exceptions::throw_runtime_error(
                vm,
                thread,
                RuntimeError::StackOverflowError,
            ) {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(thread, exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        // `VmError::Linkage` variants are catchable Java errors in the
        // `java.lang.LinkageError` hierarchy (NoClassDefFoundError,
        // NoSuchFieldError, NoSuchMethodError, …). The interpreter
        // already converts them at opcode boundaries via
        // `convert_class_not_found` (`runtime/exceptions.rs`); the JIT
        // dispatch path previously fell through to the catch-all and
        // wrapped them in `java/lang/InternalError`, breaking
        // `catch (NoClassDefFoundError)` / `catch (LinkageError)` in
        // application code — notably JUnit Platform's test-execution
        // error handler in `EngineExecutionOrchestrator.execute`, which
        // expects `NoClassDefFoundError` from a JIT-compiled callee to
        // propagate as-is so the orchestrator can record the test as
        // failed instead of aborting the whole JVM.
        //
        // Mirror the interpreter: build the matching Java throwable so
        // the caller's exception table can find a handler.
        MethodCallFailed::InternalError(VmError::Linkage(linkage_err)) => {
            let converted =
                crate::runtime::exceptions::throw_linkage_error(vm, thread, linkage_err);
            match converted {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(thread, exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        // A class-resolution miss in the resolver surfaces as
        // `VmError::ClassFile(ClassNotFound)`. The interpreter maps this
        // to `NoClassDefFoundError` (see `raise_no_class_def_found`); do
        // the same here so JIT-dispatched callees behave identically.
        MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound {
            ref class_name,
        })) => {
            // `CRATONVM_DBG_LINKAGE_BT=1` -- the third place a
            // `NoClassDefFoundError` reaches Java (see the matching hooks in
            // `runtime::exceptions`). Only the Rust backtrace names the JIT
            // dispatch site that could not resolve the class.
            if false {
                let bt = std::backtrace::Backtrace::force_capture();
                eprintln!("[DBG_LINKAGE_BT] jit NoClassDefFoundError {class_name}\n{bt}");
            }
            // Interpreter round i5 wave 100, lane L2: the interpreter's
            // conversion (`convert_class_not_found_for`), with the SITE's class
            // as the initiating one. A user-loader site's error carries
            // HotSpot's `ClassNotFoundException` cause (`p.Missing`); this arm
            // built a bare `NoClassDefFoundError` with no cause
            // (`L2W100CompiledSiteMissingClass`).
            // A synthetic site (`declaring_class_id == 0`: the thin direct
            // natives, the indy bridge) has no initiating class and keeps the
            // bare error.
            let missing = class_name.clone();
            let converted = (info.declaring_class_id != 0).then(|| {
                crate::runtime::exceptions::convert_class_not_found_for(
                    vm,
                    thread,
                    Some(ClassId::new(info.declaring_class_id)),
                    &missing,
                    MethodCallFailed::InternalError(VmError::ClassFile(
                        ClassFileError::ClassNotFound {
                            class_name: missing.clone(),
                        },
                    )),
                )
            });
            match converted {
                Some(MethodCallFailed::ExceptionThrown(exc)) => {
                    set_jit_pending_exception(thread, exc);
                }
                Some(MethodCallFailed::InternalError(_)) | None => {
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/NoClassDefFoundError",
                        Some(&missing),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        MethodCallFailed::InternalError(vm_err) => {
            // Format a message that points at the failing dispatch site so
            // the user can see WHICH callee blew up. This is the difference
            // between a silent rc=0 and a visible "Exception in thread main"
            // for Spring Boot.
            let msg = format!(
                "JIT dispatch into {}.{}{} failed: {}",
                info.class_name, info.method_name, info.descriptor, vm_err,
            );
            // Try to wrap in a Java `InternalError`; on any allocation /
            // load failure, fall through to the legacy silent drop so we
            // never make things worse than before this fix.
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                "java/lang/InternalError",
                Some(&msg),
            ) {
                set_jit_pending_exception(thread, exc);
            }
        }
    }
    // Return the deopt sentinel iff a pending exception was actually
    // stashed; otherwise `0` (legacy silent-drop — exception construction
    // itself failed, nothing for the caller to route).
    //
    // Read the field through `thread` rather than `jit_pending_exception_is_set()`:
    // that helper resolves the thread from the raw `JIT_THREAD` pointer, and we
    // are holding a live `&mut JvmThread` for the same thread, so going through
    // the pointer here would manufacture an aliasing reference for no reason.
    let pending = thread.jit_pending_exception.is_some();
    // Interpreter round i5 wave 106, lane L2: the re-posted throwable has left
    // `info`'s method; say so instead of leaving it unstamped.
    note_dispatch_error_escape(pending, info.method_name, info.descriptor);
    if pending {
        i64::MIN
    } else {
        0
    }
}

/// The two runtime-error shapes [`handle_jit_dispatch_error`] raises as a
/// `java.lang.StackOverflowError`: the variant itself, and the operand-stack
/// overflow `NotImplemented` both interpreter conversion points normalize to it.
pub fn dispatch_error_is_stack_overflow(err: &crate::error::RuntimeError) -> bool {
    use crate::error::RuntimeError;
    match err {
        RuntimeError::StackOverflowError => true,
        RuntimeError::NotImplemented { feature } => feature == "operand stack overflow",
        _ => false,
    }
}

/// Round 12 wave 4 (exc2): whether the non-dispatch JIT helpers (class-init
/// guards of `new` / `getstatic` / `putstatic`, the monitor helpers) convert a
/// `VmError::Linkage` into its Java throwable through [`stash_helper_failure`].
/// `CRATONVM_JIT_HELPER_FAILURE_CONVERSION=0` restores their pre-wave-4
/// `java/lang/InternalError` wrap.
pub fn helper_failure_conversion_on() -> bool {
    true
}

/// The `MethodCallFailed` -> pending-Java-throwable conversion the JIT helpers
/// outside [`handle_jit_dispatch_error`] share: `ExceptionThrown` as is;
/// `VmError::Runtime` (when `convert_runtime`) through `throw_runtime_error`,
/// the stack-overflow shapes included under `CRATONVM_JIT_DISPATCH_SOE_THROWS`
/// exactly as the dispatch arm does (both interpreter conversion points throw
/// them); `VmError::Linkage` (when `convert_linkage`) through
/// `throw_linkage_error`; anything else, and a conversion that itself failed,
/// as `java/lang/InternalError("{context}: {err}")`.
///
/// JVMS 5.5 step 11: an `Error` raised while initializing a class is rethrown
/// as itself, so a `<clinit>` entered at the interpreter's depth ceiling from
/// compiled code must surface as `StackOverflowError`, not `InternalError`.
///
/// Returns whether a throwable is now pending.
#[cold]
pub fn stash_helper_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    context: &dyn std::fmt::Display,
    convert_runtime: bool,
    convert_linkage: bool,
) -> bool {
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    let err = match err {
        MethodCallFailed::InternalError(VmError::Runtime(rt)) if convert_runtime => {
            if dispatch_error_is_stack_overflow(&rt) {
                {
                    crate::runtime::exceptions::throw_runtime_error(
                        vm,
                        thread,
                        RuntimeError::StackOverflowError,
                    )
                }
            } else if matches!(rt, RuntimeError::NotImplemented { .. }) {
                MethodCallFailed::InternalError(VmError::Runtime(rt))
            } else {
                crate::runtime::exceptions::throw_runtime_error(vm, thread, rt)
            }
        }
        MethodCallFailed::InternalError(VmError::Linkage(le)) if convert_linkage => {
            crate::runtime::exceptions::throw_linkage_error(vm, thread, le)
        }
        other => other,
    };
    match err {
        MethodCallFailed::ExceptionThrown(exc) => set_jit_pending_exception(thread, exc),
        MethodCallFailed::InternalError(vm_err) => {
            let msg = format!("{context}: {vm_err}");
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                "java/lang/InternalError",
                Some(&msg),
            ) {
                set_jit_pending_exception(thread, exc);
            }
        }
    }
    thread.jit_pending_exception.is_some()
}


/// The loader-faithful owner of a compiled `invokestatic` — but ONLY when it is
/// a different class from the one the flat global binary-name map would hand
/// back. `None` means "the by-name answer is already right; take the path this
/// arm has always taken".
///
/// Returning `None` for the agreeing case is not an optimization, it is the
/// safety property: `invoke_or_native` carries a large amount of behaviour that
/// the on-class form does not reproduce (the native-registry override order, the
/// `SyntheticStub`-yields-to-real-bytecode rule, the `DowncallHandle` and
/// signature-polymorphic intercepts). Diverting every static dispatch through a
/// second path to fix the case where they disagree would be trading a rare wrong
/// answer for a common one.
///
/// The gates are ordered cheapest-first, because this sits on every compiled
/// static call:
///
/// 1. no call-site class (`0`) — synthetic call sites naming JDK classes that
///    have exactly one definition, per `JitInvokeInfo::declaring_class_id`;
/// 2. no user-defined loader has ever defined ANY class in this process — one
///    relaxed atomic, and it is the precondition for a name to have two
///    definitions at all;
/// 3. this NAME has one definition — `classify_loaded_name` is O(1) against the
///    definition-count index, and `Unique`/`Absent` both mean the by-name answer
///    cannot be the wrong copy.
///
/// Only past all three does it ask the caller's own loader, via the same
/// `find_class_by_name_for_class` the direct-call path's `callee_compiler`
/// already uses to pick a callee — a pure lookup.
///
/// EXCEPT for a caller a user-defined loader defined. There the question is
/// what THAT loader resolves the name to, which is known only once it has
/// defined or initiated it (`class_resolved_without_loading`); gate 3 is no
/// answer, because a child-first loader that has not been asked yet is about
/// to define a SECOND copy of a name that has one definition today. So such a
/// caller compares the known answer with the by-name one, and when nothing is
/// known yet drives the caller's own loader exactly as the interpreter's
/// `invokestatic` would at the same program point -- `thread` is the calling
/// thread, which is about to run the callee's Java anyway.
///
/// AND EXCEPT, before any of those gates, a hidden caller naming ITSELF
/// ([`jit_hidden_self_owner`]; JIT round 21 wave 14, lane hidden-static): its
/// class-file name has no definition (or an ordinary class's), and the
/// by-name tail would load the class path's copy and call that.
pub fn jit_static_owner_override(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
) -> Option<ClassId> {
    if info.declaring_class_id == 0 {
        return None;
    }
    if let Some(owner) =
        jit_hidden_self_owner(vm, ClassId::new(info.declaring_class_id), info.class_name)
    {
        return Some(owner);
    }
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&vm.loader_identity) {
        return None;
    }
    let caller = ClassId::new(info.declaring_class_id);
    let decided = {
        let cm = vm.classes.class_manager.read();
        if matches!(
            cm.get_loader_id(caller),
            Some(cratonvm_types::ClassLoaderId::UserDefined(_))
        ) {
            // A JDK-global name the caller's loader has not answered yet
            // drives it below, as the interpreter's `invokestatic` does
            // (interpreter round i1 wave 37, lane L2). The `Methodref`'s
            // `CONSTANT_Class` entry's record first: it is what the
            // interpreter's `invokestatic` of the entry answers, where the
            // memo keeps a later drive's answer (round 16, lane classsite,
            // CS2-2; R2 of `r15w5-classsite2-review-findings-FIXED-20261001.md`).
            crate::runtime::interpreter::jit_bridge::jit_recorded_class_entry_in(
                vm,
                &cm,
                caller,
                None,
                info.class_name,
            )
            .or_else(|| {
                crate::runtime::interpreter::class_resolved_without_loading(
                    vm,
                    &cm,
                    caller,
                    info.class_name,
                )
                .and_then(|id| {
                    crate::runtime::interpreter::jit_bridge::loader_faithful_global_name_answer(
                        vm,
                        &cm,
                        caller,
                        info.class_name,
                        id,
                    )
                })
            })
        } else if let Some(owner) = loader_faithful_static_owner(&cm, caller, info.class_name) {
            Some(owner)
        } else if builtin_caller_sole_copy_is_user_defined(vm, &cm, caller, info.class_name) {
            // The only definition is another loader's: drive the caller's own
            // loader below, as the interpreter's `invokestatic` does.
            None
        } else {
            return None;
        }
    };
    let owner = match decided {
        Some(owner) => owner,
        // Through the entry for an isolated loader's caller, so the drive
        // is recorded once for the entry (CS2-2).
        None => jit_bridged_owner_entry(vm, thread, caller, info.class_name)?,
    };
    if vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(info.class_name)
        == Some(owner)
    {
        return None;
    }
    // DIAG (`CRATONVM_DBG=coerce`, the loader-split diagnostic this shares a
    // subject with): a silent fix cannot be told from a fix that never fires.
    // This line is what proves the arm ran on the run that went green.
    if crate::runtime::env_cache::dbg_coerce() {
        let by_name = vm
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(info.class_name);
        eprintln!(
            "[DBG_COERCE] jit invokestatic: {}.{}{} from cid={:?} -- by-name owner {by_name:?} \
is NOT the caller's loader's copy {owner:?}; dispatching on the caller's",
            info.class_name, info.method_name, info.descriptor, caller,
        );
    }
    Some(owner)
}

/// The class a statically bound compiled call site of `caller` means by the
/// constant-pool owner `class_name` when that owner is `caller` ITSELF and
/// `caller` is a hidden class, or `None` (JIT round 21 wave 14, lane
/// hidden-static, `CRATONVM_JIT_HIDDEN_SELF_BY_ID`; page
/// `docs/feature-designs/class-resolution-and-caching.md`).
///
/// A hidden class is stored as `<class-file name>/0x<hex>` and is in no
/// loader's namespace, but its constant pool still names it by the class-file
/// name, so every self-naming `Methodref` reaches `jit_invoke_dispatch` with a
/// `class_name` no name lookup answers. The interpreter's `invokestatic`
/// answers it from the frame's own class (`dispatch_static`'s
/// `self_class_id`, `constants::is_self_class_reference`); the compiled static
/// tail went by name (`invoke_static_or_native_prepared`), whose last resort
/// LOADS the name: the class path's ordinary copy of the bytes, a third class
/// with its own statics, whose method it then ran. Every by-name probe
/// (`get_loaded_class_id`) answered that copy from then on.
///
/// `R21Fz3HiddenAccess1`, arm `HN` (the bytes defined twice, `H` and `HN`):
/// by the count, the payload's own `loop` runs OSR-compiled from its 2000th
/// back edge in the `HN` pass, and each of its remaining 437 `selfCtor()`
/// calls ran the copy's `selfCtor`, whose reflective `newInstance`
/// incremented the COPY's `sCount`: `fact hot HN outer.privStaticField =
/// 1910` against HotSpot's 2347 (worked out from the code, not measured; the
/// page's recipe checks it).
///
/// Cost (review item 6 of `r21-review-w15-native-findings-FIXED-20261020.md`): for
/// a caller that is not hidden -- every site but a hidden class's own -- one
/// load of this VM's lock-free hidden-class bitmap
/// (`ClassRealm::hidden_classes`, set at each hidden define) and no lock. Only
/// a hidden caller (or, once a hidden class lives past the bitmap, an id there) takes the
/// class-manager read to compare its class-file name with `class_name`. No
/// process global is read. The switch is read only for a site that is such a
/// reference.
pub fn jit_hidden_self_owner(
    vm: &SharedVm,
    caller: ClassId,
    class_name: &str,
) -> Option<ClassId> {
    if caller == ClassId::new(0)
        || !crate::runtime::interpreter::constants::hidden_class_may_refer_to_itself(vm, caller)
    {
        return None;
    }
    // A hidden caller's site naming ANOTHER class (a lambda proxy calling its
    // target), ruled out from this VM's lock-free table of recorded
    // class-file name hashes, takes no class-manager read
    // (`CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER`, opt-in; JIT round 21 wave
    // 22, lane hidden-resid; item 6 of `r21-review-w15-native-findings-FIXED-20261020.md`).
    if hidden_self_prefilter_rules_out(&vm.classes.hidden_classes, caller, class_name) {
        return None;
    }
    let is_self = {
        let cm = vm.classes.class_manager.read();
        crate::runtime::interpreter::constants::hidden_self_reference_in(&cm, caller, class_name)
    };
    hidden_self_owner_decision(
        is_self,
        caller,
        crate::runtime::interpreter::constants::jit_hidden_self_by_id_enabled,
    )
}

/// The opt-in lock-free exit of [`jit_hidden_self_owner`]: `true` only when
/// `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER` is on for this VM and its table
/// proves `class_name` is not `caller`'s recorded class-file name
/// (`HiddenClassBits::self_name_rules_out`). The switch is asked first, so
/// `=0` (the default) reads nothing else.
#[inline]
pub fn hidden_self_prefilter_rules_out(
    bits: &cratonvm_classloading::HiddenClassBits,
    caller: ClassId,
    class_name: &str,
) -> bool {
    bits.self_name_prefilter() && bits.self_name_rules_out(caller, class_name)
}

/// The decision of [`jit_hidden_self_owner`] from its facts: the caller's own
/// id for a hidden self-reference while the switch (asked only then) is on.
pub fn hidden_self_owner_decision(
    is_self: bool,
    caller: ClassId,
    enabled: impl FnOnce() -> bool,
) -> Option<ClassId> {
    (is_self && enabled()).then_some(caller)
}

/// `true` when a statically-bound call from `caller` to `class_name` must NOT
/// be served a compiled body found or compiled BY NAME: resolved in
/// `caller`'s own loader, `class_name` is a different class from the one the
/// flat binary-name map answers.
///
/// Also `true` for a hidden caller's reference to ITSELF
/// ([`jit_hidden_self_owner`]; JIT round 21 wave 14, lane hidden-static): the
/// name finds no class, or an ordinary class of the same class file, never
/// the caller. Asked before the loader gates, which say nothing about it.
///
/// Every by-name shortcut -- `try_jit_compile_callee`, and a `JitCache` probe
/// keyed by `get_loaded_class_id(name)` -- hands such a site the OTHER
/// loader's body. MEASURED 2026-09-23 on H2 `TestUpgrade`, which loads H2
/// 1.2.120 through `ClassLoader(null)` beside the current H2: both define
/// `org.h2.compress.CompressLZF`, the old one with INSTANCE `first/next/hash`
/// and the new one with STATIC methods of the same names and descriptors. An
/// OSR compile of the old `compress` baked the new, static `first` in as a
/// direct `invokespecial` target, so every argument arrived one slot over
/// (`this` as the `byte[]`, the array's address as the index): an
/// `ArrayIndexOutOfBoundsException: Index <pointer bits> out of bounds for
/// length 0`, or a SIGSEGV. `--nojit` and `CRATONVM_JIT_OSR=0` were clean.
///
/// Such a site keeps the dispatch helper's interpreter route, which resolves
/// the callee in the caller's loader (`invoke_special_shared_on_class` /
/// [`jit_static_owner_override`]). The gates are the ones that function uses,
/// in the same cheapest-first order, so a process with one definition of every
/// name never gets past the relaxed atomic.
pub fn jit_callee_name_is_another_loaders_copy(
    vm: &SharedVm,
    caller: ClassId,
    class_name: &str,
) -> bool {
    if caller == ClassId::new(0) {
        return false;
    }
    if jit_hidden_self_owner(vm, caller, class_name).is_some() {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] callee-guard caller={} callee={class_name}: hidden \
                 self-reference, no by-name body",
                caller.as_u32()
            );
        }
        return true;
    }
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&vm.loader_identity) {
        return false;
    }
    let cm = vm.classes.class_manager.read();
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] callee-guard caller={} callee={class_name} sole={:?} caller_loader={:?}",
            cm.get_class(caller).map(|c| c.name.to_string()).unwrap_or_default(),
            cm.find_unique_class_by_name(class_name).map(|c| c.as_u32()),
            cratonvm_native_builtins::classloader::defining_loader_for(&vm.loader_identity, caller.as_u32()).is_some(),
        );
    }
    if loader_faithful_static_owner(&cm, caller, class_name).is_some() {
        return true;
    }
    // A class a USER-DEFINED caller's loader has not answered for yet. A name with
    // one definition (the application loader's) is exactly the case
    // `loader_faithful_static_owner` waves through, and for a child-first loader
    // (Spring Boot's `ModifiedClassPathClassLoader`) that has not been asked yet
    // it is about to define a SECOND copy: the by-name callee probe then compiles
    // and binds the application loader's body for it, and the instances that body
    // allocates are not the caller's `TypeDescription`s
    // (`NoSpringWebFilterRegistrationBeanTests`: `TypeDescription$ArrayProjection.of`
    // allocating the application copy under a Mockito mock of
    // `jakarta.servlet.ServletContext`; `ClassCastException` from a JIT-compiled
    // `MethodDescription$AbstractBase.isBridgeCompatible`, `--nojit` clean).
    // `jit_static_owner_override` asks the same question of the same loader at
    // run time and drives it; this keeps the compiled site on that path until the
    // loader's answer is known. Cheap: one class-manager read and, only for a
    // user-defined caller, the recorded-answer probes.
    //
    // The mirror image: a caller of a BUILT-IN loader whose callee name has ONE
    // definition, and that definition is a USER-DEFINED loader's. The built-in
    // loader cannot see it -- it is about to define (or find) its own copy -- but
    // the by-name probe binds the only one there is. A compiled
    // `LoggingSystemFactory.lambda$fromSpringFactories$0` of the application
    // loader bound a `ModifiedClassPathClassLoader` fork's `SpringFactoriesLoader`
    // (the only definition after an earlier fork test), whose
    // `AnnotationAwareOrderComparator` then read none of the application's `@Order`
    // annotations: `LoggingSystem.get` answered `JavaLoggingSystem`
    // (`LoggingSystemTests.logbackIsTheDefaultLoggingSystem`, JIT only).
    if builtin_caller_sole_copy_is_user_defined(vm, &cm, caller, class_name) {
        return true;
    }
    if matches!(
        cm.get_loader_id(caller),
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    ) && !class_name.starts_with('[')
    {
        let known = crate::runtime::interpreter::jit_bridge::jit_recorded_class_entry_in(
            vm, &cm, caller, None, class_name,
        )
        .or_else(|| {
            crate::runtime::interpreter::class_resolved_without_loading(vm, &cm, caller, class_name)
        });
        return known.is_none();
    }
    false
}

/// The decision inside [`jit_static_owner_override`], without the two
/// process-wide pre-gates.
///
/// Split out so it can be tested: the gates it drops are a `0` call-site class
/// and the `any_defining_loader_registered` latch, both of which are pure
/// short-circuits onto the `None` this returns anyway. The latch in particular
/// is a process-global that a unit test would have to arm for the whole test
/// binary, which is a worse trade than testing the decision it guards. That
/// the wrapper still consults them is a source-level fact, and
/// `jit_invokestatic_owner_override_is_gated_before_it_is_consulted` is what
/// holds it.
pub fn loader_faithful_static_owner(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    class_name: &str,
) -> Option<ClassId> {
    // A name with one definition cannot resolve to the wrong copy, and this is
    // O(1) against the definition-count index — so it runs before the two
    // lookups below rather than after them.
    if !matches!(
        cm.classify_loaded_name(class_name),
        cratonvm_classloading::NameResolution::Ambiguous { .. }
    ) {
        return None;
    }
    let owner = cm.find_class_by_name_for_class(class_name, caller)?;
    // The caller's loader agrees with the global map: nothing to correct, and
    // the previous path is the one with all the special cases.
    if cm.get_loaded_class_id(class_name) == Some(owner) {
        return None;
    }
    Some(owner)
}

/// A caller of a BUILT-IN loader (no defining loader) whose callee name has
/// exactly ONE definition, and that definition is a USER-DEFINED loader's: the
/// built-in loader cannot see it and is about to define (or find) its own copy,
/// but every by-name shortcut binds the only one there is.
pub fn builtin_caller_sole_copy_is_user_defined(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    class_name: &str,
) -> bool {
    if class_name.starts_with('[')
        || cratonvm_native_builtins::classloader::defining_loader_for(
            &vm.loader_identity,
            caller.as_u32(),
        )
        .is_some()
    {
        return false;
    }
    cm.find_unique_class_by_name(class_name)
        .is_some_and(|sole| {
            cratonvm_native_builtins::classloader::defining_loader_for(
                &vm.loader_identity,
                sole.as_u32(),
            )
            .is_some()
        })
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo (heap-allocated, outlives this call).
// args_ptr/num_args form a valid i64 slice of JIT-encoded arguments.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// with known signatures matching the compiled method's calling convention.
#[allow(clippy::too_many_arguments)]
/// Consecutive declines after which a compiled call site stops consulting
/// the offload hook.
///
/// The hook is not free: a decline costs ~0.5 us, against ~10 ns for the
/// compiled call it is standing in front of. A site whose arrays are
/// always below `--gpu-min-work` would pay that forever, which would
/// trade the 40x this whole change removes for a new one on the kernel
/// call itself.
///
/// Shares `CRATONVM_GPU_MIN_WORK_GIVEUP` with the interpreter's
/// per-site promotion, because it is the same policy question about the
/// same kind of site. `0` never retires. Unlike the interpreter's
/// version this needs no pc-keying caveat: a `JitInvokeInfo` IS one call
/// site, so retiring one cannot deoptimise its siblings.
#[cfg(feature = "gpu-offload")]
pub fn compiled_offload_giveup_after() -> u32 {
    pub static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(256)
    })
}

/// What one compiled call site is, as far as offload is concerned.
///
/// Keyed by `JitInvokeInfo` address — which is one call site *while that
/// address is live*, so a decision here cannot leak to another call of the
/// same method elsewhere.
///
/// RT-6. That sentence used to end "…which IS the site", full stop, and it is
/// false after retirement. The address is a `Box<JitInvokeInfo>` inside
/// `CompiledMethod::_jit_invoke_infos`; when the artifact retires the box is
/// freed and the allocator is free to hand the same address to an unrelated
/// compile's `JitInvokeInfo`. A stale `Active` would only cost a wasted
/// `try_dispatch` probe, because the target identity is re-derived from the
/// CURRENT `info` further down — but a stale `NotKernel` or `Retired` at a
/// recycled address permanently disables offload for a genuine kernel site,
/// which is the exact failure [`CompiledOffloadSite::Unresolved`] was added
/// to fix, arriving by a different road.
///
/// [`CompiledOffloadSiteEntry`] closes it: every entry carries the identity it
/// was resolved FOR, and a lookup that finds a different identity at the same
/// address treats the entry as absent and resolves afresh.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompiledOffloadSite {
    /// Not a kernel call. The overwhelming majority, and the reason this
    /// is resolved ONCE per site rather than per call.
    NotKernel,
    /// A kernel call that has declined `n` times in a row.
    Active(u32),
    /// Declined past the cap; the hook is never consulted here again.
    Retired,
    /// The target's declaring class was not loaded when this site was first
    /// resolved, so neither the registry nor the gate could answer. `n` is how
    /// many times we have come back; see [`OFFLOAD_SITE_UNRESOLVED_TRIES`].
    ///
    /// AUDIT 2026-09-06. Without this the first execution of a site decided
    /// `NotKernel` FOREVER, using a registry that could not yet know — and the
    /// class that same dispatch was about to resolve was very often the
    /// kernel's own. That is the "forward references" limitation in
    /// `runtime::offload_jit_gate`'s module docs, and it cost the device for
    /// the life of the process on any caller that got hot before its kernel
    /// branch was first taken.
    Unresolved(u8),
}

/// How many times a site whose target class is still unloaded is re-asked
/// before it is written off as [`CompiledOffloadSite::NotKernel`].
///
/// One retry would do in the ordinary case: the dispatch this helper falls
/// through to resolves and initializes the class, so the SECOND execution of
/// the site has a real answer. The cap exists for the case where that dispatch
/// throws — a site whose every call ends in `NoClassDefFoundError` must not
/// re-run the gate query forever.
#[cfg(feature = "gpu-offload")]
pub const OFFLOAD_SITE_UNRESOLVED_TRIES: u8 = 4;

/// Per-site offload state.
///
/// AUDIT 2026-09-04. This started as a name lookup on every compiled
/// dispatch -- `offload_hook::is_kernel(class, method, descriptor)`,
/// which allocates three `Box<str>` to build its key. Measured on
/// `GpuHookOverheadBench`, that put `small_ns_per_call` at **214 ns**
/// against 13.8 for the same loop without `--gpu`: the string work, not
/// the hook, and paid by every compiled static call in the program.
///
/// The name lookup now happens once, the first time a site is seen, and
/// every later call is one `usize` hash. The retirement streak lives in
/// the same entry, so a retired site costs that one lookup and nothing
/// else.
#[cfg(feature = "gpu-offload")]
pub fn compiled_offload_sites(
    // addr-keyed: not a heap address -- `JitInvokeInfo`, identity-checked per read.
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<usize, CompiledOffloadSiteEntry>> {
    pub static T: std::sync::OnceLock<
        // addr-keyed: not a heap address -- `JitInvokeInfo`, identity-checked per read.
        parking_lot::RwLock<rustc_hash::FxHashMap<usize, CompiledOffloadSiteEntry>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Everything about a `JitInvokeInfo` that says WHICH call site it is, taken
/// as plain machine words so it stays comparable after the `info` it came from
/// has been freed.
///
/// RT-6. The three name fields of a `JitInvokeInfo` are `&'static str`s that
/// point into the owning `CompiledMethod`'s storage. This captures their
/// ADDRESS and LENGTH and never their bytes: a stored identity is compared, at
/// most, against a live `info`'s fat-pointer fields, so nothing here ever
/// dereferences a pointer that may have been freed. Comparing the text instead
/// would be a use-after-free on precisely the path this exists to make safe.
///
/// What a pointer-identity comparison can and cannot tell apart:
///
/// * a DIFFERENT site at a recycled address — caught, because two compiles do
///   not produce the same three string addresses by accident. This is the
///   failure the entry-identity check exists for;
/// * the SAME method recompiled, whose `JitInvokeInfo` landed at the same
///   address and reuses the same interned names — not caught, and it does not
///   need to be: the decision (`NotKernel`, or a decline streak) is about that
///   method's target, which has not changed.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledOffloadSiteIdentity {
    pub class_name: (usize, usize),
    pub method_name: (usize, usize),
    pub descriptor: (usize, usize),
    pub invoke_kind: u8,
    pub declaring_class_id: u32,
    /// The VM the decision was reached in (`SharedVm::vm_identity`, never
    /// reissued; 0 from [`Self::of`] alone). [`compiled_offload_sites`] is one
    /// map for the process, and a `JitInvokeInfo` address — and, through the
    /// synthetic direct-dispatch sites, a whole identity — can recur in another
    /// VM, whose registry, gate and class ids answered nothing here. Part of
    /// the identity, so another VM's row is treated as absent, like a recycled
    /// address's (interpreter round i1 wave 38, lane L6: per-VM state).
    pub vm_identity: usize,
}

#[cfg(feature = "gpu-offload")]
impl CompiledOffloadSiteIdentity {
    pub fn of(info: &JitInvokeInfo) -> Self {
        Self {
            class_name: (info.class_name.as_ptr() as usize, info.class_name.len()),
            method_name: (info.method_name.as_ptr() as usize, info.method_name.len()),
            descriptor: (info.descriptor.as_ptr() as usize, info.descriptor.len()),
            invoke_kind: info.invoke_kind,
            declaring_class_id: info.declaring_class_id,
            vm_identity: 0,
        }
    }

    /// [`Self::of`], in the VM `vm_identity`.
    pub fn of_in(vm_identity: usize, info: &JitInvokeInfo) -> Self {
        Self {
            vm_identity,
            ..Self::of(info)
        }
    }
}

/// One row of [`compiled_offload_sites`]: the decision, plus the identity it
/// was reached for.
///
/// The identity is not part of the KEY because the key has to stay one `usize`
/// hash — that is the whole point of the memo, which replaced a three-`Box<str>`
/// name lookup that cost 200 ns on every compiled static call. It is checked on
/// the hit instead: four word comparisons on a path that was already touching
/// this cache line.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompiledOffloadSiteEntry {
    pub identity: CompiledOffloadSiteIdentity,
    pub state: CompiledOffloadSite,
}

/// Write one site's decision, stamped with the identity it was reached for.
///
/// The only writer. Going through it rather than `insert`ing at each of the
/// four update sites is what makes "an entry always carries the identity its
/// state was decided under" a property of the code rather than of four
/// remembered call sites — which is the shape of mistake RT-6 is.
#[cfg(feature = "gpu-offload")]
pub fn record_compiled_offload_site(
    site: usize,
    identity: CompiledOffloadSiteIdentity,
    state: CompiledOffloadSite,
) {
    compiled_offload_sites()
        .write()
        .insert(site, CompiledOffloadSiteEntry { identity, state });
}

/// Decode the JIT argument slots of a STATIC call into `Value`s.
///
/// One slot per declared parameter, including `long`/`double` -- the JIT
/// flattens its outgoing argument array rather than using JVM slot
/// widths, which is the same assumption `forward_jit_reference_args`
/// makes when it walks this descriptor.
///
/// `None` on any descriptor this cannot read exactly. Guessing would
/// hand the marshaller an argument list that does not match the kernel
/// signature, and the failure would be a wrong ANSWER on the device
/// rather than a refusal.
#[cfg(feature = "gpu-offload")]
///
/// # SAFETY
///
/// A non-zero reference slot is treated as a live heap pointer. That is
/// the same contract `forward_jit_reference_args` runs under one line
/// earlier in the caller, and these are the arguments it just forwarded.
pub unsafe fn decode_static_args(descriptor: &str, args: &[i64]) -> Option<Vec<cratonvm_types::Value>> {
    use cratonvm_types::{ObjectRef, Value};
    let bytes = descriptor.as_bytes();
    let mut p = bytes.iter().position(|&b| b == b'(')? + 1;
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0usize;
    while p < bytes.len() && bytes[p] != b')' {
        let raw = *args.get(i)?;
        let v = match bytes[p] {
            b'I' | b'Z' | b'B' | b'S' | b'C' => {
                p += 1;
                Value::Int(raw as i32)
            }
            b'J' => {
                p += 1;
                Value::Long(raw)
            }
            b'F' => {
                p += 1;
                Value::Float(f32::from_bits(raw as u32))
            }
            b'D' => {
                p += 1;
                Value::Double(f64::from_bits(raw as u64))
            }
            b'L' => {
                while p < bytes.len() && bytes[p] != b';' {
                    p += 1;
                }
                p = p.checked_add(1)?;
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as *mut u8)))
                }
            }
            b'[' => {
                while p < bytes.len() && bytes[p] == b'[' {
                    p += 1;
                }
                if p < bytes.len() && bytes[p] == b'L' {
                    while p < bytes.len() && bytes[p] != b';' {
                        p += 1;
                    }
                }
                p = p.checked_add(1)?;
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as *mut u8)))
                }
            }
            _ => return None,        };
        out.push(v);
        i += 1;
    }
    // Every declared parameter consumed exactly one slot, and there were
    // exactly that many. A mismatch means this walk and the JIT disagree
    // about the argument layout, which is not something to offload on.
    if i != args.len() {
        return None;
    }
    Some(out)
}

/// Try to run this compiled `invokestatic` on the GPU.
///
/// `Some(ret)` when the device ran it and `ret` is what the compiled
/// caller should receive; `None` to fall through to the ordinary
/// dispatch below, which is always safe -- `try_dispatch` leaves the
/// operand stack and locals untouched on every refusal and on every
/// failure.
///
/// # SAFETY
///
/// `info` and `args` are the dispatch helper's own, already validated
/// and GC-forwarded. The thread borrow is the standard `jit_thread_mut`
/// scoped one.
#[cfg(feature = "gpu-offload")]
/// What one compiled static call site is, as far as offload is concerned.
///
/// The cheap answer first: `offload_hook`'s registry, populated as a side
/// effect of `offload_jit_gate` scanning CALLERS. A hit is the common path and
/// costs the three `Box<str>` `is_kernel` allocates, once per site.
///
/// AUDIT 2026-09-06: a MISS used to mean `NotKernel`, permanently. That is
/// wrong whenever the registry could not have known — the gate skips any call
/// target whose declaring class is not loaded at the moment the caller is
/// scanned (its "forward references" limitation), and the first execution of a
/// site is very often the thing that loads that very class. A caller that got
/// hot before its kernel branch was first taken therefore lost the device for
/// the rest of the run, silently, with the census printing zeroes.
///
/// So a miss now asks the gate itself:
///
/// * [`descriptor_could_ever_dispatch`] first, off `info.descriptor` alone —
///   no lock, no allocation, no bytecode. `try_dispatch` launches only `)V`, or
///   `)I`/`)J` for a reduction, and above `--gpu-min-work` 0 it needs an array
///   parameter. Nearly every compiled static site in a program fails this and
///   stops here.
/// * [`target_is_dispatchable_kernel`] for the few that pass — the identical
///   question `judge_target` asks during a caller scan. `None` means the class
///   is STILL unloaded, and the site is left [`CompiledOffloadSite::Unresolved`]
///   so the next call re-asks rather than being written off.
/// * A kernel it finds is `note_kernel`ed, which is what repairs the
///   compile-time doors too: `jit/src/lib.rs` and `jit/src/x64/bytecode_walk.rs`
///   read the same registry to decide whether to bind a static call directly or
///   inline it, and both of those are one-way once taken.
/// Does a registry miss get to ask the gate, or is it written off on the spot?
///
/// `CRATONVM_GPU_JIT_GATE_LATE_REGISTER=0` restores the pre-2026-09-06
/// behaviour, in which the FIRST execution of a compiled static call site
/// decided `NotKernel` permanently from a registry that could not yet know
/// about a forward-referenced kernel. Kept as a switch and not just as history:
/// it is the control arm for measuring what the fix is worth, on ONE binary --
/// see `test_classes/gpu/GpuForwardRef.java`, whose two arms differ only in
/// whether the caller gets hot before or after the kernel's class is loaded.
#[cfg(feature = "gpu-offload")]
pub fn offload_site_late_register() -> bool {
    // j27 wave 7 (lane T1IC): read per call (was a process `OnceLock`); a
    // site resolution is cold.
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .ok()
        .as_deref()
        != Some("0")
}

#[cfg(feature = "gpu-offload")]
pub fn resolve_offload_site(vm: &SharedVm, info: &JitInvokeInfo, tries: u8) -> CompiledOffloadSite {
    // This VM's kernels only (interpreter round i1 wave 40, lane L2).
    if cratonvm_jit::offload_hook::is_kernel(
        vm.vm_identity,
        info.class_name,
        info.method_name,
        info.descriptor,
    ) {
        return CompiledOffloadSite::Active(0);
    }
    if !offload_site_late_register() {
        return CompiledOffloadSite::NotKernel;
    }
    if !crate::runtime::offload_jit_gate::descriptor_could_ever_dispatch(
        info.descriptor,
        vm.config.gpu_min_work,
    ) {
        return CompiledOffloadSite::NotKernel;
    }
    match crate::runtime::offload_jit_gate::target_is_dispatchable_kernel(
        vm,
        info.class_name,
        info.method_name,
        info.descriptor,
    ) {
        Some(true) => {
            cratonvm_jit::offload_hook::note_kernel(
                vm.vm_identity,
                info.class_name,
                info.method_name,
                info.descriptor,
            );
            cratonvm_types::gpu_jit_gate_census::note_late_registered(format!(
                "{}.{}{}",
                info.class_name, info.method_name, info.descriptor
            ));
            CompiledOffloadSite::Active(0)
        }
        Some(false) => CompiledOffloadSite::NotKernel,
        None if tries + 1 >= OFFLOAD_SITE_UNRESOLVED_TRIES => CompiledOffloadSite::NotKernel,
        None => CompiledOffloadSite::Unresolved(tries + 1),
    }
}

#[cfg(feature = "gpu-offload")]
pub unsafe fn try_compiled_offload(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    site: usize,
    args: &[i64],
) -> Option<i64> {
    use crate::runtime::offload::DispatchOutcome;
    use cratonvm_types::Value;

    let cap = compiled_offload_giveup_after();

    // One `usize` hash on the hot path. Everything below the hit runs once per
    // SITE, not once per call.
    //
    // RT-6: a hit is believed only when the entry's recorded identity still
    // matches this `info`. `site` is a `Box<JitInvokeInfo>` address inside a
    // retired artifact's `_jit_invoke_infos`, so the allocator can hand it to
    // an unrelated compile; without this check an inherited `NotKernel` or
    // `Retired` would permanently disable offload for a genuine kernel at the
    // new site. A mismatch is treated exactly like a miss — resolve afresh and
    // overwrite, which also keeps the map's size tied to live addresses rather
    // than to every address the process ever used.
    let identity = CompiledOffloadSiteIdentity::of_in(vm.vm_identity, info);
    let state = {
        let known = compiled_offload_sites()
            .read()
            .get(&site)
            .copied()
            .filter(|e| e.identity == identity)
            .map(|e| e.state);
        match known {
            Some(CompiledOffloadSite::Unresolved(tries)) => {
                // Come back: the dispatch that followed our last visit
                // resolved the class, so the gate can answer now.
                let st = resolve_offload_site(vm, info, tries);
                record_compiled_offload_site(site, identity, st);
                st
            }
            Some(st) => st,
            None => {
                let st = resolve_offload_site(vm, info, 0);
                record_compiled_offload_site(site, identity, st);
                st
            }
        }
    };
    match state {
        CompiledOffloadSite::NotKernel
        | CompiledOffloadSite::Retired
        | CompiledOffloadSite::Unresolved(_) => return None,
        CompiledOffloadSite::Active(_) => {}
    }

    cratonvm_types::gpu_compiled_offload_census::note_considered();

    let Some(values) = (unsafe { decode_static_args(info.descriptor, args) }) else {
        cratonvm_types::gpu_compiled_offload_census::note_undecodable();
        return None;
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        cratonvm_types::gpu_compiled_offload_census::note_undecodable();
        return None;
    };

    // `frame_idx` reaches only a `tracing::debug!` inside `try_dispatch`,
    // and `tracing` is built with `max_level_info`, so it is compiled out
    // of every release binary. There is no compiled frame index to give.
    let outcome = crate::runtime::offload::try_dispatch(
        vm,
        thread,
        0,
        info.class_name,
        info.method_name,
        info.descriptor,
        &values,
    );
    let Ok(outcome) = outcome else {
        cratonvm_types::gpu_compiled_offload_census::note_undecodable();
        return None;
    };

    match outcome {
        DispatchOutcome::Handled => {
            cratonvm_types::gpu_compiled_offload_census::note_offloaded();
            record_compiled_offload_site(site, identity, CompiledOffloadSite::Active(0));
            // A `)V` kernel: the results reached Java through the D2H
            // writeback into the output array, and the caller expects
            // nothing on its operand stack.
            Some(0)
        }
        DispatchOutcome::HandledWithValue(v) => {
            cratonvm_types::gpu_compiled_offload_census::note_offloaded();
            record_compiled_offload_site(site, identity, CompiledOffloadSite::Active(0));
            match v {
                Value::Int(i) => Some(i as i64),
                Value::Long(l) => Some(l),
                // Only `)I`/`)J` reductions are transparently dispatched,
                // so anything else here means the dispatcher and this
                // decode disagree. Fall through rather than invent a
                // return value.
                _ => None,
            }
        }
        _ => {
            cratonvm_types::gpu_compiled_offload_census::note_declined();
            if cap > 0 {
                let CompiledOffloadSite::Active(n) = state else {
                    return None;
                };
                let n = n.saturating_add(1);
                let next = if n > cap {
                    cratonvm_types::gpu_compiled_offload_census::note_site_retired();
                    CompiledOffloadSite::Retired
                } else {
                    CompiledOffloadSite::Active(n)
                };
                record_compiled_offload_site(site, identity, next);
            }
            None
        }
    }
}


/// The class a statically bound (`invoke_kind == 1`) site's `class_name`
/// names when the compile SUBSTITUTED that name for the constant-pool class
/// (`JitInvokeInfo::owner_class_id`: the declaring class of a private or
/// `final` target, a super call's selection start): the recorded id, while it
/// still names a loaded class called `class_name`. `None` for every other
/// site, and for an id the store no longer holds under that name; such a site
/// resolves `class_name` in the caller's loader as before.
///
/// Why the id and not the name: the substituted name is not one the caller's
/// constant pool references, so resolving it through the caller's loader asks
/// that loader for a class its bytecode never names
/// (`java/lang/AbstractStringBuilder` for `StringBuilder.length()`), and can
/// answer another loader's copy of the ancestor
/// (`interpreter-L2-substituted-owner-dispatch-resolves-the-declaring-class-by-name`).
///
/// JIT round 21 lane reflect4 (REFLECT3-21-2): also an `invoke_kind == 3`
/// site with an `owner_class_id`, which only the hidden-owner leaf of a
/// compiled `invokeBasic` / `linkToStatic` link carries
/// ([`mh_bind_hidden_leaf_info`]; no compile mints a static site with one): a
/// hidden class (a spun `LambdaForm$MH`) is never found by name, so every
/// service of the compiled call that asks this first resolves it by id.
pub fn jit_substituted_owner(vm: &SharedVm, info: &JitInvokeInfo) -> Option<ClassId> {
    if !matches!(info.invoke_kind, 1 | 3) || info.owner_class_id == 0 {
        return None;
    }
    let owner = ClassId::new(info.owner_class_id);
    let cm = vm.classes.class_manager.read();
    cm.get_class(owner)
        .is_some_and(|class| &*class.name == info.class_name)
        .then_some(owner)
}

/// [`jit_substituted_owner`] for the hidden-owner static leaf only
/// (`invoke_kind == 3` with an `owner_class_id`; JIT round 21 lane reflect4).
/// Every ordinary static site answers `None` at the first compare.
pub fn jit_hidden_static_owner(vm: &SharedVm, info: &JitInvokeInfo) -> Option<ClassId> {
    if info.invoke_kind != 3 || info.owner_class_id == 0 {
        return None;
    }
    jit_substituted_owner(vm, info)
}

/// [`jit_callee_name_is_another_loaders_copy`] for a site with a substituted
/// owner: `true` when the flat binary-name map answers a different class for
/// `class_name` than the one the substitution found, so a compiled body found
/// or compiled BY NAME would be the other copy's. Same cheapest-first gates.
pub fn jit_substituted_owner_is_another_copy(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    if info.invoke_kind != 1
        || info.owner_class_id == 0
        || !cratonvm_native_builtins::classloader::any_defining_loader_registered(
            &vm.loader_identity,
        )
    {
        return false;
    }
    substituted_owner_differs_from_by_name(&vm.classes.class_manager.read(), info)
}

/// The decision inside [`jit_substituted_owner_is_another_copy`], without the
/// process-wide latch (split out for the same reason as
/// [`loader_faithful_static_owner`]).
pub fn substituted_owner_differs_from_by_name(
    cm: &crate::classloading::ClassManager,
    info: &JitInvokeInfo,
) -> bool {
    if info.invoke_kind != 1 || info.owner_class_id == 0 {
        return false;
    }
    if !matches!(
        cm.classify_loaded_name(info.class_name),
        cratonvm_classloading::NameResolution::Ambiguous { .. }
    ) {
        return false;
    }
    cm.get_loaded_class_id(info.class_name) != Some(ClassId::new(info.owner_class_id))
}

/// nio21 wave 20 L1: the dispatch helper's door to
/// [`crate::runtime::exceptions::jit_new_throwable_direct`] for an
/// `invokespecial <init>` site with `args` (receiver first, forwarded). `true`
/// when the constructor's effect is complete; `false` (nothing changed) sends
/// the call down the ordinary route. A cheap textual screen first: only the
/// `(String)V` and `()V` constructors are candidates.
pub unsafe fn try_throwable_init_direct_at_dispatch(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> bool {
    let expected_args = match info.descriptor {
        "(Ljava/lang/String;)V" => 2,
        "()V" => 1,
        _ => return false,
    };
    if args.len() != expected_args || args[0] == 0 {
        return false;
    }
    // Cast: a JIT argument word holding an object reference is its address.
    let receiver = ObjectRef::from_raw(args[0] as usize as *mut u8);
    let message = args
        .get(1)
        .copied()
        .filter(|&m| m != 0)
        // Cast: as above.
        .map(|m| ObjectRef::from_raw(m as usize as *mut u8));
    let Some((thread, _guard)) = jit_thread_mut() else {
        return false;
    };
    crate::runtime::exceptions::jit_new_throwable_direct(
        vm,
        thread,
        info.class_name,
        info.descriptor,
        receiver,
        message,
    )
}

/// gci i12/mf (`gci-i8dm-jni-native-call-sites-re-resolve-by-name-every-call-FIXED-20261017`,
/// the JIT's virtual arms): the hit half of the compiled virtual / interface
/// JNI call-site rows (`vm_exec`'s "gci i12/mf" block), asked by the three
/// by-name tails a compiled virtual call of a table-bound JNI native reaches:
/// `jit_invoke_dispatch_body`'s `0 | 2` arm and `jit_invoke_virtual_mic_body`'s
/// entryless-hit and miss arms. `None` for an array or class-0 receiver (no
/// row holds one), with the memo off (one byte compare), and for a receiver
/// class other than the row's.
#[inline]
pub(crate) fn jit_jni_vsite_lookup(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_ptr: usize,
    receiver: Option<ClassId>,
) -> Option<crate::vm::vm_exec::JniJitVSiteHit> {
    let receiver = receiver.filter(|cid| cid.as_u32() != 0)?;
    crate::vm::vm_exec::jni_jit_vsite_memo_lookup(
        vm,
        thread,
        ClassId::new(info.declaring_class_id),
        crate::vm::vm_exec::jni_jit_vsite(info_ptr),
        (info.class_name, info.method_name, info.descriptor),
        receiver,
    )
}

/// gci i12/mf: a compiled virtual call-site hit, as the tail it stands in for
/// ends: the door on the receiver's class, then the tail's S111r12 rescue on a
/// `NoSuchMethodError` miss named for `rescue_class` (the blind arm asks the
/// receiver's class name, the MIC arms the dispatch target's) when that is not
/// the site's class, with the arguments re-read from pins as the tail keeps them;
/// every other error goes to `handle_jit_dispatch_error`.
pub(crate) fn jit_jni_vsite_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    hit: &crate::vm::vm_exec::JniJitVSiteHit,
    values: &mut [Value],
    rescue_class: &str,
) -> i64 {
    let rescue_differs = !info.class_name.is_empty() && rescue_class != info.class_name;
    let result = match crate::vm::vm_exec::jni_jit_vsite_memo_call(
        vm,
        thread,
        hit,
        values,
        rescue_differs,
    ) {
            Ok(v) => v,
            Err(e)
                if rescue_differs
                    && is_dispatch_no_such_method_miss(
                        &e,
                        rescue_class,
                        info.method_name,
                        info.descriptor,
                    ) =>
            {
                match crate::vm::invoke_or_native(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    values,
                ) {
                    Ok(v) => v,
                    Err(e2) => return handle_jit_dispatch_error(vm, thread, e2, info),
                }
            }
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        };
    match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    }
}

/// JIT round 20 (lane stash;
/// `r19-lean-native-door-runs-outside-the-helper-panic-scope-FIXED-20261012.md`): a
/// door an `extern "C"` dispatch helper asks BEFORE its `contain`ed body (the
/// SBR5 lean native door, the FFM direct door) runs under the same panic scope.
/// A panic in a door (a header guard, the return coercion, building the
/// throwable in `handle_jit_dispatch_error`) used to unwind out of the
/// `extern "C"` shell and abort the process; contained, it becomes the
/// helper's pending `InternalError` and the `i64::MIN` sentinel, as a panic in
/// the body does. The non-panicking path costs the scope counter's two
/// thread-local updates (`contain_compile_panic`); `catch_unwind` itself is
/// free on x64. Correctness, unconditional.
#[inline(always)]
pub fn contain_door(
    name: &'static str,
    vm_ptr: i64,
    door: impl FnOnce() -> Option<i64>,
) -> Option<i64> {
    contain(name, OnPanic::Throw { vm_ptr }, Some(i64::MIN), door)
}

thread_local! {
    /// `(vm_identity, sender, receiver class)` of the last pair
    /// [`iface_special_receiver_guard`] checked and passed on this thread. A
    /// class's supertypes never change, so a pass is permanent for the pair.
    pub static IFACE_SPECIAL_GUARD_PASSED: Cell<(usize, u32, u32)> = const { Cell::new((0, 0, 0)) };
    /// `(vm_identity, sender)` of the last statically bound site's holder the
    /// guard found to be a CLASS (never checked; the verifier proves its
    /// receivers).
    pub static IFACE_SPECIAL_GUARD_CLASS_SENDER: Cell<(usize, u32)> = const { Cell::new((0, 0)) };
}

/// Interpreter round i7 wave 136, lane L2
/// (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, performance, default OFF;
/// page `i135-L6-trait-static-accessors-run-uncached-and-uncompiled`): the
/// compiled twin of the interpreter's receiver check for an `invokespecial`
/// whose sender is an interface (`invoke::interface_special_receiver_refusal`,
/// HotSpot's `LinkResolver::runtime_resolve_special_method`). With the switch
/// on, a method-entry door compiles such a site whose receiver is not proven
/// to be `this` (Scala's `static T.m$(T)`, `aload_0; invokespecial T.m`) as a
/// call through `jit_invoke_dispatch` (`invoke_kind == 1`, no direct bind, no
/// splice: `jit_bridge::iface_special_guarded_target`); this is that call's
/// check. `Some(i64::MIN)` with the `IllegalAccessError` pending when the
/// receiver's class is neither the sender nor a subtype of it; `None` to go
/// on dispatching.
///
/// The sender is `info.declaring_class_id` (the class whose method holds the
/// site). Asked only for a site that names a member some unproven
/// `invokespecial` of the sender names
/// (`jit_bridge::interface_special_receivers_proven`), so a devirtualized
/// kind-1 `invokevirtual` (`Object.getClass()` from a default method) is never
/// checked. A class sender is remembered per thread and costs one compare; a
/// passing `(sender, receiver class)` pair likewise.
///
/// # Safety
/// Called from [`jit_invoke_dispatch_body`] with its forwarded argument words;
/// `jit_thread_mut` is that helper's own thread contract.
pub unsafe fn iface_special_receiver_guard(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    let message = iface_special_receiver_refusal_message(vm, info, args)?;
    let (thread, _guard) = jit_thread_mut()?;
    // Interpreter round i7 wave 137, lane L2: built as the other compiled
    // throws are, with the compiled frames snapshotted HERE (the sending
    // `T.m$` is the innermost one, at its call's published safepoint), LIVE:
    // every frame the snapshot names is still on the machine stack and nothing
    // ran since. The capture then takes it in place of its own compiled walk,
    // so the trace's top frame is the sender's, as HotSpot's (the IAE is
    // raised AT the `invokespecial`, `LinkResolver::runtime_resolve_special_method`).
    note_trap_snapshot_bound(vm);
    let snapshot = if trap_frame_snapshot_enabled() {
        let frames = trap_snapshot_walk();
        (!frames.is_empty()).then_some(frames)
    } else {
        None
    };
    match crate::runtime::exceptions::create_exception_object_with_trap_frames_live(
        vm,
        thread,
        "java/lang/IllegalAccessError",
        Some(&message),
        snapshot,
        true,
    ) {
        Ok(exc) => set_jit_pending_exception(thread, exc),
        // The check FAILED: never fall through to the arms that call the
        // callee (the w136 form's `.ok()?` did, on an allocation failure).
        // The construction's own error (an `OutOfMemoryError`, a linkage
        // error) is what the call raises instead.
        Err((err, _)) => return Some(handle_jit_dispatch_error(vm, thread, err, info)),
    }
    // The call failed before the callee was entered, as a linkage error the
    // helper raises does: its handlers do not apply
    // ([`note_dispatch_error_escape`], which `handle_jit_dispatch_error`
    // writes for its own re-posts).
    note_dispatch_error_escape(true, info.method_name, info.descriptor);
    Some(i64::MIN)
}

/// [`iface_special_receiver_guard`]'s question without the raise: the
/// `IllegalAccessError` message when the receiver word `args[0]` fails the
/// interface sender's receiver check, `None` when it passes or is not checked.
/// Interpreter round i7 wave 137, lane L6 (review of wave 136 L2): also asked
/// by the bound-site inline cache's publication
/// ([`publish_bound_site_entry`]), which publishes BEFORE its dispatch call
/// runs the guard, keyed by the receiver's class; a refused receiver class
/// published there would hit the cache on its next call and skip the guard.
///
/// # Safety
/// `args` holds the call's forwarded argument words, receiver first.
pub unsafe fn iface_special_receiver_refusal_message(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<String> {
    let sender = info.declaring_class_id;
    if sender == 0
        || info.method_name == "<init>"
        || !crate::runtime::interpreter::invoke::interface_special_receiver_check_enabled(vm)
    {
        return None;
    }
    let vm_id = vm.vm_identity;
    if IFACE_SPECIAL_GUARD_CLASS_SENDER.with(|c| c.get()) == (vm_id, sender) {
        return None;
    }
    let raw = *args.first()? as u64;
    // The null receiver was answered (NPE) before this guard; anything that
    // is not an object address is left to the arms below, as the object
    // native cache leaves it.
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let receiver = vm.mem.heap.is_object_address(raw as usize)?;
    let receiver_class = vm.mem.heap.class_id_of(receiver);
    // Interpreter round i7 wave 136, lane L6 (cross-lane): an ARRAY receiver's
    // header holds its COMPONENT's class id, so it never reads or fills the
    // passed memo (a `C[]` would hit the entry `C` filled), and it is checked
    // through the object form below, which asks the array class itself.
    let is_array = vm.mem.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array;
    if !is_array
        && IFACE_SPECIAL_GUARD_PASSED.with(|c| c.get()) == (vm_id, sender, receiver_class.as_u32())
    {
        return None;
    }
    let sender_id = ClassId::new(sender);
    let checked = {
        let cm = vm.classes.class_manager.read();
        match cm.get_class(sender_id) {
            None => false,
            Some(class) if !class.is_interface() => {
                IFACE_SPECIAL_GUARD_CLASS_SENDER.with(|c| c.set((vm_id, sender)));
                return None;
            }
            Some(_) => !crate::runtime::interpreter::jit_bridge::interface_special_receivers_proven(
                &cm,
                sender_id,
                info.class_name,
                info.method_name,
                info.descriptor,
            ),
        }
    };
    if !checked {
        return None;
    }
    let Some(message) =
        crate::runtime::interpreter::invoke::interface_special_receiver_refusal(
            vm, sender_id, receiver,
        )
    else {
        if !is_array {
            IFACE_SPECIAL_GUARD_PASSED.with(|c| c.set((vm_id, sender, receiver_class.as_u32())));        }
        return None;
    };
    Some(message)}

/// Does [`iface_special_receiver_guard`] check the calls of site `info` (an
/// `invokespecial` whose sender is an interface and whose receiver is not
/// proven to be `this`), with the receiver left out? Asked by the caches that
/// answer a kind-1 site without a receiver-class guard and AHEAD of the
/// guard (the lean native door, [`lean_native_site_fill`]), which must not
/// file such a site. Undecidable (the class manager busy) answers `true`, so
/// the site stays off the cache. Interpreter round i7 wave 138, lane L2. Cold.
pub fn iface_special_site_is_guarded(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    if info.invoke_kind != 1 {
        return false;
    }
    let sender = info.declaring_class_id;
    if sender == 0
        || info.method_name == "<init>"
        || !crate::runtime::interpreter::invoke::interface_special_receiver_check_enabled(vm)
    {
        return false;
    }
    let Some(cm) = vm.classes.class_manager.try_read() else {
        return true;
    };
    let sender_id = ClassId::new(sender);
    match cm.get_class(sender_id) {
        Some(class) if class.is_interface() => {
            !crate::runtime::interpreter::jit_bridge::interface_special_receivers_proven(
                &cm,
                sender_id,
                info.class_name,
                info.method_name,
                info.descriptor,
            )
        }
        _ => false,
    }
}

/// `(vm_identity, info address)` of a compiled call site
/// [`jit_site_access_guard`] has judged. An address alone does not name a
/// site: a retired body's `JitInvokeInfo` and name strings go back to the
/// allocator and a recompiled (or redefined) caller can get the same
/// addresses back for ANOTHER reference
/// (`docs/feature-designs/jit-ir-optimizations.md`), so every
/// memo hit is confirmed against the reference's TEXT ([`JitSiteAccessEntry`]).
pub type JitSiteAccessKey = (usize, usize);

/// What an admitted site named: the caller class id, the invoke kind and the
/// reference's text. Equal entries name the same symbolic reference of the
/// same class (a redefinition keeps the class id and its loader, and one
/// loader answers one name with one class, JVMS 5.3.4), so they share one
/// verdict whatever body or class version carries the site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitSiteAccessEntry {
    pub caller: u32,
    pub kind: u8,
    pub class_name: Box<str>,
    pub method_name: Box<str>,
    pub descriptor: Box<str>,
}

impl JitSiteAccessEntry {
    pub fn of(info: &JitInvokeInfo) -> Self {
        Self {
            caller: info.declaring_class_id,
            kind: info.invoke_kind,
            class_name: info.class_name.into(),
            method_name: info.method_name.into(),
            descriptor: info.descriptor.into(),
        }
    }

    pub fn names(&self, info: &JitInvokeInfo) -> bool {
        self.caller == info.declaring_class_id
            && self.kind == info.invoke_kind
            && *self.class_name == *info.class_name
            && *self.method_name == *info.method_name
            && *self.descriptor == *info.descriptor
    }
}

thread_local! {
    /// The sites of this thread [`jit_site_access_guard`] admitted.
    pub static JIT_SITE_ACCESS_PASSED: std::cell::RefCell<
        std::collections::HashMap<JitSiteAccessKey, JitSiteAccessEntry>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    /// `(code-cache generation, key)` of the last admitted (or confirmed)
    /// site: the common repeat costs one atomic load and one compare. Valid
    /// only within the generation it was set in: freeing a `JitInvokeInfo`
    /// takes an invalidation, which advances the generation
    /// (`cratonvm_jit::jit_cache_generation`), so a recycled address cannot
    /// match it.
    pub static JIT_SITE_ACCESS_LAST: Cell<(u64, JitSiteAccessKey)> = const { Cell::new((0, (0, 0))) };
    static JIT_SITE_ACCESS_RING: [Cell<(u64, JitSiteAccessKey)>; 32] =
        [const { Cell::new((0, (0, 0))) }; 32];
}

#[inline(always)]
fn jit_site_access_ring_slot(key: JitSiteAccessKey) -> usize {
    ((key.1 >> 4) ^ key.0) & 31
}

/// t1 wave 2, lane P (`CRATONVM_JIT_SITE_ACCESS_CHECK`, correctness, default
/// ON, `=0` kill switch): JVMS §5.4.3 / §5.4.4 for a compiled call site whose
/// method reference the compile door could not judge.
///
/// The door peeks at every invoke site (`jit_bridge::CpResolvers::invoke`)
/// and leaves an `IllegalAccessError` site to the interpreter, but a site
/// whose OWNER no loader had loaded when the method was compiled resolves
/// "uncached, unchecked" (`invoke::resolve_method_metadata_noting`: the check
/// waits for the resolution after the owner's load), and the compiled code
/// then reaches the callee by NAME through this helper or the inline-cache
/// miss, which load the owner and call it without that second resolution.
/// So a class of a child loader calling a package-private class (or a
/// package-private / protected member) of its parent's same-named package ran
/// the call where HotSpot throws `IllegalAccessError: failed to access class
/// T1JiteaRows$Late4 from class T1JiteaRows$Holder (...)`
/// (`T1JiteaRows.java` row `typecheck-loaders`; the focused
/// probe `T1PRuntimePackageAccess.java`, `AccJit.fresh*`).
/// `--nojit` was right: `execute_invokestatic` resolves again after loading.
///
/// Here, at a site's first compiled execution on this thread, the site is
/// resolved as the interpreter resolves it: the caller's constant-pool entry
/// for the site's `(class, name, descriptor)` is found
/// ([`jit_site_cp_method_ref`]), the owner is resolved through the caller's
/// loader for every invoke kind (`resolve_class_entry_loader_aware`, the
/// `CONSTANT_Class` door; `invokestatic` only before wave 3), and the reference is resolved (`resolve_method_ref_settling`, which
/// runs the class and member checks against the owner in the caller's
/// namespace). An `IllegalAccessError` is settled against the entry's record
/// (`settle_method_resolution_failure`: later executions rethrow it, JVMS
/// §5.4.3) and raised at the call; anything else is left to the dispatch as
/// before. A site with no such entry (a synthetic or substituted site) is not
/// judged. The arguments are pinned across the loads and written back.
///
/// `Some(sentinel)` when the call must not run (its exception pending);
/// `None` to dispatch. Admitted sites are remembered per thread, so the steady
/// state is one thread-local compare.
///
/// # Safety
/// The dispatch helpers' contract: `vm_ptr` / `info_ptr` are the live VM and
/// site, `args_ptr` / `num_args` the compiled caller's argument words; no
/// `jit_thread_mut` borrow may be live.
#[inline]
pub unsafe fn jit_site_access_guard(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    if vm_ptr == 0 || info_ptr == 0 {
        return None;
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    if info.declaring_class_id == 0 || info.owner_class_id != 0 || info.invoke_kind > 3 {
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let key: JitSiteAccessKey = (vm.vm_identity, info_ptr as usize);
    let generation = cratonvm_jit::jit_cache_generation();
    let slot = jit_site_access_ring_slot(key);
    if JIT_SITE_ACCESS_RING.with(|ring| ring[slot].get() == (generation, key)) {
        return None;
    }
    if jit_site_access_admitted(key, info) {
        JIT_SITE_ACCESS_RING.with(|ring| ring[slot].set((generation, key)));
        return None;
    }
    jit_site_access_guard_slow(vm, info, key, args_ptr, num_args)
}

/// [`jit_site_access_guard`]'s repeat test, outside the helpers' panic scope:
/// a site in this thread's 32-entry ring, in this code-cache generation (one
/// atomic load and one thread-local compare; it cannot panic). Measured in
/// wave 3 (`T1PDispatchBench`, doubling the guard): the guard behind the
/// panic scope cost ~2 ns per `jit_invoke_dispatch` and ~4 ns per inline-cache
/// miss.
#[inline(always)]
pub unsafe fn jit_site_access_repeat(vm_ptr: i64, info_ptr: i64) -> bool {
    if vm_ptr == 0 {
        return false;
    }
    let key: JitSiteAccessKey = (
        (*(vm_ptr as *const SharedVm)).vm_identity,
        info_ptr as usize,
    );
    let slot = jit_site_access_ring_slot(key);
    JIT_SITE_ACCESS_RING.with(|ring| ring[slot].get() == (cratonvm_jit::jit_cache_generation(), key))
}

/// Does this thread's memo admit `info` at `key`? A hit whose text names
/// another reference (a recycled address) is not an admission.
pub fn jit_site_access_admitted(key: JitSiteAccessKey, info: &JitInvokeInfo) -> bool {
    JIT_SITE_ACCESS_PASSED.with(|passed| {
        passed
            .borrow()
            .get(&key)
            .is_some_and(|entry| entry.names(info))
    })
}

/// Remember an admitted site ([`jit_site_access_guard`]). Bounded: a thread
/// that has judged a very large number of sites starts over.
pub fn jit_site_access_admit(key: JitSiteAccessKey, info: &JitInvokeInfo) {
    JIT_SITE_ACCESS_PASSED.with(|passed| {
        let mut passed = passed.borrow_mut();
        if passed.len() >= 1 << 16 {
            passed.clear();
        }
        passed.insert(key, JitSiteAccessEntry::of(info));
    });
    let generation = cratonvm_jit::jit_cache_generation();
    let slot = jit_site_access_ring_slot(key);
    JIT_SITE_ACCESS_RING.with(|ring| ring[slot].set((generation, key)));
    JIT_SITE_ACCESS_LAST.with(|last| last.set((generation, key)));
}

/// The `(Methodref index, its CONSTANT_Class index)` of `caller`'s pool that
/// names `info`'s `(class, name, descriptor)`, or `None` (no such entry: the
/// site's names are not the pool's). Cold: once per site and thread.
pub fn jit_site_cp_method_ref(
    vm: &SharedVm,
    caller: ClassId,
    info: &JitInvokeInfo,
) -> Option<(u16, u16)> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let cm = vm.classes.class_manager.read();
    let pool = &cm.get_class(caller)?.constant_pool;
    (1..u16::try_from(pool.len()).unwrap_or(u16::MAX)).find_map(|index| {
        let (class_index, nat_index) = match pool.get(index)? {
            ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }
            | ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            } => (*class_index, *name_and_type_index),
            _ => return None,
        };
        (pool.get_class_name(class_index) == Some(info.class_name)
            && pool.get_name_and_type(nat_index) == Some((info.method_name, info.descriptor)))
        .then_some((index, class_index))
    })
}

/// [`jit_site_access_guard`]'s first look at a site on this thread.
#[cold]
pub unsafe fn jit_site_access_guard_slow(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    key: JitSiteAccessKey,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    use crate::error::{LinkageError, MethodCallFailed, VmError};
    let caller = ClassId::new(info.declaring_class_id);
    let Some((cp_idx, class_idx)) = jit_site_cp_method_ref(vm, caller, info) else {
        jit_site_access_admit(key, info);
        return None;
    };
    // A cached resolution passed the checks when it was filled.
    let resolver = crate::runtime::resolve::MemberResolver::new(vm);
    if matches!(
        resolver.probe_method_ref(resolver.scope(caller), cp_idx),
        crate::runtime::resolve::CacheProbe::Hit(_)
    ) {
        jit_site_access_admit(key, info);
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    let args: &[i64] = if args_ptr == 0 || num_args <= 0 {
        &[]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };
    let pins = CompileArgPins::pin_raw(vm, thread, info, args);
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let refusal: Option<MethodCallFailed> = 'judge: {
        if let Some(recorded) =
            crate::runtime::interpreter::constants::recorded_member_owner_failure(
                vm, thread, caller, cp_idx,
            )
        {
            break 'judge Some(recorded);
        }
        // The reference's `CONSTANT_Class` entry is resolved through the
        // caller's loader first, as HotSpot's `LinkResolver::resolve_klass`
        // does for every invoke kind (`execute_invokestatic` loads its owner;
        // the receiver doors drive the caller's loader for an owner it has not
        // answered). Wave 3 (residual 1 of the t1-P page): for an
        // `invokevirtual` / `invokeinterface` / `invokespecial` whose owner the
        // caller's USER loader had not answered yet, the resolution below was
        // "unverified" (never enforced) and the site was admitted for good
        // (`T1PRuntimePackageAccess` `AccRecv.*Call`).
        //
        // A failed load (`t1-V1` page, secondary): under `--real-jdk` it is
        // the resolution's error, recorded against the class entry and raised
        // here, as `execute_invokestatic` records an owner failure; the
        // dispatch would otherwise ask the loader a second time. Under
        // `--compatible` the interpreter falls to the flat route after a
        // class-entry refusal, so the dispatch keeps that route.
        if !info.class_name.starts_with('[') {
            if let Err(failure) =
                crate::runtime::interpreter::constants::resolve_class_entry_loader_aware(
                    vm,
                    thread,
                    (caller, class_idx),
                    info.class_name,
                    fill_as_of,
                )
            {
                if !vm.config.is_jdk_only() {
                    break 'judge None;
                }
                let error = crate::runtime::exceptions::convert_class_not_found(
                    vm,
                    thread,
                    info.class_name,
                    failure,
                );
                break 'judge Some(
                    crate::runtime::interpreter::constants::record_member_owner_failure_as_of(
                        vm, caller, cp_idx, error, fill_as_of,
                    ),
                );
            }
        }
        match crate::runtime::interpreter::invoke::resolve_method_ref_settling(
            vm, thread, caller, cp_idx,
        ) {
            Err(
                e @ MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::IllegalAccessError { .. },
                )),
            ) => Some(
                crate::runtime::interpreter::invoke::settle_method_resolution_failure(
                    vm, thread, caller, cp_idx, e, fill_as_of,
                ),
            ),
            _ => None,
        }
    };
    if let Some(error) = refusal {
        pins.release(thread);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] site access refused: class {} -> {}.{}{}",
                caller.as_u32(),
                info.class_name,
                info.method_name,
                info.descriptor
            );
        }
        return Some(handle_jit_dispatch_error(vm, thread, error, info));
    }
    if let Some(moved) = pins.release_raw(thread, args) {
        let out = args_ptr as *mut i64;
        for (i, word) in moved.iter().enumerate().take(args.len()) {
            *out.add(i) = *word;
        }
    }
    // Admitted: the owner is in the caller's namespace now (loaded through
    // its loader above), so the answer was judged; a later call of the site
    // must not pay this resolution again.
    jit_site_access_admit(key, info);
    None
}


pub unsafe extern "C" fn jit_invoke_dispatch(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // t1 wave 2, lane P: a site the compile door could not judge is judged at
    // its first compiled execution (JVMS §5.4.4).
    if !jit_site_access_repeat(vm_ptr, info_ptr) {
        if let Some(bits) = contain_door("jit_invoke_dispatch", vm_ptr, || {
            jit_site_access_guard(vm_ptr, info_ptr, args_ptr, num_args)
        }) {
            return bits;
        }
    }
    // SBR5 lane ncall: a site already known to be a plain callback native.
    // JIT round 20 (lane stash): under the helper's panic scope, as its body.
    if let Some(bits) = contain_door("jit_invoke_dispatch", vm_ptr, || {
        try_lean_native_door(vm_ptr, info_ptr, args_ptr, num_args, false)
    }) {
        return bits;
    }
    let f = || jit_invoke_dispatch_body(vm_ptr, info_ptr, args_ptr, num_args);
    let bits = contain(
        "jit_invoke_dispatch",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: a native callee's object result is compiled code's now.
    native_return_handed_to_compiled_code();
    bits
}

