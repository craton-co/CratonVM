#[allow(unused_imports)]
use super::*;

/// [`try_jit_site_cached_native_dispatch_mode`] for a caller whose
/// `args_slice` is its own GC-updated frame buffer (`NativeSiteDoorMode::Serve`).
///
/// SAFETY: as [`try_jit_site_cached_native_dispatch_mode`].
#[inline(always)]
pub unsafe fn try_jit_site_cached_native_dispatch(
    vm: &SharedVm,
    thread: Option<&mut JvmThread>,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    prevalidated: u64,
) -> Option<i64> {
    try_jit_site_cached_native_dispatch_mode(
        vm,
        thread,
        info,
        info_key,
        args_slice,
        prevalidated,
        NativeSiteDoorMode::Serve,
    )
}

/// The leaf-native fast path for compiled code, probed once per
/// `jit_invoke_dispatch` before any of the compile/dispatch machinery runs.
///
/// `Some(ret)` means the call was fully served here; `None` means this site is
/// not a leaf native and the caller must continue down its ordinary route.
///
/// # Why this is safe to take instead of `invoke_or_native`
///
/// `resolve_native_site` (which runs once per site) is where every
/// obligation is discharged: the invoke kind, the `invoke_or_native` special
/// cases, the `SyntheticStub` arbitration, the capability policy and the
/// `--real-jdk` admission. What is left on this path is the receiver-class
/// guard — re-tested on every hit so a site that goes polymorphic drops out —
/// and the call itself.
///
/// # Redefinition
///
/// The entry is discarded when the target class has been redefined, on the
/// same terms as the Integer cache below it, and re-resolved when the native
/// registry's generation moves (so a lazily-registered native, or one whose
/// re-registration dropped the leaf claim, is picked up rather than memoized
/// wrong — the failure the `NativeCallSite` doc calls out for `OnceLock`).
///
/// SAFETY: `args_slice` is the JIT caller's own stack buffer, already forwarded
/// by `forward_jit_reference_args` in the caller -- or, under
/// `NativeSiteDoorMode::WarmOnly` / `FillOnly`, words no resolution in this
/// call can outlive (see [`NativeSiteDoorMode`]).
pub unsafe fn try_jit_site_cached_native_dispatch_mode(
    vm: &SharedVm,
    // BORROWED FROM THE CALLER, never re-derived here.
    //
    // This used to do its own `jit_thread_mut()?` at the top of the leaf-native
    // arm. That is sound from `jit_invoke_dispatch`, whose own borrow is scoped
    // to an `if let` that closes before the call — and UNSOUND from
    // `jit_invoke_virtual_mic`, which takes `(thread, _jit_thread_guard)` and
    // holds the guard for the whole function. Two `&mut JvmThread` derived from
    // the same raw pointer were live at once, with no `set_jit_thread` boundary
    // between them to make the inner one a child reborrow — a genuine sibling,
    // which is exactly what `jit_thread_mut`'s debug assertion says when it
    // fires. And the outer reference is USED after the inner one is created
    // (`safe_native_call`, `handle_jit_dispatch_error`, … all take it as
    // `&mut`), so the invalidated borrow is not merely held but dereferenced.
    //
    // The `debug_assert!` is `#[cfg(debug_assertions)]`; the aliasing is not.
    // Release built the same two derivations and simply did not look.
    //
    // `Option`, not `&mut`, and consumed at exactly the point the old
    // `jit_thread_mut()?` stood — several counted bails
    // (`site_refusal::note_and_decline`) run before it, and hoisting the
    // thread requirement above them would silently stop counting a refusal
    // whenever no JIT thread is installed.
    thread: Option<&mut JvmThread>,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`, JIT round 19 wave 2, lane
    // funnel2): the caller's `forward_jit_reference_args_validated` mask for
    // `args_slice`, `0` when the switch is off. A set bit stands for the heap
    // walk this door would otherwise repeat on that word.
    prevalidated: u64,
    // JIT round 20 (lane fixLN): see [`NativeSiteDoorMode`].
    mut mode: NativeSiteDoorMode<'_>,
) -> Option<i64> {
    // One-flag kill switch (`CRATONVM_JIT=-native-site-cache`). Default ON —
    // see `native_site_cache_enabled` for what it is for.
    if !native_site_cache_enabled() {
        return None;
    }
    // JIT round 17 wave 2 (lane funnel): `CRATONVM_DBG_NATIVE_DOOR_SPLIT=1`
    // times a crossing this door serves through a funnel (front / funnel /
    // tail, `NativeMethodRegistry::note_native_door_split`). One field load
    // when off.
    let split_t0 = vm
        .natives
        .native_methods
        .dbg_native_door_split()
        .then(mic_prof::now);
    // Every bail below is counted, including these pre-resolution ones. An
    // uncounted `return None` here is what made the first cut of this path
    // unexplainable: `AtomicInteger.get` showed neither a hit nor a refusal,
    // because it was arriving at `jit_invoke_virtual_mic` — a different entry
    // point — and never reaching this function at all.
    if !site_cache_admits_invoke_kind(info.invoke_kind) {
        return site_refusal::note_and_decline(0);
    }
    // r9w2-vm2: a site this thread already resolved to "not a leaf native" at
    // the current registry generation answers `None` below whatever its
    // receiver (the cached refusal is not receiver-keyed: `entry?` returns
    // before the receiver guard), so answer it BEFORE the conservative
    // receiver validation. That validation is a heap-membership walk, and
    // `jit_invoke_virtual_mic` calls this for EVERY helper-side virtual
    // dispatch, native or not — the overwhelming majority of them sites
    // whose steady state is exactly this cached refusal.
    //
    // r9w5 (typecheck5): the ONE probe of the site's memo on this path. The
    // warm-hit lookup further down used to probe the thread-local map a second
    // time for the same key; nothing between the two can write it (the
    // receiver validation is a heap read), so the copy taken here is the one
    // that lookup would have returned.
    let generation = vm.natives.native_methods.generation();
    let cached = NATIVE_SITE_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
    if matches!(cached, Some((gen, None)) if gen == generation) {
        return None;
    }
    // The receiver's runtime class both selects the override to resolve and
    // guards a warm entry. A static site has neither.
    // The resolved receiver is threaded through to `decode_dispatch_values_into`
    // and to `class_id_of_validated`, so this is the ONLY conservative header
    // validation on the path. It used to be three: here, again inside
    // `VmHeap::class_id_of`, and a third time in the argument decode — 3.2 ns
    // each on a ~100 ns call (`jit_native_dispatch_profile`).
    let mut receiver = None;
    let receiver_class_id = if info.invoke_kind == 3 {
        None
    } else {
        let raw = *args_slice.first()? as u64;
        if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            return site_refusal::note_and_decline(0);
        }
        let validated = match prevalidated_jit_ref(prevalidated, 0, raw as i64) {
            // FUNNEL17-2: the forwarding pass walked this word already.
            Some(obj) => Some(obj),
            None => {
                crate::jit::helpers::note_membership_walk(1);
                vm.mem.heap.is_object_address(raw as usize)
            }
        };
        match validated {
            Some(obj) => {
                receiver = Some(obj);
                // `invokespecial` validates and decodes its receiver like any
                // instance call, but is statically bound: its entry carries no
                // receiver guard (see `resolve_native_site`), so neither does
                // the key it is compared against.
                if info.invoke_kind == 1 {
                    None
                } else {
                    match jit_dispatch_class_validated(vm, obj) {
                        JitDispatchClass::Class(cid) => Some(cid),
                        // An array's header carries its COMPONENT's class id,
                        // so the class-id key below would take an `Integer[]`
                        // for an `Integer`: resolve `Integer.hashCode` for it,
                        // or hit an entry an `Integer` warmed, and run that
                        // native on the array (round 11 wave 12, probe
                        // `R11IcArrayReceiverAlias`). An array receiver is
                        // served by the general dispatch, which resolves
                        // `Object`'s methods for it. The cached refusal above
                        // is not receiver-keyed, so declining here records
                        // nothing.
                        JitDispatchClass::JavaLangObject => {
                            return site_refusal::note_and_decline(0);
                        }
                    }
                }
            }
            None => return site_refusal::note(0).map(|_| 0),
        }
    };

    // `warm`: served from the memo as it stood on entry, with no resolution in
    // this call (the lean funnel's precondition, `safe_native_call_jit_lean`:
    // a resolution can load and initialize classes, which runs Java).
    let mut warm = true;
    let entry = match cached {
        Some((gen, entry)) if gen == generation => entry,
        _ => {
            if mode.refuses_resolution() {
                return None;
            }
            warm = false;
            let transient = std::cell::Cell::new(false);
            let resolved = resolve_native_site_tracked(vm, info, receiver_class_id, &transient);
            // W4B: a refusal that is an artefact of this call (class manager
            // write-locked) is served by the funnel and not remembered.
            if transient.get() {
                return None;
            }
            // JVMS §5.5: an `invokestatic` initializes the declaring class
            // before the native runs. A static entry is cached only once that
            // class is initialized (initialization is monotone, so the hit
            // needs no re-check); until then the site is declined WITHOUT
            // caching and the funnel tail initializes it
            // (`jit_static_owner_init_before_native`). A cached refusal would
            // stick until the registry generation moves. Fill time only.
            if info.invoke_kind == 3
                && resolved.is_some()
                && static_native_site_init_pending(vm, info)
            {
                return site_refusal::note_and_decline(11);
            }
            // JIT round 19 wave 6 (lane mega6; FUNNEL3-19-1,
            // `CRATONVM_JIT_NATIVE_SITE_REFUSAL_BY_RECEIVER`): a virtual or
            // interface site's refusal is kept for THIS receiver class only.
            // Off: the site-wide `None`, as before.
            let resolved =
                native_site_refusal_keyed_by_receiver(vm, info, receiver_class_id, resolved);
            NATIVE_SITE_CACHE.with(|cache| {
                cache.borrow_mut().insert(info_key, (generation, resolved));
            });
            resolved
        }
    };
    let receiver_guard = receiver_class_id.map(|cid| cid.as_u32());
    // Round 16 (lane redefine; backlog sweepB S4): a receiver-keyed
    // redefined-owner refusal answers only its own receiver. Another receiver
    // re-resolves the site once; what that caches is final for the generation
    // (`native_site_refill_outcome`). Virtual/interface sites only: a static
    // or `invokespecial` site never caches one. Cold.
    let entry = if native_site_refill_for_receiver(entry.as_ref(), receiver_guard) {
        if mode.refuses_resolution() {
            return None;
        }
        warm = false;
        let transient = std::cell::Cell::new(false);
        let resolved = native_site_refill_outcome(resolve_native_site_tracked(
            vm,
            info,
            receiver_class_id,
            &transient,
        ));
        if transient.get() {
            return None;
        }
        NATIVE_SITE_CACHE.with(|cache| {
            cache.borrow_mut().insert(info_key, (generation, resolved));
        });
        resolved
    } else {
        entry
    };
    let mut entry = entry?;
    // Re-check the receiver guard on every hit: the entry was resolved against
    // one runtime class, and this site may since have seen another.
    if entry.receiver_class_id != receiver_guard {
        // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-1,
        // `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): a second receiver class
        // has its own resolution, so a site alternating between two natives
        // is served for both (and files both in the lean table's set). Off:
        // one field load, then the old `None`.
        // JIT r20 (lane fixLN): `WarmOnly` declines where this would resolve.
        let may_resolve = !matches!(mode, NativeSiteDoorMode::WarmOnly(_));
        let second = native_site_second_receiver_entry(
            vm,
            info,
            info_key,
            generation,
            receiver_class_id,
            &mut warm,
            may_resolve,
        );
        if !warm && mode.refuses_resolution() {
            return None;
        }
        entry = second?;
        if entry.receiver_class_id != receiver_guard {
            return None;
        }
    }
    // JIT round 20 (lane fixLN): the site is settled for this receiver class;
    // a fill-only call serves nothing.
    if matches!(mode, NativeSiteDoorMode::FillOnly) {
        return None;
    }
    // JIT round 20 (lane fixLN): a resolution in this call was a GC point, so
    // the receiver validated before it is re-read from the caller's word,
    // which the collector kept current (`Serve`: the caller's frame buffer;
    // `WarmOnly` never gets here having resolved).
    if !warm {
        if let Some(recv) = receiver.as_mut() {
            *recv = ObjectRef::from_raw(*args_slice.first()? as *mut u8);
        }
    }
    // This receiver's owner was woven (slot 12, counted at fill): the funnel
    // decides every call, as for the site-wide refusal.
    if entry.refused_for_receiver {
        return None;
    }
    // Both of these open with the same process-global `any_class_redefined()`
    // load and answer `false` for every run in which nothing was ever
    // redefined. Hoisting that one test out costs a redefining run nothing and
    // saves every other run a second cross-crate call (1.2 ns each, measured).
    if crate::classloading::any_class_redefined() {
        // Round 15 wave 4 (lane walk3): an owner-scoped entry was judged by
        // the redefine rule at fill and is re-judged by its owner's generation
        // below; these two "ever redefined" tests never turn back to `false`
        // and would decline it forever.
        if !entry.owner_scoped {
            if site_class_was_redefined(vm, info, info_key) {
                return None;
            }
            if let Some(cid) = receiver_class_id {
                if class_was_redefined(vm, cid) {
                    return None;
                }
            }
        }
        // The ancestor that owns the native (round 15 wave 4, lane walk2):
        // one atomic load while nothing was redefined since the fill.
        if let Some((owner, owner_generation)) = entry.native_owner {
            if entry.fill_redefine_epoch != vm.jit.jit_cache.redefine_epoch()
                && vm.natives.native_methods.ancestor_redefine_rule()
                && !native_site_owner_still_current(
                    vm,
                    info_key,
                    generation,
                    owner,
                    owner_generation,
                )
            {
                return None;
            }
        }
    }

    // CAPABILITY GATE. Same gate, same point in the call, as
    // `invoke_or_native`'s "dispatch site 1 of 3": the last place before the
    // native actually runs, because a capability is only exercised by a native
    // that executes. Reached only by the classified triples — process spawn,
    // library load, `Unsafe`, Panama, file and socket I/O — and skipped by a
    // not-taken branch for everything else.
    //
    // A refusal returns `None` rather than propagating the error: this is a
    // fast path whose contract is "served, or not served here", and the funnel
    // it declines to raises the identical error from the identical gate. That
    // costs a refused call one extra traversal and keeps the fast path unable
    // to invent a failure the slow path would not have produced.
    if let Some(kind) = entry.capability_kind {
        if crate::vm::check_native_dispatch_capability_for_slot(vm, kind, entry.native_id).is_err()
        {
            return site_refusal::note_and_decline(3);
        }
    }
    // The old `let (thread, _guard) = jit_thread_mut()?;` stood here. Same
    // position, same early-out, but the reference is the caller's.
    let thread = thread?;
    // `--real-jdk` (interpreter round i1 wave 37, lane L4): an invoke-exact
    // `VarHandle` refuses a call site that is not its access mode type, before
    // the in-funnel fast paths below serve it. One relaxed load and a field
    // read. The thin helpers' synthetic sites carry a stand-in descriptor;
    // they are judged against the one their compiled caller's invoke names
    // (wave 49, `varhandle_stand_in_site_descriptor`).
    if entry.poly {
        if let Some(exact_slot) = crate::runtime::interpreter::var_handle_exact_gate(vm) {
            if let Some(vh) = receiver {
                if crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(info.class_name)
                    && crate::runtime::interpreter::var_handle_receiver_is_exact(vm, vh, exact_slot)
                {
                    if let Some(error) = jit_var_handle_exact_judgement(vm, thread, info, vh) {
                        return Some(handle_jit_dispatch_error(vm, thread, error, info));
                    }
                }
            }
        }
    }
    // JIT round 16 wave 12 (lane ffmdoor; FFC16-1): a `MethodHandle` carrier
    // entry whose handle is a bound plain downcall carrier calls the plain
    // path without the `invokeExact` door (`try_jit_ffm_downcall_bound`;
    // `CRATONVM_JIT_FFM_DOWNCALL_BIND`, default on: one load).
    if entry.poly && entry.sync.is_none() && info.method_name == "invokeExact" {
        if let Some(handle) = receiver {
            if let Some(bits) =
                try_jit_ffm_downcall_bound(vm, thread, info, info_key, handle, args_slice, &entry)
            {
                return Some(bits);
            }
        }
    }
    if entry.kind == LeafNativeKind::ThreadCurrentThread {
        // The mirror is a per-thread GC root the collector remaps, and handing
        // it to the caller roots it again with no allocation in between. When
        // it does not exist yet, fall through: the ordinary dispatcher runs
        // `current_thread_object`'s allocating slow path that builds it.
        let obj = thread.java_thread_obj?;
        note_leaf_native_hit();
        count_jit_native_dispatch(vm, entry.native_id);
        // Object-return handoff root, same contract as every other JIT native
        // fast path (see `jit_integer_value_of_direct`).
        thread.native_pending_return = Some(obj);
        return Some(obj.as_ptr() as i64);
    }
    // r9w4 vm4 (NOTES-w3-collections3.md): `ArrayList.get(int)` / `size()` on
    // an ordinary ArrayList, answered without the argument decode and the
    // `safe_native_call_*` wrapper. A decline falls through to the ordinary
    // callback path below, which runs the same native.
    //
    // Real-JDK: nothing to ask, and until 2026-09-22 this stood aside anyway.
    //
    // The entry exists only because `admit_jit_fast_native_resolved` admitted
    // this native under the active policy — that IS the policy question, asked
    // once at cache-fill time, and the probe below is not a second answer to
    // it. `cratonvm_native_collections::jit_arraylist_get` is `native_al_get`'s
    // own hot path, exported: same `al_fast_state`, same bounds test, same
    // `get_array_element`. Standing aside sent the call to `native_al_get`,
    // which ran those exact lines one `safe_native_call_*` wrapper later.
    //
    // That is the shape
    // `docs/feature-designs/jit-ir-optimizations.md`
    // is about — a refusal whose tail runs what it refused — at a third door,
    // and the old comment here cited the thin helpers' guard as its precedent.
    // That guard now asks whether the tail would refuse; this one has no tail
    // question left to ask, because the callback it would fall to is already
    // admitted and is the same code. So the term is gone rather than
    // rephrased.
    //
    // Still not counted in `jit_direct_helper_jdk_only_refusals`, for the
    // reason that counter's doc gives: nothing is refused here in either mode.
    if entry.sync.is_none()
        && matches!(
            entry.kind,
            LeafNativeKind::ArrayListGet | LeafNativeKind::ArrayListSize | LeafNativeKind::ChmGet
        )
    {
        if let Some(recv) = receiver {
            // FUNNEL3-19-2 (lane mega6): `None` with the switch off.
            let validated_arg =
                site_leaf_probe_validated_arg(vm, entry.kind, warm, prevalidated, args_slice);
            if let Some(bits) =
                try_jit_arraylist_leaf(vm, thread, entry.kind, recv, args_slice, validated_arg)
            {
                if entry.leaf {
                    note_leaf_native_hit();
                } else {
                    note_site_cached_native_hit();
                }
                JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.collection_leaf_hits));
                count_jit_native_dispatch(vm, entry.native_id);
                // FUNNEL2-19-2 (lane funnel3): the next call of this site
                // takes the lean door's probe.
                if vm.natives.native_methods.jit_native_site_door_leaf_kinds() {
                    lean_native_site_fill(vm, info, info_key.1, &entry, generation);
                }
                return Some(bits);
            }
        }
    }
    // `Integer.hashCode()` / `Integer.equals(Object)` (round 11 wave 11): the
    // same admitted-entry argument as the collection probes above, for the two
    // natives `HashMap.hash` / `HashMap.getNode` reach on every Integer-keyed
    // `put`/`get`. Not a collection hit, so it does not feed that counter; the
    // site-cached hit and the §4 census count it as before.
    if entry.sync.is_none()
        && matches!(
            entry.kind,
            LeafNativeKind::IntWrapperHashCode
                | LeafNativeKind::IntWrapperEquals
                | LeafNativeKind::ObjectGetClass
                | LeafNativeKind::ObjectHashCode
                | LeafNativeKind::ReferenceRefersTo
                | LeafNativeKind::ClassIsInstance
                | LeafNativeKind::CompatThreadLocalGet
                | LeafNativeKind::ZoneInfoGetOffsets
                | LeafNativeKind::ZoneInfoGetOffsetsByWall
        )
    {
        if let Some(recv) = receiver {
            // FUNNEL3-19-2 (lane mega6): `None` with the switch off.
            let validated_arg =
                site_leaf_probe_validated_arg(vm, entry.kind, warm, prevalidated, args_slice);
            if let Some(bits) =
                try_jit_arraylist_leaf(vm, thread, entry.kind, recv, args_slice, validated_arg)
            {
                if entry.leaf {
                    note_leaf_native_hit();
                } else {
                    note_site_cached_native_hit();
                }
                count_jit_native_dispatch(vm, entry.native_id);
                // FUNNEL2-19-2 (lane funnel3), as for the collection kinds.
                if vm.natives.native_methods.jit_native_site_door_leaf_kinds() {
                    lean_native_site_fill(vm, info, info_key.1, &entry, generation);
                }
                return Some(bits);
            }
        }
    }
    // `VarHandle` read modes on an ordinary instance field are a field load
    // wearing a native's clothes. Serving them here rather than through the
    // funnel is what takes `VarHandle.get` off the boxing round trip — see
    // `try_varhandle_instance_field_read`.
    if entry.poly && entry.sync.is_none() {
        // An element of a real `int[]` / `long[]` handle
        // (`AtomicIntegerArray`, `AtomicLongArray`, netty's `MpscIntQueue`):
        // one hardware access instead of the generic VarHandle native.
        if let Some(bits) = try_varhandle_array_element(vm, info, args_slice) {
            note_site_cached_native_hit();
            JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_array_element_served));
            count_jit_native_dispatch(vm, entry.native_id);
            return Some(bits);
        }
        if let Some(bits) = try_varhandle_byte_array_view_access(vm, info, args_slice)
            .or_else(|| try_varhandle_byte_buffer_view_access(vm, info, args_slice, thread))
            .or_else(|| try_varhandle_instance_field_read(vm, info, args_slice, thread))
        {
            note_site_cached_native_hit();
            if info.method_name == "get" {
                JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_field_read_hits));
            }
            count_jit_native_dispatch(vm, entry.native_id);
            return Some(bits);
        }
        // SBR3 lane ffmperf: a real `SegmentVarHandle` over a real native
        // segment (see `try_varhandle_segment_access`).
        if let Some(bits) = try_varhandle_segment_access(vm, info, args_slice, thread) {
            note_site_cached_native_hit();
            count_jit_native_dispatch(vm, entry.native_id);
            return Some(bits);
        }
        // The write direction had no equivalent, which is why a funnelled CAS
        // cost ~182 ns more than a bound call while a funnelled READ cost ~49.
        // See `try_varhandle_instance_field_cas`.
        if varhandle_cas_funnel_fast_enabled() {
            if let Some(bits) = try_varhandle_instance_field_cas(vm, info, args_slice) {
                note_site_cached_native_hit();
                JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_field_cas_served));
                count_jit_native_dispatch(vm, entry.native_id);
                return Some(bits);
            }
            JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_field_cas_declined));
        }
    }
    // SBR5 lane ncall: a plain callback entry that passed every gate above is
    // remembered in the lean door's table, so the NEXT call of this site is
    // served by `try_lean_native_door` at the top of the extern helper.
    lean_native_site_fill(vm, info, info_key.1, &entry, generation);
    // FUNNEL17-2: the mask is good only up to the first possible GC point. A
    // resolution in this call (`warm == false`) can load classes, so the
    // parameters are walked again after one, as before the switch existed.
    let prevalidated = if warm { prevalidated } else { 0 };
    // `JitDecodedArgs::new()` and not `with_capacity(args_slice.len())`: the
    // latter is an outlined call whose ~144-byte return the caller has to
    // materialise, and it measured 10.3 ns for a zero-argument decode that has
    // nothing to do (`jit_native_dispatch_profile`). This is the buffer the
    // decode fills in place.
    let mut decoded_values: JitDecodedArgs;
    let values: &[Value] = if args_slice.is_empty() && receiver.is_none() {
        &[]
    } else {
        decoded_values = JitDecodedArgs::new();
        decode_dispatch_values_into_shaped(
            vm,
            info,
            args_slice,
            receiver.map(Some),
            entry.desc.params(),
            prevalidated,
            &mut decoded_values,
        );
        &decoded_values
    };
    if entry.leaf {
        note_leaf_native_hit();
    } else {
        note_site_cached_native_hit();
    }
    count_jit_native_dispatch(vm, entry.native_id);
    // `decode_dispatch_values` heap-validated every object argument, which is
    // exactly the precondition `..._prevalidated_objects` names — and it is the
    // same call `invoke_or_native` would have made after its cascade, so the
    // non-leaf path here differs from the generic route only by what it did
    // NOT re-resolve.
    // Nothing between this and the native call runs Java, so no nested
    // access can consume the cell.
    if entry.vh_value_site != 0 {
        cratonvm_native_api::poly_call_site::arm_vh_param_site(entry.vh_value_site);
    }
    // Round 13 wave 10 (lane callcost4): a `MethodHandle` carrier entry
    // (`method_handle_carrier_site`, the only `poly` entry under these two
    // names) publishes the call-site descriptor its door takes first, as the
    // signature-polymorphic block does (`arms_poly_call_site`).
    if entry.poly && crate::vm::vm_exec::arms_poly_call_site(info.class_name, info.method_name) {
        // Interpreter round i5 wave 107, lane L4 (`CRATONVM_MH_ARMED_CALLER`,
        // default ON in the consumer since the w107 A/B): the site's own class rides along, so
        // a warm before-linker lane need not walk the compiled frames for it.
        cratonvm_native_api::poly_call_site::arm_with_caller(
            info.descriptor,
            info.declaring_class_id,
        );
    }
    // Round 15 wave 2 hand-back (lane vh2; VH2-3): the handle's variable kind
    // for the reference-return cast below, read while `receiver` is current
    // (the native may move it). `None` for every site that is not a narrower-
    // than-`Object` reference read.
    let vh_variable_is_reference = if entry.poly {
        crate::vm::vm_exec::vh_reference_return_variable(
            vm,
            thread,
            info.method_name,
            info.descriptor,
            receiver,
        )
    } else {
        None
    };
    // JIT round 17 wave 2 (lane funnel; RD16-3 = BD14-1 = STR16-P4):
    // `CRATONVM_JIT_NATIVE_LEAN_DOOR=1` sends a plain non-leaf entry served
    // WARM (no resolution in this call, so nothing since the caller's
    // `forward_jit_reference_args` and the decode above was a GC point) through
    // `safe_native_call_jit_lean`, which skips the argument re-forward and the
    // interpreter-only native-array GC hook and is otherwise the funnel.
    // Sync, signature-polymorphic, `VarHandle`-armed and capability-gated
    // entries keep their own route.
    let lean = warm
        && !entry.leaf
        && entry.sync.is_none()
        && !entry.poly
        && entry.vh_value_site == 0
        && entry.capability_kind.is_none()
        && vm.natives.native_methods.jit_native_lean_door();
    // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-3,
    // `CRATONVM_JIT_NATIVE_FRESH_RESULT`): a plain non-leaf native proven to
    // return a fresh live base heals its result by the forwarding read alone.
    let fresh_result = !entry.leaf
        && entry.sync.is_none()
        && !entry.poly
        && entry.vh_value_site == 0
        && vm.natives.native_methods.jit_native_fresh_result()
        && cratonvm_native_builtins::is_fresh_object_result_native(entry.callback);
    let split_t1 = split_t0.map(|_| mic_prof::now());
    let called = if let Some(sync) = entry.sync {
        // `ACC_SYNCHRONIZED`: the full funnel under the method's monitor, as
        // `invoke_or_native` runs it (`by_name_native_call`). No shortcut
        // above served it.
        crate::runtime::interpreter::safe_native_call_synchronized(
            vm,
            thread,
            entry.callback,
            values,
            Some(sync),
        )
    } else if entry.leaf {
        crate::vm::safe_native_call_leaf(vm, thread, entry.callback, values)
    } else if fresh_result {
        crate::vm::safe_native_call_jit_fresh_result(vm, thread, entry.callback, values, lean)
    } else if lean {
        crate::vm::safe_native_call_jit_lean(vm, thread, entry.callback, values)
    } else {
        crate::vm::safe_native_call_prevalidated_objects(vm, thread, entry.callback, values)
    };
    let split_t2 = split_t0.map(|_| mic_prof::now());
    // A native that did not take the cell (not an access mode, or a door that
    // served before its take) must not leave it for a later, unarmed native.
    let mut called = called;
    if entry.vh_value_site != 0 {
        cratonvm_native_api::poly_call_site::clear_vh_value_site();
        // An `asType` refusal names this site's descriptor, as the
        // interpreter's doors do (interpreter round i2 wave 53, lane L4).
        called = match called {
            Err(failed @ crate::error::MethodCallFailed::ExceptionThrown(_)) => {
                Err(jit_vh_astype_refusal_site_text(vm, thread, failed, info))
            }
            other => other,
        };
    }
    let result = match called {
        // A signature-polymorphic native returns the erased `Object` the
        // registration promises, so it needs the call site's own descriptor
        // applied — including the `WrongMethodTypeException` rule
        // `unbox_poly_return_checked` owns. That call can itself fail (it
        // raises the exception), so it is folded into the same error arm the
        // native's own failure takes.
        Ok(value) if entry.poly => {
            match crate::vm::vm_exec::unbox_poly_return_checked_vh(
                vm,
                thread,
                value,
                info.descriptor,
                info.method_name,
                vh_variable_is_reference,
            ) {
                Ok(unboxed) => crate::vm::coerce_native_return_typed(unboxed, entry.desc.ret),
                Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
            }
        }
        Ok(value) => crate::vm::coerce_native_return_typed(value, entry.desc.ret),
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    let bits = match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    };
    // `CRATONVM_DBG_NATIVE_DOOR_SPLIT`: the three spans of a crossing served
    // here (a throwing native's crossing is not counted). `front` starts at
    // this function's entry, so the dispatch helper's own prologue is not in
    // it (`CRATONVM_DBG_MIC_PROF`'s `cyc_disp_total` / `cyc_mic_total` cover
    // the whole helper).
    if let (Some(t0), Some(t1), Some(t2)) = (split_t0, split_t1, split_t2) {
        vm.natives.native_methods.note_native_door_split(
            lean,
            t1.wrapping_sub(t0),
            t2.wrapping_sub(t1),
            mic_prof::now().wrapping_sub(t2),
        );
    }
    Some(bits)
}

/// FUNNEL2-19-1 (JIT round 19 wave 3, lane funnel3;
/// `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): the site cache's answer for a
/// virtual/interface site's SECOND receiver class, when the site's own entry
/// guards another class.
///
/// `NATIVE_SITE_CACHE` holds one resolution per site. A site whose receiver
/// alternates between two classes (`o.hashCode()` over `Integer`s and
/// `String`s, a bimorphic `List.get`) served its first class and sent every
/// call with the other one to the generic dispatcher. This memo, keyed by
/// `(site, receiver class)` and stamped with the registry generation like the
/// site's own, resolves that class once (`resolve_native_site`, the same
/// resolution the site's first call ran) and keeps the answer, a refusal
/// included. `None` when the switch is off, for a static or `invokespecial`
/// site, or a refusal. `warm` is cleared when this call resolved.
#[inline(never)]
pub fn native_site_second_receiver_entry(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    generation: u32,
    receiver_class_id: Option<ClassId>,
    warm: &mut bool,
    // JIT round 20 (lane fixLN): `false` declines (clearing `warm`, resolving
    // nothing) where this would resolve (`NativeSiteDoorMode::WarmOnly`).
    may_resolve: bool,
) -> Option<NativeSiteCache> {
    if !vm.natives.native_methods.jit_native_site_door_two_way()
        || !matches!(info.invoke_kind, 0 | 2)
    {
        return None;
    }
    let key = (info_key, receiver_class_id?.as_u32());
    match NATIVE_SITE_CACHE_ALT.with(|cache| cache.borrow().get(&key).copied()) {
        Some((gen, entry)) if gen == generation => entry,
        _ => {
            *warm = false;
            if !may_resolve {
                return None;
            }
            let transient = std::cell::Cell::new(false);
            let resolved = resolve_native_site_tracked(vm, info, receiver_class_id, &transient);
            if transient.get() {
                return None;
            }
            NATIVE_SITE_CACHE_ALT.with(|cache| {
                cache.borrow_mut().insert(key, (generation, resolved));
            });
            resolved
        }
    }
}

// ---------------------------------------------------------------------------
// SBR5 lane ncall: the lean native door
// ---------------------------------------------------------------------------
//
// A compiled call to a registered native used to cost 190-250 ns of CPU: the
// extern helper's panic scope, SATB flush, `forward_jit_reference_args`' descriptor
// walk, the site-memo generation flush, a hash probe that copies a ~120-byte
// entry out of a `RefCell<HashMap>`, a second heap-membership walk per object
// argument, and only then the call wrapper. All of those are per-SITE facts or
// guards for work a plain callback cannot do (a collection cannot run between
// the compiled caller's argument staging and this call, because the caller
// polls safepoints only at its own poll points).
//
// `try_jit_site_cached_native_dispatch` already decides, once per site and
// registry generation, that a site is a plain callback native. When it serves
// such a site it also files a compact record here (`lean_native_site_fill`):
// a direct-mapped, thread-local table keyed by the `JitInvokeInfo` address.
// `try_lean_native_door` is the hit: it re-tests exactly the guards the memo's
// own hit re-tests (the JIT cache generation that retires an address, the
// native registry generation, the receiver's class for a virtual site, "no class
// was ever redefined") and calls the same wrappers the memo would
// (`safe_native_call_leaf` for a leaf claim, the funnel otherwise). Anything
// else -- a miss, a changed guard, a null/array receiver, an unexpected
// argument count -- answers `None` and the call takes today's route unchanged.
//
// `CRATONVM_JIT=-native-site-cache` (the memo's own kill switch) keeps the table
// empty: one binary, two arms for the A/B.

/// Slots of the thread-local table. A power of two; collisions evict.
///
/// JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-1,
/// `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): under that switch the same 256
/// slots are read as 128 two-way sets, `{i, i ^ LEAN_NATIVE_SLOTS / 2}`
/// ([`lean_native_alt_slot`]). The primary slot of a key is the one the
/// direct-mapped table uses, so the switch off probes exactly what it always
/// probed.
pub const LEAN_NATIVE_SLOTS: usize = 256;

#[derive(Clone, Copy)]
pub struct LeanNativeSite {
    /// The `JitInvokeInfo` address; `0` is an empty slot.
    pub key: usize,
    pub vm_identity: usize,
    /// The JIT cache's RETIRE count at the fill (`callee_verdict_retire_epoch`):
    /// an address is only ever re-issued under a newer one (`Drop for
    /// CompiledMethod` bumps it before the `JitInvokeInfo` box is freed). W4B: a
    /// plain publication no longer retires the record -- a native served from
    /// the table was never preferred over, or falsified by, a new body (the
    /// table is consulted before any compiled-callee machinery), and javac's
    /// continuous compile storm was declining the door and re-resolving every
    /// site per publication per thread.
    pub jit_gen: u64,
    pub registry_gen: u32,
    /// The exact receiver class of a virtual/interface site, `0` otherwise.
    pub receiver_class: u32,
    pub invoke_kind: u8,
    pub leaf: bool,
    pub ret: u8,
    pub nparams: u8,
    pub params: [u8; INLINE_JIT_NATIVE_ARGS],
    pub callback: Option<cratonvm_native_api::NativeCallback>,
    pub native_id: Option<cratonvm_native_api::NativeMethodId>,
    pub capability_kind: Option<cratonvm_native_api::CapabilityKind>,
    /// The site cache's [`LeafNativeKind`]: `Callback` unless
    /// `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS` filed a leaf kind
    /// (FUNNEL2-19-2, [`lean_leaf_kind_fileable`]).
    pub kind: LeafNativeKind,
    /// The callback returns a fresh live base
    /// (`cratonvm_native_builtins::is_fresh_object_result_native`) and
    /// `CRATONVM_JIT_NATIVE_FRESH_RESULT` was on at the fill (FUNNEL2-19-3).
    pub fresh_result: bool,
}

impl LeanNativeSite {
    pub const EMPTY: LeanNativeSite = LeanNativeSite {
        key: 0,
        vm_identity: 0,
        jit_gen: 0,
        registry_gen: 0,
        receiver_class: 0,
        invoke_kind: 0,
        leaf: false,
        ret: 0,
        nparams: 0,
        params: [0; INLINE_JIT_NATIVE_ARGS],
        callback: None,
        native_id: None,
        capability_kind: None,
        kind: LeafNativeKind::Callback,
        fresh_result: false,
    };
}

thread_local! {
    pub static LEAN_NATIVE_SITES: std::cell::UnsafeCell<[LeanNativeSite; LEAN_NATIVE_SLOTS]> =
        const { std::cell::UnsafeCell::new([LeanNativeSite::EMPTY; LEAN_NATIVE_SLOTS]) };
}

#[inline]
pub fn lean_native_slot(info_addr: usize) -> usize {
    (info_addr >> 4) & (LEAN_NATIVE_SLOTS - 1)
}

/// FUNNEL2-19-1: the other slot of `slot`'s two-way set. An involution, and
/// both members share the low seven bits of [`lean_native_slot`], so a key's
/// set is the same whichever of the two it was filed in.
#[inline]
pub fn lean_native_alt_slot(slot: usize) -> usize {
    slot ^ (LEAN_NATIVE_SLOTS / 2)
}

/// Where a two-way fill writes (FUNNEL2-19-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeanPlace {
    /// The key's primary slot.
    Primary,
    /// The other way of the set.
    Alternate,
    /// Both ways hold other records: the primary's record moves to the
    /// alternate (evicting what was there) and the new record takes the
    /// primary. The most recently filed record of a set is in its primary way.
    Demote,
}

/// The two-way fill policy, as a pure function of the set's two identities
/// `(key, receiver class)` (`0` key = empty) and the record being filed. A
/// record already present is rewritten in place (a guard that moved refills
/// it); an empty way is taken before anything is evicted.
pub fn lean_native_two_way_place(
    primary: (usize, u32),
    alternate: (usize, u32),
    record: (usize, u32),
) -> LeanPlace {
    if primary == record {
        LeanPlace::Primary
    } else if alternate == record {
        LeanPlace::Alternate
    } else if primary.0 == 0 {
        LeanPlace::Primary
    } else if alternate.0 == 0 {
        LeanPlace::Alternate
    } else {
        LeanPlace::Demote
    }
}

/// FUNNEL2-19-2: the site-cache leaf kinds the lean table may file under
/// `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS`: every kind
/// [`try_jit_arraylist_leaf`] answers for a validated virtual/interface
/// receiver. `ThreadCurrentThread` (a static site the site-cache door answers
/// before any fill) and `Callback` (always filed) are not in the list.
pub fn lean_leaf_kind_fileable(kind: LeafNativeKind) -> bool {
    matches!(
        kind,
        LeafNativeKind::ArrayListGet
            | LeafNativeKind::ArrayListSize
            | LeafNativeKind::ChmGet
            | LeafNativeKind::IntWrapperHashCode
            | LeafNativeKind::IntWrapperEquals
            | LeafNativeKind::ObjectGetClass
            | LeafNativeKind::ReferenceRefersTo
            | LeafNativeKind::ClassIsInstance
            | LeafNativeKind::CompatThreadLocalGet
            | LeafNativeKind::ZoneInfoGetOffsets
            | LeafNativeKind::ZoneInfoGetOffsetsByWall
    )
}

/// File `entry` (the memo's answer for the site at `info_addr`) in the lean
/// table when it is a plain callback the door can serve. Cold: runs on the
/// memo's way to a call, which is the slow route this table removes.
#[inline(never)]
pub fn lean_native_site_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_addr: usize,
    entry: &NativeSiteCache,
    registry_gen: u32,
) {
    let natives = &vm.natives.native_methods;
    // (The table is filled only from `try_jit_site_cached_native_dispatch`,
    // which `CRATONVM_JIT=-native-site-cache` already switches off, so that
    // one declared kill switch removes the lean door too.)
    //
    // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-2): under
    // `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS` a leaf kind is filed too; the
    // door's hit runs the same no-wrapper probe the site-cache door runs and
    // falls back to the record's callback, as that door does.
    let kind_filed = entry.kind == LeafNativeKind::Callback
        || (natives.jit_native_site_door_leaf_kinds() && lean_leaf_kind_fileable(entry.kind));
    if !kind_filed
        || entry.poly
        || entry.sync.is_some()
        || entry.vh_value_site != 0
        || entry.refused_for_receiver
        || info.invoke_kind > 3
    {
        return;
    }
    // Interpreter round i7 wave 138, lane L2 (page
    // `interpreter-i138-L2-lean-native-door-skips-the-interface-special-receiver-guard-FIXED-20261021`):
    // the lean door answers at the top of `jit_invoke_dispatch`, BEFORE the
    // interface sender's receiver check (`iface_special_receiver_guard`), and
    // a kind-1 record carries no receiver guard. A record a passing receiver
    // filed would serve a later refused receiver class with no
    // `IllegalAccessError`, so such a site is never filed (it keeps the
    // site-cache door, which runs after the check).
    if iface_special_site_is_guarded(vm, info) {
        return;
    }
    let Some(params) = entry.desc.params() else {
        return;
    };
    // The diagnostic doors the extern helpers feed stay on the old route.
    if lean_native_site_diagnostics_on() {
        return;
    }
    // A custom loader's resource accessors take a null-name NPE before any
    // dispatch (both extern helpers).
    if matches!(
        (info.method_name, info.descriptor),
        ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
            | (
                "getResources",
                "(Ljava/lang/String;)Ljava/util/Enumeration;"
            )
            | (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;"
            )
    ) {
        return;
    }
    let callback = entry.callback;
    let mut tags = [0u8; INLINE_JIT_NATIVE_ARGS];
    tags[..params.len()].copy_from_slice(params);
    let site = LeanNativeSite {
        key: info_addr,
        vm_identity: vm.vm_identity,
        jit_gen: callee_verdict_retire_epoch(),
        registry_gen,
        receiver_class: entry.receiver_class_id.unwrap_or(0),
        invoke_kind: info.invoke_kind,
        leaf: entry.leaf,
        ret: entry.desc.ret,
        nparams: params.len() as u8, // Cast: at most INLINE_JIT_NATIVE_ARGS
        params: tags,
        callback: Some(callback),
        native_id: entry.native_id,
        capability_kind: entry.capability_kind,
        kind: entry.kind,
        // FUNNEL2-19-3 (lane funnel3): the leaf wrapper has no fresh-result
        // variant (a leaf cannot allocate, and neither proven body is one).
        fresh_result: !entry.leaf
            && natives.jit_native_fresh_result()
            && cratonvm_native_builtins::is_fresh_object_result_native(callback),
    };
    // A virtual/interface entry without a receiver guard cannot be served.
    // (`ClassId(0)` is a real class here -- `java/lang/Object` -- so the guard
    // is "present", not "non-zero".)
    if matches!(info.invoke_kind, 0 | 2) && entry.receiver_class_id.is_none() {
        return;
    }
    lean_native_site_file(site, natives.jit_native_site_door_two_way());
}

/// The diagnostic doors the extern helpers feed (each keeps a call on the old
/// route, so no lean record is filed while one is on).
pub fn lean_native_site_diagnostics_on() -> bool {
    site_alias_detect_enabled()
        || crate::runtime::env_cache::jit_dispatch_dbg()
        || crate::runtime::env_cache::jit_putfield_diag()
        || crate::runtime::env_cache::disable_jit()
        || mic_prof::trace_enabled()
        || mic_prof::enabled()
        || cv_trace_enabled()
        || mic_trace_method_filter().is_some()
}

/// File `site` in this thread's lean table (its placement: the direct-mapped
/// slot, or under `two_way` the set's way [`lean_native_two_way_place`]
/// picks).
pub fn lean_native_site_file(site: LeanNativeSite, two_way: bool) {
    let info_addr = site.key;
    LEAN_NATIVE_SITES.with(|t| {
        // SAFETY: the table is thread-local and no reference into it is live
        // across this write (the door copies what it needs before any call).
        let table = unsafe { &mut *t.get() };
        let primary = lean_native_slot(info_addr);
        if !two_way {
            table[primary] = site;
            return;
        }
        // FUNNEL2-19-1 (lane funnel3): a record is identified by its site AND
        // its receiver class, so one site alternating between two receiver
        // classes keeps a record per class.
        let alternate = lean_native_alt_slot(primary);
        match lean_native_two_way_place(
            (table[primary].key, table[primary].receiver_class),
            (table[alternate].key, table[alternate].receiver_class),
            (site.key, site.receiver_class),
        ) {
            LeanPlace::Primary => table[primary] = site,
            LeanPlace::Alternate => table[alternate] = site,
            LeanPlace::Demote => {
                table[alternate] = table[primary];
                table[primary] = site;
            }
        }
    });
}

/// Interpreter round i3 wave 83, lane L3 (`CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE`,
/// `VmConfig::jit_reflect_door_lean_site`, default off; page
/// `interpreter-i81-L3-proposal-name-the-reflective-calls-gap-and-reentry-KEPT-OFF-20261019.md`,
/// "Progress (wave 83)"): file a compiled `Method.invoke` /
/// `Constructor.newInstance` site that the virtual-MIC helper's direct
/// reflective arm just served in the lean native table, as a plain callback
/// record for the receiver's exact class.
///
/// The host census put ~240 ns of a reflective call's way in at the helper
/// before the funnel (`gap_in_helper`): the body's prefix (the MH-constant
/// redirect, the boundary and SATB bookkeeping, the argument forwarding walk,
/// the dispatch-cache flush, the native site-cache probe that refuses the
/// `invoke` name, the `VarHandle` name test) and the forced native's by-name
/// memo, all re-run on every call to reach the same callback. The lean door
/// (`try_lean_native_door`) runs at the helper's first statement and serves
/// exactly this callback under its own guards: the VM, the JIT cache
/// generation (a recycled site address), the native registry generation (the
/// forced native is a registry answer, `native_override_for_cached_reflect_invoke`
/// memoizes on that generation), no class redefined, and the receiver's exact
/// class; it runs the call under the JIT-dispatch depth guard and the full
/// funnel (or the lean funnel under `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL`),
/// as for every other callback native. The record has no native id and no
/// capability gate, as the direct arm's `safe_native_call` has none.
#[inline(never)]
pub fn reflect_door_lean_site_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_addr: usize,
    receiver: ObjectRef,
    callback: cratonvm_native_api::NativeCallback,
) {
    if !matches!(info.invoke_kind, 0 | 2) || lean_native_site_diagnostics_on() {
        return;
    }
    let JitDispatchClass::Class(receiver_class) = jit_dispatch_class(vm, receiver) else {
        return;
    };
    let desc = NativeSiteDescriptor::decode(info.descriptor);
    let Some(params) = desc.params() else {
        return;
    };
    let mut tags = [0u8; INLINE_JIT_NATIVE_ARGS];
    tags[..params.len()].copy_from_slice(params);
    let natives = &vm.natives.native_methods;
    let site = LeanNativeSite {
        key: info_addr,
        vm_identity: vm.vm_identity,
        jit_gen: callee_verdict_retire_epoch(),
        registry_gen: natives.generation(),
        receiver_class: receiver_class.as_u32(),
        invoke_kind: info.invoke_kind,
        leaf: false,
        ret: desc.ret,
        nparams: params.len() as u8, // Cast: at most INLINE_JIT_NATIVE_ARGS
        params: tags,
        callback: Some(callback),
        native_id: None,
        capability_kind: None,
        kind: LeafNativeKind::Callback,
        fresh_result: false,    };
    lean_native_site_file(site, natives.jit_native_site_door_two_way());
}

/// FUNNEL2-19-2: the leaf kinds whose second argument word is a reference
/// (the `ChmGet` key, the `IntWrapperEquals` other, the `ReferenceRefersTo`
/// referent); the others take an `int` index or nothing.
pub fn lean_leaf_kind_takes_reference(kind: LeafNativeKind) -> bool {
    matches!(
        kind,
        LeafNativeKind::ChmGet
            | LeafNativeKind::IntWrapperEquals
            | LeafNativeKind::ReferenceRefersTo
            | LeafNativeKind::ClassIsInstance
    )
}

/// `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS`: count one lean-door event. Cold and
/// out of line; the door asks the switch first (one field load).
#[cold]
#[inline(never)]
pub fn lean_door_census(vm: &SharedVm, event: cratonvm_native_api::NativeSiteDoorEvent, add: u64) {
    vm.natives.native_methods.note_native_site_door(event, add);
}

/// The lean door's census on a decline: counted only while the switch is on.
#[inline]
pub unsafe fn lean_door_decline(vm_ptr: i64, event: cratonvm_native_api::NativeSiteDoorEvent) {
    if vm_ptr == 0 {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if vm.natives.native_methods.dbg_native_site_door_census() {
        lean_door_census(vm, event, 1);
    }
}

/// The per-call guards of a lean record: the VM, the JIT cache generation
/// that retires an address, the native registry generation, and "no class was
/// ever redefined".
///
/// SAFETY: `slot` points into this thread's lean table.
#[inline(always)]
pub unsafe fn lean_native_guards_hold(slot: *const LeanNativeSite, vm: &SharedVm) -> bool {
    (*slot).vm_identity == vm.vm_identity
        && (*slot).jit_gen == callee_verdict_retire_epoch()
        && (*slot).registry_gen == vm.natives.native_methods.generation()
        && !crate::classloading::any_class_redefined()
}

/// The lean door: serve a compiled call to a plain callback native from the
/// thread-local table, or answer `None` and let the ordinary route run.
///
/// `from_mic` is the `invokevirtual`/`invokeinterface` helper's entry, which
/// runs a non-leaf native under the JIT-dispatch depth guard (a native that
/// re-enters Java must not outrun the native stack).
///
/// SAFETY: `vm_ptr`, `info_ptr`, `args_ptr` and `num_args` are the extern
/// helper's own arguments (a live `SharedVm`, a live `JitInvokeInfo`, the
/// caller's argument staging block).
#[inline]
pub unsafe fn try_lean_native_door(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    from_mic: bool,
) -> Option<i64> {
    use cratonvm_native_api::NativeSiteDoorEvent as Ev;
    let info_addr = info_ptr as usize;
    let base: *const LeanNativeSite = LEAN_NATIVE_SITES.with(|t| t.get() as *const LeanNativeSite);
    let mut index = lean_native_slot(info_addr);
    // FUNNEL2-19-1 (lane funnel3): the record was found in the set's other way.
    let mut second_way = false;
    if (*base.add(index)).key != info_addr {
        // FUNNEL2-19-1 (`CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): ask the
        // set's other way. One field load on this (slow-route) miss path
        // while the switch is off.
        if vm_ptr != 0 {
            let vm = &*(vm_ptr as *const SharedVm);
            let alternate = lean_native_alt_slot(index);
            if vm.natives.native_methods.jit_native_site_door_two_way()
                && (*base.add(alternate)).key == info_addr
            {
                index = alternate;
                second_way = true;
            }
        }
        if !second_way {
            // JIT round 19 wave 2 (lane funnel2): the census separates an
            // empty slot from one another site holds (direct-mapped eviction),
            // which is what decides whether a per-site record would serve more
            // crossings (FUNNEL17-5).
            if vm_ptr != 0 {
                let vm = &*(vm_ptr as *const SharedVm);
                if vm.natives.native_methods.dbg_native_site_door_census() {
                    lean_door_census(vm, Ev::Probe, 1);
                    let ev = if (*base.add(index)).key == 0 {
                        Ev::MissEmpty
                    } else {
                        Ev::MissOtherSite
                    };
                    lean_door_census(vm, ev, 1);
                }
            }
            return None;
        }
    }
    let mut slot: *const LeanNativeSite = base.add(index);
    let vm = &*(vm_ptr as *const SharedVm);
    // `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS` (JIT round 19 wave 2, lane
    // funnel2): one field load when off.
    let census_t0 = vm
        .natives
        .native_methods
        .dbg_native_site_door_census()
        .then(|| {
            lean_door_census(vm, Ev::Probe, 1);
            mic_prof::now()
        });
    if !lean_native_guards_hold(slot, vm) {
        lean_door_decline(vm_ptr, Ev::DeclineGuard);
        return None;
    }
    // The site's shape. Both ways of a set that hold this key are this site's
    // records (FUNNEL2-19-1), so they agree on it.
    let invoke_kind = (*slot).invoke_kind;
    let nparams = (*slot).nparams as usize;

    let has_receiver = invoke_kind != 3;
    if num_args < 0 || (num_args > 0 && args_ptr == 0) {
        lean_door_decline(vm_ptr, Ev::DeclineShape);
        return None;
    }
    let args: &[i64] = if num_args == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };
    if args.len() != nparams + usize::from(has_receiver) {
        lean_door_decline(vm_ptr, Ev::DeclineShape);
        return None;
    }
    // GPU offload routes a static call through the kernel table first.
    #[cfg(feature = "gpu-offload")]
    if invoke_kind == 3 && cratonvm_jit::offload_hook::any_kernels() {
        return None;
    }

    // JIT round 19 wave 2 (lane funnel2; FUNNEL17-5 residual):
    // `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL=1` sends a non-leaf hit
    // through the lean funnel (`safe_native_call_jit_lean`), which does not
    // re-forward its arguments. The dispatch helpers treat the SATB flush as a
    // possible GC point ("can participate in a moving collection",
    // `jit_invoke_dispatch_body`), so on that route the flush runs HERE,
    // before any argument word is read. After it nothing up to the call is a
    // GC point (TLS reads, the capability gate, the depth guard), every
    // reference is validated below, and this call made no resolution: the
    // lean funnel's precondition, the argument `try_jit_site_cached_native_dispatch`
    // makes for its `warm` entries. The record is never a sync,
    // signature-polymorphic or `VarHandle`-armed entry
    // (`lean_native_site_fill`); a capability-gated one keeps the full funnel,
    // as in the site-cache door.
    //
    // Decided on the record found first. When the receiver selects the set's
    // other way (FUNNEL2-19-1) the lean funnel is used there only if this
    // flush already ran.
    let flushed_for_lean = !(*slot).leaf
        && (*slot).capability_kind.is_none()
        && (*slot).kind == LeafNativeKind::Callback
        && vm.natives.native_methods.jit_native_site_door_lean_funnel();
    if flushed_for_lean {
        vm.mem.heap.flush_thread_satb();
    }

    let mut receiver = None;
    if has_receiver {
        let raw = args[0] as u64;
        if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            lean_door_decline(vm_ptr, Ev::DeclineReceiver);
            return None;
        }
        note_membership_walk(1);
        let Some(obj) = vm.mem.heap.is_object_address(raw as usize) else {
            lean_door_decline(vm_ptr, Ev::DeclineReceiver);
            return None;
        };
        if invoke_kind != 1 {
            let cid = match jit_dispatch_class_validated(vm, obj) {
                JitDispatchClass::Class(cid) => cid.as_u32(),
                _ => {
                    lean_door_decline(vm_ptr, Ev::DeclineReceiver);
                    return None;
                }
            };
            if (*slot).receiver_class != cid {
                // FUNNEL2-19-1 (lane funnel3): a site alternating between two
                // receiver classes keeps one record per class, in the two
                // ways of its set. One field load while the switch is off.
                let other = base.add(lean_native_alt_slot(index));
                if vm.natives.native_methods.jit_native_site_door_two_way()
                    && (*other).key == info_addr
                    && (*other).receiver_class == cid
                    && lean_native_guards_hold(other, vm)
                {
                    slot = other;
                    second_way = !second_way;
                } else {
                    lean_door_decline(vm_ptr, Ev::DeclineReceiver);
                    return None;
                }
            }
        }
        receiver = Some(obj);
    }
    // Everything the call needs, read out before anything runs (a nested
    // dispatch may refill this slot).
    let callback = (*slot).callback?;
    let leaf = (*slot).leaf;
    let ret = (*slot).ret;
    let params = (*slot).params;
    let native_id = (*slot).native_id;
    let capability_kind = (*slot).capability_kind;
    let kind = (*slot).kind;
    let fresh_result = (*slot).fresh_result;
    let lean_funnel = flushed_for_lean && !leaf && capability_kind.is_none();
    if second_way && census_t0.is_some() {
        lean_door_census(vm, Ev::HitSecondWay, 1);
    }

    // From here on the call is served: nothing below may answer `None`.
    let (thread, _guard) = jit_thread_mut()?;

    // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-2): a filed leaf kind
    // runs the site-cache door's no-wrapper probe first. Its record exists
    // only under `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS`, is a
    // virtual/interface site (the receiver guard held above), never sync,
    // poly or capability-gated (`lean_native_site_fill`). The probe gets the
    // words Route S hands it: the receiver and the one reference argument
    // forwarded (the site-cache route's `forward_jit_reference_args`), here
    // with the forwarding read of a base this door validated.
    if kind != LeafNativeKind::Callback {
        if let Some(obj) = receiver {
            let recv = vm.mem.heap.load_and_forward_validated(obj);
            let mut staged = [recv.as_ptr() as i64, 0, 0];
            // FUNNEL3-19-2 (JIT round 19 wave 6, lane mega6;
            // `CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS`): the argument this door
            // just validated and forwarded, handed to the probe so it does not
            // walk the word again. `None` with the switch off.
            let mut validated_arg = None;
            if let Some(&raw) = args.get(1) {
                staged[1] = raw;
                let bits = raw as u64;
                if lean_leaf_kind_takes_reference(kind)
                    && bits != 0
                    && (bits & 0x7) == 0
                    && bits < (1u64 << 48)
                {
                    note_membership_walk(2);
                    if let Some(o) = vm.mem.heap.is_object_address(bits as usize) {
                        let forwarded = vm.mem.heap.load_and_forward_validated(o);
                        staged[1] = forwarded.as_ptr() as i64;
                        if vm.natives.native_methods.jit_leaf_probe_validated_args() {
                            validated_arg = Some(forwarded);
                        }
                    }
                }
            }
            if let Some(&raw) = args.get(2) {
                staged[2] = raw;
                let bits = raw as u64;
                if lean_leaf_kind_takes_ref_arg2(kind)
                    && bits != 0
                    && (bits & 0x7) == 0
                    && bits < (1u64 << 48)
                {
                    note_membership_walk(3);
                    if let Some(o) = vm.mem.heap.is_object_address(bits as usize) {
                        let forwarded = vm.mem.heap.load_and_forward_validated(o);
                        staged[2] = forwarded.as_ptr() as i64;
                        if vm.natives.native_methods.jit_leaf_probe_validated_args() {
                            validated_arg = Some(forwarded);
                        }
                    }
                }
            }
            let staged = &staged[..args.len().min(3)];
            cratonvm_jit::jit_ic_quiescent_point();
            thread.native_pending_return = None;
            if let Some(bits) =
                try_jit_arraylist_leaf(vm, thread, kind, recv, staged, validated_arg)
            {
                if leaf {
                    note_leaf_native_hit();
                } else {
                    note_site_cached_native_hit();
                }
                if matches!(
                    kind,
                    LeafNativeKind::ArrayListGet
                        | LeafNativeKind::ArrayListSize
                        | LeafNativeKind::ChmGet
                ) {
                    JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.collection_leaf_hits));
                }
                count_jit_native_dispatch(vm, native_id);
                if census_t0.is_some() {
                    lean_door_census(vm, Ev::HitLeafKind, 1);
                }
                return Some(bits);
            }
            if census_t0.is_some() {
                lean_door_census(vm, Ev::LeafKindFallback, 1);
            }
        }
    }

    let mut decoded_values: JitDecodedArgs;
    let mut stack_buf: [Value; 8] = [Value::Int(0); 8];
    let total_values = nparams + usize::from(receiver.is_some());
    let values: &[Value] = if total_values == 0 {
        &[]
    } else if total_values <= 8 {
        let mut idx = 0usize;
        if let Some(obj) = receiver {
            stack_buf[0] = Value::Object(Some(obj));
            idx = 1;
        }
        let first = usize::from(has_receiver);
        for (i, &tag) in params[..nparams].iter().enumerate() {
            let raw = args[first + i];
            stack_buf[idx + i] = match tag {
                b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(raw as i32),
                b'J' => Value::Long(raw),
                b'F' => Value::Float(f32::from_bits(raw as u32)),
                b'D' => Value::Double(f64::from_bits(raw as u64)),
                b'L' | b'[' => {
                    let bits = raw as u64;
                    if bits == 0 {
                        Value::Object(None)
                    } else if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                        note_membership_walk(2);
                        Value::Object(vm.mem.heap.is_object_address(bits as usize))
                    } else {
                        Value::Object(None)
                    }
                }
                _ => Value::Int(raw as i32),
            };
        }
        if lean_funnel {
            for value in &mut stack_buf[..total_values] {
                if let Value::Object(Some(object)) = value {
                    *object = vm.mem.heap.load_and_forward_validated(*object);
                }
            }
        }
        &stack_buf[..total_values]
    } else {
        decoded_values = JitDecodedArgs::new();
        if let Some(obj) = receiver {
            decoded_values.push(Value::Object(Some(obj)));
        }
        let first = usize::from(has_receiver);
        for (i, &tag) in params[..nparams].iter().enumerate() {
            let raw = args[first + i];
            decoded_values.push(match tag {
                b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(raw as i32),
                b'J' => Value::Long(raw),
                b'F' => Value::Float(f32::from_bits(raw as u32)),
                b'D' => Value::Double(f64::from_bits(raw as u64)),
                b'L' | b'[' => {
                    let bits = raw as u64;
                    if bits == 0 {
                        Value::Object(None)
                    } else if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                        note_membership_walk(2);
                        Value::Object(vm.mem.heap.is_object_address(bits as usize))
                    } else {
                        Value::Object(None)
                    }
                }
                _ => Value::Int(raw as i32),
            });
        }
        if lean_funnel {
            for value in decoded_values.iter_mut() {
                if let Value::Object(Some(object)) = value {
                    *object = vm.mem.heap.load_and_forward_validated(*object);
                }
            }
        }
        &decoded_values
    };

    if let Some(kind) = capability_kind {
        if crate::vm::check_native_dispatch_capability_for_slot(vm, kind, native_id).is_err() {
            lean_door_decline(vm_ptr, Ev::DeclineCapability);
            return None;
        }
    }
    // The extern helper's own entry/exit roots, done here: the previous
    // native's returned object stops being a root, and this call's result is
    // compiled code's the moment it returns.
    cratonvm_jit::jit_ic_quiescent_point();
    thread.native_pending_return = None;
    let _depth_guard = if from_mic && !leaf {
        match enter_jit_dispatch(vm, 0) {
            Ok(g) => Some(g),
            Err(sentinel) => return Some(sentinel),
        }
    } else {
        None
    };
    count_jit_native_dispatch(vm, native_id);
    let census_t1 = census_t0.map(|_| mic_prof::now());
    let called = if leaf {
        note_leaf_native_hit();
        crate::vm::safe_native_call_leaf_prevalidated(vm, thread, callback, values)
    } else {
        note_site_cached_native_hit();
        if fresh_result {
            // FUNNEL2-19-3 (lane funnel3; `CRATONVM_JIT_NATIVE_FRESH_RESULT`):
            // the same funnel (lean or full, as below) with the result healed
            // by the forwarding read alone.
            if census_t0.is_some() {
                lean_door_census(vm, Ev::FreshResult, 1);
            }
            if !lean_funnel {
                vm.mem.heap.flush_thread_satb();
            }
            crate::vm::safe_native_call_jit_fresh_result(vm, thread, callback, values, lean_funnel)
        } else if lean_funnel {
            // The SATB flush ran before the decode (above).
            crate::vm::safe_native_call_jit_lean(vm, thread, callback, values)
        } else {
            vm.mem.heap.flush_thread_satb();
            crate::vm::safe_native_call_prevalidated_objects(vm, thread, callback, values)
        }
    };
    let census_t2 = census_t0.map(|_| mic_prof::now());
    let value = match called {
        Ok(value) => crate::vm::coerce_native_return_typed(value, ret),
        Err(error) => {
            let info = &*(info_ptr as *const JitInvokeInfo);
            return Some(handle_jit_dispatch_error(vm, thread, error, info));
        }
    };
    thread.native_pending_return = None;
    let bits = match value {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    };
    // A throwing native's crossing is not timed (the split census's rule).
    if let (Some(t0), Some(t1), Some(t2)) = (census_t0, census_t1, census_t2) {
        let hit = if leaf {
            Ev::HitLeaf
        } else if lean_funnel {
            Ev::HitLeanFunnel
        } else {
            Ev::HitFunnel
        };
        lean_door_census(vm, hit, 1);
        lean_door_census(vm, Ev::CyclesFront, t1.wrapping_sub(t0));
        lean_door_census(vm, Ev::CyclesCall, t2.wrapping_sub(t1));
        lean_door_census(vm, Ev::CyclesTail, mic_prof::now().wrapping_sub(t2));
    }
    Some(bits)
}


/// Site-cached `ArrayList.get(int)` / `size()` hits answered by
/// `cratonvm_native_collections::jit_arraylist_get` / `jit_arraylist_size`
/// without the native-call wrapper ([`LeafNativeKind::ArrayListGet`]).
///
/// Zero beside a non-zero site-cached/leaf hit count on an ArrayList-heavy run
/// means the classification never matched (e.g. the registration moved to a
/// different body) or every receiver declined — which timings cannot tell
/// from "no faster". Counted per thread.
pub fn jit_collection_leaf_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.collection_leaf_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// `VarHandle` instance-field CAS operations served inside the funnel, and the
/// ones that declined to the generic native.
///
/// The pair is the point, exactly as it is for the write direct helpers: a
/// zero HIT count beside a large DECLINE count says the fast path is reached
/// and refuses every handle, which looks identical to "no CAS happened" if
/// only one of the two is printed.
///
/// Counted per thread (r9-vmside) — see [`JitCounterBlock::vh_direct_served`].
///
/// `(served, declined)` counts for the in-funnel `VarHandle` CAS fast path.
pub fn varhandle_field_cas_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        jit_counter_total(|b| b.vh_field_cas_served.load(Relaxed)),
        jit_counter_total(|b| b.vh_field_cas_declined.load(Relaxed)),
    )
}

/// `CRATONVM_JIT_VARHANDLE_CAS_FUNNEL_FAST=0` -- send every `VarHandle` CAS
/// back through the generic native, as before this fast path existed. Default
/// ON. Same role as the read and write bind switches: it is what makes an A/B
/// on ONE binary possible, which is the only kind that means anything when the
/// host's load drifts between runs.
///
/// JIT round 25 review: the default-ON opt-out rule of
/// [`cratonvm_types::flags::runtime_flag_default_on`] (`0` / `false` / `off` /
/// `no` kill); it was a hand-rolled `Ok("0")` match under which `=false` and
/// `=off` left the fast path on, as `varhandle_array_element_fast_from_flags`
/// did.
pub fn varhandle_cas_funnel_fast_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}
/// `VarHandle` instance-field READS served as a direct field load, reported by
/// `CRATONVM_DBG=intrinsic-stats`. Zero here with a non-zero site-cached count
/// means every VarHandle site refused the plan — which is a different problem
/// from "no VarHandle site was reached", and only a counter separates them.
/// Counted per thread (r9-vmside).
pub fn varhandle_field_read_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.vh_field_read_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// Serve a signature-polymorphic `VarHandle` READ mode as what it actually is
/// — one field load — instead of a native call.
///
/// The funnel route for `VH.get(receiver)` is: build a `NativeContextImpl`,
/// record a thread transition, run `varhandle_get`'s access-shape cascade,
/// **allocate a wrapper object** for the erased `Object` return, then unbox it
/// again against the call site's descriptor and throw the wrapper away. That
/// round trip is ~40% of a `VarHandle.get`, and netty pays it twice per
/// `ByteBuf.writeByte` through `ensureAccessible()` -> `RefCnt.isLiveNonVolatile`.
///
/// # What it refuses, and why each refusal matters
///
/// * **Anything but a read mode.** `set`/CAS/`getAndAdd` mutate, and the
///   write-side semantics (`safeConstructPutInt`, the ordered stores) live in
///   the native.
/// * **Any handle the side table does not describe as a resolved instance
///   field** — `varhandle_instance_field_plan_for_handle` answers `None` for static,
///   array-element, byte-array/ByteBuffer-view and `SegmentVarHandle`
///   handles, and for one whose slot is still unresolved (the native resolves
///   and memoises it, so the second call qualifies).
/// * **A receiver that is not an instance of the handle's class** — the native
///   raises `ClassCastException`; the slot index would name another layout.
/// * **A boxed primitive reaching a reference return, or the reverse.** That
///   is precisely the shape `unbox_poly_return_checked`'s W6-1 rule turns
///   into a `WrongMethodTypeException`; declining hands it back to the funnel
///   rather than reimplementing the rule here.
///
/// Every refusal is a fall-through to the existing dispatch, so the worst
/// case is the cost that was already being paid.
/// `VarHandle.compareAndSet` on an ordinary instance field, served from inside
/// the native funnel.
///
/// The twin of [`try_varhandle_instance_field_read`], and it exists because the
/// two access directions were not symmetric. A funnelled READ is caught here
/// and costs ~49 ns; a funnelled CAS ran the whole `varhandle_compare_and_set`
/// native — its segment-handle probe, its FFM-layout probe, its byte-view probe
/// and its array probe — before ever reaching the instance-field case, and cost
/// ~182 ns more than a bound call. Measured with the read and write kill
/// switches on `HibfixVarHandleProbe`.
///
/// Serving it here rather than only from a thin direct call is deliberate: this
/// route also covers the sites the JIT declines and every interpreter dispatch,
/// which a compile-time bind cannot reach.
///
/// Declines to the generic native for anything it cannot prove: a coordinate
/// count that is not `[VarHandle, receiver, expected, new]`, a handle the side
/// table does not describe as a resolved instance field, or a call site whose
/// declared operand types disagree with the variable's own kind. The native
/// then applies the `VarHandle` access-mode rules, as it does today.
///
/// SAFETY: the raw words come from a live compiled frame; every reference among
/// them is heap-validated before any dereference.
pub unsafe fn try_varhandle_instance_field_cas(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
) -> Option<i64> {
    if info.method_name != "compareAndSet" {
        return None;
    }
    // `[VarHandle, receiver, expected, new]` and nothing else. An array-element
    // or byte-view CAS carries an index and is not an instance-field CAS.
    if args_slice.len() != 4 {
        return None;
    }
    if info.return_type != b'Z' {
        return None;
    }
    let vh_raw = args_slice[0] as u64;
    let recv_raw = args_slice[1] as u64;
    if vh_raw == 0 || recv_raw == 0 {
        return None;
    }
    // The call site's declared operand types have to agree with the variable's
    // own kind, for the reason the read path checks its return type: the
    // registered descriptor is the erased `([Ljava/lang/Object;)Z`, so the
    // site's is the only statement of what the caller actually passed. A BOUND
    // site does not pay this parse — its slot is that statement, decided once
    // at bind time. On this route it is per call, and a native profile put it
    // at 10.3% of a reference CAS (`CharSearcher::next_match`) before the bind
    // existed to take the hot sites away from it. Still memoized per call
    // site below (`vh_cas_kinds_memoized`) for whatever declined/unbound
    // sites keep reaching this route -- a hot loop with no bind is exactly
    // the shape `HibfixComposeProbe2`'s own per-thread `VarHandle`s produce
    // whenever a chain outruns the bind's own warm-up.
    let (expected_desc, new_desc) = vh_cas_kinds_memoized(
        jit_site_key(vm.vm_identity, info as *const JitInvokeInfo as usize),
        info.descriptor,
    )?;
    if expected_desc != new_desc {
        return None;
    }
    // One implementation, shared with the thin direct helper, so what the two
    // consider a servable handle cannot drift.
    let swapped = varhandle_instance_field_cas_shared(
        vm,
        vh_raw,
        recv_raw,
        expected_desc,
        args_slice[2],
        args_slice[3],
    )?;
    Some(i64::from(swapped))
}

thread_local! {
    /// One-slot memo of [`varhandle_cas_operand_kinds`]: `(call site, the JIT
    /// cache generation it was filled at, answer)`.
    ///
    /// Keyed by the call site's [`JitSiteKey`] (its VM and its
    /// `JitInvokeInfo` address), with the generation beside it, and NOT by
    /// the descriptor's address alone, as it was until interpreter round i1
    /// wave 38 (lane L6). `JitInvokeInfo::descriptor` is only nominally
    /// `'static`: it points into a `Box<str>` the owning `CompiledMethod`
    /// keeps (`_jit_strings`) and is freed with it, so a later compilation
    /// could be handed the same address for another descriptor and read this
    /// site's operand kinds. The `JitInvokeInfo` address can be re-issued the
    /// same way, which is why every other site-keyed memo is flushed on a
    /// generation change ([`site_keyed_memos!`]); the drop of a
    /// `CompiledMethod` that owns `JitInvokeInfo`s moves
    /// `cratonvm_jit::jit_cache_generation`, so an entry stamped with an older
    /// generation is never believed. One slot and a stamp rather than a
    /// `site_keyed_memos!` map: a hit stays two compares and one load, the
    /// cost of the pointer compare it replaces plus the load.
    ///
    /// A miss (another site, the first call, or any JIT publication since)
    /// falls back to the uncached parse and replaces the slot; it is never
    /// wrong, only sometimes not yet warm. `vm_identity` 0 is never a real VM,
    /// so the initial value never matches.
    pub static VH_CAS_KINDS_MEMO: Cell<(JitSiteKey, u64, Option<(u8, u8)>)> =
        const { Cell::new(((0, 0), 0, None)) };
}

/// [`varhandle_cas_operand_kinds`] for the call site `site`, memoized per
/// thread. See [`VH_CAS_KINDS_MEMO`] for the key.
pub fn vh_cas_kinds_memoized(site: JitSiteKey, descriptor: &str) -> Option<(u8, u8)> {
    let generation = cratonvm_jit::jit_cache_generation();
    let (key, stamp, kinds) = VH_CAS_KINDS_MEMO.with(Cell::get);
    if key == site && stamp == generation {
        return kinds;
    }
    let kinds = varhandle_cas_operand_kinds(descriptor);
    VH_CAS_KINDS_MEMO.with(|c| c.set((site, generation, kinds)));
    kinds
}

/// The last two parameter kinds of a `compareAndSet` call site, collapsed the
/// way [`cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle`]
/// collapses a field descriptor: any reference is `b'L'`.
///
/// `None` unless the descriptor is exactly `(receiver, expected, new)` — three
/// parameters, the first a reference.
pub fn varhandle_cas_operand_kinds(descriptor: &str) -> Option<(u8, u8)> {
    // The CLOSING `)`, walked, not the first one (a class name may hold `)`).
    let after_open = descriptor.strip_prefix('(')?;
    let close = cratonvm_jit_api::descriptor_param_list_end(descriptor)?;
    let params = after_open.get(..close.checked_sub(1)?)?;
    let mut kinds = [0u8; 3];
    let mut n = 0usize;
    let bytes = params.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if n == kinds.len() {
            return None;
        }
        match bytes[i] {
            b'L' => {
                let end = params[i..].find(';')? + i;
                kinds[n] = b'L';
                i = end + 1;
            }
            b'[' => {
                let mut j = i;
                while j < bytes.len() && bytes[j] == b'[' {
                    j += 1;
                }
                if j >= bytes.len() {
                    return None;
                }
                i = if bytes[j] == b'L' {
                    params[j..].find(';')? + j + 1
                } else {
                    j + 1
                };
                kinds[n] = b'L';
            }
            c @ (b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D') => {
                kinds[n] = c;
                i += 1;
            }
            _ => return None,
        }
        n += 1;
    }
    if n != 3 || kinds[0] != b'L' {
        return None;
    }
    Some((kinds[1], kinds[2]))
}

/// Decode one raw JIT-ABI operand word against the variable's kind.
///
/// The inverse of the read path's encode, and the same table: a reference is
/// its raw address (zero for `null`), a `float`/`double` travels as its bit
/// pattern, and the whole int category as the low word. A non-zero reference
/// that is not a heap address declines rather than being dereferenced.
pub fn varhandle_operand_value(vm: &SharedVm, raw: i64, value_desc: u8) -> Option<Value> {
    Some(match value_desc {
        b'L' => {
            if raw == 0 {
                Value::Object(None)
            } else {
                // `is_object_address` is a safe fn: it is the validation,
                // not something that assumes it.
                Value::Object(Some(vm.mem.heap.is_object_address(raw as usize)?))
            }
        }
        b'J' => Value::Long(raw),
        b'F' => Value::Float(f32::from_bits(raw as u32)), // Cast: JIT ABI -- float bits
        b'D' => Value::Double(f64::from_bits(raw as u64)), // Cast: JIT ABI -- double bits
        b'Z' | b'B' | b'C' | b'S' | b'I' => Value::Int(raw as i32), // Cast: JIT ABI -- low word
        _ => return None,
    })
}

/// May a `VarHandle` fast path apply `plan`'s slot to a receiver of class
/// `recv_class_id`?
///
/// The native (`vh_instance_refusal`) raises `ClassCastException` for a
/// receiver that is not an instance of the handle's class; applying
/// `field_index` to one reads or writes a slot of an unrelated layout. The
/// exact class is one integer compare; a subclass (CompletableFuture's
/// `Completion.NEXT` sees nothing else) goes through the positive-memoised
/// subtype walk. `false` only ever declines to the native, which decides.
pub fn varhandle_plan_admits_receiver(
    vm: &SharedVm,
    plan: &cratonvm_native_builtins::lang_invoke::VarHandleInstanceFieldPlan,
    recv_class_id: u32,
) -> bool {
    if plan.class_id == 0 {
        return false;
    }
    if recv_class_id == plan.class_id {
        return true;
    }
    // nio21 wave 14 L4: a subclass receiver the constant-handle prefix
    // declined for want of a display gets one (lock-free when present).
    vh_const_ensure_display(vm, recv_class_id);
    jit_is_subclass_of_cached(vm, ClassId::new(recv_class_id), ClassId::new(plan.class_id))
}

/// Would the native store `value` through `plan` without a `ClassCastException`?
///
/// `true` for a primitive (the call site's kind already agrees with the
/// variable's) and for `null`; for a reference field, `true` when the native
/// judges nothing (`ref_value_unchecked`, an `Object` field), when a
/// one-dimensional primitive array field receives exactly that array
/// (`ref_array_element`), when a one-dimensional reference array field
/// receives a reference array whose component is its own component or a
/// subtype of it (`ref_array_component_class_id`),
/// or when the value's class is the declared type or a
/// subtype of it — the first clauses of `reference_arg_admitted`, which then
/// answers "admitted" too. Anything else declines, so the native's own rule
/// (including its by-name fallback and its array-shape rule) decides.
pub fn varhandle_plan_admits_value(
    vm: &SharedVm,
    plan: &cratonvm_native_builtins::lang_invoke::VarHandleInstanceFieldPlan,
    value: &Value,
) -> bool {
    let Value::Object(Some(obj)) = value else {
        return true;
    };
    if plan.ref_value_unchecked {
        return true;
    }
    // A one-dimensional primitive array field admits exactly that array: the
    // native's `vh_array_field_value_admitted` rule, settled by the header's
    // element tag. Any other array-typed field has `ref_type_class_id == 0`
    // and declines below, so the native decides.
    if let Some(want) = plan.ref_array_element {
        return vm
            .mem
            .heap
            .is_object_address(obj.as_ptr() as usize)
            .is_some_and(|obj| vm.mem.heap.array_element_type(obj) == Some(want));
    }
    // A one-dimensional reference-array field (rt proposals W8-2, W9-2) admits
    // a reference array whose header names the field's component or a subtype
    // of it: `Sub[]` IS a `Base[]` (JLS 4.10.3), which `checkcast` (the
    // native's judge, `array_value_casts_to_desc`, loader-blind)
    // always admits. The header's class id of a reference array is its
    // component, so the element tag is what keeps a plain instance of the
    // component class (`String` into a `String[]` field) out. The subtype
    // question is the exact ClassId relation (`is_subclass_of`, interfaces
    // included) the plain reference-field clause below already trusts, so the
    // fast path stays a subset of what the native admits.
    if plan.ref_array_component_class_id != 0 {
        let Some(arr) = vm.mem.heap.is_object_address(obj.as_ptr() as usize) else {
            return false;
        };
        if vm.mem.heap.array_element_type(arr) != Some(ArrayElementType::Reference) {
            return false;
        }
        let component = vm.mem.heap.class_id_of(arr);
        return component.as_u32() == plan.ref_array_component_class_id
            || jit_is_subclass_of_cached(
                vm,
                component,
                ClassId::new(plan.ref_array_component_class_id),
            );
    }
    if plan.ref_type_class_id == 0 {
        return false;
    }
    // Round 11 wave 13 (lane `icwide`): an ARRAY value declines, so the
    // native decides. Its header carries its COMPONENT's id, so an `Integer[]`
    // passed the `Integer`-field test and was stored into an `Integer` field
    // (`jit_dispatch_class`); the array-typed fields are the two clauses above.
    let Some(value_cid) = jit_dispatch_class(vm, *obj).class_id() else {
        return false;
    };
    if value_cid.as_u32() == plan.ref_type_class_id {
        return true;
    }
    // nio21 wave 14 L4: as the receiver arm, for a value's class.
    vh_const_ensure_display(vm, value_cid.as_u32());
    jit_is_subclass_of_cached(vm, value_cid, ClassId::new(plan.ref_type_class_id))
}

pub const VH_BYTE_VIEW_PROOF_SLOTS: usize = 8;

#[derive(Clone, Copy)]
pub struct VarHandleByteArrayViewProof {
    pub vm: usize,
    pub vh: u64,
    pub array: u64,
    pub heap_epoch: u64,
    pub metadata_generation: u64,
    pub value_desc: u8,
    pub little_endian: bool,
}

#[derive(Clone, Copy)]
pub struct VarHandleByteBufferViewProof {
    pub vm: usize,
    pub vh: u64,
    pub buffer: u64,
    pub heap_epoch: u64,
    pub metadata_generation: u64,
    pub origin_epoch: u64,
    pub layout_epoch: u64,
    pub buffer_class: u32,
    pub value_desc: u8,
    pub little_endian: bool,
    pub hb_off: usize,
    pub offset_off: usize,
    pub limit_off: usize,
    pub address_off: usize,
    pub segment_off: usize,
    pub read_only_off: usize,
    pub last_hb: u64,
}

#[inline(always)]
pub fn vh_byte_view_proof_slot(vh: u64, target: u64, value_desc: u8) -> usize {
    (((vh as usize) >> 4).wrapping_mul(31) ^ ((target as usize) >> 4) ^ usize::from(value_desc))
        & (VH_BYTE_VIEW_PROOF_SLOTS - 1)
}

/// Read or write a 2-, 4-, or 8-byte unaligned primitive at `pointer` with
/// the view's byte order, returning the encoded JIT-ABI word (`0` for a write).
// SAFETY: `pointer` must be valid for `width` bytes of plain memory with no concurrent GC movement.
#[inline(always)]
pub unsafe fn byte_view_unaligned_access(
    pointer: *mut u8,
    width: usize,
    value_desc: u8,
    little_endian: bool,
    value: Option<i64>,
) -> i64 {
    if let Some(value) = value {
        match width {
            2 => std::ptr::write_unaligned(
                pointer.cast::<u16>(),
                if little_endian {
                    (value as u16).to_le()
                } else {
                    (value as u16).to_be()
                },
            ),
            4 => std::ptr::write_unaligned(
                pointer.cast::<u32>(),
                if little_endian {
                    (value as u32).to_le()
                } else {
                    (value as u32).to_be()
                },
            ),
            8 => std::ptr::write_unaligned(
                pointer.cast::<u64>(),
                if little_endian {
                    (value as u64).to_le()
                } else {
                    (value as u64).to_be()
                },
            ),
            _ => return 0,
        }
        return 0;
    }
    let bits = match width {
        2 => {
            let word = std::ptr::read_unaligned(pointer.cast::<u16>());
            (if little_endian {
                u16::from_le(word)
            } else {
                u16::from_be(word)
            }) as u64
        }
        4 => {
            let word = std::ptr::read_unaligned(pointer.cast::<u32>());
            (if little_endian {
                u32::from_le(word)
            } else {
                u32::from_be(word)
            }) as u64
        }
        8 => {
            let word = std::ptr::read_unaligned(pointer.cast::<u64>());
            if little_endian {
                u64::from_le(word)
            } else {
                u64::from_be(word)
            }
        }
        _ => return 0,
    };
    match value_desc {
        b'S' => bits as u16 as i16 as i64,
        b'C' => bits as u16 as i64,
        b'I' | b'F' => bits as u32 as i32 as i64,
        _ => bits as i64,
    }
}

/// One unaligned plain byte[] view access, without allocation or safepoints.
/// The weak metadata identity and both heap bases are validated before payload
/// access; fragmented arrays decline to the authoritative native implementation.
// SAFETY: `vh_raw` and `array_raw` are raw reference words from a live compiled frame; both are heap-validated.
pub unsafe fn varhandle_byte_array_view_bits(
    vm: &SharedVm,
    vh_raw: u64,
    array_raw: u64,
    index: i64,
    value_desc: u8,
    value: Option<i64>,
) -> Option<i64> {
    if vh_raw == 0 || array_raw == 0 || index < 0 {
        return None;
    }
    let width = match value_desc {
        b'S' | b'C' => 2,
        b'I' | b'F' => 4,
        b'J' | b'D' => 8,
        _ => return None,
    };
    let index = usize::try_from(index).ok()?;
    let end = index.checked_add(width)?;
    let heap_epoch = vm.mem.address_epoch();
    let metadata_generation =
        cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation();
    let slot = vh_byte_view_proof_slot(vh_raw, array_raw, value_desc);
    if let Some(proof) = VH_BYTE_ARRAY_VIEW_PROOFS.with(|proofs| proofs[slot].get()) {
        if proof.vm == vm.vm_identity
            && proof.vh == vh_raw
            && proof.array == array_raw
            && proof.value_desc == value_desc
            && proof.heap_epoch == heap_epoch
            && proof.metadata_generation == metadata_generation
        {
            let heap = &vm.mem.heap;
            let array =
                heap.load_and_forward(ObjectRef::from_raw(array_raw as usize as *mut u8));
            if heap.kind_of_validated(array) == cratonvm_types::ObjectKind::Array
                && heap.element_type_of_validated(array) == ArrayElementType::Byte
                && end <= heap.array_length(array)
            {
                if let Some(base) = heap.array_data_ptr(array) {
                    return Some(byte_view_unaligned_access(
                        base.add(index),
                        width,
                        value_desc,
                        proof.little_endian,
                        value,
                    ));
                }
            }
        }
    }
    let (vh_class, key) = direct_receiver_facts(vm, vh_raw as usize)?;
    if vh_class == 0 {
        return None;
    }
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_byte_array_view_plan_for_handle(
        vm.vm_identity,
        key,
        vh_raw as usize,
    )?;
    if plan.value_desc != value_desc {
        return None;
    }
    let heap = &vm.mem.heap;
    let (array, live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(array_raw as usize as *mut u8));
    if !live
        || heap.kind_of_validated(array) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(array) != ArrayElementType::Byte
    {
        return None;
    }
    let length = heap.array_length(array);
    if end > length {
        return None;
    }
    let pointer = heap.array_data_ptr(array)?.add(index);
    if heap_epoch == vm.mem.address_epoch()
        && metadata_generation
            == cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation()
    {
        VH_BYTE_ARRAY_VIEW_PROOFS.with(|proofs| {
            proofs[slot].set(Some(VarHandleByteArrayViewProof {
                vm: vm.vm_identity,
                vh: vh_raw,
                array: array_raw,
                heap_epoch,
                metadata_generation,
                value_desc,
                little_endian: plan.little_endian,
            }));
        });
    }
    // No poll, allocation, or Java call can move the validated array between
    // the pointer acquisition and this access. Plain get/set permit misalignment.
    Some(byte_view_unaligned_access(
        pointer,
        width,
        value_desc,
        plan.little_endian,
        value,
    ))
}

// Only pure layout data is cached. Mutable limit, offset, address, backing
// array and segment fields are read again on every access.
#[derive(Clone, Copy)]
pub struct ByteBufferViewReadLayout {
    pub vm: usize,
    pub origin_epoch: u64,
    pub layout_epoch: u64,
    pub buffer_class: u32,
    pub hb: usize,
    pub offset: usize,
    pub limit: usize,
    pub address: usize,
    pub segment: usize,
    pub read_only: usize,
}
thread_local! {
    pub static BYTE_BUFFER_VIEW_READ_LAYOUT: std::cell::Cell<Option<ByteBufferViewReadLayout>> =
        const { std::cell::Cell::new(None) };
    pub static BYTE_BUFFER_VIEW_REPORTED_VMS: std::cell::Cell<(usize, usize, usize)> =
        const { std::cell::Cell::new((0, 0, 0)) };
    pub static VH_BYTE_ARRAY_VIEW_PROOFS:
        [std::cell::Cell<Option<VarHandleByteArrayViewProof>>; VH_BYTE_VIEW_PROOF_SLOTS] =
        const { [const { std::cell::Cell::new(None) }; VH_BYTE_VIEW_PROOF_SLOTS] };
    pub static VH_BYTE_BUFFER_VIEW_PROOFS:
        [std::cell::Cell<Option<VarHandleByteBufferViewProof>>; VH_BYTE_VIEW_PROOF_SLOTS] =
        const { [const { std::cell::Cell::new(None) }; VH_BYTE_VIEW_PROOF_SLOTS] };
}

pub fn note_byte_buffer_view_compact_fields_reported(vm_identity: usize) {
    BYTE_BUFFER_VIEW_REPORTED_VMS.with(|reported| {
        let (r, w, c) = reported.get();
        if c != vm_identity {
            reported.set((r, w, vm_identity));
            eprintln!(
                "[cratonvm-jitc] byte-buffer-view-compact-fields served vm={}",
                vm_identity
            );
        }
    });
}

pub fn note_byte_buffer_view_access_reported(vm_identity: usize, write: bool) {
    BYTE_BUFFER_VIEW_REPORTED_VMS.with(|reported| {
        let (r, w, c) = reported.get();
        if write {
            if w != vm_identity {
                reported.set((r, vm_identity, c));
                eprintln!("[cratonvm-jitc] byte-buffer-view-write served vm={}", vm_identity);
            }
        } else if r != vm_identity {
            reported.set((vm_identity, w, c));
            eprintln!("[cratonvm-jitc] byte-buffer-view-read served vm={}", vm_identity);
        }
    });
}

pub fn byte_buffer_view_read_layout(vm: &SharedVm) -> Option<ByteBufferViewReadLayout> {
    let origin_epoch = cratonvm_classloading::class_origin_epoch();
    let layout_epoch = cratonvm_types::layout_generation();
    if let Some(plan) = BYTE_BUFFER_VIEW_READ_LAYOUT.with(std::cell::Cell::get) {
        if plan.vm == vm.vm_identity
            && plan.origin_epoch == origin_epoch
            && plan.layout_epoch == layout_epoch
        {
            return Some(plan);
        }
    }
    let cm = vm.classes.class_manager.read();
    let buffer = cm.find_bootstrap_class_by_name("java/nio/ByteBuffer")?;
    let base = cm.find_bootstrap_class_by_name("java/nio/Buffer")?;
    if !cm.get_class(buffer)?.origin.has_real_bytes()
        || !cm.get_class(base)?.origin.has_real_bytes()
    {
        return None;
    }
    // Resolve from the API's declaring classes, so a subclass's hidden field
    // cannot replace the field the genuine buffer implementation reads.
    let slot = |class, name, desc| {
        crate::vm::resolve_field_index_in_hierarchy_desc(class, name, Some(desc), &cm.class_store)
    };
    let plan = ByteBufferViewReadLayout {
        vm: vm.vm_identity,
        origin_epoch,
        layout_epoch,
        buffer_class: buffer.as_u32(),
        hb: slot(buffer, "hb", "[B")?,
        offset: slot(buffer, "offset", "I")?,
        limit: slot(base, "limit", "I")?,
        address: slot(base, "address", "J")?,
        segment: slot(base, "segment", "Ljava/lang/foreign/MemorySegment;")?,
        read_only: slot(buffer, "isReadOnly", "Z")?,
    };
    drop(cm);
    BYTE_BUFFER_VIEW_READ_LAYOUT.with(|cell| cell.set(Some(plan)));
    Some(plan)
}

/// Read a current field of an already checked/forwarded ByteBuffer. No Java
/// call or safepoint occurs in this helper. The canonical compact slot proof
/// validates the index, storage kind and current layout generation; reference
/// fields retain the same collector barrier as compiled getfield. Legacy or
/// mismatched layouts keep the ordinary typed heap accessor.
// SAFETY: `buffer` must be a live, checked/forwarded ByteBuffer instance.
pub unsafe fn checked_byte_buffer_view_field(
    vm: &SharedVm,
    buffer: ObjectRef,
    index: usize,
    descriptor: u8,
) -> Value {
    if let Ok(index_word) = i64::try_from(index) {
        if let Some((offset, storage)) = jit_compact_field_slot(buffer.as_ptr() as i64, index_word)
        {
            if Some(storage) == cratonvm_types::FieldStorageKind::from_descriptor_byte(descriptor) {
                let ptr = buffer.as_ptr().add(offset);
                let value = if storage.is_reference() {
                    let bits = jit_load_ref_slot(
                        ptr,
                        Some(&vm.mem.heap),
                        ref_load_census::GETFIELD_COMPACT_REF,
                        "byte-buffer-view/compact-ref",
                    );
                    Value::Object(
                        (bits != 0).then(|| ObjectRef::from_raw(bits as usize as *mut u8)),
                    )
                } else {
                    cratonvm_types::read_compact_field(
                        ptr,
                        storage,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                };
                if crate::runtime::env_cache::dbg_jitc() {
                    note_byte_buffer_view_compact_fields_reported(vm.vm_identity);
                }
                return value;
            }
        }
    }
    vm.mem.heap.get_field_as(buffer, index, descriptor)
}

/// Serve a plain `ByteBuffer` view read or write from a cached compact-field
/// proof when the handle, buffer, and layout epochs still hold and `segment`
/// is null. Mutable fields (`isReadOnly`, `segment`, `limit`, `hb`, `offset`,
/// `address`) are read fresh on every access.
// SAFETY: `vh_raw` and `buffer_raw` are raw reference words from a live compiled frame; epochs and headers are verified before field reads.
pub unsafe fn varhandle_cached_byte_buffer_view_bits(
    vm: &SharedVm,
    vh_raw: u64,
    buffer_raw: u64,
    index: i64,
    value_desc: u8,
    value: Option<i64>,
) -> Option<i64> {
    if vh_raw == 0 || buffer_raw == 0 {
        return None;
    }
    let width = match value_desc {
        b'S' | b'C' => 2usize,
        b'I' | b'F' => 4usize,
        b'J' | b'D' => 8usize,
        _ => return None,
    };
    let index = i32::try_from(index).ok()?;
    if index < 0 {
        return None;
    }
    let end_index = index.checked_add(width as i32)?;
    let slot = vh_byte_view_proof_slot(vh_raw, buffer_raw, value_desc);
    let mut proof = VH_BYTE_BUFFER_VIEW_PROOFS.with(|proofs| proofs[slot].get())?;
    let heap_epoch = vm.mem.address_epoch();
    let metadata_generation =
        cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation();
    let origin_epoch = cratonvm_classloading::class_origin_epoch();
    let layout_epoch = cratonvm_types::layout_generation();
    if proof.vm != vm.vm_identity
        || proof.vh != vh_raw
        || proof.buffer != buffer_raw
        || proof.value_desc != value_desc
        || proof.heap_epoch != heap_epoch
        || proof.metadata_generation != metadata_generation
        || proof.origin_epoch != origin_epoch
        || proof.layout_epoch != layout_epoch
    {
        return None;
    }
    let heap = &vm.mem.heap;
    let buffer = heap.load_and_forward(ObjectRef::from_raw(buffer_raw as usize as *mut u8));
    let buf_ptr = buffer.as_ptr();
    if std::ptr::read(buf_ptr.cast::<u32>()) != proof.buffer_class
        || std::ptr::read(buf_ptr.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET))
            & cratonvm_types::GC_FLAG_COMPACT
            == 0
    {
        return None;
    }
    let write = value.is_some();
    if write
        && !matches!(
            cratonvm_types::read_compact_field(
                buf_ptr.add(proof.read_only_off),
                cratonvm_types::FieldStorageKind::Boolean,
                std::sync::atomic::Ordering::Relaxed,
            ),
            Value::Int(0)
        )
    {
        return None;
    }
    let segment_bits = jit_load_ref_slot(
        buf_ptr.add(proof.segment_off),
        Some(heap),
        ref_load_census::GETFIELD_COMPACT_REF,
        "byte-buffer-view/compact-ref",
    );
    if segment_bits != 0 {
        return None;
    }
    let Value::Int(limit) = cratonvm_types::read_compact_field(
        buf_ptr.add(proof.limit_off),
        cratonvm_types::FieldStorageKind::Int,
        std::sync::atomic::Ordering::Relaxed,
    ) else {
        return None;
    };
    if end_index > limit {
        return None;
    }
    let hb_bits = jit_load_ref_slot(
        buf_ptr.add(proof.hb_off),
        Some(heap),
        ref_load_census::GETFIELD_COMPACT_REF,
        "byte-buffer-view/compact-ref",
    );
    let result = if hb_bits != 0 {
        let Value::Int(offset) = cratonvm_types::read_compact_field(
            buf_ptr.add(proof.offset_off),
            cratonvm_types::FieldStorageKind::Int,
            std::sync::atomic::Ordering::Relaxed,
        ) else {
            return None;
        };
        if offset < 0 {
            return None;
        }
        let byte_index = usize::try_from(offset.checked_add(index)?).ok()?;
        let byte_end = byte_index.checked_add(width)?;
        let hb_u64 = hb_bits as u64;
        let array = if hb_u64 == proof.last_hb {
            heap.load_and_forward(ObjectRef::from_raw(hb_bits as usize as *mut u8))
        } else {
            let (arr, live) =
                heap.load_and_forward_checked(ObjectRef::from_raw(hb_bits as usize as *mut u8));
            if !live {
                return None;
            }
            proof.last_hb = hb_u64;
            VH_BYTE_BUFFER_VIEW_PROOFS.with(|proofs| proofs[slot].set(Some(proof)));
            arr
        };
        if heap.kind_of_validated(array) != cratonvm_types::ObjectKind::Array
            || heap.element_type_of_validated(array) != ArrayElementType::Byte
            || byte_end > heap.array_length(array)
        {
            return None;
        }
        let base = heap.array_data_ptr(array)?;
        byte_view_unaligned_access(
            base.add(byte_index),
            width,
            value_desc,
            proof.little_endian,
            value,
        )
    } else {
        let gate = match (write, value_desc) {
            (true, b'S') => &UNSAFE_PUT_SHORT_GATE,
            (true, b'C') => &UNSAFE_PUT_CHAR_GATE,
            (true, b'I' | b'F') => &UNSAFE_PUT_INT_GATE,
            (true, _) => &UNSAFE_PUT_LONG_GATE,
            (false, b'S') => &UNSAFE_GET_SHORT_GATE,
            (false, b'C') => &UNSAFE_GET_CHAR_GATE,
            (false, b'I' | b'F') => &UNSAFE_GET_INT_GATE,
            (false, _) => &UNSAFE_GET_LONG_GATE,
        };
        if !unsafe_accessor_capability_admits(vm, gate) {
            return None;
        }
        let Value::Long(address) = cratonvm_types::read_compact_field(
            buf_ptr.add(proof.address_off),
            cratonvm_types::FieldStorageKind::Long,
            std::sync::atomic::Ordering::Relaxed,
        ) else {
            return None;
        };
        if address <= 0 || !cratonvm_native_builtins::unsafe_arena_names_block(address) {
            return None;
        }
        let addr = address.checked_add(i64::from(index))?;
        if addr <= 0
            || cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr)
            || (addr as u64).checked_add(width as u64).is_none()
        {
            return None;
        }
        byte_view_unaligned_access(
            addr as usize as *mut u8,
            width,
            value_desc,
            proof.little_endian,
            value,
        )
    };
    if crate::runtime::env_cache::dbg_jitc() {
        note_byte_buffer_view_compact_fields_reported(vm.vm_identity);
        note_byte_buffer_view_access_reported(vm.vm_identity, write);
    }
    Some(result)
}

/// Record a compact-field proof for `(vh_raw, buffer_raw, value_desc)` after
/// the uncached path has validated and served it.
// SAFETY: `vh_raw` and `buffer_raw` are verified live references within the current epoch.
pub unsafe fn remember_varhandle_byte_buffer_view_proof(
    vm: &SharedVm,
    vh_raw: u64,
    buffer_raw: u64,
    value_desc: u8,
) -> Option<()> {
    let heap_epoch = vm.mem.address_epoch();
    let metadata_generation =
        cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation();
    let origin_epoch = cratonvm_classloading::class_origin_epoch();
    let layout_epoch = cratonvm_types::layout_generation();
    let (_, key) = direct_receiver_facts(vm, vh_raw as usize)?;
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_byte_buffer_view_plan_for_handle(
        vm.vm_identity,
        key,
        vh_raw as usize,
    )?;
    if plan.value_desc != value_desc {
        return None;
    }
    let layout = byte_buffer_view_read_layout(vm)?;
    let (buffer_class, _) = direct_receiver_facts(vm, buffer_raw as usize)?;
    if buffer_class == 0
        || !jit_is_subclass_of_cached(
            vm,
            ClassId::new(buffer_class),
            ClassId::new(layout.buffer_class),
        )
    {
        return None;
    }
    let heap = &vm.mem.heap;
    let buffer = heap.load_and_forward(ObjectRef::from_raw(buffer_raw as usize as *mut u8));
    let buf_ptr = buffer.as_ptr();
    if std::ptr::read(buf_ptr.cast::<u32>()) != buffer_class
        || std::ptr::read(buf_ptr.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET))
            & cratonvm_types::GC_FLAG_COMPACT
            == 0
    {
        return None;
    }
    let compact_off = |slot_idx: usize, expected: cratonvm_types::FieldStorageKind| -> Option<usize> {
        let (off, kind) = jit_compact_field_slot(buf_ptr as i64, i64::try_from(slot_idx).ok()?)?;
        (kind == expected).then_some(off)
    };
    let hb_off = compact_off(layout.hb, cratonvm_types::FieldStorageKind::Reference)?;
    let offset_off = compact_off(layout.offset, cratonvm_types::FieldStorageKind::Int)?;
    let limit_off = compact_off(layout.limit, cratonvm_types::FieldStorageKind::Int)?;
    let address_off = compact_off(layout.address, cratonvm_types::FieldStorageKind::Long)?;
    let segment_off = compact_off(layout.segment, cratonvm_types::FieldStorageKind::Reference)?;
    let read_only_off = compact_off(layout.read_only, cratonvm_types::FieldStorageKind::Boolean)?;
    let hb_bits = jit_load_ref_slot(
        buf_ptr.add(hb_off),
        Some(heap),
        ref_load_census::GETFIELD_COMPACT_REF,
        "byte-buffer-view/compact-ref",
    );
    let last_hb = if hb_bits != 0 {
        let (arr, live) =
            heap.load_and_forward_checked(ObjectRef::from_raw(hb_bits as usize as *mut u8));
        if live
            && heap.kind_of_validated(arr) == cratonvm_types::ObjectKind::Array
            && heap.element_type_of_validated(arr) == ArrayElementType::Byte
        {
            hb_bits as u64
        } else {
            0
        }
    } else {
        0
    };
    if heap_epoch != vm.mem.address_epoch()
        || metadata_generation
            != cratonvm_native_builtins::lang_invoke::varhandle_metadata_generation()
        || origin_epoch != cratonvm_classloading::class_origin_epoch()
        || layout_epoch != cratonvm_types::layout_generation()
    {
        return None;
    }
    let slot = vh_byte_view_proof_slot(vh_raw, buffer_raw, value_desc);
    VH_BYTE_BUFFER_VIEW_PROOFS.with(|proofs| {
        proofs[slot].set(Some(VarHandleByteBufferViewProof {
            vm: vm.vm_identity,
            vh: vh_raw,
            buffer: buffer_raw,
            heap_epoch,
            metadata_generation,
            origin_epoch,
            layout_epoch,
            buffer_class,
            value_desc,
            little_endian: plan.little_endian,
            hb_off,
            offset_off,
            limit_off,
            address_off,
            segment_off,
            read_only_off,
            last_hb,
        }));
    });
    Some(())
}

pub unsafe fn try_varhandle_byte_buffer_view_access(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
    thread: &mut JvmThread,
) -> Option<i64> {
    let write = info.method_name == "set";
    let prefix = "(Ljava/nio/ByteBuffer;I";
    let desc = info.descriptor.as_bytes();
    let value_desc = if write {
        if args.len() != 4
            || info.return_type != b'V'
            || !info.descriptor.starts_with(prefix)
            || desc.len() != prefix.len() + 3
            || &desc[prefix.len() + 1..] != b")V"
        {
            return None;
        }
        desc[prefix.len()]
    } else {
        if info.method_name != "get"
            || args.len() != 3
            || !info.descriptor.starts_with("(Ljava/nio/ByteBuffer;I)")
            || desc.len() != prefix.len() + 2
        {
            return None;
        }
        info.return_type
    };
    if args[0] == 0 || args[1] == 0 {
        return None;
    }
    let width = match value_desc {
        b'S' | b'C' => 2,
        b'I' | b'F' => 4,
        b'J' | b'D' => 8,
        _ => return None,
    };
    let (_, key) = direct_receiver_facts(vm, args[0] as usize)?;
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_byte_buffer_view_plan_for_handle(
        vm.vm_identity,
        key,
        args[0] as usize,
    )?;
    if plan.value_desc != value_desc {
        return None;
    }
    let layout = byte_buffer_view_read_layout(vm)?;
    let (class, _) = direct_receiver_facts(vm, args[1] as usize)?;
    if class == 0
        || !jit_is_subclass_of_cached(vm, ClassId::new(class), ClassId::new(layout.buffer_class))
    {
        return None;
    }
    let heap = &vm.mem.heap;
    let buffer =
        heap.load_and_forward_validated(ObjectRef::from_raw(args[1] as usize as *mut u8));
    if write
        && !matches!(
            checked_byte_buffer_view_field(vm, buffer, layout.read_only, b'Z'),
            Value::Int(0)
        )
    {
        return None;
    }
    // Only a segment-backed buffer has a session whose close must wait.
    let _window = match checked_byte_buffer_view_field(vm, buffer, layout.segment, b'L') {
        Value::Object(None) => None,
        Value::Object(Some(segment)) => {
            let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
            if !window.admits_fast_path() {
                return None;
            }
            let (segment, live) = heap.load_and_forward_checked(segment);
            if !live {
                return None;
            }
            let session = buffer_stored_session_with(vm, segment, || {
                let ctx = crate::vm::NativeContextImpl { shared: vm, thread };
                cratonvm_native_builtins::lang_invoke::byte_buffer_view_stored_session(
                    &ctx, segment,
                )
            })?;
            if !cratonvm_native_builtins::ffm_fast::is_validated(session.as_ptr() as u64, false) {
                return None;
            }
            Some(window)        }
        _ => return None,
    };
    let Value::Int(limit) = checked_byte_buffer_view_field(vm, buffer, layout.limit, b'I') else {
        return None;
    };
    let index = i32::try_from(args[2]).ok()?;
    if index < 0 || index.checked_add(width as i32)? > limit {
        return None;
    }
    let (base, offset) = match checked_byte_buffer_view_field(vm, buffer, layout.hb, b'L') {
        Value::Object(Some(array)) => {
            let (array, live) = heap.load_and_forward_checked(array);
            if !live
                || heap.kind_of(array) != cratonvm_types::ObjectKind::Array
                || heap.element_type_of_validated(array) != ArrayElementType::Byte
            {
                return None;
            }
            let Value::Int(offset) =
                checked_byte_buffer_view_field(vm, buffer, layout.offset, b'I')
            else {
                return None;
            };
            if offset < 0 {
                return None;
            }
            (
                array.as_ptr() as i64,
                16i64
                    .checked_add(i64::from(offset))?
                    .checked_add(i64::from(index))?,
            )
        }
        Value::Object(None) => {
            let gate = match (write, value_desc) {
                (true, b'S') => &UNSAFE_PUT_SHORT_GATE,
                (true, b'C') => &UNSAFE_PUT_CHAR_GATE,
                (true, b'I' | b'F') => &UNSAFE_PUT_INT_GATE,
                (true, _) => &UNSAFE_PUT_LONG_GATE,
                (false, b'S') => &UNSAFE_GET_SHORT_GATE,
                (false, b'C') => &UNSAFE_GET_CHAR_GATE,
                (false, b'I' | b'F') => &UNSAFE_GET_INT_GATE,
                (false, _) => &UNSAFE_GET_LONG_GATE,
            };
            if !unsafe_accessor_capability_admits(vm, gate) {
                return None;
            }
            let Value::Long(address) =
                checked_byte_buffer_view_field(vm, buffer, layout.address, b'J')
            else {
                return None;
            };
            if address <= 0 {
                return None;
            }
            // Native-owned blocks only. External/raw JNI addresses retain
            // the authoritative native callback and its provenance contract.
            if !cratonvm_native_builtins::unsafe_arena_names_block(address) {
                return None;
            }
            (0, address.checked_add(i64::from(index))?)        }
        _ => return None,
    };
    let mut bytes = [0u8; 8];
    if write {
        let bits = args[3] as u64;
        for (i, byte) in bytes[..width].iter_mut().enumerate() {
            let shift = 8 * if plan.little_endian { i } else { width - i - 1 };
            *byte = (bits >> shift) as u8;
        }
        if !raw_accessor_write_bytes(vm, base, offset, &bytes[..width]) {
            return None;
        }
        if crate::runtime::env_cache::dbg_jitc() {
            note_byte_buffer_view_access_reported(vm.vm_identity, true);
        }
        let _ = remember_varhandle_byte_buffer_view_proof(
            vm,
            args[0] as u64,
            args[1] as u64,
            value_desc,
        );
        return Some(0);
    }
    if !raw_accessor_read_bytes(vm, base, offset, &mut bytes[..width]) {
        return None;
    }
    let mut bits = 0u64;
    for (i, byte) in bytes[..width].iter().enumerate() {
        let shift = 8 * if plan.little_endian { i } else { width - i - 1 };
        bits |= u64::from(*byte) << shift;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        note_byte_buffer_view_access_reported(vm.vm_identity, false);
    }
    let _ = remember_varhandle_byte_buffer_view_proof(
        vm,
        args[0] as u64,
        args[1] as u64,
        value_desc,
    );
    Some(match info.return_type {
        b'S' => bits as u16 as i16 as i64,
        b'C' => bits as u16 as i64,
        b'I' | b'F' => bits as u32 as i32 as i64,
        _ => bits as i64,
    })
}

pub unsafe fn try_varhandle_byte_array_view_access(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    if !info.descriptor.starts_with("([BI") {
        return None;
    }
    let (slot, value) = match args.len() {
        3 => (
            cratonvm_jit::varhandle_byte_array_view_read_slot(info.method_name, info.descriptor)?,
            None,
        ),
        4 => (
            cratonvm_jit::varhandle_byte_buffer_view_write_slot(info.method_name, info.descriptor)?,
            Some(args[3]),
        ),
        _ => return None,
    };
    let value_desc = cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[slot];
    let expected_return = if value.is_some() { b'V' } else { value_desc };
    if info.return_type != expected_return {
        return None;
    }
    varhandle_byte_array_view_bits(
        vm,
        args[0] as u64,
        args[1] as u64,
        args[2],
        value_desc,
        value,
    )
}

pub static SYSTEM_ARRAYCOPY_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/System",
    method_name: "arraycopy",
    descriptor: "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    num_jit_args: 5,
    return_type: b'V',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The primitive copy is allocation/poll free. Reference arrays, fragmented
/// carriers and every fault retain the authoritative native's exception order,
/// partial-copy semantics and collector barriers.
pub unsafe fn try_primitive_arraycopy(
    vm: &SharedVm,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
) -> Option<()> {
    if src == 0 || dst == 0 {
        return None;
    }
    let src_pos = usize::try_from(i32::try_from(src_pos).ok()?).ok()?;
    let dst_pos = usize::try_from(i32::try_from(dst_pos).ok()?).ok()?;
    let len = usize::try_from(i32::try_from(len).ok()?).ok()?;
    let heap = &vm.mem.heap;
    let (src, src_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(src as usize as *mut u8));
    let (dst, dst_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(dst as usize as *mut u8));
    if !src_live
        || !dst_live
        || heap.kind_of(src) != cratonvm_types::ObjectKind::Array
        || heap.kind_of(dst) != cratonvm_types::ObjectKind::Array
    {
        return None;
    }
    let kind = heap.element_type_of_validated(src);
    if kind == ArrayElementType::Reference || kind != heap.element_type_of_validated(dst) {
        return None;
    }
    let (src_len, dst_len) = (heap.array_length(src), heap.array_length(dst));
    try_primitive_arraycopy_pair(vm, kind, src, src_pos, src_len, dst, dst_pos, dst_len, len)
}

/// [`try_primitive_arraycopy`] past its validation: `src` / `dst` are live
/// arrays of the one primitive element type `kind` (validated by the caller
/// with no safepoint since), of `src_len` / `dst_len` elements.
#[allow(clippy::too_many_arguments)]
pub unsafe fn try_primitive_arraycopy_pair(
    vm: &SharedVm,
    kind: ArrayElementType,
    src: ObjectRef,
    src_pos: usize,
    src_len: usize,
    dst: ObjectRef,
    dst_pos: usize,
    dst_len: usize,
    len: usize,
) -> Option<()> {
    let heap = &vm.mem.heap;
    if src_pos.checked_add(len)? > src_len || dst_pos.checked_add(len)? > dst_len {
        return None;
    }
    let width = cratonvm_types::element_byte_size(kind);
    let bytes = len.checked_mul(width)?;
    let src_offset = src_pos.checked_mul(width)?;
    let dst_offset = dst_pos.checked_mul(width)?;
    let src_data = heap.array_data_ptr(src)?.add(src_offset);
    let dst_data = heap.array_data_ptr(dst)?.add(dst_offset);
    // No allocation, poll or Java call can relocate these validated carriers.
    //
    // JIT round 21 wave 3 (lane intrinsics; correctness, default ON,
    // `CRATONVM_JIT_ARRAYCOPY_DIRECT_ELEMENT_ATOMIC=0` restores the plain
    // `memmove`): every 2/4/8-byte element moves WHOLE, as HotSpot's
    // `conjoint_j*_atomic` stubs move it. This helper is the optimizing tier's
    // `System.arraycopy` (`ir_arraycopy_entry`, `CRATONVM_JIT_IR_ARRAYCOPY`
    // default ON); round 20 (fuzz14c / fuzz14d) made the native's
    // `bulk_array_copy` and the single-pass `REP MOVS*` arm element-atomic and
    // left this third copy a byte `memmove`, whose head/tail may move one
    // element in pieces, so a racing writer's element could be read TORN here
    // (`R20FuzzGTearCopy1` `tear.copyof`'s shape, reached from an IR body).
    // Both forms are memmove-correct for an overlapping same-array copy.
    if arraycopy_direct_element_atomic(vm) {
        crate::vm::vm_exec::bulk_copy_element_atomic(
            src_data as *const u8,
            dst_data,
            bytes,
            width,
        );
    } else {
        std::ptr::copy(src_data, dst_data, bytes);
    }
    Some(())
}

/// `CRATONVM_JIT_ARRAYCOPY_DIRECT_ELEMENT_ATOMIC` (JIT round 21 wave 3, lane
/// intrinsics; correctness, **default ON**, `0` restores the byte `memmove`):
/// whether [`try_primitive_arraycopy`] moves each element whole. Read once per
/// VM per thread (the helper runs on every IR-tier `System.arraycopy`), the
/// shape `lang_math`'s `number_parse_raise_jdk_body` caches its switch in.
pub fn arraycopy_direct_element_atomic(vm: &SharedVm) -> bool {
    arraycopy_direct_switches(vm).0
}

/// The two switches of `jit_system_arraycopy_direct`, read once per VM per
/// thread into ONE thread-local (JIT r23 wave 3b, lane L5: the second,
/// `CRATONVM_JIT_REF_ARRAYCOPY_DIRECT`, shares the first's slot rather than
/// adding a static -- `vm/tests/per_vm_state_statics_ratchet.rs`):
/// `(element_atomic, ref_direct, one_validation, new_array_mirror_memo)` --
/// the third is `CRATONVM_JIT_ARRAYCOPY_DIRECT_ONE_VALIDATION` (JIT r23 wave 5,
/// lane L5, [`try_arraycopy_validated_once`]); the fourth, sharing the slot for
/// the same ratchet reason, is `CRATONVM_JIT_REFLECT_NEW_ARRAY_MIRROR_MEMO`
/// ([`reflect_new_array_serve`]); the fifth (JIT r23 wave 8, lane L8) is
/// `CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`, whose VM half is the
/// mirror-hint fill in [`reflect_new_array_serve`].
pub fn arraycopy_direct_switches(vm: &SharedVm) -> (bool, bool, bool, bool, bool) {
    thread_local! {
        pub static ON: std::cell::Cell<Option<(usize, [bool; 5])>> =
            const { std::cell::Cell::new(None) };
    }
    let id = vm.vm_identity;
    let [atomic, ref_direct, once, memo, inline_new_array] = ON.with(|c| match c.get() {
        Some((owner, on)) if owner == id => on,
        _ => {
            let on = [
                true,
                true,
                true,
                true,
                true,
            ];
            c.set(Some((id, on)));
            on
        }
    });
    (atomic, ref_direct, once, memo, inline_new_array)
}

/// JIT r23 wave 5, lane L5 (`CRATONVM_JIT_ARRAYCOPY_DIRECT_ONE_VALIDATION`,
/// perf, default ON, `=0` restores the two self-validating attempts):
/// `jit_system_arraycopy_direct` validates the two arrays ONCE and branches on
/// their element types, instead of [`try_primitive_arraycopy`] validating
/// both, declining a reference pair, and [`try_reference_arraycopy`]
/// validating both again -- and each validation was four defensive accessors
/// (`kind_of`, `element_type_of_validated`, `array_length` per array), every
/// one of them re-running the ZGC object-start registry lookup
/// (`ZObjectStarts::contains`): the RIP profile of a compiled
/// `Object[10] -> Object[10]` copy loop on n23-w4 had `is_object_address` +
/// `ZObjectStarts::contains` at 45k of ~200k samples and the helper's own
/// frame at 28k, for 170-230 ns a copy against HotSpot's 2. Here each array is
/// validated once (`load_and_forward_checked`, the same live-base test the
/// accessors run) and its kind, element type and length are read straight off
/// the header (`native_array_footprint`'s argument: the defence buys nothing
/// for a base validated a moment ago with no safepoint between). The copies
/// themselves are the unchanged `*_pair` tails.
pub unsafe fn try_arraycopy_validated_once(
    vm: &SharedVm,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
    ref_direct: bool,
) -> Option<()> {
    if src == 0 || dst == 0 {
        return None;
    }
    let src_pos = usize::try_from(i32::try_from(src_pos).ok()?).ok()?;
    let dst_pos = usize::try_from(i32::try_from(dst_pos).ok()?).ok()?;
    let len = usize::try_from(i32::try_from(len).ok()?).ok()?;
    let heap = &vm.mem.heap;
    let (src, src_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(src as usize as *mut u8));
    let (dst, dst_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(dst as usize as *mut u8));
    if !src_live || !dst_live {
        return None;
    }
    let read = |obj: ObjectRef| {
        let header = heap.get_header(obj);
        (
            header.kind(),
            header.element_type(),
            header.array_length() as usize,
        )
    };
    let (src_kind, src_elem, src_len) = read(src);
    let (dst_kind, dst_elem, dst_len) = read(dst);
    if src_kind != cratonvm_types::ObjectKind::Array || dst_kind != cratonvm_types::ObjectKind::Array
    {
        return None;
    }
    match (src_elem, dst_elem) {
        (ArrayElementType::Reference, ArrayElementType::Reference) if ref_direct => {
            try_reference_arraycopy_pair(vm, src, src_pos, src_len, dst, dst_pos, dst_len, len)
        }
        (ArrayElementType::Reference, _) | (_, ArrayElementType::Reference) => None,
        (s, d) if s == d => {
            try_primitive_arraycopy_pair(vm, s, src, src_pos, src_len, dst, dst_pos, dst_len, len)
        }
        _ => None,
    }
}

/// `CRATONVM_JIT_REF_ARRAYCOPY_DIRECT` (JIT r23 wave 3b, lane L5; perf,
/// default ON, `0` restores the native dispatch): the optimizing tier's
/// `System.arraycopy` helper also answers a REFERENCE copy itself
/// ([`try_reference_arraycopy`]) instead of crossing the native funnel to
/// `native_system_arraycopy` (`r23-w3-reference-arraycopy-costs-microseconds-FIXED-20261021.md`:
/// ~0.6-1.3 us of fixed cost per call on ZGC -- the funnel, three validations
/// of each array and a `class_id_by_name("java/lang/Object")` lookup per
/// call). Read once per VM per thread ([`arraycopy_direct_switches`]).
pub fn ref_arraycopy_direct_enabled(vm: &SharedVm) -> bool {
    arraycopy_direct_switches(vm).1
}

/// `System.arraycopy` of two REFERENCE arrays, answered here when it is
/// certain to complete: both arrays live reference arrays, every position in
/// range, and every element storable -- the destination component equal to the
/// source's or `java/lang/Object` (no check needed), or every element proven
/// (null, or a plain object whose class is the destination component or a
/// subtype, `class_is_subtype`, memoised per call). `None` for anything else
/// (a null, a non-array, a primitive array, a bound the JDK throws on, an
/// element the proof cannot settle, an auto-box wrapper): the caller's native
/// dispatch then answers, throws (the exact `ArrayStoreException` /
/// `ArrayIndexOutOfBoundsException` texts) and commits the partial copy
/// exactly as before. Nothing is written before a `None`, except by a CHECKED
/// copy (two different arrays) that stops at its first unprovable element:
/// that prefix is exactly the one the native fallback re-copies first.
///
/// The stores carry the collector's obligations as
/// `NativeContextImpl::copy_reference_array_elements` does: on a quiet ZGC the
/// word copy plus one object-keyed card (`copy_reference_slots_raw`); on G1 /
/// Generational with no marking window and no humongous array the word copy
/// plus each stored reference's post barrier (the same function's
/// `CRATONVM_G1_GEN_RAW_REF_ARRAYCOPY` arm); otherwise the heap's own per-element accessor (G1 / ZGC publish the
/// overwritten value inside it; Generational's SATB pre-barrier is
/// `publish_overwritten_array_element`, and its card is the accessor's), in
/// memmove order. No allocation, poll or Java call runs here, so neither array
/// can move (the `try_primitive_arraycopy` argument).
pub unsafe fn try_reference_arraycopy(
    vm: &SharedVm,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
) -> Option<()> {
    if src == 0 || dst == 0 {
        return None;
    }
    let src_pos = usize::try_from(i32::try_from(src_pos).ok()?).ok()?;
    let dst_pos = usize::try_from(i32::try_from(dst_pos).ok()?).ok()?;
    let len = usize::try_from(i32::try_from(len).ok()?).ok()?;
    let heap = &vm.mem.heap;
    let (src, src_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(src as usize as *mut u8));
    let (dst, dst_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(dst as usize as *mut u8));
    if !src_live
        || !dst_live
        || heap.kind_of(src) != cratonvm_types::ObjectKind::Array
        || heap.kind_of(dst) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(src) != ArrayElementType::Reference
        || heap.element_type_of_validated(dst) != ArrayElementType::Reference
    {
        return None;
    }
    let (src_len, dst_len) = (heap.array_length(src), heap.array_length(dst));
    try_reference_arraycopy_pair(vm, src, src_pos, src_len, dst, dst_pos, dst_len, len)
}

/// [`try_reference_arraycopy`] past its validation: `src` / `dst` are live
/// reference arrays of `src_len` / `dst_len` elements (validated by the caller
/// with no safepoint since), the positions and length non-negative `int`s.
#[allow(clippy::too_many_arguments)]
pub unsafe fn try_reference_arraycopy_pair(
    vm: &SharedVm,
    src: ObjectRef,
    src_pos: usize,
    src_len: usize,
    dst: ObjectRef,
    dst_pos: usize,
    dst_len: usize,
    len: usize,
) -> Option<()> {
    let heap = &vm.mem.heap;
    if src_pos.checked_add(len)? > src_len || dst_pos.checked_add(len)? > dst_len {
        return None;
    }
    if len == 0 {
        return Some(());
    }
    let src_component = heap.get_header(src).class_id;
    let dst_component = heap.get_header(dst).class_id;
    let unchecked = src_component == dst_component
        || crate::vm::vm_exec::object_class_id(vm) == Some(dst_component);
    let backward = src.as_ptr() == dst.as_ptr() && dst_pos > src_pos;
    // One proof per distinct element class, the last one memoised.
    let mut proven: Option<cratonvm_types::ClassId> = None;
    let mut storable = |elem: ObjectRef| -> bool {
        if unchecked {
            // Any reference is storable (an array element included); an
            // auto-box payload is refused by the reader below, not here.
            return true;
        }
        let header = heap.get_header(elem);
        if header.kind() != cratonvm_types::ObjectKind::Object {
            return false;
        }
        let cid = header.class_id;
        if cid == cratonvm_types::AUTOBOX_CLASS_ID {
            return false;
        }
        if proven == Some(cid) {
            return true;
        }
        if cid == dst_component
            || crate::runtime::interpreter::class_is_subtype(vm, cid, dst_component)
        {
            proven = Some(cid);
            return true;
        }
        false
    };
    // JIT r23 wave 3 review: a CHECKED copy proves the element it STORES
    // (one load, one proof, one store per slot), never a separate proof pass
    // followed by a copy -- a racing writer of `src` (an `Object[]` source
    // whose static type admits anything) could otherwise slip an unproven
    // reference into `dst` between the two and break heap type safety, which
    // HotSpot's checkcast arraycopy never allows. A checked copy is never a
    // same-array copy (one array has one component id), so when the proof
    // stops at slot `n` the prefix written is exactly what the native
    // fallback re-copies before it throws `ArrayStoreException` at that same
    // element (HotSpot's partial-copy semantics); re-copying a prefix of a
    // DIFFERENT array is idempotent.
    if !unchecked {
        let copied = match crate::vm::vm_exec::copy_reference_slots_raw_checked(
            heap,
            src,
            src_pos,
            dst,
            dst_pos,
            len,
            &mut storable,
        ) {
            Some(n) => n,
            None => {
                let mut n = 0usize;
                while n < len {
                    // The non-unboxing reader: a checked copy refuses an
                    // auto-box wrapper in `storable`, never re-boxes.
                    // One read: the value proven is the value stored.
                    let value = match heap.get_array_element(src, src_pos + n) {
                        Ok(v @ Value::Object(None)) => v,
                        Ok(v @ Value::Object(Some(elem))) if storable(elem) => v,
                        _ => break,
                    };
                    crate::vm::vm_exec::publish_overwritten_array_element(
                        heap,
                        || true,
                        dst,
                        dst_pos + n,
                    );
                    let _ = heap.set_array_element(dst, dst_pos + n, value);
                    n += 1;
                }
                n
            }
        };
        #[cfg(feature = "gpu-offload")]
        {
            if copied > 0 {
                crate::runtime::offload::input_cache::invalidate(dst);
            }
        }
        return (copied == len).then_some(());
    }
    // An UNCHECKED copy (any reference is storable): the raw word arm when the
    // collector admits it -- a quiet ZGC, or G1 / Generational with no marking
    // window and no humongous array (`CRATONVM_G1_GEN_RAW_REF_ARRAYCOPY`) --
    // words moved verbatim in memmove order.
    if crate::vm::vm_exec::copy_reference_slots_raw(heap, src, src_pos, dst, dst_pos, len, backward)
    {
        #[cfg(feature = "gpu-offload")]
        crate::runtime::offload::input_cache::invalidate(dst);
        return Some(());
    }
    // Every other collector state: the per-element accessors in memmove
    // order. An unchecked copy may overlap itself, so nothing may be written
    // unless the whole copy completes: refuse an auto-box payload (the only
    // element the unboxing reader cannot hand straight back to the store)
    // before the first store.
    for i in 0..len {
        match heap.get_array_element_unboxing(src, src_pos + i) {
            Ok(Value::Object(_)) => {}
            _ => return None,
        }
    }
    for step in 0..len {
        let i = if backward { len - 1 - step } else { step };
        crate::vm::vm_exec::publish_overwritten_array_element(heap, || true, dst, dst_pos + i);
        // The non-unboxing reader: a racing wrapper is copied verbatim (as
        // the raw arm does), never re-boxed -- no allocation runs here.
        let value = heap
            .get_array_element(src, src_pos + i)
            .unwrap_or(Value::Object(None));
        let _ = heap.set_array_element(dst, dst_pos + i, value);
    }
    #[cfg(feature = "gpu-offload")]
    crate::runtime::offload::input_cache::invalidate(dst);
    Some(())
}

pub unsafe extern "C" fn jit_system_arraycopy_direct(
    vm_ptr: i64,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
) -> i64 {
    let bits = contain(
        "jit_system_arraycopy_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            // JIT r23 wave 6, lane L5: ONE read of the switch slot per call
            // (the n23-w5 `Object[10]` copy profile had two -- 19k of ~250k
            // samples in the thread-local's accessor).
            let (_, ref_direct, once, _, _) = arraycopy_direct_switches(vm);
            let served = if once {
                try_arraycopy_validated_once(vm, src, src_pos, dst, dst_pos, len, ref_direct)
                    .is_some()
            } else {
                try_primitive_arraycopy(vm, src, src_pos, dst, dst_pos, len).is_some()
                    || (ref_arraycopy_direct_enabled(vm)
                        && try_reference_arraycopy(vm, src, src_pos, dst, dst_pos, len).is_some())
            };
            if served {
                if crate::runtime::env_cache::dbg_jitc() {
                    ARRAYCOPY_DIRECT_REPORTED_VM.with(|reported| {
                        if reported.get() != vm.vm_identity {
                            reported.set(vm.vm_identity);
                            eprintln!(
                                "[cratonvm-jitc] primitive-arraycopy direct served vm={}",
                                vm.vm_identity
                            );
                        }
                    });
                }
                return 0;
            }
            let args = [src, src_pos, dst, dst_pos, len];
            let bits = jit_invoke_dispatch(
                vm_ptr,
                &SYSTEM_ARRAYCOPY_INFO as *const _ as i64,
                args.as_ptr() as i64,
                5,
            );
            // handoff-w36b, only where a native ran: a served copy published
            // no return (wave 6: the unconditional call was the `JIT_THREAD`
            // thread-local, 9k samples, on every served copy).
            native_return_handed_to_compiled_code();
            bits
        },
    );
    bits
}
thread_local! {
    pub static ARRAYCOPY_DIRECT_REPORTED_VM: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Synthetic call-site info for [`jit_reflect_new_array_direct`]'s decline
/// edge: the registered `Array.newArray` native, exactly as the unbound site
/// dispatched it. A `const` (its `&` is promoted to one `'static` address), so
/// `per_vm_state_statics_ratchet` counts no new declaration.
pub const REFLECT_NEW_ARRAY_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/reflect/Array",
    method_name: "newArray",
    descriptor: "(Ljava/lang/Class;I)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// May [`jit_reflect_new_array_direct`] allocate an array whose component is
/// the class named `name` (the class record's own, `/`-separated) itself?
/// Only a reference component: a primitive (or `void`) mirror, and an array
/// class that already has 255 dimensions (HotSpot's `IllegalArgumentException`,
/// `Reflection::reflect_new_array`), go to the registered native, which raises
/// or allocates exactly as before.
pub fn reflect_new_array_direct_admits_name(name: &str) -> bool {
    if name.is_empty()
        || matches!(
            name,
            "void" | "V" | "int" | "long" | "float" | "double" | "boolean" | "byte" | "char"
                | "short"
        )
    {
        return false;
    }
    name.bytes().take_while(|&b| b == b'[').count() < 255
}

/// JIT r23 wave 8, lane L8: the mirror-hint kind of the class named `name`
/// for [`class_mirror_id_hint_fill`], from [`reflect_new_array_serve`] --
/// exactly the rule the `Class.isAssignableFrom` / `isInstance` natives give
/// it (`native-builtins` `note_reflect_mirror_id` over `NameFacts`), so the
/// two fillers never disagree about a class: `0` (no hint) for an array or a
/// primitive-named class, `1` (a hint whose misses are not answered) for a
/// `synthetic_implements` target, `2` otherwise.
pub fn reflect_new_array_hint_kind(name: &str) -> u8 {
    if name.starts_with('[')
        || matches!(
            name,
            "int" | "long" | "float" | "double" | "boolean" | "char" | "byte" | "short" | "void"
        )
    {
        0
    } else if cratonvm_types::synthetic_targets::synthetic_implements_may_admit(name) {
        1
    } else {
        2
    }
}


/// JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_REFLECT_NEW_ARRAY_DIRECT`, perf,
/// default ON, `=0` restores the plain dispatch; decided per site at compile
/// time): the thin helper both compile doors bind a
/// `java/lang/reflect/Array.newArray(Class, int)` site to -- the native the
/// JDK's `Array.newInstance` calls, and with it `Arrays.copyOf(T[], int)` and
/// `ArrayList.toArray(T[])`. The registered native (`native_array_new_array`)
/// costs ~0.5-2 us a call on the dispatch funnel and its per-call name
/// resolution (r23-w3 reference-arraycopy page, wave 4); this answers the
/// common case with the component mirror's reverse-map lookup, one class-record
/// read and the allocation a compiled `anewarray` makes
/// (`jit_anewarray_object_body`: its entry hooks, the TLAB bump, the collecting
/// slow path, its `OutOfMemoryError`). Everything else -- a null component
/// (NPE), a negative length (NASE), a primitive / `void` / 255-dimension
/// component, a mirror not in the reverse map, a JVMTI `VMObjectAlloc`
/// listener, an array past the TLAB cap -- is decided before anything can
/// collect and dispatches the registered native, which owns those exceptions
/// and the large-array ladder.
pub(in crate::jit) unsafe extern "C" fn jit_reflect_new_array_direct(
    vm_ptr: i64,
    component: i64,
    length: i64,
) -> i64 {
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        if let Some(array) = reflect_new_array_serve(vm_ptr, component, length) {
            return array;
        }
        let args = [component, length];
        let bits = jit_invoke_dispatch(
            vm_ptr,
            &REFLECT_NEW_ARRAY_INFO as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            2,
        );
        // handoff-w36b: the native's returned array is in RAX now.
        native_return_handed_to_compiled_code();
        bits
    };
    contain(
        "jit_reflect_new_array_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// The served arm of [`jit_reflect_new_array_direct`]; `None` declines.
pub unsafe fn reflect_new_array_serve(vm_ptr: i64, component: i64, length: i64) -> Option<i64> {
    let length = usize::try_from(length as i32).ok()?;
    if vm_ptr == 0 || component == 0 {
        return None;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    #[cfg(feature = "experimental-debug")]
    if vm
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::VmObjectAlloc)
    {
        return None;
    }
    let (mirror, live) = vm
        .mem
        .heap
        .load_and_forward_checked(ObjectRef::from_raw(component as usize as *mut u8));
    if !live {
        return None;
    }
    // The reverse-map lookup and the name test, both stable for a mirror
    // address within one GC pause generation.
    let resolve = || {
        let component_id = crate::vm::class_id_from_mirror(vm, mirror)?;
        let classes = vm.classes.class_manager.read();
        let class = classes.get_class(component_id)?;
        reflect_new_array_direct_admits_name(&class.name).then_some(component_id)
    };
    // JIT r23 wave 5, lane L5 (`CRATONVM_JIT_REFLECT_NEW_ARRAY_MIRROR_MEMO`,
    // perf, default ON, `=0` asks the two locks per call): the n23-w4 profile
    // of an `Array.newInstance` spin had `class_id_from_mirror` (the reverse
    // map's read lock + hash) at 24k and the class manager's read guard at 8k
    // of ~180k samples. A hit of the per-thread `mirror_id_memo` (keyed by VM,
    // pause generation -- loaded BEFORE the lookup -- and mirror address; the
    // contract of that module) answers both: only ADMITTED components are
    // filled (the closure returns `None` otherwise, which the memo never
    // remembers), and an admitted entry is still a true mirror -> class id row
    // for the module's other reader. That other reader
    // (`NativeContext::class_id_from_mirror` under the opt-in
    // `CRATONVM_CLASS_ID_FROM_MIRROR_MEMO`) fills unfiltered rows -- a
    // 255-dimension array class among them -- so with it on this path asks
    // the locks, and a hit here always names an admitted component.
    let component_id = if arraycopy_direct_switches(vm).3
        && !cratonvm_native_builtins::mirror_id_memo::enabled()
    {
        let generation = vm
            .mem
            .gc_barrier
            .gc_generation
            .load(std::sync::atomic::Ordering::Acquire);
        cratonvm_native_builtins::mirror_id_memo::lookup_or_fill(
            vm.vm_identity,
            generation,
            mirror.as_ptr() as usize,
            resolve,
        )?
    } else {
        resolve()?
    };
    // JIT r23 wave 8, lane L8 (`CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`):
    // publish this mirror's hint, which the optimizing tier's inline
    // `Array.newArray` prefix resolves the component through
    // (`cratonvm_jit::ir_lower::reflect_new_array_resolve_bytes`), so the
    // next compiled call of this component makes no call. Before the
    // allocation (which may collect and move the mirror). Never changes an
    // answer: compiled code proves a hint against the class word, and the
    // kind is the `Class.isAssignableFrom` filler's own rule. Steady state:
    // two loads (`class_mirror_id_hint_fill`'s early return).
    if arraycopy_direct_switches(vm).4 {
        class_mirror_id_hint_fill(vm, mirror, component_id, &|| {
            vm.classes
                .class_manager
                .read()
                .get_class(component_id)
                .map_or(0, |class| reflect_new_array_hint_kind(&class.name))
        });
    }
    // Past the TLAB cap the native's own ladder decides (old-gen / humongous
    // routing, the VM-limit refusal); decided here, before anything can
    // collect, so the decline still hands the native a valid mirror word.
    let data_size =
        cratonvm_types::array_data_size_checked(length, ArrayElementType::Reference)?;
    if cratonvm_types::ARRAY_DATA_OFFSET.checked_add(data_size)?
        > cratonvm_gc::tlab::tlab_max_alloc()
    {
        return None;
    }
    // r23 w4 review (`r23-w4-review-reflect-new-array-direct-skips-allocation-
    // entry-hooks`): the allocation is a compiled `anewarray`'s, entry hooks
    // included -- the SATB flush and the ZGC / G1 / Generational
    // concurrent-cycle starts, which let a compiled allocation loop start a
    // cycle (a TLAB-only serve never did), then the TLAB bump and, on a miss,
    // the collecting slow path. Those may run a whole cycle, so nothing past
    // this point reads the mirror (or its raw word): the allocation takes the
    // component's class id, as `anewarray` does, and there is no decline
    // after it. Its only failure is the `OutOfMemoryError` it publishes as
    // the pending exception with a 0 result; this helper's caller reads the
    // exception sentinel instead, so 0 becomes `i64::MIN`.
    let bits =
        jit_anewarray_object_body(vm_ptr, i64::from(component_id.as_u32()), length as i64);
    Some(if bits == 0 { i64::MIN } else { bits })
}

/// Decline-edge call-site infos for [`jit_string_matches_direct`] and
/// [`jit_string_replace_direct`]: the registered `String` natives, dispatched
/// as the unbound site did (`String` is final, so a special dispatch names the
/// one target). `const`s, as [`REFLECT_NEW_ARRAY_INFO`].
pub const STRING_MATCHES_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/String",
    method_name: "matches",
    descriptor: "(Ljava/lang/String;)Z",
    num_jit_args: 2,
    return_type: b'Z',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub const STRING_REPLACE_ALL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/String",
    method_name: "replaceAll",
    descriptor: "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub const STRING_REPLACE_FIRST_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/String",
    method_name: "replaceFirst",
    descriptor: "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_STRING_REGEX_DIRECT`, decided
/// per site at compile time): the served arm of the `String` regex thin
/// helpers. Runs the registered native's first step, its exact path
/// (`lang_string::string_regex_thin`), on the caller's thread without the
/// dispatch funnel; `None` (any decline, any exception) leaves the call to the
/// funnel, which runs the same native from the top and so answers -- or raises
/// -- exactly as the unbound site did.
pub unsafe fn string_regex_direct_serve(
    vm_ptr: i64,
    words: &[i64],
    op: cratonvm_native_builtins::lang_string::StringRegexThinOp,
) -> Option<i64> {
    if vm_ptr == 0 {
        return None;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let mut args = [Value::Object(None), Value::Object(None), Value::Object(None)];
    for (slot, &word) in args.iter_mut().zip(words) {
        if word == 0 {
            // A null regex / replacement is the native's NPE: dispatch it.
            return None;
        }
        let (obj, live) = vm
            .mem
            .heap
            .load_and_forward_checked(ObjectRef::from_raw(word as usize as *mut u8));
        if !live {
            return None;
        }
        *slot = Value::Object(Some(obj));
    }
    let (thread, _guard) = jit_thread_mut()?;
    // Review (wave 4): the result `String` is the exact path's one allocation,
    // and outside the funnel's `catch_unwind` the native allocators run their
    // INFALLIBLE arm, which aborts the process on a full heap where the
    // native raises a catchable `OutOfMemoryError`. The unwind scope turns a
    // refusal into a decline instead: the allocators never collect, so the
    // argument words below are still the objects' addresses, and the funnel
    // then re-runs the native (nothing was written) on its own OOM ladder.
    let answer = crate::runtime::native_oom::catch_alloc_oom_on(&mut *thread, |thread| {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread,
        };
        cratonvm_native_builtins::lang_string::string_regex_thin(
            &mut ctx,
            &args[..words.len()],
            op,
        )
    })
    .ok()??;
    match answer {
        Value::Int(b) => Some(i64::from(b)),
        Value::Object(Some(o)) => Some(o.as_ptr() as i64),
        _ => None,
    }
}

/// Thin direct-call target for compiled `String.matches(String)` sites.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_string_matches_direct(
    vm_ptr: i64,
    receiver: i64,
    regex: i64,
) -> i64 {
    use cratonvm_native_builtins::lang_string::StringRegexThinOp;
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        if receiver == 0 {
            set_jit_pending_npe_action(
                crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
            );
            return i64::MIN;
        }
        if let Some(bits) =
            string_regex_direct_serve(vm_ptr, &[receiver, regex], StringRegexThinOp::Matches)
        {
            return bits;
        }
        let args = [receiver, regex];
        jit_invoke_dispatch(
            vm_ptr,
            &STRING_MATCHES_INFO as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            2,
        )
    };
    contain("jit_string_matches_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)}

/// Thin direct-call target for compiled `String.replaceAll` (`FIRST` false) /
/// `replaceFirst` (`FIRST` true) sites.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_string_replace_direct<const FIRST: bool>(
    vm_ptr: i64,
    receiver: i64,
    regex: i64,
    replacement: i64,
) -> i64 {
    use cratonvm_native_builtins::lang_string::StringRegexThinOp;
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        if receiver == 0 {
            set_jit_pending_npe_action(
                crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
            );
            return i64::MIN;
        }
        let (op, info) = if FIRST {
            (StringRegexThinOp::ReplaceFirst, &STRING_REPLACE_FIRST_INFO)
        } else {
            (StringRegexThinOp::ReplaceAll, &STRING_REPLACE_ALL_INFO)
        };
        if let Some(bits) =
            string_regex_direct_serve(vm_ptr, &[receiver, regex, replacement], op)
        {
            return bits;
        }
        let args = [receiver, regex, replacement];
        let bits = jit_invoke_dispatch(
            vm_ptr,
            info as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            3,
        );
        // handoff-w36b: the native's returned String is in RAX now.
        native_return_handed_to_compiled_code();
        bits
    };
    contain("jit_string_replace_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)}


pub const VH_BYTE_ARRAY_VIEW_DESCRIPTORS: [&str; 6] =
    ["([BI)S", "([BI)C", "([BI)I", "([BI)J", "([BI)F", "([BI)D"];
pub const fn vh_byte_array_view_read_info(slot: usize) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/VarHandle",
        method_name: "get",
        descriptor: VH_BYTE_ARRAY_VIEW_DESCRIPTORS[slot],
        num_jit_args: 3,
        return_type: cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[slot],
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}
pub static VH_BYTE_ARRAY_VIEW_READ_INFOS: [JitInvokeInfo; 6] = [
    vh_byte_array_view_read_info(0),
    vh_byte_array_view_read_info(1),
    vh_byte_array_view_read_info(2),
    vh_byte_array_view_read_info(3),
    vh_byte_array_view_read_info(4),
    vh_byte_array_view_read_info(5),
];

/// Four-word C ABI: VM/judged site, handle, array, byte index. Exact handles
/// and all refusals use the real call-site descriptor through the native road.
pub unsafe extern "C" fn jit_varhandle_byte_array_view_read_direct<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    array: i64,
    index: i64,
) -> i64 {
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        let vm = &*(vm_ptr as *const SharedVm);
        let info = if site.is_null() {
            &VH_BYTE_ARRAY_VIEW_READ_INFOS[SLOT]
        } else {
            &*site
        };
        let value_desc = cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[SLOT];
        let bits = if info.descriptor.starts_with("(Ljava/nio/ByteBuffer;I)") {
            // The same lifetime, exactness and mutable-carrier checks as the
            // admitted native-site arm, without the MIC/native-site funnel.
            varhandle_cached_byte_buffer_view_bits(
                vm,
                vh as u64,
                array as u64,
                index,
                value_desc,
                None,
            )
            .or_else(|| {
                let (thread, _guard) = jit_thread_mut()?;
                try_varhandle_byte_buffer_view_access(vm, info, &[vh, array, index], thread)
            })
        } else {
            varhandle_byte_array_view_bits(
                vm,
                vh as u64,
                array as u64,
                index,
                value_desc,
                None,
            )
        };
        if let Some(bits) = bits {
            note_vh_direct(VH_DIRECT_READ, true);
            note_funnel_bypass_hit();
            return bits;
        }
        note_vh_direct(VH_DIRECT_READ, false);
        let args = [vh, array, index];
        jit_invoke_dispatch(
            vm_ptr,
            varhandle_thin_cold_arm_info(site, &VH_BYTE_ARRAY_VIEW_READ_INFOS[SLOT]),
            args.as_ptr() as i64,
            3,
        )
    };
    let bits = contain(
        "jit_varhandle_byte_array_view_read_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    native_return_handed_to_compiled_code();
    bits
}

pub const VH_BYTE_BUFFER_VIEW_WRITE_DESCRIPTORS: [&str; 6] = [
    "(Ljava/nio/ByteBuffer;IS)V",
    "(Ljava/nio/ByteBuffer;IC)V",
    "(Ljava/nio/ByteBuffer;II)V",
    "(Ljava/nio/ByteBuffer;IJ)V",
    "(Ljava/nio/ByteBuffer;IF)V",
    "(Ljava/nio/ByteBuffer;ID)V",
];
pub const fn vh_byte_buffer_view_write_info(slot: usize) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/VarHandle",
        method_name: "set",
        descriptor: VH_BYTE_BUFFER_VIEW_WRITE_DESCRIPTORS[slot],
        num_jit_args: 4,
        return_type: b'V',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}
pub static VH_BYTE_BUFFER_VIEW_WRITE_INFOS: [JitInvokeInfo; 6] = [
    vh_byte_buffer_view_write_info(0),
    vh_byte_buffer_view_write_info(1),
    vh_byte_buffer_view_write_info(2),
    vh_byte_buffer_view_write_info(3),
    vh_byte_buffer_view_write_info(4),
    vh_byte_buffer_view_write_info(5),
];
/// Five integer-word C ABI; floating values carry their exact raw bits.
/// Only judged sites are planned. Refusals retain the real descriptor and
/// normal dispatch, including exactness and exception priority.
pub unsafe extern "C" fn jit_varhandle_byte_buffer_view_write_direct<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    buffer: i64,
    index: i64,
    value: i64,
) -> i64 {
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let bits = contain(
        "jit_varhandle_byte_buffer_view_write_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            let args = [vh, buffer, index, value];
            if !site.is_null() {
                let array_view = (*site).descriptor.starts_with("([BI");
                let value_desc = cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[SLOT];
                let bits = if array_view {
                    varhandle_byte_array_view_bits(
                        vm,
                        vh as u64,
                        buffer as u64,
                        index,
                        value_desc,
                        Some(value),
                    )
                } else {
                    varhandle_cached_byte_buffer_view_bits(
                        vm,
                        vh as u64,
                        buffer as u64,
                        index,
                        value_desc,
                        Some(value),
                    )
                    .or_else(|| {
                        let (thread, _guard) = jit_thread_mut()?;
                        try_varhandle_byte_buffer_view_access(vm, &*site, &args, thread)
                    })
                };
                if let Some(bits) = bits {
                    note_vh_direct(VH_DIRECT_WRITE, true);
                    note_funnel_bypass_hit();
                    if crate::runtime::env_cache::dbg_jitc() {
                        BYTE_BUFFER_VIEW_DIRECT_WRITE_REPORTED_VM.with(|reported| {
                            let (identity, seen) = reported.get();
                            let seen = if identity == vm.vm_identity { seen } else { 0 };
                            let bit = 1u16 << (SLOT + if array_view { 6 } else { 0 });
                            if seen & bit == 0 {
                                reported.set((vm.vm_identity, seen | bit));
                                let family = if array_view { "byte-array" } else { "byte-buffer" };
                                eprintln!("[cratonvm-jitc] {family}-view-write-direct served vm={} slot={SLOT}", vm.vm_identity);
                            }
                        });
                    }
                    return bits;
                }
            }
            note_vh_direct(VH_DIRECT_WRITE, false);
            jit_invoke_dispatch(
                vm_ptr,
                varhandle_thin_cold_arm_info(site, &VH_BYTE_BUFFER_VIEW_WRITE_INFOS[SLOT]),
                args.as_ptr() as i64,
                4,
            )
        },
    );
    native_return_handed_to_compiled_code();
    bits
}

thread_local! {
    pub static BYTE_BUFFER_VIEW_DIRECT_WRITE_REPORTED_VM: std::cell::Cell<(usize, u16)> =
        const { std::cell::Cell::new((0, 0)) };
}
pub fn varhandle_byte_buffer_view_write_direct_fns() -> [usize; 6] {
    [
        jit_varhandle_byte_buffer_view_write_direct::<0> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<1> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<2> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<3> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<4> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<5> as *const () as usize,
    ]
}
pub fn varhandle_byte_buffer_view_write_direct_fn(slot: usize) -> usize {
    varhandle_byte_buffer_view_write_direct_fns()[slot]
}

pub fn varhandle_byte_array_view_read_direct_fns() -> [usize; 6] {
    [
        jit_varhandle_byte_array_view_read_direct::<0> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<1> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<2> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<3> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<4> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<5> as *const () as usize,
    ]
}
pub fn varhandle_byte_array_view_read_direct_fn(slot: usize) -> usize {
    varhandle_byte_array_view_read_direct_fns()[slot]
}

/// `CRATONVM_JIT_VARHANDLE_ARRAY_ELEMENT_FAST=0` -- send every array-element
/// `VarHandle` access back through the generic native, as before this fast
/// path existed. Default ON. Same role as the switches beside it: an A/B on
/// ONE binary. gci i27/kd: read once per VM
/// (`SharedVm::jit_varhandle_array_element_fast`,
/// [`varhandle_array_element_fast_from_flags`]); it was a process `OnceLock`
/// (the per-VM statics ratchet).
pub fn varhandle_array_element_fast_enabled(vm: &SharedVm) -> bool {
    vm.jit_varhandle_array_element_fast
}

/// The value rule of `CRATONVM_JIT_VARHANDLE_ARRAY_ELEMENT_FAST`, read from
/// the flag snapshot (once per VM, at construction): the default-ON opt-out
/// rule of [`cratonvm_types::flags::runtime_flag_default_on`] -- unset ON,
/// `0` / `false` / `off` / `no` OFF.
///
/// JIT round 25 review: it was a hand-rolled `Ok("0")` match, under which
/// `=false` / `=off` -- the spellings every other kill switch accepts -- left
/// the fast path on, silently (the `runtime_flag_default_on` doc's "kill
/// switch that cannot be turned off" defect), and the row's reader was
/// invisible to `types/tests/flag_default_matches_reader.rs`.
pub fn varhandle_array_element_fast_from_flags() -> bool {
    true
}


/// In-funnel array-element `VarHandle` accesses served as one hardware access,
/// reported by `CRATONVM_DBG=intrinsic-stats`. Zero beside a non-zero
/// site-cached count means every array handle site declined.
pub fn varhandle_array_element_served_count() -> u64 {
    jit_counter_total(|b| {
        b.vh_array_element_served
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

thread_local! {
    /// `(vm identity, VarHandleInts$Array class id, VarHandleLongs$Array class
    /// id)` as this thread last resolved them; `0` is "not resolved yet".
    pub static VH_ARRAY_CLASS_MEMO: Cell<(usize, u32, u32)> = const { Cell::new((0, 0, 0)) };
}

/// Is `class_id` the JDK's `VarHandleInts$Array` (`wide == false`) or
/// `VarHandleLongs$Array` (`wide == true`) -- the handles
/// `MethodHandles.arrayElementVarHandle(int[].class / long[].class)` builds,
/// which `AtomicIntegerArray` / `AtomicLongArray` keep in a `static final`.
///
/// Both are `final` classes, so the exact class id decides. The id is memoised
/// per thread after the first name read; a class that is not one of them is
/// answered by the name read every time, which only a site whose descriptor
/// already looks like an `int[]` / `long[]` element access reaches.
pub fn vh_array_handle_class_is(vm: &SharedVm, class_id: u32, wide: bool) -> bool {
    let (memo_vm, ints, longs) = VH_ARRAY_CLASS_MEMO.with(Cell::get);
    let known = if memo_vm == vm.vm_identity {
        if wide {
            longs
        } else {
            ints
        }
    } else {
        0
    };
    if known != 0 {
        return known == class_id;
    }
    let want = if wide {
        "java/lang/invoke/VarHandleLongs$Array"
    } else {
        "java/lang/invoke/VarHandleInts$Array"
    };
    // `try_read`: this runs in a dispatch that may sit inside a class
    // initialiser while the thread still owns the class-manager write lock
    // (`dispatch_virtual`'s `receiver_is_java_util` documents the same
    // hazard); a busy table declines, and the generic native answers.
    let Some(matches) = vm.classes.class_manager.try_read().map(|cm| {
        cm.get_class(ClassId::new(class_id))
            .is_some_and(|class| &*class.name == want)    }) else {
        return false;
    };
    if matches {
        VH_ARRAY_CLASS_MEMO.with(|memo| {
            let (id, mut ints, mut longs) = memo.get();
            if id != vm.vm_identity {
                ints = 0;
                longs = 0;
            }
            if wide {
                longs = class_id;
            } else {
                ints = class_id;
            }
            memo.set((vm.vm_identity, ints, longs));
        });
    }
    matches
}

/// What an array-element access does to the element, and with which ordering.
#[derive(Clone, Copy)]
pub enum VhArrayOp {
    Load(std::sync::atomic::Ordering),
    Store(std::sync::atomic::Ordering),
    CompareAndSet,
    GetAndAdd,
    GetAndSet,
}

/// An access to one element of a real `int[]` / `long[]` array `VarHandle`
/// (`MethodHandles.arrayElementVarHandle`), served as the one instruction it
/// is, in the dispatch funnel and before the generic `VarHandle` native.
///
/// `AtomicIntegerArray`, `AtomicLongArray` and netty's `MpscIntQueue` (the
/// free list of every `AdaptivePoolingAllocator` size class) reach the
/// handle's access modes. The generic native checks the handle's kind, arity,
/// coordinates, call-site return and boxing on every call; measured on a
/// single thread that is 1.2-1.6 us per `AtomicIntegerArray.get` against
/// 0.7 ns on HotSpot, and an alloc / grow / release cycle of the adaptive
/// allocator costs 17 us against 0.1 us.
///
/// Served only when every question the generic native would ask has a fixed
/// answer: the call site is exactly `([II)I`-shaped for the mode (a type the
/// handle does not take is `WrongMethodTypeException`'s, so it declines), the
/// receiver's class is exactly the JDK's final `VarHandleInts$Array` /
/// `VarHandleLongs$Array`, the array is a live, non-null, flat array of that
/// element type, and the index is in range. Anything else -- a null array, an
/// index out of bounds, a combinator handle, a `float[]` -- declines to the
/// generic native, which raises the exception HotSpot raises.
///
/// Every element of a primitive array is naturally aligned and flat on every
/// collector (`hw_cas_primitive_array_element`'s argument), so each mode is
/// one atomic access of the element's width. `get` / `getOpaque` / `set` /
/// `setOpaque` are relaxed; `getVolatile` / `getAcquire` acquire (a plain load
/// on x86); `setRelease` release; `setVolatile` sequentially consistent;
/// `compareAndSet` and its weak forms one strong `compare_exchange` (a weak
/// form may fail spuriously, never must); `getAndAdd` / `getAndSet` and their
/// acquire / release forms the sequentially consistent `fetch_add` / `swap`,
/// the strongest ordering, which is a legal implementation of each weaker one.
///
/// No safepoint, allocation or Java call lies between locating the element and
/// touching it, so a moving collector cannot invalidate the pointer. An `int` /
/// `long` array holds no references, so no barrier applies.
///
/// # Safety
/// `args` are the call's raw JIT argument words; `vm` is live.
pub unsafe fn try_varhandle_array_element(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
    let d = info.descriptor.as_bytes();
    // `([I` or `([J`, then the index `I`: every served shape.
    if d.len() < 5 || d[0] != b'(' || d[1] != b'[' || d[3] != b'I' {
        return None;
    }
    let wide = match d[2] {
        b'I' => false,
        b'J' => true,
        _ => return None,
    };
    let (op, tail, argc) = match info.method_name {
        "get" | "getOpaque" => (
            VhArrayOp::Load(Ordering::Relaxed),
            if wide { ")J" } else { ")I" },
            3,
        ),
        "getVolatile" | "getAcquire" => (
            VhArrayOp::Load(Ordering::SeqCst),
            if wide { ")J" } else { ")I" },
            3,
        ),
        "set" | "setOpaque" => (
            VhArrayOp::Store(Ordering::Relaxed),
            if wide { "J)V" } else { "I)V" },
            4,
        ),
        "setRelease" => (
            VhArrayOp::Store(Ordering::Release),
            if wide { "J)V" } else { "I)V" },
            4,
        ),
        "setVolatile" => (
            VhArrayOp::Store(Ordering::SeqCst),
            if wide { "J)V" } else { "I)V" },
            4,
        ),
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => (
            VhArrayOp::CompareAndSet,
            if wide { "JJ)Z" } else { "II)Z" },
            5,
        ),
        "getAndAdd" | "getAndAddAcquire" | "getAndAddRelease" => {
            (VhArrayOp::GetAndAdd, if wide { "J)J" } else { "I)I" }, 4)
        }
        "getAndSet" | "getAndSetAcquire" | "getAndSetRelease" => {
            (VhArrayOp::GetAndSet, if wide { "J)J" } else { "I)I" }, 4)        }
        _ => return None,
    };
    if &info.descriptor[4..] != tail || args.len() != argc {
        return None;
    }
    if !varhandle_array_element_fast_enabled(vm) {
        return None;
    }
    let vh_raw = args[0] as u64 as usize;
    let array_raw = args[1] as u64 as usize;
    // A word the compiled caller holds as an `int`: only its low half is the
    // index.
    let index = args[2] as i32;
    if vh_raw == 0 || array_raw == 0 || index < 0 {
        return None;
    }
    let (vh_class, _) = direct_receiver_facts(vm, vh_raw)?;
    if vh_class == 0 || !vh_array_handle_class_is(vm, vh_class, wide) {
        return None;
    }
    direct_receiver_facts(vm, array_raw)?;
    let heap = &vm.mem.heap;
    let array = heap.load_and_forward(ObjectRef::from_raw(array_raw as *mut u8));
    let element = if wide {
        ArrayElementType::Long
    } else {
        ArrayElementType::Int
    };
    if heap.kind_of(array) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(array) != element
    {
        return None;
    }
    let index = index as usize;
    if index >= heap.array_length(array) {
        return None;
    }
    let base = heap.array_data_ptr(array)?;
    if wide {
        let pointer = base.add(index * 8);
        if (pointer as usize) & 7 != 0 {
            return None;
        }
        let cell = AtomicI64::from_ptr(pointer.cast::<i64>());
        Some(match op {
            VhArrayOp::Load(order) => cell.load(order),
            VhArrayOp::Store(order) => {
                cell.store(args[3], order);
                0
            }
            VhArrayOp::CompareAndSet => i64::from(
                cell.compare_exchange(args[3], args[4], Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok(),
            ),
            VhArrayOp::GetAndAdd => cell.fetch_add(args[3], Ordering::SeqCst),
            VhArrayOp::GetAndSet => cell.swap(args[3], Ordering::SeqCst),
        })
    } else {
        let pointer = base.add(index * 4);
        if (pointer as usize) & 3 != 0 {
            return None;
        }
        let cell = AtomicI32::from_ptr(pointer.cast::<i32>());
        Some(match op {
            VhArrayOp::Load(order) => i64::from(cell.load(order)),
            VhArrayOp::Store(order) => {
                cell.store(args[3] as i32, order);
                0
            }
            VhArrayOp::CompareAndSet => i64::from(
                cell.compare_exchange(
                    args[3] as i32,
                    args[4] as i32,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok(),
            ),
            VhArrayOp::GetAndAdd => i64::from(cell.fetch_add(args[3] as i32, Ordering::SeqCst)),
            VhArrayOp::GetAndSet => i64::from(cell.swap(args[3] as i32, Ordering::SeqCst)),
        })
    }
}

pub unsafe fn try_varhandle_instance_field_read(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    thread: &mut JvmThread,
) -> Option<i64> {
    if !cratonvm_jit::VARHANDLE_READ_MODES.contains(&info.method_name) {
        return None;
    }
    // [VarHandle, receiver] and nothing else: a coordinate-carrying access
    // (array index, byte offset) is not an instance-field read.
    if args_slice.len() != 2 {
        return None;
    }
    let vh_raw = args_slice[0] as u64;
    let recv_raw = args_slice[1] as u64;
    if vh_raw == 0 || recv_raw == 0 {
        return None;
    }
    let bits = varhandle_instance_field_read_bits(
        vm,
        vh_raw,
        recv_raw,
        info.return_type,
        Some(&mut *thread),
    )?;
    // Round 15 wave 2 hand-back (lane vh2; VH2-3): a site declaring a narrower
    // reference than `Object` casts what it reads. A value the cast refuses
    // declines to the native road, whose `unbox_poly_return_checked_vh` raises
    // the `ClassCastException`; the field is a reference variable here (the
    // read agreed with a reference site), which is what makes a box a CCE.
    if bits != 0
        && crate::vm::vm_exec::vh_site_returns_narrow_reference(info.method_name, info.descriptor)
    {
        if let Some(site_ret) = info.descriptor.rsplit(')').next() {
            let value = Value::Object(Some(ObjectRef::from_raw(bits as usize as *mut u8)));
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            if cratonvm_native_builtins::lang_invoke::varhandle_reference_return_refusal(
                &mut ctx,
                value,
                site_ret,
                Some(true),
            )
            .is_some()
            {
                // The handoff root the read published is not handed off.
                thread.native_pending_return = None;
                return None;
            }
        }
    }
    Some(bits)
}

/// SBR3 lane ffmperf: `VarHandle.get(seg, long)` / `set(seg, long, v)` on a real
/// `SegmentVarHandle` over a real native segment, served from inside the funnel
/// as raw words -- no argument decode, no boxed `Object` return to unbox again,
/// no generic access-mode prologue. This is what
/// `MemorySegment.get(JAVA_INT, off)` compiles to once the JDK's own
/// `AbstractMemorySegmentImpl.get` runs.
///
/// Declines (to the funnel's native, which raises whatever it always raised) for
/// anything but `(segment, long)` / `(segment, long, carrier)` with the carrier
/// the handle's own, and for every access the lean path cannot prove
/// (`cratonvm_native_builtins::lang_invoke::segment_vh_lean_get_bits`).
///
/// SAFETY: the raw words come from a live compiled frame; both references are
/// heap-validated before any dereference, and nothing here allocates or reaches
/// a safepoint, so the forwarded references stay current.
pub unsafe fn try_varhandle_segment_access(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    thread: &mut JvmThread,
) -> Option<i64> {
    let write = match info.method_name {
        "get" => false,
        "set" => true,
        _ => return None,
    };
    if args_slice.len() != 3 + usize::from(write) {
        return None;
    }
    let vh_raw = args_slice[0] as u64;
    let seg_raw = args_slice[1] as u64;
    if vh_raw == 0 || seg_raw == 0 {
        return None;
    }
    // `(Lsegment;J)K` / `(Lsegment;JK)V`: the segment first, a `long` coordinate,
    // and for a store the carrier last.
    // Read from the tail, in constant time: this runs on every access.
    let d = info.descriptor.as_bytes();
    if d.len() < 7 || d[0] != b'(' || d[1] != b'L' {
        return None;
    }
    let n = d.len();
    let site_kind = if write {
        // `...;J<k>)V`
        match (d[n - 5], d[n - 4], d[n - 3], d[n - 2], d[n - 1]) {
            (
                b';',
                b'J',
                kind @ (b'I' | b'J' | b'F' | b'D' | b'Z' | b'B' | b'S' | b'C'),
                b')',
                b'V',
            ) => kind,
            _ => return None,
        }
    } else {
        // `...;J)<k>`
        if (d[n - 4], d[n - 3], d[n - 2]) != (b';', b'J', b')') {
            return None;
        }
        info.return_type
    };
    let (vh_class_id, vh_hash) = direct_receiver_facts(vm, vh_raw as usize)?;
    let (seg_class_id, _) = direct_receiver_facts(vm, seg_raw as usize)?;
    if vh_class_id == 0 || seg_class_id == 0 {
        return None;
    }
    let heap = &vm.mem.heap;
    let vh = heap.load_and_forward(ObjectRef::from_raw(vh_raw as usize as *mut u8));
    let seg = heap.load_and_forward(ObjectRef::from_raw(seg_raw as usize as *mut u8));
    let base = args_slice[2];
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    if write {
        cratonvm_native_builtins::lang_invoke::segment_vh_lean_set_bits(
            &mut ctx,
            vh,
            seg,
            base,
            site_kind,
            args_slice[3],
            vh_hash,
            seg_class_id,
        )
        .then_some(0)
    } else {
        cratonvm_native_builtins::lang_invoke::segment_vh_lean_get_bits(
            &mut ctx,
            vh,
            seg,
            base,
            site_kind,
            vh_hash,
            seg_class_id,
        )
    }
}

/// The read itself, shared by the funnel route above and the thin direct-call
/// helpers below so the two cannot drift apart in what they consider a
/// servable handle.
///
/// `site_ret` is the CALL SITE's declared return type — `info.return_type` on
/// the funnel route, the slot's baked return kind on the direct route. It is
/// what the funnel would have unboxed the erased `Object` result against, so
/// disagreeing with the variable's own kind is a refusal rather than a
/// conversion.
///
/// `thread` is required only to publish an OBJECT result as a handoff root;
/// a primitive-returning caller may pass `None`, and then a reference-valued
/// variable is declined instead of being returned unrooted.
///
/// SAFETY: `vh_raw`/`recv_raw` are raw references from a live compiled frame;
/// both are heap-validated here before any dereference.
pub unsafe fn varhandle_instance_field_read_bits(
    vm: &SharedVm,
    vh_raw: u64,
    recv_raw: u64,
    site_ret: u8,
    thread: Option<&mut JvmThread>,
) -> Option<i64> {
    // Both probes and the identity hash go through the per-epoch receiver memo.
    // This helper serves 269 768 215 calls on
    // `JdkZlibIntegrationTest#testHugeDecompress` — one per `ByteBuf.writeByte`,
    // from the `RefCnt` read inside `ensureAccessible` — and it asked
    // `is_object_address` TWICE and re-derived the handle's identity hash on
    // every one of them. `ZObjectStarts::contains` + `is_object_address` were
    // 10.3 % of that profile with every funnel already gone.
    //
    // The memo answers exactly what the two probes answered — "this address is
    // a live object base in this GC epoch" — and its `identity` is computed by
    // the same `java_identity_hash` arbitration this code used, so a thin-locked
    // or inflated header still yields the displaced value `vh_meta_get` filed
    // the handle under.
    let (vh_class_id, key) = direct_receiver_facts(vm, vh_raw as usize)?;
    if vh_class_id == 0 {
        return None;
    }
    let (recv_class_id, _) = direct_receiver_facts(vm, recv_raw as usize)?;
    if recv_class_id == 0 {
        return None;
    }
    let receiver = ObjectRef::from_raw(recv_raw as usize as *mut u8);
    let heap = &vm.mem.heap;
    // The plan of THIS handle of THIS VM (gc-common w36-a): its registry slot
    // at its current address, not the lone row filed under its hash.
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle(
        vm.vm_identity,
        key,
        vh_raw as usize,
    )?;
    // A receiver of an unrelated class would read slot `field_index` of the
    // wrong layout; the native answers it with a ClassCastException.
    if !varhandle_plan_admits_receiver(vm, &plan, recv_class_id) {
        return None;
    }
    // Reference/primitive agreement between the variable and the call site.
    let site_is_ref = matches!(site_ret, b'L' | b'[');
    let plan_is_ref = plan.value_desc == b'L';
    if site_is_ref != plan_is_ref || (!site_is_ref && site_ret != plan.value_desc) {
        return None;
    }
    // Same read `NativeContextImpl::get_field` performs, minus the class-id
    // and descriptor-cache round trip: the declared descriptor is already on
    // the plan, straight from the handle's own metadata.
    let receiver = heap.load_and_forward(receiver);
    let value = if !plan_is_ref {
        jit_compact_field_slot(receiver.as_ptr() as i64, plan.field_index as i64)
            .filter(|(_, storage)| {
                Some(*storage)
                    == cratonvm_types::FieldStorageKind::from_descriptor_byte(plan.value_desc)
            })
            .map(|(offset, storage)| {
                // The validated receiver was forwarded above, the layout
                // bounds this exact slot, and its primitive storage agrees
                // with the handle. References and legacy cells retain the
                // heap's coercion, unboxing and read barriers below.
                cratonvm_types::read_compact_field(
                    receiver.as_ptr().add(offset),
                    storage,
                    std::sync::atomic::Ordering::Acquire,
                )
            })
            .unwrap_or_else(|| {
                heap.get_field_as(receiver, plan.field_index as usize, plan.value_desc)
            })
    } else {
        heap.get_field_as(receiver, plan.field_index as usize, plan.value_desc)
    };
    // Round 15 wave 2 hand-back (lane vh2;
    // `r15w2-vh2-varhandle-acquire-reads-served-by-plain-loads-FIXED-20260930.md`):
    // `getAcquire`/`getVolatile` order the reads after them. A plain load
    // already does on x86-64 (TSO), which is left byte-for-byte as it was;
    // elsewhere every mode pays the acquire barrier (stronger than `get`/
    // `getOpaque` need, never weaker).
    #[cfg(not(target_arch = "x86_64"))]
    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    Some(match value {
        Value::Int(v) => v as i64,
        Value::Long(v) => v,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(obj)) => {
            // Object-return handoff root, same contract as every other JIT
            // native fast path (see `jit_integer_value_of_direct`): the
            // reference is live only in a register until the caller stores
            // it, so it has to be reachable across that window. With no
            // thread borrow there is nowhere to publish it, so the read is
            // declined rather than handed back unrooted.
            thread?.native_pending_return = Some(obj);
            obj.as_ptr() as i64
        }
        Value::Object(None) => 0,
        // A slot whose storage tag disagrees with the declared descriptor is
        // exactly what `get_field_as` exists to normalise; anything still
        // unexpected here goes back to the funnel rather than being guessed at.
        _ => return None,
    })
}

