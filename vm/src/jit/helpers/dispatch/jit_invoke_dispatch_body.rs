#[allow(unused_imports)]
use super::*;

/// Body of [`jit_invoke_dispatch`], run under its panic guard.
pub unsafe fn jit_invoke_dispatch_body(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    let res = jit_invoke_dispatch_body_inner(vm_ptr, info_ptr, args_ptr, num_args);
    if res != i64::MIN && num_args == 3 && info_ptr != 0 {
        let info = &*(info_ptr as *const JitInvokeInfo);
        if info.class_name == "java/lang/reflect/Method"
            && info.method_name == "invoke"
            && info.descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
        {
            crate::jit::helpers::dispatch::try_populate_method_invoke_cache(
                vm_ptr, info, args_ptr,
            );
        }
    }
    res
}

unsafe fn jit_invoke_dispatch_body_inner(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // JIT round 14 wave 2 (lane mic, M8-1): an inline-cache quiescent point
    // (no probe in flight on this thread). Must stay first.
    cratonvm_jit::jit_ic_quiescent_point();
    // JIT round 16 (lane mh, MH15-5): a bound constant-handle `invokeExact`
    // site dispatches its target directly (`mh_constant_bind_redirect`).
    let mut mh_link_threw = false;
    if let Some(route) =
        mh_constant_bind_redirect(vm_ptr, info_ptr, args_ptr, num_args, &mut mh_link_threw)
    {
        return mh_constant_bind_call(vm_ptr, route, args_ptr, num_args);
    }
    if mh_link_threw {
        // Round 18 wave 11 (lane mhresid): the site's link stashed its error.
        return i64::MIN;
    }
    // JIT round 20 wave 19, lane pRefl (`CRATONVM_JIT_MH_COMPILED_LINK`): a
    // linked `invokeExact`, an `invokeBasic` or a `linkTo*` calls its Java
    // target as a compiled call, not through its native door.
    if let Some(bits) = mh_compiled_link_redirect(vm_ptr, info_ptr, args_ptr, num_args) {
        return bits;
    }
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush. A dispatched
    // call may transitively enter the GC barrier through callee
    // allocations or `Object.wait` paths.
    jit_safepoint_flush_satb(vm_ptr);
    mic_prof::dump_maybe_disp();
    let _cyc_disp = mic_prof::CycGuard::new(&mic_prof::CYC_DISP_TOTAL);
    // WS1 diagnostic: per-call dispatch trace, naming the callee that encloses
    // the lost wall time.
    //
    // This used to ride on `mic_prof::enabled()`, justified by "the dispatch
    // helper is cold enough on the kafka repro — ~15 calls — that an eprintln
    // per call is affordable". On a call-dense workload it is not cold: on
    // `ZipContentTests` the same switch wrote **268 MB of stderr in 73 seconds**
    // and had to be killed, so the COUNTERS — which are the reason to reach for
    // MIC_PROF at all, and are cheap — could not be read on the one workload
    // that needed them. A per-call `eprintln` and a set of atomic counters do
    // not belong on one switch. The trace now has its own.
    let _disp_trace = if mic_prof::trace_enabled() {
        let info = &*(info_ptr as *const JitInvokeInfo);
        struct DispTrace {
            label: String,
            t0: u64,
        }
        impl Drop for DispTrace {
            fn drop(&mut self) {
                eprintln!(
                    "[DISP_TRACE] {} cycles={}",
                    self.label,
                    mic_prof::now().wrapping_sub(self.t0)
                );
            }
        }
        Some(DispTrace {
            label: format!(
                "{}.{}{} kind={}",
                info.class_name, info.method_name, info.descriptor, info.invoke_kind
            ),
            t0: mic_prof::now(),
        })
    } else {
        None
    };
    // SAFETY: vm_ptr and info_ptr originate from JIT code; both point to valid, live objects.
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    disp_census::note_kind(info.invoke_kind);
    // DIAGNOSTIC (gated): record the dispatched callee (restored on return) so
    // a downstream jit_putfield_int miscompile can name the offending method.
    let _callee_guard = if crate::runtime::env_cache::jit_putfield_diag() {
        Some(JitCalleeGuard::new(info))
    } else {
        None
    };
    // Defensive gate: when the user-facing CRATONVM_DISABLE_JIT kill-switch is set,
    // no JIT code should be executing — so this dispatch helper must never run.
    // Reaching it means a JIT entry point bypassed the flag (a real bug). Returning
    // 0 here is preferable to UB from a stale compiled callsite; emit a one-shot
    // warning so the bypass is visible during bisection.
    if crate::runtime::env_cache::disable_jit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        pub static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "[cratonvm] WARN: jit_invoke_dispatch reached with CRATONVM_DISABLE_JIT=1 \
                 (callee {}.{}{}). A JIT entry-point bypassed the kill-switch — \
                 returning 0 to avoid undefined behavior.",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        return 0;
    }
    if crate::runtime::env_cache::jit_dispatch_dbg() {
        let p = args_ptr as *const i64;
        let mut buf = String::new();
        if !p.is_null() && num_args > 0 {
            for i in 0..(num_args as usize).min(4) {
                // SAFETY: `p` is non-null (checked above) and `i < num_args`, so
                // `p.add(i)` stays inside the JIT-provided args slice (debug path only).
                let v = unsafe { *p.add(i) };
                buf.push_str(&format!(" arg{}=0x{:x}", i, v));
            }
        }
        eprintln!(
            "[JIT_DISPATCH] {}.{}{} kind={} num_args={}{} info_ptr=0x{:x}",
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
            num_args,
            buf,
            info_ptr,
        );
    }
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        return 0;
    }
    // SAFETY: args_ptr is non-null (checked above) and num_args >= 0.
    // The JIT caller allocated this array on its own stack frame.
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    // The SATB flush above can participate in a moving collection before the
    // helper reads the JIT caller's raw argument array.  Forward every
    // descriptor-declared reference, not only `this`: a compiled callee must
    // not receive a stale byte-array/object argument after a moving GC.
    //
    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`, JIT round 19 wave 2, lane
    // funnel2): the same pass, also recording which words it validated, so
    // the native-site door below does not walk them again. Nothing between
    // here and that door is a GC point (null tests, memo probes, the
    // object-native cache, whose leaves decline before their native runs).
    let (forwarded_args, prevalidated) = if vm.natives.native_methods.jit_native_walk_once() {
        forward_jit_reference_args_validated(vm, info, args_slice)
    } else {
        (forward_jit_reference_args(vm, info, args_slice), 0)
    };
    let args_slice = forwarded_args.as_deref().unwrap_or(args_slice);
    // jr20 wave 9, lane gcE (gch-h1jp item 2): a forwarded Rust copy of the
    // arguments is in no root set, so every arm below that calls a compiled
    // body with it roots its words across the call and the sentinel service
    // (`CompileArgPins`, read back through `args_now`): a re-run from entry
    // after a moving collection inside the callee must not read the copy's
    // pre-move words. Rare (ZGC's lazy forwarding), so the cost is too.
    let args_forwarded = forwarded_args.is_some();

    // JVMS 6.5: a statically bound INSTANCE site (`invoke_kind == 1`:
    // `invokespecial`, or an `invokevirtual` of a final / private method the
    // compiler devirtualized) with a null receiver raises NPE AT THE INVOKE,
    // before the callee exists. Every compiled-callee arm below
    // (`DISPATCH_CACHE`, the `jit_cache` probe, the tier-up compile) CALLs the
    // callee's entry with `args_slice` as it stands, and a compiled body elides
    // the null check of `this` (`aload_0` of an instance method is a non-null
    // local, `null_check_elim`), so a null receiver ran the callee with
    // `this == null` and its first field read was a bare SIGSEGV
    // (`R16ThrowpathPlain npeCall`: `Holder.get` entered from the blind
    // dispatch of a cold devirtualized `vNullHolder.get()` site once its
    // per-site counter compiled the callee; the GC_FLAGS read `[RAX + 7]` of
    // the compact `getfield` arm with RAX = 0). Raised as the `0 | 2` arm's
    // tail raises it: the pending-NPE action and the sentinel, so the
    // interpreter's drain builds HotSpot's `Cannot invoke "..."` message from
    // the invoke's bci. The direct-call lowerings already test the receiver
    // themselves (`emit_direct_cross_call`, the single-pass direct arm); this
    // is the helper door's copy of that test.
    if info.invoke_kind == 1 && args_slice.first() == Some(&0) {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }

    // Interpreter round i7 wave 136, lane L2
    // (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, performance, default OFF):
    // the receiver check of an interface sender's `invokespecial` a
    // method-entry door compiled through this helper instead of refusing the
    // method. After the null test (HotSpot checks a non-null receiver only)
    // and before every arm that could call the callee. One per-VM bool read
    // per statically bound dispatch with the switch off.
    if info.invoke_kind == 1 {
        if let Some(bits) = iface_special_receiver_guard(vm, info, args_slice) {
            return bits;
        }
    }

    // nio21 wave 20 L1 (`CRATONVM_JIT_DIRECT_THROW_NEW`): `new X(msg)` of a
    // JDK throwable whose constructor chain is the JDK 25 one, built without
    // interpreting that chain (`exceptions::jit_new_throwable_direct`). The
    // receiver is forwarded and non-null here; `<init>` returns nothing.
    if info.invoke_kind == 1
        && info.method_name == "<init>"
        && try_throwable_init_direct_at_dispatch(vm, info, args_slice)
    {
        return 0;
    }

    // GPU offload, from COMPILED code.
    //
    // Placed here and not later: every probe below this line is a
    // per-callsite memo that ends in a CALL to the callee, so reaching any
    // of them has already decided not to offload. Placed here and not
    // EARLIER because it needs `args_slice` post-forwarding -- a moving
    // collection during the SATB flush above can have relocated the very
    // arrays the kernel is about to marshal.
    #[cfg(feature = "gpu-offload")]
    if info.invoke_kind == 3 && cratonvm_jit::offload_hook::any_kernels() {
        if let Some(ret) = try_compiled_offload(vm, info, info_ptr as usize, args_slice) {
            return ret;
        }
    }

    // JIT dispatch normally calls a custom loader's inherited bytecode
    // directly. ClassLoader's resource methods must throw NPE for a null name
    // before that bytecode runs; the sentinel routes it through Java handlers.
    // ONLY for a class-loader receiver: see
    // `null_resource_name_receiver_is_class_loader`.
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
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
        )
        && null_resource_name_receiver_is_class_loader(
            vm,
            args_slice[0],
            info.method_name,
            info.descriptor,
        )
    {
        set_jit_pending_npe();
        return i64::MIN;
    }

    // `(vm_identity, info pointer)`. The VM half is not decoration: the direct
    // helpers below pass the address of a process-global `static JitInvokeInfo`,
    // which is identical in every VM. See [`JitSiteKey`].
    //
    // Revalidate BEFORE the first memo read, not after the compile probes: the
    // native fast paths immediately below are memo consults keyed on this very
    // address, and a `JitInvokeInfo` box is freed with its `CompiledMethod`, so
    // the address can already belong to a different call site. This call was
    // further down (past those probes) and left them serving the previous
    // owner's native. See `flush_raw_entry_dispatch_caches`.
    flush_raw_entry_dispatch_caches();
    let info_key = jit_site_key(vm.vm_identity, info_ptr as usize);
    // Diagnostic (`CRATONVM_DBG_SITE_ALIAS`): does this key still name its site?
    // The gate is here rather than inside, so an all-off run does not make the
    // call at all — it measured 2.2 ns switched off, which is a fifth of the
    // argument decode it precedes.
    if site_alias_detect_enabled() {
        note_site_identity(info_key, info);
    }
    // Cached exact-receiver native fast path — the FIRST per-callsite probe. The
    // resolution/insertion slow path stays further down (after the compile
    // probes); this early block only serves sites the cache has already
    // resolved. Rationale: `HashMap.put/get` and the real-layout Matcher
    // operations are registered natives with no
    // bytecode, so neither the Integer cache below nor the virtual-dispatch
    // machinery can ever serve them — yet every map call paid those probes
    // first. Probing the map cache first costs the (now direct-called on
    // x64, hence rarely dispatched) Integer sites one extra hash lookup and
    // saves one on every cached native operation. The receiver class-id equality
    // check preserves the exact-receiver guard; the `any_class_redefined`
    // gate matches the resolution site below. Runs ahead of the recursion
    // depth guard like the Integer block: the cached callbacks are native
    // leaves that never re-enter JIT code.
    if matches!(info.invoke_kind, 0 | 2) && !args_slice.is_empty() {
        let cached =
            OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
        if let Some(entry) =
            cached.filter(|entry| !class_was_redefined(vm, ClassId::new(entry.receiver_class_id)))
        {
            let receiver_raw = args_slice[0] as u64;
            if receiver_raw != 0 && (receiver_raw & 0x7) == 0 && receiver_raw < (1u64 << 48) {
                crate::jit::helpers::note_membership_walk(3);
                if let Some(receiver) = vm.mem.heap.is_object_address(receiver_raw as usize) {
                    if jit_dispatch_class_validated(vm, receiver)
                        .class_id()
                        .map(|cid| cid.as_u32())
                        == Some(entry.receiver_class_id)
                    {
                        if let Some((thread, _guard)) = jit_thread_mut() {
                            if let Some(result) = call_object_native_raw(
                                vm, thread, info, receiver, args_slice, entry,
                            ) {
                                disp_census::note(disp_census::OUT_OBJECT_NATIVE);
                                return result;
                            }
                        }
                    }
                }
            }
        }
    }
    // Leaf-native fast path. Placed here — after the exact-receiver object
    // native cache above, before the Integer cache and the whole compile /
    // virtual-dispatch machinery below — because a leaf native is by definition
    // a call with nothing to compile and nothing to dispatch: a field read, an
    // atomic, or a constant. Everything after this point is per-call work that
    // such a site was paying for no reason. See `NativeSiteCache`.
    // Unlike the `jit_invoke_virtual_mic` call site, no borrow is live here —
    // the nearest one above is scoped to an `if let` that has already closed —
    // so this one acquires it, and the guard's scope is exactly the call.
    //
    // The borrow is passed as an `Option` rather than being required, so a run
    // with no JIT thread installed still ENTERS the callee and still runs its
    // counted pre-resolution bails; the `?` inside consumes the `None` at the
    // same point the callee's own `jit_thread_mut()?` used to. Requiring it
    // here instead would have skipped those `site_refusal::note_and_decline`
    // counters, which the callee's own comment calls out as the thing that
    // made its first cut unexplainable.
    let mut thread_and_guard = jit_thread_mut();
    let site_native = try_jit_site_cached_native_dispatch(
        vm,
        thread_and_guard.as_mut().map(|(t, _)| &mut **t),
        info,
        info_key,
        args_slice,
        prevalidated,
    );
    drop(thread_and_guard);
    if let Some(result) = site_native {
        disp_census::note(disp_census::OUT_SITE_NATIVE);
        return result;
    }
    // `--real-jdk` (interpreter round i1 wave 38, lane L4): an invoke-exact
    // `VarHandle` at a site the native site cache declined (the cache switched
    // off, a receiver-class change, a refused capability) is judged here, with
    // the site's REAL descriptor, before the tail (`vm_exec`'s
    // signature-polymorphic block, which also serves the thin helpers' cold
    // arms and so cannot judge) converts it. A name test, then the one relaxed
    // load `try_jit_site_cached_native_dispatch` pays. A thin helper's
    // stand-in site is judged against the descriptor its compiled caller's
    // invoke names (wave 49, `varhandle_stand_in_site_descriptor`). Only an
    // access-mode name: `VarHandle$AccessMode.ordinal()` matches the owner
    // test too, and its receiver is no `VarHandle`.
    if crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(info.class_name)
        && crate::vm::vm_exec::is_signature_polymorphic_method_name(info.method_name)
    {
        if let Some(exact_slot) = crate::runtime::interpreter::var_handle_exact_gate(vm) {
            if let Some(error) = jit_var_handle_exact_refusal(vm, info, args_slice, exact_slot) {
                return error;
            }
        }
    }
    // Only a `java/lang/Integer` site can resolve to an entry below (both match
    // arms name it), so every other site is answered by the name test, which
    // for a different-length name is one length compare. Before this, EVERY
    // dispatch paid the redefinition check and a hash probe here to learn
    // "no"; once any class has been redefined (an agent, Mockito) that check
    // takes the class-manager read lock and does a by-name class lookup, per
    // call, on every site.
    if info.class_name == "java/lang/Integer" && !site_class_was_redefined(vm, info, info_key) {
        let cached =
            INTEGER_NATIVE_DISPATCH_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
        let integer_native = match cached {
            Some(entry) => entry,
            None => {
                // Cold, once per call site. The `(kind, callback)` table is
                // unchanged; `admit_jit_fast_native` then asks the registry
                // what `java/lang/Integer.valueOf`/`intValue` are actually
                // registered as and lets `resolve_native_dispatch_wave1`
                // decide. Under `Compatible` it hands the same callback back
                // and adds only the census handle; under `JdkOnly` a refusal
                // caches `None`, so the boxing fast path simply stops existing
                // at this site and every call falls through to the generic,
                // policy-checked tail. No strict-mode branch is added to the
                // dispatch path itself.
                let entry = match (info.class_name, info.method_name, info.descriptor) {
                    ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;") => {
                        admit_jit_fast_native(
                            vm,
                            "java/lang/Integer",
                            info.method_name,
                            info.descriptor,
                            cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_value_of,
                        )
                        .map(|(callback, native_id)| IntegerNativeDispatchCache {
                            kind: IntegerNativeKind::ValueOf,
                            callback,
                            native_id,
                        })
                    }
                    ("java/lang/Integer", "intValue", "()I") => admit_jit_fast_native(
                        vm,
                        "java/lang/Integer",
                        info.method_name,
                        info.descriptor,
                        cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_int_value,
                    )
                    .map(|(callback, native_id)| IntegerNativeDispatchCache {
                        kind: IntegerNativeKind::IntValue,
                        callback,
                        native_id,
                    }),
                    _ => None,
                };
                INTEGER_NATIVE_DISPATCH_CACHE.with(|cache| {
                    cache.borrow_mut().insert(info_key, entry);
                });
                entry
            }
        };
        if let Some(entry) = integer_native {
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Some(result) = call_integer_native_raw(vm, thread, info, args_slice, entry) {
                    disp_census::note(disp_census::OUT_INTEGER_NATIVE);
                    return result;
                }
            }
        }
    }
    // BUG-1: native-stack recursion guard for the JIT→JIT dispatch path. Bump
    // the per-thread JIT-dispatch depth and check it against the native-stack
    // ceiling BEFORE we recurse into any compiled callee. On overflow this
    // stashes a catchable `StackOverflowError` and returns the `i64::MIN`
    // deopt sentinel instead of overflowing the OS stack. The guard is held
    // for the rest of this call so the level is released on every return path.
    let _jit_dispatch_depth_guard =
        match enter_jit_dispatch(vm, constructor_receiver(info, args_slice)) {
            Ok(g) => g,
            Err(sentinel) => return sentinel,
        };

    // Fast path: check thread-local dispatch cache for a previously-compiled callee.
    // This avoids the JIT cache lock on every call.
    // Virtual/interface dispatch (invoke_kind 0/2) must resolve on the RUNTIME
    // receiver type. The callsite-keyed entry cache below and the
    // `info.class_name` (static CP class) JIT-cache lookup both assume static
    // binding, so reusing them at a polymorphic call site dispatches a
    // SUPERTYPE method — e.g. `Object.equals` (identity `==`) run on boxed
    // `Integer` receivers, so two equal Integers compare unequal and junit
    // `assertEquals` fails on identical values. Only the statically-bound kinds
    // (invokespecial=1, invokestatic=3) may use these fast paths; virtual /
    // interface fall through to the receiver-resolving slow path
    // (`ctx.invoke_virtual`). Hot monomorphic virtual sites are already served
    // by the receiver-guarded MIC helper (`jit_invoke_virtual_mic`).
    let statically_bound = matches!(info.invoke_kind, 1 | 3);
    // Class identity first: a name that no longer resolves to the ClassId a
    // memo recorded it against invalidates every dispatch memo below, and none
    // of the three signals that follow can see that happen.
    flush_class_identity_dispatch_memos(vm);
    let redefine_jit_quiesced = site_quiesced_for_redefinition(vm, info, info_key);
    // Once per redefinition per thread, not once per call. The answer above is
    // true until this flush has run for the current epoch (it was permanently
    // true under `CRATONVM_JIT_QUIESCE_BY_GENERATION=0`, and still is), and this
    // flush empties the WHOLE thread's two caches, so every call through such
    // a site (a mocked class, an agent-woven one) used to throw away every
    // other site's memo on this thread, for the rest of the process: the
    // per-dispatch flush shape the MIC's `redefine_epoch` stamp was written to
    // remove. The quiesced site itself never reads either cache (every consult
    // below is gated on `!redefine_jit_quiesced`), and staleness from the
    // redefinition is `flush_raw_entry_dispatch_caches`' job (the full
    // eviction in `redefineClass` advances the JIT generation). So one flush
    // per epoch keeps what this line was for.
    if redefine_jit_quiesced {
        let stamp = (vm.vm_identity, vm.jit.jit_cache.redefine_epoch());
        if QUIESCED_SITE_FLUSH_STAMP.with(|seen| seen.replace(stamp)) != stamp {
            DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
            VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
        }
    }
    flush_raw_entry_dispatch_caches();
    // Interpreter-only mode (a JVMTI interpreter-only event, or a JDWP
    // breakpoint / step / suspension; `jvmti_requires_interpreter`): the four
    // compiled-entry arms below stand down with the redefinition quiescence,
    // so a call from a frame that is ALREADY compiled runs its callee through
    // the interpreted tail, where the events and the breakpoints are delivered.
    // The interpreter-side doors only stop NEW compiled frames. Interpreter
    // round i1 wave 5, L7.
    let compiled_entry_forbidden = redefine_jit_quiesced
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
        || (vm.config.compatibility_mode == cratonvm_types::CompatibilityMode::Compatible
            && crate::runtime::interpreter::http_carrier_declaring_class(info.class_name));

    // The argument words as they stand after the synchronous callee compile
    // below, when a collection inside it moved one. Every later reader of
    // `args_slice` — the fresh call, the fallback decode, the slow tail —
    // must see these, not the staging block. See [`CompileArgPins`].
    let mut virtual_compile_moved_args: Option<Vec<i64>> = None;

    // A JIT call-site's CP owner is often an interface. Resolve once on the
    // actual receiver class, then cache and directly enter the compiled
    // concrete body. The generic helper otherwise re-enters invoke_virtual on
    // every element access, rebuilding conservative JIT roots each time.
    if direct_virtual_compiled_callee_entry_enabled()
        && !statically_bound
        && !compiled_entry_forbidden
        && !args_slice.is_empty()
    {
        let raw = args_slice[0] as u64;
        if raw != 0 && (raw & 7) == 0 && raw < (1u64 << 48) {
            let receiver = ObjectRef::from_raw(raw as usize as *mut u8);
            // The cache key below is `(site, class id)`; an array has none
            // (`jit_dispatch_class`). `globally_named` is already false for one
            // (`virtual_dispatch_target_cached`), and the key says so too.
            let receiver_dispatch_cid = jit_dispatch_class(vm, receiver).class_id();
            let receiver_cid = receiver_dispatch_cid.unwrap_or_else(|| ClassId::new(0));
            // Round 12 wave 2 (lane `calls`): the memo consult FIRST, before
            // the target resolution below, as `jit_invoke_virtual_mic` hoisted
            // its own. A hit's key carries the receiver's own class id and was
            // inserted only for a receiver-derived target (the publication
            // condition), so re-deriving the target on a hit (a per-thread memo
            // read and an `Rc<str>` clone per call) cannot change the answer.
            // Only a live entry is taken here; a stale one is left for the
            // consult below, which drops it and re-resolves at once.
            if receiver_dispatch_cid.is_some() && dispatch_virtual_memo_first_enabled() {
                let key = (info_key, receiver_cid.as_u32());
                let fresh = VIRTUAL_DISPATCH_CACHE.with(|dc| {
                    dc.borrow()
                        .get(&key)
                        .filter(|c| !c.names_a_stale_body())
                        .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
                });
                if let Some(cached) = fresh {
                    let routed = call_compiled_then_route(
                        vm,
                        info,
                        args_slice,
                        receiver_cid,
                        cached.3 || args_forwarded,
                        || match cached.2.as_ref() {
                            Some(owner) => try_call_compiled_entry_reentrant_owned(
                                owner, cached.0, cached.1, vm_ptr, args_slice,
                            ),
                            None => try_call_compiled_entry_reentrant(
                                cached.0, cached.1, vm_ptr, args_slice,
                            ),
                        },
                    );
                    if let Some(v) = routed {
                        disp_census::note(disp_census::OUT_VIRT_CACHE);
                        return v;
                    }
                }
            }
            // Read before the target is resolved: the shared-table publication
            // on the fill below withdraws itself across a redefinition.
            let dispatch_epoch_seen = vm.jit.jit_cache.redefine_epoch();
            // A `selection_error` target is never `cacheable_receiver`, so it
            // fills nothing below and reaches the by-name tail, which raises it.
            let target = virtual_dispatch_target_cached(vm, receiver, info, info_key);
            // Gated on a receiver-derived target, not on `globally_named`: the
            // compile below is asked for the receiver's OWN class id, so a class
            // name another loader also defines compiles this receiver's copy,
            // and the memo it fills is keyed on that id
            // (`i1-L7-callee-compile-doors-are-loader-blind`). An ARRAY
            // receiver has no dispatch class of its own (round 11 wave 13:
            // `jit_dispatch_class`), so it never fills this cache.
            //
            // Round 17 wave 3 (lane mega3, MEGA2-17-2, under
            // `CRATONVM_JIT_MEGA_CACHE_RETARGETED`): a receiver whose §5.4.6
            // selection moved to a superclass (`selected_cid != 0`, set only
            // under the switch) is receiver-derived too. Its body is compiled
            // for the SELECTED class (`compile_cid`) and memoized/published
            // under the RECEIVER's id, exactly as the MIC arm does: the memo
            // key is (this site, receiver class) and the table key (selector,
            // receiver class), and selection is a function of those alone.
            // Every later reader (`mic_callee_has_exception_table`,
            // `call_compiled_then_route`) resolves the callee through
            // `select_for_jit_receiver` from the receiver's id, so it judges
            // the selected body.
            let receiver_derived = target.publishable() && receiver_dispatch_cid.is_some();
            let compile_cid = ClassId::new(target.compile_cid(receiver_cid.as_u32()));
            // RBC.6 perf follow-up (docs/feature-designs/jit-local-exception-handlers.md)
            // — this used to also require `!mic_callee_has_exception_table(...)`,
            // excluding ANY callee that declares a local exception table from
            // this cache entirely and forcing every such call through the
            // "generic helper" fallback this comment block warns is expensive
            // ("re-enters invoke_virtual on every element access, rebuilding
            // conservative JIT roots each time"). Unlike the INLINE machine-code
            // MIC/PIC cascade (`jit/src/x64.rs`, guarded by the SAME check at its
            // own publish sites — see `BUG-H` comments there — which really does
            // bypass Rust-level exception routing since it CALLs the raw entry
            // pointer directly from compiled machine code), THIS cache is a plain
            // Rust `HashMap` consulted from inside this same Rust function — a hit
            // still calls `try_call_compiled_entry_reentrant` and then
            // `route_implicit_exc_through_callee` below EXACTLY as a cache miss
            // would, so the callee's own exception table is routed identically
            // either way. Excluding it here bought no correctness and cost a real
            // ~450s/round regression for `Response.toAbsolute()` once RBC.6 let it
            // compile (confirmed via the Tomcat suite's `TestResponsePerformance`
            // on the Linux build host).
            if receiver_derived {
                let key = (info_key, receiver_cid.as_u32());
                // An entry naming a superseded or invalidated body is dropped
                // at the hit (`DispatchCache::names_a_stale_body`), and the site
                // re-resolves NOW rather than after re-counting to the
                // threshold: the replacement is already published, so the
                // compile probe below only has to find it.
                let mut stale_dropped = false;
                if let Some(cached) = VIRTUAL_DISPATCH_CACHE.with(|dc| {
                    let mut dc = dc.borrow_mut();
                    if dc.get(&key).is_some_and(DispatchCache::names_a_stale_body) {
                        dc.remove(&key);
                        stale_dropped = true;
                        return None;
                    }
                    dc.get(&key)
                        .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
                }) {
                    let routed = call_compiled_then_route(
                        vm,
                        info,
                        args_slice,
                        receiver_cid,
                        cached.3 || args_forwarded,
                        || match cached.2.as_ref() {
                            Some(owner) => try_call_compiled_entry_reentrant_owned(
                                owner, cached.0, cached.1, vm_ptr, args_slice,
                            ),
                            None => try_call_compiled_entry_reentrant(
                                cached.0, cached.1, vm_ptr, args_slice,
                            ),
                        },
                    );
                    if let Some(v) = routed {
                        disp_census::note(disp_census::OUT_VIRT_CACHE);
                        return v;
                    }
                } else {
                    let should_compile = stale_dropped || {
                        let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                        let count = VIRTUAL_DISPATCH_COUNTER.with(|dc| {
                            let mut counts = dc.borrow_mut();
                            let count = counts.entry(key).or_insert(0);
                            *count = count.wrapping_add(1);
                            // Round 16 wave 17 (lane mega5): the late-probe
                            // mark is not part of the count
                            // (`blind_dispatch_late_probe_rewind`).
                            *count & !BLIND_LATE_PROBE_MARK
                        });
                        // Round 16 wave 8 (lane mega): a spent key re-probes
                        // at doublings of the threshold under
                        // `CRATONVM_JIT_BLIND_DISPATCH_REPROBE`, and (shape 6)
                        // a way the shared table already holds cues the probe
                        // below now, under `CRATONVM_JIT_MEGA_BLIND_DISPATCH_HINT`.
                        count == threshold
                            || blind_dispatch_spent_reprobe(count, threshold)
                            || blind_dispatch_table_hint(
                                vm,
                                info,
                                key,
                                receiver_cid.as_u32(),
                                count,
                                threshold,
                            )
                    };
                    if should_compile {
                        let (compiled, moved) =
                            compile_with_ref_args_pinned(vm, info, args_slice, || {
                                // j27 wave 3 (lane DISPATCH2): probe-only
                                // when the manager owns the compile
                                // (`virtual_dispatch_counter_door_miss`).
                                crate::runtime::interpreter::jit_bridge::try_jit_compile_callee_for_class_on_miss(
                                    vm,
                                    // The receiver's own class, or the
                                    // §5.4.6-selected one (MEGA2-17-2).
                                    compile_cid,
                                    &target.class_name,
                                    info.method_name,
                                    info.descriptor,
                                    true,
                                    virtual_dispatch_counter_door_miss(),
                                )
                                // JIT r20 w19 yLadder (DISP20-7): an inherited
                                // method's DECLARING-class body, filled and
                                // called below as a probe answer.
                                .or_else(|| probe_selected_declaring_body(vm, info, receiver_cid))
                            });
                        virtual_compile_moved_args = moved;
                        let args_slice =
                            virtual_compile_moved_args.as_deref().unwrap_or(args_slice);
                        if let Some((_callee_pin, entry, needs_context)) = compiled {
                            // Asked once per (site, receiver class), here on
                            // the cold fill, and memoized with the entry.
                            let rerun_pins = mic_callee_has_exception_table(vm, receiver_cid, info);
                            // `_callee_pin` keeps the callee mapped across the
                            // cache publication and the direct call below.
                            // Only publish a raw entry we can keep alive:
                            // `_owner` is the sole keep-alive for this
                            // thread-local pointer, so caching with `None`
                            // would let a later tier-up `put` unmap the body
                            // under it.
                            if let Some(owner) = cratonvm_jit::pin_jit_entry(entry) {
                                VIRTUAL_DISPATCH_CACHE.with(|dc| {
                                    dc.borrow_mut().insert(
                                        key,
                                        DispatchCache {
                                            entry,
                                            needs_context,
                                            owner: Some(std::rc::Rc::new(owner.into())),
                                            rerun_pins,
                                        },
                                    );
                                });
                            }
                            // Round 12 wave 3 (lane mega2, proposal W2-1 step
                            // 3): and the VM's shared megamorphic table, so the
                            // hashed stubs of every inline-cached site that
                            // resolves this method (same selector) find the
                            // body without a helper round trip. This site is
                            // blind (no PIC), so it only publishes. Admission
                            // is `jit_invoke_virtual_mic`'s for its own PIC: a
                            // receiver-derived target compiled for this
                            // receiver's class id (above), never an
                            // indy-trapping body (the MIC arm's rule for every
                            // machine-called cache), and a table callee only
                            // where that arm would publish one
                            // (`publish_mega_dispatch_entry`).
                            if mega_table_publish_dispatch_enabled()
                                && !_callee_pin.has_indy_trap
                                && (!rerun_pins
                                    || (mega_table_exc_callees_enabled()
                                        && mic_publish_exception_table_callees()))
                            {
                                install_mega_dispatch_way(
                                    vm,
                                    info,
                                    None,
                                    receiver_cid.as_u32(),
                                    entry as u64,
                                    needs_context,
                                    dispatch_epoch_seen,
                                );
                            }
                            let routed = call_compiled_then_route(
                                vm,
                                info,
                                args_slice,
                                receiver_cid,
                                rerun_pins || args_forwarded,
                                || {
                                    try_call_compiled_entry_reentrant(
                                        entry,
                                        needs_context,
                                        vm_ptr,
                                        args_slice,
                                    )
                                },
                            );
                            if let Some(v) = routed {
                                return v;
                            }
                        }
                    }
                }
            }
        }
    }
    let args_slice = virtual_compile_moved_args.as_deref().unwrap_or(args_slice);

    let cached_entry = if direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !compiled_entry_forbidden
    {
        // A superseded or invalidated body is dropped at the hit; the by-name
        // `jit_cache` probe below then finds the current one on this same
        // call. See `DispatchCache::names_a_stale_body`.
        DISPATCH_CACHE.with(|dc| {
            let mut dc = dc.borrow_mut();
            if dc
                .get(&info_key)
                .is_some_and(DispatchCache::names_a_stale_body)
            {
                dc.remove(&info_key);
                return None;
            }
            dc.get(&info_key)
                .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
        })
    } else {
        None
    };
    if let Some((entry, needs_ctx, owner, rerun_pins)) = cached_entry {
        if crate::runtime::env_cache::jit_dispatch_dbg() {
            eprintln!(
                "[JIT_DISPATCH_ARM/dcache] {}.{} entry=0x{:x}",
                info.class_name, info.method_name, entry,
            );
        }
        // SAFETY: entry is a JIT-compiled function pointer cached from a previous successful
        // compilation. `try_call_compiled_entry` selects the correct extern "C" fn signature
        // based on arg count; on overflow it returns None and we bail to the interpreter.
        // CRIT round-5 fix: the previous `_ => 0` arm silently dropped 5+-arg callees;
        // wave-2 changed it to fall through to the slow path, and this wave goes one
        // step further by routing directly through `bail_to_interpreter` so the bail is
        // explicit at the call site (matches the MIC fast-path at `:1722`).
        // BUG-H: if the callee threw an implicit exception its own `catch`
        // should handle, the routing re-runs it in the interpreter through its
        // exception table. Statically bound: no receiver class.
        let routed = call_compiled_then_route(
            vm,
            info,
            args_slice,
            ClassId::new(0),
            rerun_pins || args_forwarded,
            || {
                let rc = match owner.as_ref() {
                    Some(owner) => try_call_compiled_entry_reentrant_owned(
                        owner, entry, needs_ctx, vm_ptr, args_slice,
                    ),
                    None => try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice),
                }?;
                if crate::runtime::env_cache::jit_dispatch_dbg() {
                    eprintln!(
                        "[JIT_DISPATCH_RET/dcache] {}.{}{} ret=0x{:x}",
                        info.class_name, info.method_name, info.descriptor, rc,
                    );
                }
                Some(rc)
            },
        );
        if let Some(v) = routed {
            disp_census::note(disp_census::OUT_DCACHE);
            return v;
        }
        // Overflow: decode args once and hand off to the interpreter.
        if let Some((thread, _guard)) = jit_thread_mut() {
            let bail_args = decode_dispatch_values(vm, info, args_slice);
            return bail_to_interpreter(vm, thread, info, &bail_args);
        }
        return 0;
    }

    // Both shortcuts below find the callee BY NAME. A site whose own loader
    // sees a different class under that name must not take either; it falls
    // to the loader-faithful interpreter route at the tail. Only these two
    // write `DISPATCH_CACHE`, so the cache-hit arm above never serves it.
    // A substituted owner is compared by its recorded id, not looked up in
    // the caller's loader (`jit_substituted_owner`).
    // JIT round 21 lane reflect4: a hidden static owner (`invoke_kind` 3 with
    // an `owner_class_id`, `jit_substituted_owner`) is its id; no loader
    // answers its name for another copy.
    let by_name_is_the_callers_class = !statically_bound
        || if matches!(info.invoke_kind, 1 | 3) && info.owner_class_id != 0 {
            !jit_substituted_owner_is_another_copy(vm, info)
        } else {
            !jit_callee_name_is_another_loaders_copy(
                vm,
                ClassId::new(info.declaring_class_id),
                info.class_name,
            )
        };

    // Check JIT cache for a compiled version of this callee.
    // PERF: `JitCache::get` takes `&str`, so we can pass the static literals from
    // `info` directly. Earlier code wrapped each in `Arc::from(...)` which
    // allocated a fresh heap buffer + atomic header on every dispatch — three
    // wasted allocations per hot call. Deref coercion handles the conversion.
    // Gated on `statically_bound`: the lookup key is the static CP class, which
    // is only the correct dispatch target for invokespecial/invokestatic.
    if direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !compiled_entry_forbidden
        && by_name_is_the_callers_class
        && !WRAPPED_STATIC_CALLEE.with(|m| m.borrow().contains_key(&info_key))
    {
        // `JitInvokeInfo` carries only the static-CP class NAME baked into this
        // compiled call site at codegen time, not a loader-scoped `ClassId` —
        // extending the raw `extern "C"` JIT ABI to also pass the caller's
        // `ClassId` here would require touching the x64 codegen call-site
        // emission itself. Resolving the name globally preserves this path's
        // existing (already name-based, not loader-aware) behavior unchanged;
        // it does not newly introduce the multi-loader-same-name collision —
        // see `JitKey::declaring_class_id`'s doc comment for the interpreter-
        // side fix this mirrors.
        // JIT round 21 lane reflect4 (correctness of the hidden-owner leaf,
        // unconditional): a hidden class's body is published under its id,
        // which no name lookup answers (`jit_hidden_static_owner`).
        let info_class_id = jit_hidden_static_owner(vm, info).unwrap_or_else(|| {
            vm.classes
                .class_manager
                .read()
                .get_loaded_class_id(info.class_name)
                .unwrap_or(cratonvm_types::ClassId::new(0))
        });
        let jit_cache = vm.jit.jit_cache.read();
        let published = jit_cache.get(
            info.class_name,
            info.method_name,
            info.descriptor,
            info_class_id,
        );
        if published
            .as_ref()
            .is_some_and(|compiled| compiled.requires_wrapped_entry)
        {
            WRAPPED_STATIC_CALLEE.with(|m| {
                m.borrow_mut().insert(info_key, ());
            });
        }
        if let Some(compiled) = published
            // A raw CALL to the entry below supplies no monitor, so an
            // `ACC_SYNCHRONIZED` body must not be served here — it would run
            // unlocked and, worse, be cached in `DISPATCH_CACHE` so every later
            // call at this site runs unlocked too. Falling through leaves the site
            // on the interpreter route, whose `dispatch_static` /
            // `dispatch_virtual` arms take the monitor. See
            // `CompiledMethod::requires_wrapped_entry`.
            .filter(|compiled| !compiled.requires_wrapped_entry)
        {
            let entry = compiled.entry_ptr() as usize;
            let needs_ctx = compiled.needs_context();
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_ARM/jcache] {}.{} entry=0x{:x}",
                    info.class_name, info.method_name, entry,
                );
            }
            // Asked once per site, on this cold fill, and memoized with the entry.
            let rerun_pins = callee_has_exception_table(vm, info);
            // Cache for future calls
            DISPATCH_CACHE.with(|dc| {
                dc.borrow_mut().insert(
                    info_key,
                    DispatchCache {
                        entry,
                        needs_context: needs_ctx,
                        owner: Some(std::rc::Rc::new(compiled.clone().into())),
                        rerun_pins,
                    },
                );
            });
            // JIT round 16 wave 14 (lane throw4, TP16-20): a method compiled
            // before this callee could bind may be waiting on it.
            crate::runtime::interpreter::jit_bridge::late_bind_site_served(
                vm,
                info.class_name,
                info.method_name,
                info.descriptor,
            );
            // SAFETY: entry was obtained from a CompiledMethod in the JIT cache, whose
            // entry_ptr points to executable memory with the correct extern "C" ABI.
            // CRIT round-5 fix: on >ARG_REGS args, route directly to the interpreter
            // via `bail_to_interpreter` rather than silently returning 0 (the original
            // wave-2 fall-through was already correct; this just makes the bail
            // explicit at the call site to match the MIC fast-path).
            // BUG-H: the routing sends a callee-thrown implicit exception
            // through the callee's own exception table (see dcache site above).
            let routed = call_compiled_then_route(
                vm,
                info,
                args_slice,
                ClassId::new(0),
                rerun_pins || args_forwarded,
                || {
                    let rc =
                        try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice)?;
                    if crate::runtime::env_cache::jit_dispatch_dbg() {
                        eprintln!(
                            "[JIT_DISPATCH_RET/jcache] {}.{}{} ret=0x{:x}",
                            info.class_name, info.method_name, info.descriptor, rc,
                        );
                    }
                    Some(rc)
                },
            );
            if let Some(v) = routed {
                disp_census::note(disp_census::OUT_JCACHE);
                return v;
            }
            if let Some((thread, _guard)) = jit_thread_mut() {
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
            return 0;
        }
    }

    // Invocation counting — trigger compilation for hot callees. Gated on
    // `statically_bound`: compiling `info` (the static CP-class method) and
    // caching it under the callsite key would re-introduce the supertype
    // miscompile for a virtual/interface site.
    let should_compile = direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !compiled_entry_forbidden
        && by_name_is_the_callers_class
        && DISPATCH_COUNTER.with(|dc| {
            let mut map = dc.borrow_mut();
            let count = map.entry(info_key).or_insert(0);
            // Saturating: a debug build must not panic past 2^32 calls.
            *count = count.saturating_add(1);
            *count == crate::runtime::env_cache::jit_invocation_threshold()
        })
        // j27 wave 2 (lane NATMISC): asked only at the crossing.
        && static_dispatch_counter_compiles_inline();
    // As `virtual_compile_moved_args`: the arguments after the compile below.
    let mut static_compile_moved_args: Option<Vec<i64>> = None;
    if should_compile {
        // Try to compile the callee and cache it. The compile can run
        // `<clinit>` and collect, so the reference arguments are pinned across
        // it and re-read (r11-irlower-outgoing-arg-staging-refs-unpublished).
        let (compiled, moved) =
            compile_with_ref_args_pinned(vm, info, args_slice, || try_compile_callee(vm, info));
        static_compile_moved_args = moved;
        let args_slice = static_compile_moved_args.as_deref().unwrap_or(args_slice);
        if let Some((_callee_pin, entry, needs_ctx)) = compiled {
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_ARM/compile] {}.{} entry=0x{:x}",
                    info.class_name, info.method_name, entry,
                );
            }
            // Asked once per site, on this cold fill, and memoized with the entry.
            let rerun_pins = callee_has_exception_table(vm, info);
            // See the virtual-dispatch sibling above: an unowned raw entry
            // must not be cached.
            if let Some(owner) = cratonvm_jit::pin_jit_entry(entry) {
                DISPATCH_CACHE.with(|dc| {
                    dc.borrow_mut().insert(
                        info_key,
                        DispatchCache {
                            entry,
                            needs_context: needs_ctx,
                            owner: Some(std::rc::Rc::new(owner.into())),
                            rerun_pins,
                        },
                    );
                });
            }
            // TP16-20, as at the `jit_cache` fill above.
            crate::runtime::interpreter::jit_bridge::late_bind_site_served(
                vm,
                info.class_name,
                info.method_name,
                info.descriptor,
            );
            // SAFETY: entry was just produced by try_compile_callee, which returns a validated
            // JIT entry pointer. CRIT round-5 fix: bail explicitly to the interpreter on
            // >ARG_REGS args via `bail_to_interpreter` (matches the MIC fast-path).
            // BUG-H: the routing sends a callee-thrown implicit exception
            // through the callee's own exception table (see dcache site above).
            let routed = call_compiled_then_route(
                vm,
                info,
                args_slice,
                ClassId::new(0),
                rerun_pins || args_forwarded,
                || try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice),
            );
            if let Some(v) = routed {
                disp_census::note(disp_census::OUT_COMPILED_NOW);
                return v;
            }
            if let Some((thread, _guard)) = jit_thread_mut() {
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
            return 0;
        }
    }
    let args_slice = static_compile_moved_args.as_deref().unwrap_or(args_slice);

    disp_census::note(disp_census::OUT_TAIL);
    // Slow path: interpreter fallback
    let (thread, _jit_thread_guard) = match jit_thread_mut() {
        Some(t) => t,
        None => {
            return 0;
        }
    };

    // Registered natives cannot populate DISPATCH_CACHE because they have no
    // compiled entry pointer. Cache the callback, decoder kind, and exact
    // receiver ClassId for the hot monomorphic HashMap and Matcher sites
    // instead of repeating virtual resolution and a failed compile probe on
    // every iteration.
    let object_native_kind = hashmap_native_arg_count(info)
        .map(|_| ObjectNativeKind::HashMap)
        .or_else(|| matcher_native_arg_count(info).map(|_| ObjectNativeKind::Matcher))
        .or_else(|| stringbuilder_native_arg_count(info).map(|_| ObjectNativeKind::StringBuilder));
    if matches!(info.invoke_kind, 0 | 2) && object_native_kind.is_some() && !args_slice.is_empty() {
        let kind = object_native_kind.expect("checked above");
        let receiver_raw = args_slice[0] as u64;
        if receiver_raw != 0 && (receiver_raw & 0x7) == 0 && receiver_raw < (1u64 << 48) {
            crate::jit::helpers::note_membership_walk(3);
            if let Some(receiver) = vm.mem.heap.is_object_address(receiver_raw as usize) {
                // An array (no dispatch class id) is keyed as class 0, which no
                // entry carries and whose name is never `expected_class`, so it
                // neither hits nor fills (`jit_dispatch_class`).
                let receiver_class_id = jit_dispatch_class_validated(vm, receiver)
                    .class_id()
                    .map_or(0, |cid| cid.as_u32());
                if receiver_class_id != 0
                    && !class_was_redefined(vm, ClassId::new(receiver_class_id))
                {
                    let cached = OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| {
                        cache
                            .borrow()
                            .get(&info_key)
                            .copied()
                            .filter(|entry| entry.receiver_class_id == receiver_class_id)
                    });
                    if let Some(entry) = cached {
                        if let Some(result) =
                            call_object_native_raw(vm, thread, info, receiver, args_slice, entry)
                        {
                            return result;
                        }
                    } else {
                        let expected_class = match kind {
                            ObjectNativeKind::HashMap => "java/util/HashMap",
                            ObjectNativeKind::Matcher => "java/util/regex/Matcher",
                            ObjectNativeKind::StringBuilder => "java/lang/StringBuilder",
                        };
                        let is_exact_receiver = {
                            let classes = vm.classes.class_manager.read();
                            classes
                                .get_class(ClassId::new(receiver_class_id))
                                .map(|class| class.name.as_ref() == expected_class)
                                .unwrap_or(false)
                        };
                        if is_exact_receiver {
                            // Cold, once per call site. Each of these three
                            // resolvers now applies Real-JDK policy internally
                            // (see `admit_jit_fast_native`) and returns the
                            // §4 census handle alongside the callback. A
                            // `None` under `JdkOnly` means "policy declined
                            // this optimization": nothing is cached, and the
                            // call falls through to the generic
                            // `invoke_or_native` tail below, which is the
                            // single authoritative resolver and raises the
                            // structured `VmError::JdkOnly(..)` if the triple
                            // is genuinely unrunnable. Never a stub, never a
                            // wild jump.
                            let callback = match kind {
                                ObjectNativeKind::HashMap => hashmap_native_callback(vm, info),
                                ObjectNativeKind::Matcher => matcher_native_callback(vm, info),
                                ObjectNativeKind::StringBuilder => {
                                    stringbuilder_native_callback(vm, info)
                                }
                            };
                            if let Some((callback, native_id)) = callback {
                                let entry = NativeDispatchCache {
                                    receiver_class_id,
                                    callback,
                                    kind,
                                    native_id,
                                };
                                OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| {
                                    cache.borrow_mut().insert(info_key, entry);
                                });
                                if let Some(result) = call_object_native_raw(
                                    vm, thread, info, receiver, args_slice, entry,
                                ) {
                                    return result;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Lambda proxies are synthetic and therefore cannot participate in the
    // class-store MIC. Give the erased primitive adapter its own receiver-guarded
    // direct path before allocating decoded Values for the generic fallback.
    //
    // The lambda call site's own cached invoke target goes FIRST and takes any
    // arity — this is the SECOND door into lambda dispatch from compiled code
    // (`jit_invoke_virtual_mic` is the other), and a fast path wired into only
    // one of them is a fast path that a workload can miss entirely for reasons
    // that have nothing to do with its shape.
    if matches!(info.invoke_kind, 0 | 2) && !args_slice.is_empty() {
        crate::jit::helpers::note_membership_walk(3);
        if let Some(proxy) = vm.mem.heap.is_object_address(args_slice[0] as usize) {
            // An array of lambda proxies carries the proxy's id in its
            // header but dispatches on `Object` (`jit_dispatch_class`).
            let proxy_class_id = jit_dispatch_class_validated(vm, proxy)
                .class_id()
                .unwrap_or_else(|| ClassId::new(0));
            if proxy_class_id != ClassId::new(0) && vm.classes.is_lambda_proxy_class(proxy_class_id)
            {
                if let Some(result) = try_lambda_site_direct_call(
                    vm,
                    thread,
                    proxy,
                    proxy_class_id,
                    info,
                    args_slice,
                    vm_ptr,
                    // This door has no inline-cache slot to install into: it is
                    // reached from a call site the codegen did not give one.
                    None,
                ) {
                    return result;
                }
            }
        }
    }
    if matches!(info.invoke_kind, 0 | 2) && args_slice.len() == 2 {
        crate::jit::helpers::note_membership_walk(3);
        if let Some(proxy) = vm.mem.heap.is_object_address(args_slice[0] as usize) {
            // An array of lambda proxies carries the proxy's id in its
            // header but dispatches on `Object` (`jit_dispatch_class`).
            let proxy_class_id = jit_dispatch_class_validated(vm, proxy)
                .class_id()
                .unwrap_or_else(|| ClassId::new(0));
            if proxy_class_id != ClassId::new(0) && vm.classes.is_lambda_proxy_class(proxy_class_id)
            {
                match try_fast_lambda_int_to_double_apply(
                    vm,
                    thread,
                    proxy,
                    proxy_class_id,
                    info,
                    args_slice,
                ) {
                    Ok(Some(result)) => return result,
                    Ok(None) => {}
                    Err(error) => return handle_jit_dispatch_error(vm, thread, error, info),
                }
            }
        }
    }

    // Round-5 CRIT-1 fix: share arg-decoding with the three cache-hit
    // overflow bailouts above via `decode_dispatch_values`.
    // `mut`: the `invokestatic` arm re-reads its reference arguments through
    // pins across a `<clinit>` (`jit_static_owner_init_before_native`).
    let mut values = decode_dispatch_values(vm, info, args_slice);

    let result: Option<Value> = match info.invoke_kind {
        0 | 2 => {
            if values.is_empty() {
                return 0;
            }
            let receiver_ref = match values[0] {
                Value::Object(Some(obj)) => obj,
                // JVM semantics: invokevirtual/invokeinterface on a null
                // receiver throws NullPointerException. Signal it like the
                // array helpers — set the pending-NPE flag and return the
                // i64::MIN sentinel so the interpreter's post-JIT drain builds
                // the real NPE and routes it through the method's exception
                // table. Previously this returned `0`, silently SWALLOWING the
                // NPE: a null deref inside a JIT-compiled method (e.g. a
                // once-called lambda body that reaches this cold dispatch
                // helper before any inline null-check / MIC warm-up) completed
                // normally instead of throwing, so `() -> nullRef.foo()` ran
                // as a no-op under the JIT (the bug only reproduced JIT-on).
                Value::Object(None) => {
                    // `INVOKE_RECEIVER` names the opcode family that trapped
                    // rather than a message: the JEP 358 action half here is
                    // `Cannot invoke "Owner.name(sig)"`, which only the call
                    // site's bytecode knows. This helper is called AT a
                    // published safepoint, so the frame's safepoint-id slot
                    // holds this invoke's bci, and the code lets
                    // `runtime::interpreter::jit_npe_message` corroborate that
                    // bci before reading the opcode there.
                    set_jit_pending_npe_action(
                        crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
                    );
                    return i64::MIN;
                }
                // A non-object receiver slot is a miscompile, not a legitimate
                // null — keep the defensive 0 bail (does not mask a real NPE).
                _ => return 0,
            };
            // A lambda-proxy receiver must be dispatched through the proxy
            // registry, never by name — see `try_lambda_proxy_sam_dispatch`
            // for what resolving one by its functional-interface name runs
            // instead of the lambda body.
            if let Some(result) = try_lambda_proxy_sam_dispatch(vm, thread, info, &values) {
                return match result {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // A callee the JIT did not compile: enter it through this
            // (site, receiver class)'s cached interpreter frame template rather
            // than re-resolving it from `info.class_name` on every call. This is
            // the TAIL — `OUT_TAIL` was already counted above and every compiled
            // -callee probe (`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`,
            // `JitCache`, the tier-up arm) has already declined — so there is no
            // compiled body being passed over here. Declines (and caches the
            // refusal) for anything the by-name path must still handle.
            let receiver_dispatch_cid = jit_dispatch_class(vm, receiver_ref).class_id();
            // gci i12/mf: first, a (site, receiver class) whose slow call
            // already ended in a table-bound JNI native goes straight to the
            // door (`CRATONVM_JNI_DISPATCH_MEMO`; `vm_exec`'s compiled virtual
            // call-site rows). One byte compare when the memo is off.
            if let Some(hit) =
                jit_jni_vsite_lookup(vm, thread, info, info_key.1, receiver_dispatch_cid)
            {
                return jit_jni_vsite_call(
                    vm,
                    thread,
                    info,
                    &hit,
                    &mut values[..],
                    &hit.receiver_name,
                );
            }
            if let Some(r) = try_jit_virtual_bytecode_callee(
                vm,
                thread,
                info,
                info_key,
                receiver_ref,
                // Class 0 (an array, `jit_dispatch_class`) is refused inside.
                receiver_dispatch_cid.unwrap_or_else(|| ClassId::new(0)),
                &values,
                true,
            ) {
                return match r {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // Match the register-overflow bail path: `NativeContext::invoke_virtual`
            // resolves solely from the heap object's class id. That is insufficient
            // for a synthetic/ClassId(0) receiver (common for Lucene iterator
            // adapters): it turns `Iterator.hasNext()` into `Object.hasNext()`.
            // `virtual_dispatch_target_for_receiver` preserves the real receiver
            // class when available and otherwise supplies the CP-resolved class;
            // `invoke_or_native` then applies the VM's interface/abstract retarget.
            //
            // The memoized form (this call ran `flush_class_identity_dispatch_memos`
            // above): the target now carries the §5.4.6 retarget, which costs a
            // walk and an owner lookup, so the per-call tail reads it from the
            // (site, receiver class) memo instead of recomputing it under a lock.
            let dispatch_target = virtual_dispatch_target_cached(vm, receiver_ref, info, info_key);
            if dispatch_target.selection_error {
                if let Some(error) = jit_selection_failure(vm, thread, receiver_ref, info) {
                    return handle_jit_dispatch_error(vm, thread, error, info);
                }
            }
            let jni_vsite_fill_ok = !dispatch_target.selection_error;
            let dispatch_class = dispatch_target.class_name;
            crate::vm::dbg_dispatch_tally(
                "jit_dispatch_virtual_tail",
                &dispatch_class,
                info.method_name,
                info.descriptor,
            );
            // gci i12/mf: the compiled virtual call-site rows' fill probe
            // brackets the by-name tail (armed only with the memo on).
            let jni_vsite_probe = crate::vm::vm_exec::jni_jit_vsite_probe_arm(
                vm,
                thread,
                info.method_name,
                info.descriptor,
            );
            // gch h1/jp (fr review item 3): the rescue below resends `values`,
            // so a failed attempt re-reads it from pins kept across it.
            let virt_result = crate::vm::invoke_or_native_keeping_args_current(
                vm,
                thread,
                &dispatch_class,
                info.method_name,
                info.descriptor,
                &mut values[..],
                !info.class_name.is_empty(),
            );
            if let Some(save) = jni_vsite_probe {
                crate::vm::vm_exec::jni_jit_vsite_probe_finish(
                    vm,
                    thread,
                    save,
                    ClassId::new(info.declaring_class_id),
                    crate::vm::vm_exec::jni_jit_vsite(info_key.1),
                    receiver_dispatch_cid.unwrap_or_else(|| ClassId::new(0)),
                    &dispatch_class,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    virt_result.is_ok() && jni_vsite_fill_ok,
                );
            }
            // The receiver as the failed attempt left it (the rescue reads its
            // class).
            let receiver_ref = match values.first() {
                Some(Value::Object(Some(receiver))) => *receiver,
                _ => receiver_ref,
            };
            match virt_result {
                Ok(v) => v,
                Err(e) => {
                    // S111r12 — JIT virtual-dispatch rescue: when the
                    // receiver's `class_id_of` returns a stub class
                    // (e.g. `java/lang/Comparable` for a malformed
                    // ClassLoader instance) that doesn't declare the
                    // CP-resolved method, `invoke_virtual` raises
                    // `NoSuchMethodError`. The CP method-ref class
                    // carried in `info.class_name` (e.g.
                    // `java/lang/ClassLoader`) is the spec-correct
                    // resolution target — retry the dispatch through
                    // it. Mirrors the S111r10 receiver-walk fallback
                    // for invokeinterface and the S111r8 cid=0 →
                    // CP-class fallback in `execute_invoke`.
                    if !info.class_name.is_empty() {
                        // An array's dispatch class is `java/lang/Object`,
                        // not its header's component (`jit_dispatch_class`).
                        let recv_name_opt = match jit_dispatch_class(vm, receiver_ref) {
                            JitDispatchClass::Class(recv_cid) => {
                                let cm = vm.classes.class_manager.read();
                                cm.get_class(recv_cid).map(|c| c.name.to_string())
                            }
                            JitDispatchClass::JavaLangObject => {
                                Some("java/lang/Object".to_string())
                            }
                        };
                        let cp_differs = recv_name_opt
                            .as_deref()
                            .map(|n| n != info.class_name)
                            .unwrap_or(true);
                        if cp_differs
                            && recv_name_opt.as_deref().is_some_and(|recv_name| {
                                is_dispatch_no_such_method_miss(
                                    &e,
                                    recv_name,
                                    info.method_name,
                                    info.descriptor,
                                )
                            })
                        {
                            let r = crate::vm::invoke_or_native(
                                vm,
                                thread,
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                                &values,
                            );
                            match r {
                                Ok(v) => v,
                                Err(e2) => {
                                    return handle_jit_dispatch_error(vm, thread, e2, info);
                                }
                            }
                        } else {
                            return handle_jit_dispatch_error(vm, thread, e, info);
                        }
                    } else {
                        return handle_jit_dispatch_error(vm, thread, e, info);
                    }
                }
            }
        }
        1 => {
            // invokespecial: dispatch must NOT virtually re-target onto the
            // receiver's runtime class. `invoke_or_native` -> `invoke_on_class_shared`
            // applies the iface/abstract -> receiver-class retarget that
            // `invokevirtual` semantics require, which for invokespecial turns
            // a super-call into a self-call and produces unbounded recursion
            // (e.g. `RunLast.execute` invokespecial-calls `AbstractParseResultHandler.execute`;
            // APRH is abstract, so the retarget bounces back to `RunLast.execute`
            // and we recurse forever — surfaced as `StackOverflowError` inside the
            // picocli `execute` chain on neo4j / keycloak). The `<init>` and
            // `<clinit>` carve-outs that protect that path in
            // `invoke_on_class_shared_inner` do NOT cover ordinary super-calls,
            // so we have to take an invokespecial-aware dispatch path here.
            //
            // `invoke_special_shared` matches the interpreter's invokespecial
            // semantics: walk the hierarchy from the CP-resolved class to the
            // declaring class for the requested method, then invoke through
            // `invoke_on_class_shared_no_retarget` so the virtual retarget never
            // fires. Native-override priority is preserved (same as
            // `invoke_or_native`).
            //
            // BUG-JIT-INVOKESPECIAL-LOADER-20260726: resolve the CP class
            // through the CALLER's loader before dispatching. `info.class_name`
            // is constant-pool text; with two loaders defining the same binary
            // name, `invoke_special_shared`'s own `load_class_concurrent` picks
            // whichever copy the global map holds — see
            // `JitInvokeInfo::declaring_class_id`. `resolve_class_loader_aware`
            // is the same resolver the interpreter's invokespecial uses, and it
            // short-circuits to the global answer whenever no user-defined
            // loader has ever defined a class, so single-loader processes pay
            // one relaxed atomic load.
            // A callee the JIT did not compile: enter it through this site's
            // cached interpreter frame template. Same position in the tail as
            // the static and virtual arms — every compiled-callee probe has
            // already declined by here.
            if let Some(r) = try_jit_special_bytecode_callee(vm, thread, info, info_key, &values) {
                return match r {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // gci i12/mf: a statically bound site whose slow call already ended
            // in a table-bound JNI native goes straight to the door on the
            // declaring class (`CRATONVM_JNI_DISPATCH_MEMO`; `vm_exec`'s kind-1
            // rows, refused once anything was redefined). One byte compare when
            // the memo is off.
            if let Some(hit) = crate::vm::vm_exec::jni_jit_special_memo_lookup(
                vm,
                thread,
                ClassId::new(info.declaring_class_id),
                crate::vm::vm_exec::jni_jit_ssite(info_key.1),
                (info.class_name, info.method_name, info.descriptor),
            ) {
                return match crate::vm::vm_exec::jni_jit_special_memo_call(
                    vm, thread, &hit, &values,
                ) {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // An owner the compile substituted for the constant-pool class
            // is taken by its recorded id: its name is not one the caller's
            // loader was ever asked for (`jit_substituted_owner`).
            // Through the caller's `CONSTANT_Class` entry for an isolated
            // loader's caller (round 16, lane classsite, CS2-2).
            let resolved_owner = if let Some(owner) = jit_substituted_owner(vm, info) {
                Some(owner)
            } else if info.declaring_class_id != 0 {
                jit_bridged_owner_entry(
                    vm,
                    thread,
                    ClassId::new(info.declaring_class_id),
                    info.class_name,
                )
            } else {
                None
            };
            // Round 15 wave 5 (lane walk4; F1 of
            // `r15w4-walk3-review-findings-FIXED-20260930.md`): a JVMTI agent wove
            // the owner's own body for this method (a Mockito inline mock's
            // advice) and the native registered on it is not one CratonVM's
            // objects must keep: run that body, as the interpreter's
            // invokespecial and `invoke_or_native` do. (`invoke_special_shared`
            // asks the same question since round 15 wave 6, WK4-2; answering it
            // here runs the body without the door's re-resolution.)
            let woven_owner = resolved_owner
                .filter(|&owner| special_site_runs_woven_body(vm, owner, info, &values));
            let r = match (woven_owner, resolved_owner) {
                (Some(owner), _) => crate::runtime::interpreter::execute(
                    vm,
                    thread,
                    owner,
                    info.method_name,
                    info.descriptor,
                    &values,
                ),
                (None, Some(owner)) => {
                    // gci i12/mf: the kind-1 rows' fill probe brackets the
                    // resolved-owner call (armed only with the memo on).
                    let jni_site_probe = crate::vm::vm_exec::jni_jit_vsite_probe_arm(
                        vm,
                        thread,
                        info.method_name,
                        info.descriptor,
                    );
                    let r = crate::vm::invoke_special_shared_on_class(
                        vm,
                        thread,
                        owner,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        &values,
                    );
                    if let Some(save) = jni_site_probe {
                        crate::vm::vm_exec::jni_jit_special_probe_finish(
                            vm,
                            thread,
                            save,
                            ClassId::new(info.declaring_class_id),
                            crate::vm::vm_exec::jni_jit_ssite(info_key.1),
                            owner,
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                            r.is_ok(),
                        );
                    }
                    r
                }
                (None, None) => crate::vm::invoke_special_shared(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &values,
                ),
            };
            match r {
                Ok(v) => v,
                Err(e) => {
                    return handle_jit_dispatch_error(vm, thread, e, info);
                }
            }
        }
        3 => {
            // invokestatic: no receiver, so no dispatch retarget can happen —
            // but the OWNER is still constant-pool text, and a class NAME is
            // not a class identity. This arm used to read "no retarget
            // concern. The historical `invoke_or_native` path is correct
            // here", which was true of the DISPATCH and false of the
            // IDENTITY: `invoke_or_native` resolves `info.class_name` through
            // the flat global binary-name map, so when two loaders define
            // that name it binds the call to whichever copy the map holds.
            // Exactly the defect the `invoke_kind == 1` arm above was fixed
            // for (BUG-JIT-INVOKESPECIAL-LOADER-20260726) — the fix landed on
            // invokespecial and stopped there.
            //
            // Measured 2026-08-10 on `AotIntegrationTests
            // .endToEndTestsForBeanOverrides` (spring-test, under
            // `@CompileWithForkedClassLoader`, which defines its own copy of
            // every `org.springframework.*` class): a compiled static call
            // made from the FORKED copy of `ResolvableType` landed in the
            // APPLICATION copy, so the `ResolvableType` it returned belonged
            // to the other class. `ResolvableType$WildcardBounds.get` then
            // ran `while (…) candidate = candidate.resolveType()` against a
            // `NONE` singleton from the wrong copy, `candidate == NONE` could
            // never hold, and one core spun for as long as the machine was
            // given — the hang this test was known for. The same method
            // passes under `--nojit`, because the interpreter resolves the
            // owner through the caller's own loader.
            //
            // STRICTLY ADDITIVE, by construction: the loader-faithful owner
            // is consulted only behind `jit_static_owner_override`'s gates
            // and used only when it DIFFERS from the global by-name answer,
            // so a process with one copy of the name takes the identical
            // previous path — including every special case
            // `invoke_or_native` carries that the on-class form does not.
            // A callee the JIT did not compile: enter it through this site's
            // cached interpreter frame template rather than re-resolving it
            // from `info.class_name` on every call. Declines (and caches the
            // refusal) for anything the by-name path must still handle.
            //
            // gci i9/cs: first, a site whose slow call already ended in a
            // table-bound JNI native goes straight to the door on the class it
            // resolved (`CRATONVM_JNI_DISPATCH_MEMO`; `vm_exec`'s JNI call-site
            // memo, keyed by this info's address and re-checked against its
            // names). One byte compare when the memo is off.
            if let Some(hit) = crate::vm::vm_exec::jni_site_memo_lookup(
                vm,
                thread,
                ClassId::new(info.declaring_class_id),
                crate::vm::vm_exec::jni_jit_site(info_key.1),
                Some((info.class_name, info.method_name, info.descriptor)),
                crate::vm::vm_exec::JNI_SITE_HIT_JIT,
            ) {
                match crate::vm::vm_exec::jni_site_memo_call(vm, thread, &hit, &values) {
                    Ok(v) => v,
                    Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
                }
            } else if let Some(r) =
                try_jit_static_bytecode_callee(vm, thread, info, info_key, &values)
            {
                match r {
                    Ok(v) => v,
                    Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
                }
            } else {
                // JIT round 21 lane reflect4: a hidden static owner by its id
                // (`jit_hidden_static_owner`); its name resolves nowhere.
                let owner_override = jit_hidden_static_owner(vm, info)
                    .or_else(|| jit_static_owner_override(vm, thread, info));
                // zt t27/jd1 (`CRATONVM_JIT_STATIC_OWNER_LOADS_FIRST`): an
                // owner no loader holds is loaded first, by the route
                // `execute_invokestatic` takes, so the native-vs-bytecode
                // decision below (`invoke_or_native`'s
                // `registered_native_yields_to_loaded_bytecode`) is made on the
                // loaded declaration, as the interpreter makes it. A
                // loader-faithful override names a loaded owner already.
                if owner_override.is_none() {
                    crate::vm::load_static_owner_before_dispatch(
                        vm,
                        thread,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        &mut values,
                    );
                }
                // JVMS §5.5, before a registered native can run: neither
                // `invoke_static_shared_on_class` nor `invoke_or_native`'s
                // registry arms initialize the declaring class.
                if let Err(e) = jit_static_owner_init_before_native(
                    vm,
                    thread,
                    info,
                    owner_override,
                    &mut values,
                ) {
                    return handle_jit_dispatch_error(vm, thread, e, info);
                }
                let r = match owner_override {
                    Some(owner) => crate::vm::invoke_static_shared_on_class(
                        vm,
                        thread,
                        owner,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        &values,
                    ),
                    None => {
                        crate::vm::dbg_dispatch_tally(
                            "jit_dispatch_static_tail",
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        );
                        // The static tail: it initializes the declaring class,
                        // never the symbolic owner of an inherited static
                        // (JVMS §5.5; wave 14). The native prelude ran above.
                        // gci i9/cs: the JNI call-site memo's fill probe
                        // brackets the by-name tail (armed only with the memo
                        // on); the flat by-name owner is the site's owner.
                        let site_probe = crate::vm::vm_exec::jni_site_probe_arm(
                            vm,
                            thread,
                            info.method_name,
                            info.descriptor,
                        );
                        let r = crate::vm::invoke_static_or_native_prepared(
                            vm,
                            thread,
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                            &values,
                        );
                        if let Some(save) = site_probe {
                            crate::vm::vm_exec::jni_site_probe_finish(
                                vm,
                                thread,
                                save,
                                ClassId::new(info.declaring_class_id),
                                crate::vm::vm_exec::jni_jit_site(info_key.1),
                                None,
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                                values.len(),
                                r.is_ok(),
                            );
                        }
                        r
                    }
                };
                match r {
                    Ok(v) => v,
                    Err(e) => {
                        return handle_jit_dispatch_error(vm, thread, e, info);
                    }
                }
            }
        }
        _ => None,
    };

    let ret = match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    };
    if crate::runtime::env_cache::jit_dispatch_dbg() {
        eprintln!(
            "[JIT_DISPATCH_RET] {}.{}{} ret=0x{:x} ({})",
            info.class_name, info.method_name, info.descriptor, ret, ret,
        );
    }
    ret
}

/// Kill switch for the bytecode-callee templates' redefinition handling
/// (`STATIC_` / `VIRTUAL_` / `SPECIAL_BYTECODE_CALLEE_CACHE`; interpreter
/// round i1 wave 18, lane L4). `true`: a redefinition retires per class. A
/// template's key is a compiled caller's `JitInvokeInfo` address, and a
/// redefined caller's bodies are withdrawn with the code-cache flush (their
/// site memos with them), so the caller side needs nothing here. On the
/// target side the template's `RedefineGate` retires it when its declaring
/// class is redefined; a template filled after that is served like any other
/// since wave 40 ([`callee_template_declines_redefined_target`]; before, a
/// callee whose declaring class was EVER redefined kept the by-name path). A redefinition cannot turn a body into a native or back (it
/// refuses a change to a method's access flags), and a template exists only
/// for a `(name, descriptor)` no native anywhere carries, so no shadow
/// decision is cached. `false`: the historical latch — no template is served
/// for the rest of the process after the first redefinition of any class.
pub const CALLEE_TEMPLATES_SURVIVE_REDEFINITION: bool = true;

/// Whether the bytecode-callee templates are latched off by a redefinition:
/// never, unless the kill switch restores the latch.
#[inline(always)]
pub fn callee_templates_latched_off() -> bool {
    !CALLEE_TEMPLATES_SURVIVE_REDEFINITION && crate::classloading::any_class_redefined()
}

/// Kill switch for serving a bytecode-callee template whose declaring class
/// was redefined before the template was filled (interpreter round i1 wave
/// 40, lane L2;
/// `docs/feature-designs/profile-guided-inlining.md`).
/// `true`: such a template is served like any other -- counted, nominated
/// and entered compiled when a body exists
/// (`bytecode_callee_compiled_or_nominate`), as the interpreter's cached
/// dispatchers and doors now treat a redefined target
/// (`invoke_fast::REDEFINED_TARGETS_TIER_UP`, waves 39-40). The template's
/// gate is checked first on every hit, so a template filled before the
/// class's latest redefinition is evicted and one filled after it holds the
/// current body, the body the by-name path it replaces would run. `false`:
/// the wave-18 policy, which kept a redefined callee on the by-name path on
/// the ground that the interpreter did not offer it to the JIT.
/// SWITCHED OFF by the wave-40 orchestrator (2026-10-04): with it on, a
/// multi-class redefinition tears for compiled readers
/// (`L3W38RedefineTwoClassesSamePool.java`, 60
/// interleaved runs on the host: `dev` 59/60 and 60/60, on 36/60 and 47/60
/// even with the publication hold, off 60/60 and 60/60). See
/// `docs/feature-designs/class-resolution-and-caching.md`.
/// SWITCHED BACK ON by interpreter round i1 wave 42, lane L2, with the
/// bytecode-read witness of the compile doors (`jit_bridge::try_jit_compile_callee_slow`:
/// a compile of the old bytecode was published after the redefinition); in a
/// commit of its own, so the host can judge the page's table with and without it.
pub const CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS: bool = true;

/// Is a template whose declaring class has been redefined (its gate's
/// generation, current since `is_stale` passed) declined? Only under
/// [`CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`]'s kill switch: a constant
/// `false` otherwise, so the three template paths lose the branch.
#[inline(always)]
pub fn callee_template_declines_redefined_target(gate_generation: u32) -> bool {
    !CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS && gate_generation != 0
}

/// A compiled caller's `invokestatic` to a callee the JIT did NOT compile.
///
/// # The cost this removes
///
/// Without it such a call falls out of `jit_invoke_dispatch`'s fast arms into
/// `crate::vm::invoke_or_native` -> `invoke_on_class_shared_inner`, the fully
/// name-keyed generic dispatch — so a call site that is monomorphic and whose
/// callee never changes re-resolves the owner class from a STRING, re-runs the
/// native-override arbitration, re-clones the `CodeAttribute` and re-pads the
/// bytecode, on every single call. `perf`, steady state, on the denied arm of
/// `probes/XferProbe.java`: `invoke_on_class_shared_inner` 14.72% self, the
/// `(ClassLoaderId, Arc<str>)` by-name class lookup 3.41%, `__memcmp_evex_movbe`
/// 3.04%, the native-override gate 1.93%, and `CodeAttribute::clone` +
/// `padded_bytecode_for_method` + `Arc<[u8]>::drop_slow` 4.50%.
///
/// The interpreter pays none of it — its call sites hold a
/// `CachedInvokeTarget::Bytecode` with a prebuilt `Arc<CachedBytecodeMethod>`.
/// The JIT helper had a cache for a COMPILED callee (`DISPATCH_CACHE`,
/// `VIRTUAL_DISPATCH_CACHE`) and none at all for an interpreted one, which is
/// why compiling the caller and not the callee measured **5x slower than
/// compiling neither** (1242-1902 ns/op against 236-385).
///
/// # Why `invokestatic` first
///
/// It is where the volume is — `CRATONVM_DBG=mic-prof` on
/// `probes/ReactorProbe.java`: `kind_static=2_986_402` of
/// `disp_calls=3_356_461`, 89% — and it has the smallest correctness surface:
/// no receiver, so no virtual retarget, no interface rules and no
/// receiver-class guard to re-test.
///
/// # Why it is safe to take instead of `invoke_or_native`
///
/// Same shape of argument as [`try_jit_site_cached_native_dispatch`]: every
/// obligation is discharged ONCE, at resolution, and a site that cannot
/// discharge them caches a refusal and never asks again. The resolution refuses
/// unless ALL of these hold, so anything unusual keeps the old path verbatim:
///
/// * the invoke kind is `3` and the method name is not one `invoke_or_native`
///   special-cases (`site_name_is_special_cased` — the same gate cascade the
///   leaf-native path defers to);
/// * `jit_static_owner_override` declines, i.e. no loader-faithful owner
///   differs from the global by-name answer. That is what keeps the whole
///   loader-identity surface — BUG-JIT-INVOKESPECIAL-LOADER-20260726 and the
///   `AotIntegrationTests` static-owner hang — on the untouched path;
/// * NO native anywhere has this `(name, descriptor)`
///   (`might_have_method_descriptor`, the same cheap pre-filter
///   `execute_invokevirtual_vtable_fast` uses). That one refusal removes the
///   native-override, `SyntheticStub`-yield and redefine-shadow questions
///   entirely rather than reproducing them — and it subsumes
///   `force_native_over_real_jdk_bytecode`, whose triples name a REGISTERED
///   native that must beat real JDK bytecode: with no registration carrying
///   this `(name, descriptor)` at all, such a gate entry declines and the
///   bytecode runs, which is what this path does;
/// * the owner class is loaded AND the class declaring the callee is already
///   initialised — the by-name tail would run its `<clinit>`, and this path
///   must never be the thing that skips it. Initialisation is monotone, so
///   checking once is checking forever;
/// * `build_lambda_impl_cached` (nothing lambda-specific about it, see its doc)
///   accepts the resolved method: not native, not `synchronized`, not abstract,
///   has a `Code` attribute, no native shadow on the declaring class;
/// * the method really is `static`, and its parameter count matches the decoded
///   argument count.
///
/// Per hit, only what can change is re-tested: the declaring class's
/// `RedefineGate` (see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION` and
/// `CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`).
///
/// SAFETY: `values` are the caller's already-decoded arguments; `thread` is the
/// JIT helper's own borrow.
pub unsafe fn try_jit_static_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !crate::runtime::env_cache::jit_static_bytecode_callee() {
        return None;
    }
    if info.invoke_kind != 3 {
        return None;
    }
    // A redefinition retires what it can change per class (the gate below);
    // see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION`.
    if callee_templates_latched_off() {
        return None;
    }
    let cached = match STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&info_key).cloned()) {
        Some(hit) => hit,
        None => {
            let mut init_pending = false;
            let resolved = resolve_static_bytecode_callee(vm, thread, info, &mut init_pending);
            // Not cached while the owner is uninitialised: that refusal ends
            // at `<clinit>`, which moves no epoch these memos are flushed on.
            if !init_pending {
                STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
                    c.borrow_mut().insert(info_key, resolved.clone());
                });
            }
            resolved
        }
    };
    let Some((cached, gate)) = cached else {
        disp_census::note(disp_census::OUT_STATIC_BC_REFUSED);
        return None;
    };
    if gate.is_stale() {
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().remove(&info_key);
        });
        return None;
    }
    if callee_template_declines_redefined_target(gate.generation) {
        return None;
    }
    // A static callee takes exactly its declared parameters — no receiver. An
    // arity disagreement means the decode and the template describe different
    // methods, which is a refusal, never a guess.
    if values.len() != cached.num_params as usize {
        return None;
    }
    // Count the invocation the ordinary path would have counted, and stand
    // down if a compiled body now exists — see
    // `bytecode_callee_compiled_or_nominate`. Without this a callee served here
    // is never nominated, so it stays interpreted for the life of the process;
    // and a callee that IS compiled would be run interpreted anyway, which is a
    // straight loss rather than a missed win. MEASURED at 15% on
    // `probes/ExchangeProbe.java` before it was added.
    let warmup = compiled_template_warmup(info_key, cached.invoc_key());
    if crate::runtime::interpreter::bytecode_callee_compiled_or_nominate(vm, thread, &cached, &warmup) {
        disp_census::note(disp_census::OUT_STATIC_BC_REFUSED);
        return None;
    }
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // In-place install, not a by-value build: `execute_prebuilt_frame` pushes
    // through `push_frame_and_fire_entry`, which harvests and trims the
    // retired slot, so this helper used to undo frame-slot reuse for the
    // interpreter frames beneath it once per call. See
    // `install_and_run_cached_frame`.
    //
    // The `_monitored` form serves a `static synchronized` template with the
    // doors' uncontended acquire on the class mirror, and declines on
    // contention or while no mirror exists; the caller then continues to
    // `invoke_or_native`. A non-synchronized template is exactly
    // `install_and_run_cached_frame`.
    let served = crate::runtime::interpreter::install_and_run_cached_frame_monitored(
        vm,
        thread,
        cached,
        values,
        Some("jit-static-bc"),
    );
    disp_census::note(if served.is_some() {
        disp_census::OUT_STATIC_BC
    } else {
        disp_census::OUT_STATIC_BC_REFUSED
    });
    served
}

/// The once-per-site half of [`try_jit_static_bytecode_callee`]. Every refusal
/// is cached as `None`, so an ineligible site asks these questions once —
/// except "the callee's declaring class is not initialised yet", which sets
/// `init_pending` and is NOT cached (round 11): no flush follows `<clinit>`,
/// so a site first reached before initialisation stayed on the slow tail for
/// the life of the process.
pub fn resolve_static_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    if site_name_is_special_cased(info.method_name) {
        return None;
    }
    // A loader-faithful owner that differs from the global by-name answer is
    // exactly the case this path must not try to reproduce.
    if jit_static_owner_override(vm, thread, info).is_some() {
        return None;
    }
    // No native may be in play for this triple, by either route.
    if vm
        .natives
        .native_methods
        .might_have_method_descriptor(info.method_name, info.descriptor)
    {
        return None;
    }
    let owner = {
        let cm = vm.classes.class_manager.read();
        cm.get_loaded_class_id(info.class_name)
    }?;
    if owner == ClassId::new(0) {
        return None;
    }
    // The by-name tail would run `<clinit>` on the way in. Refuse until it
    // has, rather than becoming the path that skips it. The DECLARING class
    // (`jit_static_init_pending`), not `owner`: the static tail never
    // initializes the symbolic owner of an inherited static (JVMS §5.5,
    // wave 14), so an owner test would keep such a site off this path for good.
    if jit_static_init_pending(vm, owner, info).is_some() {
        *init_pending = true;
        return None;
    }
    // `admit_synchronized`: a `static synchronized` callee is served too;
    // `try_jit_static_bytecode_callee` takes its class-mirror monitor through
    // `install_and_run_cached_frame_monitored`
    // (r11w5-tier-static-synchronized-callee-template-patch). Refusing it sent
    // every compiled call of one down the by-name `invoke_or_native` tail.
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        owner,
        info.method_name,
        info.descriptor,
        true,
    )?;
    if !cached.is_static {
        return None;
    }
    // The body this template runs must belong to an initialized class, by
    // the template's own resolution as well as by `jit_static_init_pending`'s.
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// The class a compiled `invokestatic` through the symbolic owner `owner`
/// must still initialize (JVMS §5.5), or `None`: the by-name routes' one
/// answer, `crate::vm::static_owner_init_pending` (declaring class, reviewed
/// VM-service receivers left alone under `--real-jdk`). Interpreter round i1
/// wave 12, lane L5; shared with `invoke_static_or_native` since wave 13.
pub fn jit_static_init_pending(vm: &SharedVm, owner: ClassId, info: &JitInvokeInfo) -> Option<ClassId> {
    crate::vm::static_owner_init_pending(
        vm,
        owner,
        info.class_name,
        info.method_name,
        info.descriptor,
    )
}

/// `true` when the native site cache must not cache a static entry yet: the
/// by-name owner is loaded and its declaring class is not initialized
/// ([`jit_static_init_pending`]). By name, and never loading, because the
/// fill runs with the caller's raw argument words live; the registry the
/// entry is resolved from is name-keyed too. Fill time only.
///
/// An owner no loader holds is not initialized either, and since zt t27/jd1
/// it declines the same way whenever the interpreter's route would load it
/// before deciding (`crate::vm::static_owner_load_pending`): the entry was
/// resolved against the absent owner, where a `Bridge` or an allow-listed
/// `SyntheticStub` cannot yield to the JDK body, and a cached entry kept
/// serving that native after the owner was loaded. The static tail loads the
/// owner (`load_static_owner_before_dispatch`, arguments pinned) and the next
/// call resolves the site on the loaded declaration.
/// `CRATONVM_JIT_STATIC_OWNER_LOADS_FIRST=0`: an absent owner has nothing to
/// initialize and the entry is cached, as before.
pub fn static_native_site_init_pending(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    let owner = vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(info.class_name);
    match owner {
        Some(owner) => jit_static_init_pending(vm, owner, info).is_some(),
        None => crate::vm::static_owner_load_pending(
            vm,
            info.class_name,
            info.method_name,
            info.descriptor,
        )
        .is_some(),
    }
}

/// Initialize the declaring class of a compiled `invokestatic` before the
/// funnel tail can run a registered native for it (JVMS §5.5; the interpreter
/// does so in `execute_invokestatic` since wave 10). `invoke_or_native`'s
/// registry arms and `invoke_static_shared_on_class`'s native override run
/// the native without initializing; a bytecode callee is initialized by the
/// funnel itself, so a triple no native carries (`might_have_method_descriptor`)
/// pays one registry prefilter and nothing else.
///
/// `owner_override` is the loader-faithful owner the tail dispatches on
/// (`jit_static_owner_override`); otherwise the by-name owner. `<clinit>` can
/// collect, and `values` is a Rust copy no collector sees: its references are
/// pinned across the initializer and read back afterwards. Every mode. The
/// body is `crate::vm::init_static_native_owner`, which the by-name static
/// entry `invoke_static_or_native` shares (wave 13).
pub fn jit_static_owner_init_before_native(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    owner_override: Option<ClassId>,
    values: &mut [Value],
) -> Result<(), crate::error::MethodCallFailed> {
    crate::vm::init_static_native_owner(
        vm,
        thread,
        owner_override,
        info.class_name,
        info.method_name,
        info.descriptor,
        values,
    )
}

/// The `invokevirtual` / `invokeinterface` twin of
/// [`try_jit_static_bytecode_callee`]: enter an UNCOMPILED callee through this
/// `(site, receiver class)`'s own cached interpreter frame template instead of
/// re-resolving it by name on every call.
///
/// `Some(..)` means the call was served here. `None` means this site is not
/// eligible and the caller must continue down its ordinary route — the
/// `invoke_or_native` tail — byte-for-byte as before.
///
/// # Why the static half's argument does not simply carry over
///
/// The static half had no receiver, hence no dispatch retarget, no interface
/// rules and no receiver guard. All three come back here, and the way each is
/// discharged is the whole safety argument:
///
/// * **The retarget IS the resolution.** `build_lambda_impl_cached` resolves
///   through `find_method_recursive` starting at the RECEIVER'S OWN class id —
///   the same walk `invoke_on_class_shared` performs after its retarget, and
///   the same one the MIC's compile probe uses (see the "VIRTUAL DISPATCH FIX"
///   note in `jit_invoke_virtual_mic`). A callee it cannot land on — an
///   abstract declaration, an interface method with no implementation on this
///   receiver — has no `Code` attribute and is refused rather than guessed at.
/// * **The receiver guard is the KEY.** The memo is keyed on
///   `(site, receiver class id)`, so a hit is by construction an answer
///   resolved for exactly this receiver class. A site that goes polymorphic
///   gets one entry per class instead of serving the wrong body.
/// * **Loader identity is not in play at all**, and this is the one place this
///   path is STRICTLY safer than its static sibling. That one resolves its
///   owner through `get_loaded_class_id(info.class_name)` — a class NAME, which
///   two loaders can both define — and had to join
///   `flush_class_identity_dispatch_memos` for it. Here the owner is the
///   receiver's own `ClassId`, taken from its header. No name is consulted, so
///   the `globally_named` term that `virtual_dispatch_target_cached` computes
///   has nothing to decide. The memo joins that flush anyway, because a class
///   id can be retired by unloading and the key would then name a class that no
///   longer exists.
///
/// Every other refusal is the static half's, verbatim and for the same reason:
///
/// * an ARRAY receiver is refused (`ObjectKind::Object` only). An array header
///   carries its COMPONENT class id, so `(site, class id)` does not identify
///   one — the invariant `VIRTUAL_TARGET_CACHE` and the KC26 `array.clone()`
///   note already turn on;
/// * **no native anywhere has this `(name, descriptor)`**
///   (`might_have_method_descriptor`), which removes the native-override,
///   `SyntheticStub`-yield, redefine-shadow and
///   `force_native_over_real_jdk_bytecode` questions rather than reproducing
///   them;
/// * the DECLARING class is already initialised — `invoke_shared` would run
///   `<clinit>` on the way in, and this path must never be the thing that skips
///   it. (For a virtual call the receiver's own class is necessarily
///   initialised, but the resolved body may be declared on a supertype.)
/// * `site_name_is_special_cased`;
/// * the method is NOT `static` — an `invokevirtual` that resolves to a static
///   is a miscompile, not a fast path — and the decoded argument count matches
///   `receiver + declared parameters`.
///
/// Per hit, only what can change is re-tested: the declaring class's
/// `RedefineGate` (see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION` and
/// `CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`).
///
/// An instance `synchronized` callee IS served (since 2026-09-23): the
/// implicit monitor is taken by the interpreter doors' uncontended acquire and
/// released by the frame's `monitor_on_exit` — see
/// `install_and_run_cached_frame_monitored`. A contended acquire declines to
/// the by-name tail, which blocks properly.
///
/// SAFETY: `values` are the caller's already-decoded arguments with the
/// receiver at index 0; `thread` is the JIT helper's own borrow.
pub unsafe fn try_jit_virtual_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    receiver_ref: ObjectRef,
    receiver_class_id: ClassId,
    values: &[Value],
    // Round 16 wave 17 (lane mega5): called from `jit_invoke_dispatch`'s blind
    // virtual arm (`blind_dispatch_late_probe_rewind`), not a MIC miss.
    blind: bool,
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !crate::runtime::env_cache::jit_virtual_bytecode_callee() {
        return None;
    }
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    // A redefinition retires what it can change per class (the gate below);
    // see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION`.
    if callee_templates_latched_off() {
        return None;
    }
    let receiver_cid = receiver_class_id.as_u32();
    if receiver_cid == 0 {
        return None;
    }
    // See the array note in the doc comment above.
    if vm.mem.heap.kind_of(receiver_ref) != cratonvm_types::ObjectKind::Object {
        return None;
    }
    let key = (info_key, receiver_cid);
    let cached = match VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        Some(hit) => hit,
        None => {
            let mut init_pending = false;
            // `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` (wave 132, lane
            // L5b): asked once per (site, receiver class), with the thread the
            // predicate's context needs; no GC point (class-store reads only).
            let user_loader_resource_body =
                compiled_user_loader_resource_body(vm, thread, info, receiver_class_id);
            let resolved = resolve_virtual_bytecode_callee(
                vm,
                info,
                receiver_class_id,
                user_loader_resource_body,
                &mut init_pending,
            );
            // See the static twin: an uninitialised declaring class is not a
            // cacheable refusal.
            if !init_pending {
                VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| {
                    c.borrow_mut().insert(key, resolved.clone());
                });
            }
            resolved
        }
    };
    let Some((cached, gate)) = cached else {
        disp_census::note(disp_census::OUT_VIRTUAL_BC_REFUSED);
        return None;
    };
    if gate.is_stale() {
        VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().remove(&key);
        });
        return None;
    }
    if callee_template_declines_redefined_target(gate.generation) {
        return None;
    }
    // An instance callee takes the receiver plus its declared parameters. An
    // arity disagreement means the decode and the template describe different
    // methods, which is a refusal, never a guess.
    if values.len() != cached.num_params as usize + 1 {
        return None;
    }
    // Count the invocation the ordinary path would have counted, and stand
    // down if a compiled body now exists — see
    // `bytecode_callee_compiled_or_nominate`. Without this a callee served here
    // is never nominated, so it stays interpreted for the life of the process;
    // and a callee that IS compiled would be run interpreted anyway, which is a
    // straight loss rather than a missed win. MEASURED at 15% on
    // `probes/ExchangeProbe.java` before it was added.
    let warmup = compiled_template_warmup(info_key, cached.invoc_key());
    if crate::runtime::interpreter::bytecode_callee_compiled_or_nominate(vm, thread, &cached, &warmup) {
        // Round 16 wave 17 (lane mega5): a compiled body exists, so the
        // caller is about to take the by-name tail. At the blind arm, let the
        // key's next call probe for it instead of re-counting to the threshold.
        blind_dispatch_late_probe_rewind(key, blind);
        disp_census::note(disp_census::OUT_VIRTUAL_BC_REFUSED);
        return None;
    }
    // `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` (wave 132, lane L5b):
    // the `Bridge` native this template stands in for counts the
    // `--compatible` `JdkUrlClassLoaders` divergence per call; so does its
    // stand-in, so the alignment census reads the same in both arms. Only an
    // admitted user-loader resource body has this class and triple here (the
    // ordinary resolver refuses those names). Off: one per-VM bool load.
    if vm.config.user_loader_resource_bytecode_compiled
        && !vm.config.is_jdk_only()
        && &*cached.class_name == "java/lang/ClassLoader"
        && is_class_loader_resource_triple(info.method_name, info.descriptor)
    {
        vm.classes
            .note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders);
    }
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // In-place install, not a by-value build: `execute_prebuilt_frame` pushes
    // through `push_frame_and_fire_entry`, which harvests and trims the
    // retired slot, so this helper used to undo frame-slot reuse for the
    // interpreter frames beneath it once per call. See
    // `install_and_run_cached_frame`.
    //
    // The `_monitored` form serves a `synchronized` template (admitted by
    // `resolve_virtual_bytecode_callee`) with the interpreter doors' own
    // uncontended acquire, and declines on contention -- the caller then
    // continues to `invoke_or_native`, whose acquire blocks properly. A
    // non-synchronized template is exactly `install_and_run_cached_frame`.
    let served = crate::runtime::interpreter::install_and_run_cached_frame_monitored(
        vm,
        thread,
        cached,
        values,
        Some("jit-virtual-bc"),
    );
    disp_census::note(if served.is_some() {
        disp_census::OUT_VIRTUAL_BC
    } else {
        disp_census::OUT_VIRTUAL_BC_REFUSED
    });
    served
}

/// [`site_name_is_special_cased`], minus `apply`.
///
/// That list is a NAME-only filter shared with the leaf-native path, and it is
/// deliberately over-broad there: a leaf native resolves to a callback and
/// cannot afford to reason about which of `invoke_or_native`'s special cases
/// might apply. Deferring to it here cost the single most common interface
/// method in Java. MEASURED, `probes/XferProbe2.java` with
/// `CRATONVM_DBG=mic-prof`, before this narrowing:
///
/// ```text
/// virtual  out_virtual_bc=1048575  out_virtual_bc_refused=0          <- 100% served
/// iface    out_virtual_bc=0        out_virtual_bc_refused=1048575    <- 100% refused
/// ```
///
/// `apply` is the SAM of `java.util.function.Function`, i.e. every reactive
/// operator that is a CLASS rather than a lambda, which is exactly the
/// population the interpreted-callee page exists for. It earns its place on the
/// shared list through ONE rescue: `invoke_or_native` redirects
/// `apply(Ljava/lang/Object;)Ljava/lang/Object;` to
/// `applyAsInt`/`applyAsLong`/`applyAsDouble` when the dispatch class is
/// `java/util/function/To{Int,Long,Double}Function`, because those interfaces
/// declare no `apply` at all and naive dispatch raises `NoSuchMethodError`
/// (Spring/Eureka).
///
/// Narrowed to that rescue's own triple. Two independent things already make it
/// unreachable from here, and the explicit test is so a later change to either
/// does not quietly re-open it: an interface that declares no `apply` has no
/// `Code` for it, so `build_lambda_impl_cached` refuses; and a receiver whose
/// class is an interface DIFFERENT from the call site's never reaches
/// resolution at all, because `virtual_dispatch_target_cached` answers
/// `cacheable_receiver = false` for it.
pub fn virtual_site_name_is_special_cased(info: &JitInvokeInfo) -> bool {
    if info.method_name == "apply" {
        return info.descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
            && matches!(
                info.class_name,
                "java/util/function/ToIntFunction"
                    | "java/util/function/ToLongFunction"
                    | "java/util/function/ToDoubleFunction"
            );
    }
    site_name_is_special_cased(info.method_name)
}

/// The once-per-(site, receiver class) half of
/// [`try_jit_virtual_bytecode_callee`]. Every refusal is cached as `None`, so
/// an ineligible pair asks these questions once.
pub fn resolve_virtual_bytecode_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    // [`compiled_user_loader_resource_body`]'s verdict for this receiver class.
    user_loader_resource_body: bool,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    if user_loader_resource_body {
        return resolve_user_loader_resource_body_callee(vm, info, receiver_class_id, init_pending);
    }
    if virtual_site_name_is_special_cased(info) {
        return None;
    }
    // No native may be in play for this triple, by either route.
    if vm
        .natives
        .native_methods
        .might_have_method_descriptor(info.method_name, info.descriptor)
    {
        return None;
    }
    // `build_bytecode_callee_cached` walks from the receiver's class. Where
    // that walk is not §5.4.6's selection (a private hider, a cross-package
    // package-private "override"), refuse: the by-name tail then dispatches
    // from the selected class (`receiver_selection_retarget`). The guard is
    // dropped before the build, which takes its own.
    let selection_moves = {
        let cm = vm.classes.class_manager.read();
        jit_site_owner(&cm, info).is_some_and(|owner| {
            crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                cm.class_store(),
                receiver_class_id,
                owner,
                info.method_name,
                info.descriptor,
            )
            .is_some()
        })
    };
    if selection_moves {
        return None;
    }
    // Nor where selection ends in an error: the by-name tail raises it
    // (`CachedDispatchTarget::selection_error`).
    if jit_selection_errs(
        vm,
        &vm.classes.class_manager.read(),
        receiver_class_id,
        info,
    ) {
        return None;
    }
    // `admit_synchronized`: an instance `synchronized` callee is served too —
    // `try_jit_virtual_bytecode_callee` owns its monitor (see there). Refusing
    // it sent every compiled call of one down the by-name `invoke_or_native`
    // route for the life of the process: the only body the JIT publishes for
    // an `ACC_SYNCHRONIZED` method is a wrapped entry (`requires_wrapped_entry`),
    // which the compile probe above -- a RAW door -- correctly refuses, and the
    // by-name tail then interprets the method. The hot case is
    // `Throwable.fillInStackTrace()`, which the real `Throwable.<init>` calls
    // under `--real-jdk` for every throwable a compiled method constructs. The
    // same-binary A/B is the doors' own kill switch,
    // `CRATONVM_JIT_NO_DOOR_SYNC=1`: the call then takes the by-name tail
    // exactly as before.
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        true,
    )?;
    if cached.is_static {
        return None;
    }
    // `invoke_shared` would run `<clinit>` on the way in. Refuse until it has,
    // rather than becoming the path that skips it. The DECLARING class, not the
    // receiver's: the resolved body may live on a supertype.
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// The three instance resource triples of `java/lang/ClassLoader` whose
/// registered `Bridge` natives (`classloader::cl_get_resource_essential` and
/// siblings) route a user-loader receiver to the JDK's own body: the
/// interpreter's own list (`dispatch_virtual::class_loader_resource_triple`),
/// not a copy of it (i73 closure, 2026-11-02).
#[inline]
pub fn is_class_loader_resource_triple(method_name: &str, descriptor: &str) -> bool {
    crate::runtime::interpreter::class_loader_resource_triple(method_name, descriptor)
}

/// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` (interpreter round i6
/// wave 132, lane L5b; the compiled half of
/// `i73-L5-proposal-user-loader-get-resource-without-the-native-hop`, default
/// OFF, effective only with `CRATONVM_USER_LOADER_RESOURCE_BYTECODE`): may a
/// compiled `invokevirtual` of a `ClassLoader` resource triple on a receiver
/// of `receiver_class_id` enter the JDK's own body through this module's
/// frame template?
///
/// Today such a call is refused by NAME here (`site_name_is_special_cased`,
/// `might_have_method_descriptor`) and takes the by-name tail, whose
/// `Bridge` native asks `jdk_only_user_loader_receiver` and, for a
/// user-defined loader or a bare `URLClassLoader`, re-enters exactly that body
/// through `invoke_special_bytecode_only`. The interpreter's virtual cache
/// skips the hop since wave 119/120; compiled callers did not, so the two
/// tiers ran different paths (same answer). The verdict is the interpreter's
/// own (`classloader::user_loader_resource_body_for_class`, a function of the
/// class: `jdk_url_class_loaders()` and the `LoaderKind` computation), asked
/// once per (site, receiver class). It reads the class store only (no
/// allocation, no load), so the caller's unrooted `values` stay valid.
///
/// `invokevirtual` only: `invokespecial` (`super.getResource(name)`) and the
/// static `getSystemResource*` forms keep the native, as in the interpreter
/// half; a null name never reaches here (both helpers raise its NPE before).
pub fn compiled_user_loader_resource_body(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> bool {
    if !(vm.config.user_loader_resource_bytecode_compiled)
    {
        return false;
    }
    if info.invoke_kind != 0 || !is_class_loader_resource_triple(info.method_name, info.descriptor)
    {
        return false;
    }
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    cratonvm_native_builtins::classloader::user_loader_resource_body_for_class(
        &mut ctx,
        receiver_class_id,
    )
}

/// [`resolve_virtual_bytecode_callee`] for a receiver class
/// [`compiled_user_loader_resource_body`] admitted. The template is built
/// past the native-shadow refusal (the `ClassLoader` `Bridge` is the native
/// being stood in for), and then admitted only when it is exactly what that
/// native would have re-entered:
///
/// * the selected body is `java/lang/ClassLoader`'s own (a loader class that
///   overrides the method, or `URLClassLoader.getResourceAsStream`, keeps the
///   ordinary refusal and the by-name tail);
/// * NO native is registered for the triple on any class from the receiver up
///   to (not including) `ClassLoader` -- such a native would win the by-name
///   dispatch, so it must keep winning;
/// * the resolver's own selection refusals hold (a §5.4.6 move, a selection
///   error), and the declaring class is initialised.
///
/// Every refusal is cached as `None` by the caller, as for the ordinary
/// resolver, so an ineligible pair asks once.
pub fn resolve_user_loader_resource_body_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached_admitting_native(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
    )?;
    if cached.is_static || cached.is_synchronized || &*cached.class_name != "java/lang/ClassLoader"
    {
        return None;
    }
    let selection_moves = {
        let cm = vm.classes.class_manager.read();
        jit_site_owner(&cm, info).is_some_and(|owner| {
            crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                cm.class_store(),
                receiver_class_id,
                owner,
                info.method_name,
                info.descriptor,
            )
            .is_some()
        })
    };
    if selection_moves {
        return None;
    }
    if jit_selection_errs(
        vm,
        &vm.classes.class_manager.read(),
        receiver_class_id,
        info,
    ) {
        return None;
    }
    let reaches_class_loader_without_native = {
        let cm = vm.classes.class_manager.read();
        let store = cm.class_store();
        let mut probe = Some(receiver_class_id);
        let mut reached = false;
        while let Some(cid) = probe {
            if cid == cached.declaring_class_id {
                reached = true;
                break;
            }
            let Some(class) = store.get(cid) else { break };
            if vm
                .natives
                .native_methods
                .find(&class.name, info.method_name, info.descriptor)
                .is_some()
            {
                break;
            }
            probe = class.superclass;
        }
        reached
    };
    if !reaches_class_loader_without_native {
        return None;
    }
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// The `invokespecial` twin. Same memo, same refusals; what differs is where
/// the owner comes from and why the key does not carry a receiver class.
///
/// `invokespecial` resolves against the constant-pool class and must NOT
/// virtually retarget onto the receiver's runtime class — the retarget is what
/// turns `RunLast.execute`'s super-call into a self-call and recurses until the
/// stack ends (the picocli `StackOverflowError` the `invoke_kind == 1` arm was
/// written for). `find_method_recursive` walking UP from the CP class is
/// exactly `invoke_special_shared`'s own "walk the hierarchy from the
/// CP-resolved class to the declaring class", so this path reproduces that
/// dispatch rather than approximating it, and the target is a pure function of
/// the site — hence a site-only key.
///
/// The loader question is answered the same way that arm answers it, and it is
/// the reason the resolver takes a `thread`: `info.class_name` is constant-pool
/// TEXT, and with two loaders defining that binary name a by-name lookup binds
/// the call to whichever copy the global map holds
/// (BUG-JIT-INVOKESPECIAL-LOADER-20260726). This refuses outright unless
/// `declaring_class_id` is present AND `resolve_class_loader_aware` answers, so
/// the owner is a `ClassId` resolved through the CALLER's loader or the memo is
/// never built.
///
/// A `synchronized` instance callee is served only under
/// [`special_bytecode_callee_sync_enabled`] (JIT round 16 wave 8, lane sync),
/// through the virtual twin's monitored door ([`special_bytecode_callee_monitored`]).
///
/// SAFETY: as [`try_jit_virtual_bytecode_callee`].
pub unsafe fn try_jit_special_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !crate::runtime::env_cache::jit_virtual_bytecode_callee() {
        return None;
    }
    if info.invoke_kind != 1 {
        return None;
    }
    if callee_templates_latched_off() {
        return None;
    }
    let cached = match SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&info_key).cloned()) {
        Some(hit) => hit,
        None => {
            let mut init_pending = false;
            let resolved = resolve_special_bytecode_callee(vm, thread, info, &mut init_pending);
            // See the static twin: an uninitialised owner is not a cacheable
            // refusal.
            if !init_pending {
                SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| {
                    c.borrow_mut().insert(info_key, resolved.clone());
                });
            }
            resolved
        }
    };
    let Some((cached, gate)) = cached else {
        disp_census::note(disp_census::OUT_SPECIAL_BC_REFUSED);
        return None;
    };
    if gate.is_stale() {
        SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().remove(&info_key);
        });
        return None;
    }
    if callee_template_declines_redefined_target(gate.generation) {
        return None;
    }
    // Receiver plus declared parameters, and the receiver must be a live
    // object: `invokespecial` on null is an NPE the by-name path raises, and
    // this fast path must not turn it into a frame with a null `this`.
    if values.len() != cached.num_params as usize + 1 {
        return None;
    }
    if !matches!(values.first(), Some(Value::Object(Some(_)))) {
        return None;
    }
    // Count the invocation the ordinary path would have counted, and stand
    // down if a compiled body now exists — see
    // `bytecode_callee_compiled_or_nominate`. Without this a callee served here
    // is never nominated, so it stays interpreted for the life of the process;
    // and a callee that IS compiled would be run interpreted anyway, which is a
    // straight loss rather than a missed win. MEASURED at 15% on
    // `probes/ExchangeProbe.java` before it was added.
    let warmup = compiled_template_warmup(info_key, cached.invoc_key());
    if crate::runtime::interpreter::bytecode_callee_compiled_or_nominate(vm, thread, &cached, &warmup) {
        disp_census::note(disp_census::OUT_SPECIAL_BC_REFUSED);
        return None;
    }
    // JIT round 16 wave 8 (lane sync): the resolver admits a `synchronized`
    // template only under `CRATONVM_JIT_SPECIAL_BC_SYNC`; every other template
    // takes the unchanged path below.
    if cached.is_synchronized {
        return special_bytecode_callee_monitored(vm, thread, cached, values);
    }
    disp_census::note(disp_census::OUT_SPECIAL_BC);
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // In-place install, not a by-value build: `execute_prebuilt_frame` pushes
    // through `push_frame_and_fire_entry`, which harvests and trims the
    // retired slot, so this helper used to undo frame-slot reuse for the
    // interpreter frames beneath it once per call. See
    // `install_and_run_cached_frame`. Interpreter round i7 wave 138, lane L1
    // (`CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP`): the compiled caller's
    // form, which re-invokes a callee its `MethodEntry` callback popped.
    Some(
        crate::runtime::interpreter::install_and_run_cached_frame_from_compiled(
            vm,
            thread,
            cached,
            values,
            Some("jit-special-bc"),
        ),
    )
}

/// `CRATONVM_JIT_SPECIAL_BC_SYNC` (JIT round 16 wave 8, lane sync; default
/// ON since the round-16 w8b A/B, `0` refuses): may the `invokespecial` callee door
/// ([`try_jit_special_bytecode_callee`]) serve a `synchronized` INSTANCE
/// template, as the virtual door has since 2026-09-23?
///
/// The arm this covers is not only `super.m()` and private calls: the JIT
/// devirtualizes an `invokevirtual` whose receiver class is `final` (or whose
/// method is) to this kind, so a compiled call of a `synchronized` method on a
/// final class (`StringBuffer`, a `final` user class) whose only published
/// body is a WRAPPED entry -- the self-locking route refuses methods with an
/// exception table or a nested `synchronized` block -- was refused here and
/// went down the by-name `invoke_special_shared` tail for the life of the
/// process: about 9-11 us a call on the Windows box against ~50-100 ns for
/// the unsynchronized twin (`R16SyncDoorCalls` `tc`/`bl`/`th`, base binary,
/// `CRATONVM_DBG=mic-prof` shows `out_special_bc_refused` = every call). The
/// virtual twin's door takes such a callee's monitor with the interpreter
/// doors' uncontended acquire, declines on contention (the tail then blocks
/// properly), and enters its wrapped body under that hold when the body is
/// one the door may enter.
///
/// Read once per site: [`resolve_special_bytecode_callee`]'s answer is
/// memoized in `SPECIAL_BYTECODE_CALLEE_CACHE`.
pub fn special_bytecode_callee_sync_enabled() -> bool {
    true
}

/// [`try_jit_special_bytecode_callee`]'s arm for a `synchronized` instance
/// template: `install_and_run_cached_frame_monitored`, the virtual door's
/// serve, with its census. `None` (contention) continues to the by-name tail.
pub fn special_bytecode_callee_monitored(
    vm: &SharedVm,
    thread: &mut JvmThread,
    cached: std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    let served = crate::runtime::interpreter::install_and_run_cached_frame_monitored(
        vm,
        thread,
        cached,
        values,
        Some("jit-special-bc"),
    );
    disp_census::note(if served.is_some() {
        disp_census::OUT_SPECIAL_BC
    } else {
        disp_census::OUT_SPECIAL_BC_REFUSED
    });
    served
}


/// The once-per-site half of [`try_jit_special_bytecode_callee`].
pub fn resolve_special_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    if site_name_is_special_cased(info.method_name) {
        return None;
    }
    // `<init>` and `<clinit>` are refused by name rather than by consequence.
    // `invoke_on_class_shared_inner` carries explicit carve-outs for both, an
    // `<init>` is where object initialisation and the VM's own synthetic
    // layouts meet, and neither is a throughput path worth opening that surface
    // for.
    if info.method_name.starts_with('<') {
        return None;
    }
    if vm
        .natives
        .native_methods
        .might_have_method_descriptor(info.method_name, info.descriptor)
    {
        return None;
    }
    // Loader-faithful owner or nothing — see the doc comment. A substituted
    // owner is its recorded id (`jit_substituted_owner`).
    if info.declaring_class_id == 0 {
        return None;
    }
    // The caller's `CONSTANT_Class` entry for an isolated loader's caller
    // (round 16, lane classsite, CS2-2).
    let owner = match jit_substituted_owner(vm, info) {
        Some(owner) => owner,
        None => jit_bridged_owner_entry(
            vm,
            thread,
            ClassId::new(info.declaring_class_id),
            info.class_name,
        )?,
    };
    if owner == ClassId::new(0) {
        return None;
    }
    if !crate::vm::is_class_initialized_via_manager(vm, owner) {
        *init_pending = true;
        return None;
    }
    // `admit_synchronized` under `CRATONVM_JIT_SPECIAL_BC_SYNC` only (JIT
    // round 16 wave 8, lane sync): `try_jit_special_bytecode_callee` then
    // owns the monitor, as the virtual twin does. Off, this is exactly
    // `build_lambda_impl_cached`.
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        owner,
        info.method_name,
        info.descriptor,
        special_bytecode_callee_sync_enabled(),
    )?;
    if cached.is_static {
        return None;
    }
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// Round 15 wave 5 (lane walk4; F1 of `r15w4-walk3-review-findings-FIXED-20260930.md`,
/// W3-4 of `jit-r15w4-walk3-proposals-RETIRED-20261003.md`): must the compiled `invokespecial`
/// tail run `owner`'s own (woven) bytecode for this site instead of the native
/// `invoke_special_shared` would run first?
///
/// `invoke_special_shared_impl` runs the native registered on the exact
/// constant-pool triple before anything else and asks no redefine question,
/// so once a JVMTI agent (a Mockito inline mock) had woven `owner`'s body, a
/// compiled `super.m()` / private call ran the native and skipped the advice,
/// while the interpreter's invokespecial (stackless step 1's
/// `class_redefined`) and `invoke_or_native`'s probe run the woven body.
///
/// Yes iff all hold: something was redefined; the one redefine rule is on
/// (`CRATONVM_ANCESTOR_REDEFINE_RULE`, its kill switch); the method is not a
/// class initializer (`<init>` is admitted since round 15 wave 6, see
/// [`woven_owner_body_wins`]);
/// `owner` is the constant-pool class itself (not a substituted owner, whose
/// name the native is not registered under); a native is registered on that
/// triple and is not a `SyntheticStub` that already yields (the tail runs the
/// bytecode then); `owner` itself declares the method with a body; the rule
/// reads it `Woven` (`owner_redefinition`, W2-1's predicate);
/// and the native is outside the receiver-aware forced immunity, the
/// reading `invoke_or_native` and the stackless door give a single call.
/// Every term is a read; nothing allocates or runs Java, so the caller's
/// decoded arguments stay valid for `execute`. The terms are
/// [`woven_owner_body_wins`]'s.
pub fn special_site_runs_woven_body(
    vm: &SharedVm,
    owner: ClassId,
    info: &JitInvokeInfo,
    values: &[Value],
) -> bool {
    let receiver = match values.first() {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };
    woven_owner_body_wins(
        vm,
        owner,
        info.class_name,
        info.method_name,
        info.descriptor,
        receiver,
    )
}

/// Round 15 wave 5 (lane walk4): does a JVMTI agent's woven body of
/// `owner_name.method_name descriptor` (declared by `owner`, whose name it is)
/// win over the native registered on that exact triple? The question every
/// door that is about to run the native REGISTERED ON THE DECLARING CLASS
/// itself must ask: the compiled `invokespecial` tail
/// ([`special_site_runs_woven_body`]) and the bytecode arm of
/// `invoke_on_class_shared_inner` (its `override_cb` re-check, which
/// `invoke_or_native`'s tail and `invoke_special_shared`'s reach; see
/// `r15w5-walk4-woven-owner-native-override-patch-FIXED-20260930.md`).
///
/// Yes iff all hold: something was redefined; the one redefine rule is on
/// (`CRATONVM_ANCESTOR_REDEFINE_RULE`, its kill switch); the method is not
/// `<clinit>`; a native is registered on the triple and
/// is not a `SyntheticStub` that already yields (the caller runs the bytecode
/// then); `owner` is the class named `owner_name` and declares the method with
/// a body; `owner_redefinition` reads it `Woven`; and the native is outside the
/// receiver-aware forced immunity (`receiver` is `None` for a static call),
/// the reading `invoke_or_native`'s probe and the stackless door give a single
/// call. Every term is a read: nothing allocates or runs Java. The class
/// manager is taken with `try_read`; a writer answers `false` (the native, as
/// before).
///
/// `<init>` (WK4-5, round 15 wave 6 lane sweepB; R2 of
/// `r15w5-walk4-review-findings-FIXED-20260930.md`): answered like any method. The
/// interpreter's stackless door already drops a woven owner's non-immune
/// constructor native (its `class_redefined` guard reads every name), and
/// the woven owner's other methods run their bytecode in every door, so
/// keeping the native constructor in compiled code alone mixed a
/// native-built object with bytecode readers, differed between the cold and
/// the compiled calls of one site, and skipped a constructor advice (Mockito's
/// `mockConstruction`) HotSpot runs.
/// `CRATONVM_WOVEN_OWNER_INIT_BODY=0` keeps the native constructor, as
/// before (read only when the answer would be the woven `<init>`).
pub fn woven_owner_body_wins(
    vm: &SharedVm,
    owner: ClassId,
    owner_name: &str,
    method_name: &str,
    descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    if !crate::classloading::any_class_redefined() || method_name == "<clinit>" {
        return false;
    }
    let registry = &vm.natives.native_methods;
    if !registry.ancestor_redefine_rule()
        || registry
            .resolve_id(owner_name, method_name, descriptor)
            .is_none()
        || crate::runtime::interpreter::synthetic_stub_should_yield_to_real_bytecode(
            vm,
            owner_name,
            method_name,
            descriptor,
        )
    {
        return false;
    }
    {
        let Some(cm) = vm.classes.class_manager.try_read() else {
            return false;
        };
        let declares_body = cm.get_class(owner).is_some_and(|class| {
            class.name.as_ref() == owner_name
                && class
                    .find_method(method_name, descriptor)
                    .is_some_and(|m| !m.is_native() && !m.is_abstract() && m.code().is_some())
        });
        if !declares_body
            || crate::runtime::interpreter::owner_redefinition(
                &cm,
                registry,
                owner,
                method_name,
                descriptor,
            ) != crate::runtime::interpreter::OwnerRedefinition::Woven
        {
            return false;
        }
    }
    !crate::runtime::interpreter::redefine_immune_forced_native_for_receiver(
        vm,
        owner_name,
        method_name,
        descriptor,
        receiver,
    ) && (method_name != "<init>"
        || true)
}

/// WK4-2 (round 15 wave 6, lane sweepB; R1/R4 of
/// `r15w5-walk4-review-findings-FIXED-20260930.md`): [`woven_owner_body_wins`] for
/// `invoke_special_shared_impl`'s first arm, which runs the native registered
/// on `class_name` BEFORE it resolves the class -- so every caller of
/// `invoke_special_shared*` (a `findSpecial` handle, a native's
/// `ctx.invoke_special`, the compiled bridge tail, the two-loader static tail
/// `invoke_static_shared_on_class`) gets the answer the compiled
/// `invokespecial` tail already gives. `pre_resolved` is the caller's
/// loader-faithful class; without it the class is looked up among the LOADED
/// classes by name (a class never loaded was never redefined), the same name
/// the impl's own `load_class_concurrent` resolves next. `receiver` is `None`
/// for a static call. One relaxed load when nothing was ever redefined; the
/// class manager is taken with `try_read` (a writer answers `false`: the
/// native, as before). `CRATONVM_ANCESTOR_REDEFINE_RULE=0` answers `false`.
pub fn special_shared_owner_body_wins(
    vm: &SharedVm,
    pre_resolved: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let owner = match pre_resolved {
        Some(owner) => Some(owner),
        None => vm
            .classes
            .class_manager
            .try_read()
            .and_then(|cm| cm.get_loaded_class_id(class_name)),
    };
    owner.is_some_and(|owner| {
        woven_owner_body_wins(vm, owner, class_name, method_name, descriptor, receiver)
    })
}

/// Is the ancestor `owner` that a site-cache entry serves the native of still
/// at the redefine generation the fill read (`owner_generation`)? Asked by the
/// hit once the VM's redefine epoch moved past the entry's stamp (round 15
/// wave 4, lane walk2; `r15w3-walk-helpers-site-cache-owner-redefine-patch-FIXED-20260930.md`).
///
/// The fill applied the one redefine rule (`redefined_ancestor_guard`, door
/// `JitSiteCache`) to the owner as it was then, so the entry stays right until
/// the owner is redefined again. Then (`false`) the entry is dropped and this
/// call declined: the funnel's walk applies the rule to this call, and the
/// next one re-resolves the site. Until then (`true`) the entry is re-stamped
/// with the epoch read here, so its later hits are one atomic load again. A
/// redefinition that ends a publication-sensitive or retire flush drops the
/// memo anyway; this covers the scoped redefinitions (the owner registers no
/// native under any method its class file declares, so
/// `scoped_redefinition_admitted`) and the window between a fill and the
/// flush. Sound by the order `site_class_was_redefined` relies on: the
/// redefinition bumps the class's generation under the class-manager writer
/// before the epoch, and the epoch is read here before the generation. Cold.
#[cold]
#[inline(never)]
pub fn native_site_owner_still_current(
    vm: &SharedVm,
    info_key: JitSiteKey,
    registry_generation: u32,
    owner: ClassId,
    owner_generation: u32,
) -> bool {
    let epoch = vm.jit.jit_cache.redefine_epoch();
    let Some(current) = vm
        .classes
        .class_manager
        .try_read()
        .map(|cm| cm.class_redefine_generation(owner))
    else {
        // A writer holds the class manager: decline this call, keep the entry.
        return false;
    };
    if current != owner_generation {
        NATIVE_SITE_CACHE.with(|cache| {
            cache.borrow_mut().remove(&info_key);
        });
        return false;
    }
    NATIVE_SITE_CACHE.with(|cache| {
        if let Some((generation, Some(entry))) = cache.borrow_mut().get_mut(&info_key) {
            if *generation == registry_generation {
                entry.fill_redefine_epoch = epoch;
            }
        }
    });
    true
}


// ---------------------------------------------------------------------------
// The bound downcall row (JIT round 16 wave 12, lane ffmdoor; FFC16-1,
// `CRATONVM_JIT_FFM_DOWNCALL_BIND`, default ON since wave 12, `=0` off)
// ---------------------------------------------------------------------------
//
// A compiled `MethodHandle.invokeExact` whose handle is a Panama downcall
// carrier reaches the native site cache's `MethodHandle` carrier entry
// (`method_handle_carrier_site`) and runs the erased `invokeExact` native:
// the whole door, then `pe_downcall_invoke`, then a box the caller unboxes
// (`r16-ffm-plain-downcall-costs-microseconds-FIXED-20261007.md`: ~2.4 us per call
// on Linux against HotSpot's ~8-22 ns; the constant bind refuses downcall
// carriers). With the switch on, a site whose handle the native side admits
// (`panama::jit_downcall_bind_target`: a plain carrier at its own exact type,
// where every step of the door is the identity) is bound once per handle and
// collection epoch, and its calls go through the same funnel to
// `panama::jit_downcall_bound_native` -- the plain path -- with the site's
// own argument decode and the same return handling as the carrier entry.

/// One compiled site's downcall bind ([`try_jit_ffm_downcall_bound`]).
pub struct FfmBindSite {
    pub epoch: u64,
    /// The handle's address at the fill.
    pub handle: u64,
    /// The call site's descriptor the admission was asked about: the key is a
    /// site info's address, which a later site may reuse.
    pub site: Box<str>,
    /// The handle's slot-0 `type` object at the fill (`mh_bind_slot_key`), or
    /// `None` when the admission refused the handle.
    pub bound_type: Option<u64>,
    /// Fills within one collection epoch for another handle; past
    /// [`FFM_BIND_MAX_CHURN`] the site stays with the door.
    pub churn: u32,
    /// JIT round 16 wave 16 (lane ffmlean; FFD16-1): the bound handle's
    /// precompiled call (`CRATONVM_JIT_FFM_DOWNCALL_DIRECT`), read at the fill
    /// from the handle the bind validates per call.
    pub direct: Option<cratonvm_native_builtins::panama::JitDowncallDirect>,
    /// `CRATONVM_DBG_FFM`: this site's first direct call was reported.
    pub direct_reported: std::cell::Cell<bool>,
    /// JIT round 18 wave 11 (lane ffmret): `CRATONVM_DBG_FFM`, this site's
    /// warm check reported its direct stub (`[ffm-direct] bind`), and its
    /// first declined direct call was reported (`[ffm-direct] declined`).
    pub bind_reported: std::cell::Cell<bool>,
    pub decline_reported: std::cell::Cell<bool>,
}

/// JIT round 18 wave 11 (lane ffmret;
/// `r18-ffm-direct-hit-line-prints-before-the-row-serves-20261006`):
/// `CRATONVM_DBG_FFM` lines of the direct row, printed by
/// [`ffm_downcall_direct_call`] once it knows the outcome: `[ffm-direct] hit`
/// after the site's first SERVED direct call, `[ffm-direct] declined ...
/// reason=admits|segment` after its first decline (the bound call then serves
/// it). Once each per site and thread, keyed as [`FFM_BIND_SITES`] is.
#[cold]
pub fn ffm_direct_outcome_report(
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
    declined: Option<&'static str>,
) {
    let first = FFM_BIND_SITES.with(|sites| {
        let sites = sites.borrow();
        let Some(site) = sites.get(&info_key) else {
            return false;
        };
        match declined {
            None => !site.direct_reported.replace(true),
            Some(_) => !site.decline_reported.replace(true),
        }
    });
    if !first {
        return;
    }
    match declined {
        None => eprintln!(
            "[ffm-direct] hit site {}.{}{} fn=0x{:x} critical={} segments={} returns-object={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            stub.fn_addr(),
            stub.critical(),
            stub.segments(),
            stub.returns_object()
        ),
        Some(reason) => eprintln!(
            "[ffm-direct] declined site {}.{}{} fn=0x{:x} critical={} segments={} reason={reason}",
            info.class_name,
            info.method_name,
            info.descriptor,
            stub.fn_addr(),
            stub.critical(),
            stub.segments()
        ),
    }
}

/// [`ffm_downcall_site_bound`]'s answer.
#[derive(Clone, Copy)]
pub enum FfmBindAnswer {
    /// Not bound: the door serves the call.
    Door,
    /// Bound, with the handle's direct call when it has one.
    Bound(Option<cratonvm_native_builtins::panama::JitDowncallDirect>),
}

/// Re-fills for another handle within one epoch after which a site gives up.
pub const FFM_BIND_MAX_CHURN: u32 = 8;
/// Bound (or refused) sites kept per thread.
pub const FFM_BIND_MAX_SITES: usize = 1024;

thread_local! {
    /// Downcall binds, by the compiled site's key.
    pub static FFM_BIND_SITES: std::cell::RefCell<rustc_hash::FxHashMap<JitSiteKey, FfmBindSite>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Is `handle` bound at this site ([`FfmBindSite`])? Asks the admission on a
/// miss (the cold fill). [`FfmBindAnswer::Door`]: the door serves the call.
pub fn ffm_downcall_site_bound(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    handle: ObjectRef,
) -> FfmBindAnswer {
    let epoch = vm.mem.address_epoch();
    let raw = handle.as_ptr() as u64;
    // `Some(answer)` on a warm hit; `None` asks the admission.
    let warm = FFM_BIND_SITES.with(|sites| {
        let sites = sites.borrow();
        let site = sites.get(&info_key)?;
        if site.churn > FFM_BIND_MAX_CHURN {
            return Some(FfmBindAnswer::Door);
        }
        if site.epoch != epoch || site.handle != raw || &*site.site != info.descriptor {
            return None;
        }
        match site.bound_type {
            None => Some(FfmBindAnswer::Door),
            // The handle the fill read, in the fill's epoch: its `type` slot
            // is still the bound one, or the admission is asked again.
            Some(word) => (mh_bind_slot_key(vm.mem.heap.get_field(handle, 0)) == word).then(|| {
                if let Some(direct) = site.direct {
                    // JIT round 18 wave 11 (lane ffmret): this is the bind's
                    // warm check, before the row decides, so the line says
                    // `bind`; `[ffm-direct] hit` / `declined` are printed by
                    // `ffm_downcall_direct_call` once the row has answered.
                    if !site.bind_reported.replace(true)
                        && cratonvm_native_builtins::panama::ffm_dbg_enabled()
                    {
                        eprintln!(
                            "[ffm-direct] bind site {}.{}{} fn=0x{:x} critical={} segments={}",
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                            direct.fn_addr(),
                            direct.critical(),
                            direct.segments()
                        );
                    }
                }
                FfmBindAnswer::Bound(site.direct)
            }),
        }
    });
    if let Some(answer) = warm {
        return answer;
    }
    ffm_downcall_site_fill(vm, thread, info, info_key, handle, epoch)
}

/// The cold half of [`ffm_downcall_site_bound`].
#[cold]
pub fn ffm_downcall_site_fill(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    handle: ObjectRef,
    epoch: u64,
) -> FfmBindAnswer {
    use cratonvm_native_builtins::panama::{
        jit_downcall_bind_target, jit_downcall_direct_stub, JitDowncallBind,
    };
    let raw = handle.as_ptr() as u64;
    let (answer, direct) = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let answer = jit_downcall_bind_target(&mut ctx, handle, info.descriptor);
        // JIT round 16 wave 16 (lane ffmlean): the bound handle's direct
        // call, read now from the handle the bind then validates per call.
        let direct = match answer {
            JitDowncallBind::Bound(_) => jit_downcall_direct_stub(&mut ctx, handle),
            _ => None,
        };
        (answer, direct)
    };
    let bound_type = match answer {
        // Nothing recorded: the door's call classifies the handle.
        JitDowncallBind::NotYet => return FfmBindAnswer::Door,
        JitDowncallBind::Bound(mt) => Some(mh_bind_slot_key(Value::Object(Some(mt)))),
        JitDowncallBind::Refused => None,
    };
    if cratonvm_native_builtins::panama::ffm_dbg_enabled() {
        eprintln!(
            "[ffm-bind] site {}.{}{} bound={} direct={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            bound_type.is_some(),
            direct.is_some()
        );
    }
    FFM_BIND_SITES.with(|sites| {
        let mut sites = sites.borrow_mut();
        // Another handle within the epoch of the last fill is churn; a fill
        // after a collection is the same handle re-validated.
        let churn = sites.get(&info_key).map_or(0, |prev| {
            if prev.epoch == epoch && prev.handle != raw {
                prev.churn + 1
            } else {
                prev.churn
            }
        });
        if !sites.contains_key(&info_key) && sites.len() >= FFM_BIND_MAX_SITES {
            return;
        }
        sites.insert(
            info_key,
            FfmBindSite {
                epoch,
                handle: raw,
                site: Box::from(info.descriptor),
                bound_type,
                churn,
                direct,
                direct_reported: std::cell::Cell::new(false),
                bind_reported: std::cell::Cell::new(false),
                decline_reported: std::cell::Cell::new(false),
            },
        );
    });
    match bound_type {
        Some(_) => FfmBindAnswer::Bound(direct),
        None => FfmBindAnswer::Door,
    }
}

/// The bound downcall row of the native site cache's `MethodHandle` carrier
/// entry (FFC16-1, `CRATONVM_JIT_FFM_DOWNCALL_BIND`): `Some(bits)` when
/// `handle` is bound at this site and the call was made; `None` hands the call
/// to the carrier entry's door, unchanged.
///
/// The call is the carrier entry's tail with another callback: the site's own
/// argument decode, the same funnel (`safe_native_call_prevalidated_objects`:
/// pins, the native state, a downcall's upcalls and blocking), the erased
/// result through `unbox_poly_return_checked_vh` against the site (a raw
/// primitive stays as is) and `coerce_native_return_typed`, and an error
/// through `handle_jit_dispatch_error`. Not armed: the call-site channel,
/// which only the door reads.
///
/// SAFETY: as `try_jit_site_cached_native_dispatch`: `args_slice` is the
/// compiled caller's forwarded argument buffer, `handle` its validated
/// receiver.
pub unsafe fn try_jit_ffm_downcall_bound(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    handle: ObjectRef,
    args_slice: &[i64],
    entry: &NativeSiteCache,
) -> Option<i64> {
    // A JVMTI agent that needs the interpreter sees the door's native, as it
    // does for the constant bind's direct call.
    if !cratonvm_native_builtins::panama::jit_downcall_bind_enabled()
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return None;
    }
    let direct = match ffm_downcall_site_bound(vm, thread, info, info_key, handle) {
        FfmBindAnswer::Door => return None,
        FfmBindAnswer::Bound(direct) => direct,
    };
    // JIT round 16 wave 16 (lane ffmlean; FFD16-1): the precompiled call,
    // when the handle has one (`CRATONVM_JIT_FFM_DOWNCALL_DIRECT`, default
    // off) and this call admits it.
    if let Some(stub) = direct {
        // `r16-ffm-plain-downcall-costs-microseconds` (FFM217-2): the epoch
        // and the handle as the bind just validated them, read before the C
        // call (an upcall may collect: the door then misses and re-binds).
        let epoch = vm.mem.address_epoch();
        if let Some(bits) =
            ffm_downcall_direct_call(vm, thread, info, info_key, args_slice, entry, &stub)
        {
            ffm_direct_door_fill(vm, info_key, handle, epoch, entry, &stub);
            // JIT round 22 (lane memory, FFM22-1): the stub as this handle's
            // compiled call, for the optimizing tier's constant sites.
            crate::runtime::interpreter::jit_bridge::ffm_const_note_fill(
                vm,
                handle,
                epoch,
                &stub,
                info.descriptor,
                entry.native_id,
                entry.desc.ret,
            );
            return Some(bits);
        }
    }
    let mut values = JitDecodedArgs::new();
    decode_dispatch_values_into_shaped(
        vm,
        info,
        args_slice,
        Some(Some(handle)),
        entry.desc.params(),
        0,
        &mut values,
    );
    note_site_cached_native_hit();
    count_jit_native_dispatch(vm, entry.native_id);
    // JIT round 16 wave 13 (lane ffmseg; FFD16-1): a plain call without the
    // funnel, when switched on (`CRATONVM_JIT_FFM_DOWNCALL_LEAN`, default off).
    if jit_ffm_downcall_lean_enabled() {
        if let Some(bits) = ffm_downcall_lean_call(vm, thread, info, &values, entry) {
            return Some(bits);
        }
    }
    let called = crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        cratonvm_native_builtins::panama::jit_downcall_bound_native,
        &values,
    );
    let result = match called {
        Ok(value) => match crate::vm::vm_exec::unbox_poly_return_checked_vh(
            vm,
            thread,
            value,
            info.descriptor,
            info.method_name,
            None,
        ) {
            Ok(unboxed) => crate::vm::coerce_native_return_typed(unboxed, entry.desc.ret),
            Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
        },
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    })
}

/// `CRATONVM_JIT_FFM_DOWNCALL_LEAN` (default OFF, opt-in; JIT round 16 wave
/// 13, lane ffmseg, proposal FFD16-1 of `jit-r16-ffmdoor-proposals-RETIRED-20261006.md`): a
/// bound downcall site whose handle is PLAIN makes its call through
/// [`ffm_downcall_lean_call`] instead of the native funnel. Read once.
pub fn jit_ffm_downcall_lean_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)
}

/// The lean bound downcall (FFD16-1, [`jit_ffm_downcall_lean_enabled`]):
/// `Some(bits)` when the plain path served the call
/// (`panama::jit_downcall_plain_only`), `None` -- nothing done -- when it
/// declined, and the caller makes the ordinary bound call through the funnel.
///
/// What the funnel (`safe_native_call_prevalidated_objects`) does that a plain
/// downcall does not need: argument forwarding and pins (the only reference is
/// the handle, which the site validated this call and which the plain path
/// does not touch after `ffi_call`), the stop-the-world probe (the compiled
/// caller polls), the collection hooks for natives that allocate (a plain call
/// allocates nothing), the diagnostics mask, the JNI pending-exception drain
/// (a plain FFM callee has no `JNIEnv`) and the debugger's running-native
/// slot. What it keeps: the `NativeRunning` span (the state the
/// stop-the-world census waits for, and the state an upcall from C leaves),
/// `catch_unwind`, the pending-return slot cleared, and the bound row's own
/// return handling. The GC-safe region, when on, is the plain path's.
/// Run an allocating direct native body once. The C call may have side
/// effects, so an allocation refusal is caught locally and never replayed.
pub fn jit_native_alloc_once(
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    body: impl FnOnce(&mut crate::vm::NativeContextImpl<'_>) -> Option<crate::error::MethodCallResult>,
) -> Option<crate::error::MethodCallResult> {
    let pin_base = ctx.thread.native_pin_roots.len();
    match crate::runtime::native_oom::catch_alloc_oom(|| body(ctx)) {
        Ok(answer) => answer,
        Err(oom) => {
            ctx.thread.native_pin_roots.truncate(pin_base);
            Some(Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(oom.into_runtime_error()),
            )))
        }
    }
}

pub fn ffm_downcall_lean_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    values: &[Value],
    entry: &NativeSiteCache,
) -> Option<i64> {
    // A debugger that reports native entries sees the funnel's.
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return None;
    }
    let span = thread_state::enter_native_state("jit::ffm_downcall_lean");
    let served = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            jit_native_alloc_once(&mut ctx, |ctx| {
                cratonvm_native_builtins::panama::jit_downcall_plain_only(ctx, values)
            })
        }))
    };
    span.restore("jit::ffm_downcall_lean:return");
    thread.native_pending_return = None;
    let called = match served {
        Ok(Some(called)) => called,
        Ok(None) => return None,
        // The C call may have run: report, never repeat it through the funnel.
        Err(_) => Err(crate::error::MethodCallFailed::InternalError(
            crate::error::VmError::Internal {
                message: "lean FFM downcall panicked".into(),
            },
        )),
    };
    let result = match called {
        Ok(value) => match crate::vm::vm_exec::unbox_poly_return_checked_vh(
            vm,
            thread,
            value,
            info.descriptor,
            info.method_name,
            None,
        ) {
            Ok(unboxed) => crate::vm::coerce_native_return_typed(unboxed, entry.desc.ret),
            Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
        },
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    })
}

/// The direct bound downcall (JIT round 16 wave 16, lane ffmlean; FFD16-1 /
/// FFS16-4, `CRATONVM_JIT_FFM_DOWNCALL_DIRECT`): `Some(bits)` when the call
/// was made through the handle's direct stub
/// (`panama::jit_downcall_direct_invoke`), `None` -- nothing done -- when
/// `panama::jit_downcall_direct_admits` declines it, and the bound call
/// proceeds as before.
///
/// Against the bound call through the funnel it drops: the `Value` decode
/// (the compiled caller's raw slots go straight into the registers; the bind
/// matched the site's descriptor to the memo's kinds), the funnel's argument
/// forwarding and pins (the handle is the only reference and is not touched
/// after the bind's check), its stop-the-world probe (the compiled caller
/// polls), ordinary funnel allocation dispatch, its JNI pending-exception
/// drain (no `JNIEnv`), five handle field reads, the security-state mutex
/// (read through its exact mirror), the plain path's checks that the fill
/// already made, libffi's `ffi_call`, and the poly-return's strict box checks
/// (primitive results retain checked unboxing/coercion; object returns are
/// materialized after session release with allocation-only reclaim).
///
/// Thread state: a `critical` handle makes no transition at all (HotSpot's
/// critical contract: no upcall, no blocking, no safepoint; the call is a leaf
/// and the thread stays exactly what it was). A non-critical one keeps what
/// the default bound call gives it: the `NativeRunning` span the census waits
/// for and an upcall leaves, the context guard an upcall reads, and -- under
/// `CRATONVM_FFM_DOWNCALL_GC_SAFE` / `_IN_NATIVE_LAZY` -- the memo path's
/// GC-safe region (such a call declines here). `catch_unwind` stays; a panic
/// after the C call is reported, never repeated.
pub fn ffm_downcall_direct_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    entry: &NativeSiteCache,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
) -> Option<i64> {
    ffm_downcall_direct_call_with(
        vm,
        thread,
        info,
        info_key,
        args_slice,
        entry.native_id,
        entry.desc.ret,
        stub,
        false,
    )
}

/// [`ffm_downcall_direct_call`], with `trim` (JIT round 22, lane memory,
/// FFM22-2, `CRATONVM_JIT_FFM_DOOR_TRIM`; only the FFM direct door passes
/// `true`, after its fill declared the slot's tally a floor): no per-call
/// native invocation tally or site-cache hit counter (a shared `lock xadd`
/// and a thread-local bump per call, read only by the census, which
/// `mark_invocations_incomplete` now labels), and no allocation-refusal frame
/// for a plain stub that answers a primitive (`jit_downcall_direct_invoke`
/// allocates nothing; `catch_unwind` still stands around it).
#[allow(clippy::too_many_arguments)]
pub fn ffm_downcall_direct_call_with(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    native_id: Option<cratonvm_native_api::NativeMethodId>,
    site_ret: u8,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
    trim: bool,
) -> Option<i64> {
    use cratonvm_native_builtins::panama::{
        jit_downcall_direct_admits, jit_downcall_direct_invoke,
    };
    // A debugger that reports native entries sees the funnel's.
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return None;
    }
    // JIT round 18 wave 11 (lane ffmret): the engagement lines report the
    // row's outcome, not the bind's.
    let dbg = cratonvm_native_builtins::panama::ffm_dbg_enabled();
    // Slot 0 is the handle (the receiver); one slot per C parameter follows.
    let raw = args_slice.get(1..)?;
    let served = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        if !jit_downcall_direct_admits(&ctx, stub, raw.len()) {
            if dbg {
                ffm_direct_outcome_report(info, info_key, stub, Some("admits"));
            }
            return None;
        }
        // A plain stub always serves; a segment stub may still decline, and
        // the bound call then counts the hit itself.
        if !stub.segments() && !trim {
            note_site_cached_native_hit();
            count_jit_native_dispatch(vm, native_id);
        }
        // JIT round 18 wave 11 (lane ffmret; FFMCOST18-2/-3): a stub that
        // answers an object allocates (or calls the allocator, Java) after the
        // C call, so even a critical one runs in the native span the funnel
        // gives every allocating native (the census waits for it).
        let span = (!stub.critical() || stub.returns_object())
            .then(|| thread_state::enter_native_state("jit::ffm_downcall_direct"));
        let allocates = !trim || stub.segments() || stub.returns_object();
        let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if !allocates {
                // SAFETY: as the plain arm below.
                return Some(Ok(Some(unsafe {
                    jit_downcall_direct_invoke(&mut ctx, stub, raw)
                })));
            }
            jit_native_alloc_once(&mut ctx, |ctx| {
            if stub.segments() {
                // JIT round 17 wave 2 (lane ffm2; FFMSEG17-1,
                // `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_SEGMENTS`): a segment
                // stub. Each reference slot is validated as the funnel's
                // decode validates it (`decode_dispatch_values_into_shaped`).
                let mut resolve = |bits: i64| -> Option<ObjectRef> {
                    let bits = bits as u64;
                    if bits == 0 || bits & 0x7 != 0 || bits >= (1u64 << 48) {
                        return None;
                    }
                    crate::jit::helpers::note_membership_walk(2);
                    vm.mem.heap.is_object_address(bits as usize)
                };
                // SAFETY: as below; the segment slots are references the
                // compiled caller holds for this call.
                unsafe {
                    cratonvm_native_builtins::panama::jit_downcall_direct_segment_invoke(
                            ctx,
                        stub,
                        raw,
                        &mut resolve,
                    )
                }
            } else {
                // SAFETY: `raw` is the compiled caller's slots for this site, one
                // per parameter of the descriptor the bind matched against the
                // memo (`admits` checked the count); `stub` was filled from the
                // handle the bind validated this call.
                Some(Ok(Some(unsafe {
                        jit_downcall_direct_invoke(ctx, stub, raw)
                })))
            }
            })
        }));
        if let Some(span) = span {
            span.restore("jit::ffm_downcall_direct:return");
        }
        served
    };
    thread.native_pending_return = None;
    if stub.segments() && !trim && !matches!(served, Ok(None)) {
        note_site_cached_native_hit();
        count_jit_native_dispatch(vm, native_id);
    }
    if dbg {
        match &served {
            Ok(None) => ffm_direct_outcome_report(info, info_key, stub, Some("segment")),
            Ok(Some(Ok(_))) => ffm_direct_outcome_report(info, info_key, stub, None),
            _ => {}
        }
    }
    let value = match served {
        // A segment stub declined before any effect: the bound call serves it.
        Ok(None) => return None,
        Ok(Some(Ok(value))) => value.unwrap_or(Value::Object(None)),
        // An acquire a racing close refused (`Already closed`), raised as the
        // memo path raises it.
        Ok(Some(Err(error))) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
        // The C call may have run: report, never repeat it through the funnel.
        Err(_) => {
            let error =
                crate::error::MethodCallFailed::InternalError(crate::error::VmError::Internal {
                    message: "direct FFM downcall panicked".into(),
                });
            return Some(handle_jit_dispatch_error(vm, thread, error, info));
        }
    };
    // `r16-ffm-plain-downcall-costs-microseconds` (round 19): a primitive the
    // row already produced in the site's own return kind (the bind matched
    // the site's descriptor to the memo) is the answer as it stands; the
    // descriptor parse and the coercion below were ~4 % of a call.
    match (site_ret, value) {
        (b'J', Value::Long(v)) => return Some(v),
        (b'I', Value::Int(v)) => return Some(v as i64),
        (b'D', Value::Double(d)) => return Some(d.to_bits() as i64),
        (b'F', Value::Float(f)) => return Some(f.to_bits() as i64),
        _ => {}
    }
    let unboxed = crate::vm::vm_exec::unbox_poly_return(vm, Some(value), info.descriptor);
    Some(
        match crate::vm::coerce_native_return_typed(unboxed, site_ret) {
            Some(Value::Int(v)) => v as i64,
            Some(Value::Long(v)) => v,
            Some(Value::Float(f)) => f.to_bits() as i64,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Object(Some(obj))) => {
                // JIT round 18 wave 11 (lane ffmret): an object a direct row
                // minted (an `ADDRESS` return's segment, a group return's
                // allocator segment) is handed over with the object-return
                // handoff root, as every other JIT native fast path does.
                thread.native_pending_return = Some(obj);
                obj.as_ptr() as i64
            }
            Some(Value::Object(None)) | None => 0,
            Some(_) => 0,
        },
    )
}

/// JIT round 22 (lane memory, FFM22-1 step (b), `CRATONVM_JIT_IR_FFM_LAZY_CALL`):
/// the thin helper an optimizing-tier `invokeExact` of a constant
/// NON-critical bound downcall calls (`ir_lower::Lowerer::emit_ffm_lazy_call`),
/// with the jit-dispatch ABI: `entry_ptr` is the VM's
/// `jit_bridge::FfmConstEntry` for the field (whose row the compile read),
/// `args_ptr` the staged arguments (the handle, then one slot per C
/// parameter), `info_ptr` the site's `JitInvokeInfo`.
///
/// What it keeps of the door and the bound row: the entry point's quiescent
/// point and pending-return clear, the SATB flush and the dispatch depth
/// guard (an up-call runs Java), the `NativeRunning` span the census waits for
/// and an up-call leaves, and the row's own call
/// (`panama::jit_downcall_direct_invoke`: the context guard, the callee-saved
/// spill and band captured in that frame, the lazy in-native transition, the
/// C call). What it drops, each proven at the row's filing instead: the door's
/// table probe and handle identity test (the field is `static final`; the row
/// is the handle's), its generations (a recompile or a redefinition replaces
/// this compiled body, not the handle), the bind and its per-call admission
/// (the arity and descriptor were matched at the compile, the switches are
/// process latches, the access verdict's two changeable words were tested by
/// the compiled caller), the invocation tally (the slot was declared a floor)
/// and the alloc-refusal frame (a plain primitive stub allocates nothing).
/// A debugger that reports native entries, or no installed JIT thread, takes
/// `jit_invoke_dispatch` with the same words.
///
/// # Safety
///
/// The compiled caller's: a live `SharedVm`, a live entry, `1 + arity` staged
/// words, a live `JitInvokeInfo`.
/// JIT round 22 (lane memory, FFM22-1 step (c),
/// `CRATONVM_JIT_IR_FFM_SEGMENT_CALL`): the thin helper an optimizing-tier
/// `invokeExact` of a constant bound downcall calls when the compiled caller
/// cannot make the call itself -- a SEGMENT stub (`MemorySegment` arguments, a
/// capture-state segment), an `ADDRESS` or by-value group return (a minted
/// segment, a `SegmentAllocator` lead slot) -- with
/// [`jit_ffm_const_lazy_call`]'s ABI. It is the FFM direct door after its
/// lookup: the reference slots forwarded (a relocation in progress moves them
/// without moving the epoch), the capability gate's twin (the row's guard
/// words, tested by the caller), the quiescent point, the SATB flush and
/// depth guard for a call that can reach Java, and the bound row's own call
/// (`ffm_downcall_direct_call_with`, trimmed), which validates every segment,
/// acquires and releases the sessions, mints the result and raises exactly
/// as the door's call does. A segment stub that declines (an unresolved
/// scope, a heap segment, `null`) has done nothing: the site's dispatch
/// helper serves it, as the door's caller would.
///
/// # Safety
///
/// As [`jit_ffm_const_lazy_call`]'s, with `1 + slot_count` staged words.
pub unsafe extern "C" fn jit_ffm_const_direct_call(
    vm_ptr: i64,
    entry_ptr: i64,
    args_ptr: i64,
    info_ptr: i64,
) -> i64 {
    let vm = &*(vm_ptr as *const SharedVm);
    let entry = &*(entry_ptr as *const crate::runtime::interpreter::jit_bridge::FfmConstEntry);
    // Cast: the staged word count, as the dispatch helper takes it.
    let num_args = (usize::from(entry.row.arity) + 1) as i64;
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args);
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let forwarded = forward_jit_reference_args(vm, info, args);
    let args = forwarded.as_deref().unwrap_or(args);
    // The thread borrow and the depth guard end with this block: a declined
    // call is handed to the dispatch helper, which takes its own.
    let served = 'served: {
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'served None;
        };
        cratonvm_jit::jit_ic_quiescent_point();
        thread.native_pending_return = None;
        let stub = &entry.stub;
        let _depth_guard = if stub.critical() && !stub.returns_object() {
            None
        } else {
            vm.mem.heap.flush_thread_satb();
            match enter_jit_dispatch(vm, constructor_receiver(info, args)) {
                Ok(g) => Some(g),
                Err(sentinel) => break 'served Some(sentinel),
            }
        };
        let info_key = jit_site_key(vm.vm_identity, info_ptr as usize);
        let bits = ffm_downcall_direct_call_with(
            vm,
            thread,
            info,
            info_key,
            args,
            entry.native_id,
            entry.site_ret,
            stub,
            true,
        );
        // Compiled code holds the result from here
        // (`native_return_handed_to_compiled_code`).
        thread.native_pending_return = None;
        bits
    };
    match served {
        Some(bits) => bits,
        None => jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args),
    }
}

/// JIT round 22 (lane memory, FFM22-3, `CRATONVM_JIT_IR_FFM_SEGMENT_INLINE`):
/// the helper an optimizing-tier `invokeExact` of a constant bound downcall
/// with ONE native segment parameter calls once it has checked that segment
/// itself (`ir_lower::Lowerer::emit_ffm_seg_call`), with step (b)'s ABI. After
/// the `1 + arity` staged arguments the caller staged the
/// `ir_lower::FFM_SEG_STAGE_EXTRA` words it read: the segment's `min` and
/// `length`, and its session -- a `ConfinedSession` it found open and owned by
/// this thread, with the compact offset of its `acquireCount` -- or `0` for the
/// global session (whose `acquire0` / `release0` do nothing).
///
/// What it keeps of step (c)'s call (`jit_downcall_direct_segment_invoke`):
/// the reference forwarding test, the quiescent point, the SATB flush and the
/// depth guard for a call that can reach Java, the arena-call record and the
/// row's address screens (`panama::jit_segment_call_address`), the pin of the
/// session across the call and the acquire before it and release after it,
/// the `NativeRunning` span, the row's call (`jit_downcall_direct_invoke_regs`:
/// the context guard, the spill, the lazy transition), and the result as the
/// row finishes it (`panama::jit_downcall_direct_finish`, after the release).
/// What it drops: the segment and session field reads through the native
/// context (`segment_row_read`, `p67_real_session_observe`: the row's `front`)
/// and the session's slot map. A critical call neither pins nor acquires a
/// confined session, as the row's lean handling (only this thread could close
/// it, and a critical call runs no Java). The count moves by compare-and-swap
/// as the row's does.
///
/// Anything it cannot make -- a forwarded argument, an address a screen
/// refuses, a count at its limit or moved under it, no JIT thread, a debugger
/// that reports native entries -- is handed, before any effect, to step (c)'s
/// helper (`jit_ffm_const_direct_call`) on the same staged arguments, which
/// reads everything again and raises what applies.
///
/// # Safety
///
/// As [`jit_ffm_const_lazy_call`]'s, with `1 + arity + FFM_SEG_STAGE_EXTRA`
/// staged words, the extra ones as above.
pub unsafe extern "C" fn jit_ffm_const_seg_call(
    vm_ptr: i64,
    entry_ptr: i64,
    args_ptr: i64,
    info_ptr: i64,
) -> i64 {
    use std::sync::atomic::{AtomicI32, Ordering};
    let vm = &*(vm_ptr as *const SharedVm);
    let entry = &*(entry_ptr as *const crate::runtime::interpreter::jit_bridge::FfmConstEntry);
    let arity = usize::from(entry.row.arity);
    let num_args = arity + 1;
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return jit_ffm_const_direct_call(vm_ptr, entry_ptr, args_ptr, info_ptr);
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args);
    let extra = std::slice::from_raw_parts(
        (args_ptr as *const i64).add(num_args),
        cratonvm_jit::ir_lower::FFM_SEG_STAGE_EXTRA,
    );
    let row = &entry.row;
    let stub = &entry.stub;
    let seg = usize::from(row.seg_param);
    // The thread borrow, the depth guard and the arena record end with this
    // block: a declined call is handed to step (c)'s helper, which takes its
    // own.
    let served = 'served: {
        // A reference the caller read through moved under it: the row reads
        // everything again from the forwarded words.
        if seg >= arity || forward_jit_reference_args(vm, info, args).is_some() {
            break 'served None;
        }
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'served None;
        };
        let _arena = cratonvm_native_builtins::panama::JitArenaCallScope::open();
        let Some(addr) =
            cratonvm_native_builtins::panama::jit_segment_call_address(extra[0], extra[1])
        else {
            break 'served None;
        };
        // A confined session of a non-critical call: its count, at the offset
        // the caller baked under its layout epoch.
        let count = (extra[2] != 0 && !stub.critical()).then(|| {
            // SAFETY: the caller found this word a compact `ConfinedSession`
            // and the offset its `acquireCount`'s, with no GC point since.
            unsafe { &*((extra[2] + extra[3]) as usize as *const AtomicI32) }
        });
        cratonvm_jit::jit_ic_quiescent_point();
        thread.native_pending_return = None;
        let calls_java = !stub.critical() || stub.returns_object();
        let _depth_guard = if calls_java {
            vm.mem.heap.flush_thread_satb();
            match enter_jit_dispatch(vm, constructor_receiver(info, args)) {
                Ok(g) => Some(g),
                Err(sentinel) => break 'served Some(sentinel),
            }
        } else {
            None
        };
        // The acquire, before any effect: only this thread moves a confined
        // session's count (`ConfinedSession.acquire0`; `release0` from another
        // thread counts `asyncReleaseCount`), so a compare-and-swap that fails
        // or a count at `MAX_FORKS` is handed to the row, which raises.
        if let Some(count) = count {
            let seen = count.load(Ordering::Acquire);
            if !(0..i32::MAX).contains(&seen)
                || count
                    .compare_exchange(seen, seen + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                break 'served None;
            }
        }
        let span = calls_java.then(|| thread_state::enter_native_state("jit::ffm_const_seg"));
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        // SAFETY: as `count` above; pinned for the call, which may collect.
        let session = count.map(|_| unsafe { ObjectRef::from_raw(extra[2] as usize as *mut u8) });
        let pin = session.map(|s| ctx.pin_native_root(s));
        let mut regs = [0u64; cratonvm_native_builtins::panama_libffi::DIRECT_STUB_MAX_ARGS];
        for (i, ((reg, &slot), &code)) in
            regs.iter_mut().zip(&args[1..]).zip(&row.params).enumerate()
        {
            *reg = if i == seg {
                addr
            } else {
                ffm_const_widen(slot as u64, code)
            };
        }
        let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `regs` are the site's `arity` C parameter slots matched
            // to the stub's kinds at the compile, the segment's the screened
            // address of a segment whose session is held for the call.
            cratonvm_native_builtins::panama::jit_downcall_direct_invoke_regs(&mut ctx, stub, &regs)
        }));
        if let (Some(s), Some(pin)) = (session, pin) {
            let now = ctx.read_native_pin(pin, s);
            ctx.unpin_native_roots(pin);
            let offset = extra[3] as usize;
            // SAFETY: the pinned session's current address; the same field.
            let count = unsafe { &*((now.as_ptr() as usize + offset) as *const AtomicI32) };
            let mut seen = count.load(Ordering::Acquire);
            while seen > 0 {
                match count.compare_exchange(seen, seen - 1, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(moved) => seen = moved,
                }
            }
        }
        let answer = match called {
            Ok(rax) if row.ret != cratonvm_jit::ir_lower::FFM_CONST_REF => {
                // Cast: the register's bits, as the compiled caller's slot
                // holds them.
                Ok(ffm_const_widen(rax, row.ret) as i64)
            }
            // An `ADDRESS` return: the minted segment, after the release.
            Ok(rax) => match jit_native_alloc_once(&mut ctx, |ctx| {
                Some(cratonvm_native_builtins::panama::jit_downcall_direct_finish(ctx, stub, rax))
            }) {
                // Held through the object-return handoff root across the
                // span's end, and read back from it.
                Some(Ok(Some(Value::Object(Some(obj))))) => {
                    ctx.thread.native_pending_return = Some(obj);
                    Ok(-1)
                }
                Some(Ok(_)) | None => Ok(0),
                Some(Err(error)) => Err(error),
            },
            // The C call may have run: report, never repeat it.
            Err(_) => Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Internal {
                    message: "compiled FFM segment downcall panicked".into(),
                },
            )),
        };
        if let Some(span) = span {
            span.restore("jit::ffm_const_seg:return");
        }
        let bits = match answer {
            Ok(-1) if row.ret == cratonvm_jit::ir_lower::FFM_CONST_REF => thread
                .native_pending_return
                .map_or(0, |obj| obj.as_ptr() as i64),
            Ok(bits) => bits,
            Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
        };
        // Compiled code holds the result from here
        // (`native_return_handed_to_compiled_code`).
        thread.native_pending_return = None;
        Some(bits)
    };
    match served {
        Some(bits) => bits,
        None => jit_ffm_const_direct_call(vm_ptr, entry_ptr, args_ptr, info_ptr),
    }
}

/// One register image widened by an `ir_lower::FfmConstRow` code, as the
/// compiled call's `ffm_const_widen_bytes` widens it (`0` for no value).
#[inline(always)]
pub fn ffm_const_widen(bits: u64, code: u8) -> u64 {
    use cratonvm_jit::ir_lower as jl;
    match code {
        jl::FFM_CONST_S8 => bits as u8 as i8 as i64 as u64,
        jl::FFM_CONST_S16 => bits as u16 as i16 as i64 as u64,
        jl::FFM_CONST_S32 => bits as u32 as i32 as i64 as u64,
        jl::FFM_CONST_U16 => u64::from(bits as u16),
        jl::FFM_CONST_W64 => bits,
        _ => 0,
    }
}

pub unsafe extern "C" fn jit_ffm_const_lazy_call(
    vm_ptr: i64,
    entry_ptr: i64,
    args_ptr: i64,
    info_ptr: i64,
) -> i64 {
    let vm = &*(vm_ptr as *const SharedVm);
    let entry = &*(entry_ptr as *const crate::runtime::interpreter::jit_bridge::FfmConstEntry);
    let arity = usize::from(entry.row.arity);
    // Cast: the staged word count, as the dispatch helper takes it.
    let num_args = (arity + 1) as i64;
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args);
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args);
    };
    let info = &*(info_ptr as *const JitInvokeInfo);
    let raw = std::slice::from_raw_parts((args_ptr as *const i64).add(1), arity);
    cratonvm_jit::jit_ic_quiescent_point();
    thread.native_pending_return = None;
    vm.mem.heap.flush_thread_satb();
    let _depth_guard = match enter_jit_dispatch(vm, 0) {
        Ok(g) => g,
        Err(sentinel) => return sentinel,
    };
    let span = thread_state::enter_native_state("jit::ffm_const_lazy");
    let served = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let mut regs = [0u64; cratonvm_native_builtins::panama_libffi::DIRECT_STUB_MAX_ARGS];
        for ((reg, &slot), &code) in regs.iter_mut().zip(raw).zip(&entry.row.params) {
            *reg = ffm_const_widen(slot as u64, code);
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `regs` are the site's `arity` C parameter slots, matched
            // to the row's kinds at the compile and widened by its codes; the
            // stub is the row's.
            cratonvm_native_builtins::panama::jit_downcall_direct_invoke_regs(
                &mut ctx,
                &entry.stub,
                &regs,
            )
        }))
    };
    span.restore("jit::ffm_const_lazy:return");
    thread.native_pending_return = None;
    match served {
        // Cast: the register's bits, as the compiled caller's slot holds them.
        Ok(rax) => ffm_const_widen(rax, entry.row.ret) as i64,
        // The C call may have run: report, never repeat it.
        Err(_) => {
            let error =
                crate::error::MethodCallFailed::InternalError(crate::error::VmError::Internal {
                    message: "compiled FFM downcall panicked".into(),
                });
            handle_jit_dispatch_error(vm, thread, error, info)
        }
    }
}

// ---------------------------------------------------------------------------
// The FFM direct door (`r16-ffm-plain-downcall-costs-microseconds`, FFM217-2;
// `CRATONVM_JIT_FFM_DIRECT_DOOR`, default ON, `=0` off)
// ---------------------------------------------------------------------------
//
// A bound downcall site with a direct stub (`ffm_downcall_direct_call`) still
// paid, per call, everything in front of the stub: `jit_invoke_virtual_mic`'s
// prologue (the panic scope, the SATB flush, `forward_jit_reference_args`, the
// raw-entry cache flush), the native site cache's probe (a `RefCell` map copy
// of a ~120-byte entry, the receiver's membership walk and class lookup, the
// capability and redefinition tests) and the bind's warm check (a second
// `RefCell` map, a descriptor string compare, the `type` slot read). The SBR5
// lean native door removed the same chain for plain callback natives; this is
// its twin for the direct downcall row.
//
// When a bound site's direct call returns, `ffm_direct_door_fill` files the
// site in a direct-mapped thread-local table keyed by the `JitInvokeInfo`
// address: the handle's address and the collection epoch the bind validated,
// the site cache entry, and the stub. `try_ffm_direct_door` (the first thing
// `jit_invoke_virtual_mic` asks) serves a call only when the receiver is that
// same handle in that same epoch (no collection since the fill, so nothing
// moved and no address was re-used: the bind's own identity test), the JIT
// cache and native registry generations are the fill's, no class was
// redefined and no agent needs the interpreter. It then runs the capability
// gate, the depth guard and SATB flush for a call that can reach Java, and
// the bound row's own `ffm_downcall_direct_call`, which still asks
// `jit_downcall_direct_admits` per call (access verdict, entry call site,
// GC-safe switches) and may decline (a segment stub), in which case nothing
// was done and the ordinary route serves the call. A compiled caller polls
// only at its own poll points, so nothing moves between its argument staging
// and this door (the lean door's argument): the raw slots need no forwarding.

/// Slots of the thread-local FFM door table. A power of two; collisions evict.
pub const FFM_DIRECT_DOOR_SLOTS: usize = 64;

#[derive(Clone, Copy)]
pub struct FfmDirectDoorSite {
    /// The `JitInvokeInfo` address.
    pub key: usize,
    pub vm_identity: usize,
    pub jit_gen: u64,
    pub registry_gen: u32,
    /// `mem.address_epoch()` when the bind validated the handle (gci i2/wr).
    pub epoch: u64,
    /// The bound handle's address in that epoch.
    pub handle: u64,
    pub entry: NativeSiteCache,
    pub stub: cratonvm_native_builtins::panama::JitDowncallDirect,
}

thread_local! {
    pub static FFM_DIRECT_DOOR_SITES: std::cell::RefCell<[Option<FfmDirectDoorSite>; FFM_DIRECT_DOOR_SLOTS]> =
        const { std::cell::RefCell::new([None; FFM_DIRECT_DOOR_SLOTS]) };
}

#[inline]
pub fn ffm_direct_door_slot(info_addr: usize) -> usize {
    (info_addr >> 4) & (FFM_DIRECT_DOOR_SLOTS - 1)
}

/// `CRATONVM_JIT_FFM_DIRECT_DOOR` (default ON, `=0` off): file bound direct
/// downcall sites for [`try_ffm_direct_door`]. Read once per VM
/// (`SharedVm::jit_ffm_direct_door`, gci i14/sx; it was a process latch).
#[inline]
pub fn ffm_direct_door_enabled(vm: &SharedVm) -> bool {
    vm.jit_ffm_direct_door
}

/// File a bound site whose direct call just returned (the door's fill). The
/// diagnostic modes that print per-site lines or count the MIC keep the
/// ordinary route.
#[inline(never)]
pub fn ffm_direct_door_fill(
    vm: &SharedVm,
    info_key: JitSiteKey,
    handle: ObjectRef,
    epoch: u64,
    entry: &NativeSiteCache,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
) {
    if !ffm_direct_door_enabled(vm)
        || cratonvm_native_builtins::panama::ffm_dbg_enabled()
        || mic_prof::trace_enabled()
        || mic_prof::enabled()
        || cv_trace_enabled()
        || crate::runtime::env_cache::jit_dispatch_dbg()
        || crate::runtime::env_cache::jit_putfield_diag()
    {
        return;
    }
    // FFM22-2 (`CRATONVM_JIT_FFM_DOOR_TRIM`): the door's calls skip the
    // native invocation tally, so the slot's count is declared a floor, once
    // per fill (`NativeMethodRegistry::mark_invocations_incomplete`'s
    // contract for a bypassing path).
    if vm.jit_ffm_door_trim {
        if let Some(id) = entry.native_id {
            vm.natives.native_methods.mark_invocations_incomplete(id);
        }
    }
    let info_addr = info_key.1;
    let site = FfmDirectDoorSite {
        key: info_addr,
        vm_identity: vm.vm_identity,
        jit_gen: cratonvm_jit::jit_cache_generation(),
        registry_gen: vm.natives.native_methods.generation(),
        epoch,
        handle: handle.as_ptr() as u64,
        entry: *entry,
        stub: *stub,
    };
    FFM_DIRECT_DOOR_SITES.with(|t| {
        if let Ok(mut t) = t.try_borrow_mut() {
            t[ffm_direct_door_slot(info_addr)] = Some(site);
        }
    });
}

/// May [`try_ffm_direct_door`] serve the site at `info_ptr`? Under
/// `CRATONVM_JIT_FFM_DIRECT_DOOR_EXACT_ONLY` (default ON; JIT round 21, lane
/// native, FFL16-1 / FUNNEL2-19-4) only an `invokeExact` site: the door's
/// table is filled from `try_jit_ffm_downcall_bound`, which runs only for
/// `info.method_name == "invokeExact"`, so any other site would miss anyway.
/// `=0` (or a null info) asks the door for every call, as before.
///
/// SAFETY: `info_ptr` is `0` or the extern helper's own live `JitInvokeInfo`.
#[inline(always)]
pub unsafe fn ffm_direct_door_may_serve(info_ptr: i64) -> bool {
    if info_ptr == 0 || !cratonvm_types::flags().jit.ffm_direct_door_exact_only {
        return true;
    }
    (*(info_ptr as *const JitInvokeInfo)).method_name == "invokeExact"
}

/// The FFM direct door: serve a compiled `invokeExact` of a bound direct
/// downcall from the thread-local table, or answer `None` -- nothing done --
/// and let `jit_invoke_virtual_mic` take its ordinary route.
///
/// SAFETY: the extern helper's own arguments (a live `SharedVm`, a live
/// `JitInvokeInfo`, the caller's argument staging block).
// JIT round 23 wave 2f (lane L1): out of line, so the frame of the helper on every
// compiled-call level (which stays live for the depth of a recursion) does not reserve
// this door's locals; it returns before the call that recurses.
#[inline(never)]
pub unsafe fn try_ffm_direct_door(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    let info_addr = info_ptr as usize;
    // Copied out before anything runs: a nested call may refill the slot.
    let site = FFM_DIRECT_DOOR_SITES.with(|t| {
        t.try_borrow()
            .ok()
            .and_then(|t| t[ffm_direct_door_slot(info_addr)])
    })?;
    if site.key != info_addr || num_args < 1 || args_ptr == 0 {
        return None;
    }
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let vm = &*(vm_ptr as *const SharedVm);
    if args[0] as u64 != site.handle
        || site.vm_identity != vm.vm_identity
        || site.epoch != vm.mem.address_epoch()
        || site.jit_gen != cratonvm_jit::jit_cache_generation()
        || site.registry_gen != vm.natives.native_methods.generation()
        || crate::classloading::any_class_redefined()
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return None;
    }
    // Every reference argument forwarded first, as the MIC body forwards them
    // before it reads one (`forward_jit_reference_args`; the bound row's
    // contract is a forwarded block): the epoch above counts finished
    // collections, and a segment row handed a stale address would pin it. A
    // moved handle misses.
    //
    // JIT round 22 (lane memory, FFM22-2, `CRATONVM_JIT_FFM_DOOR_TRIM`): a
    // PLAIN stub's only reference is the handle, which the row never reads
    // (the stub is the fill's copy), and its compare below is the identity
    // test whatever copy of the address the caller holds in this epoch; so
    // only a segment stub, whose row reads its reference slots, walks them.
    let info = &*(info_ptr as *const JitInvokeInfo);
    let forwarded = if vm.jit_ffm_door_trim && !site.stub.segments() {
        None
    } else {
        forward_jit_reference_args(vm, info, args)
    };
    let args = forwarded.as_deref().unwrap_or(args);
    if args[0] as u64 != site.handle {
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    if let Some(kind) = site.entry.capability_kind {
        if crate::vm::check_native_dispatch_capability_for_slot(vm, kind, site.entry.native_id)
            .is_err()
        {
            return None;
        }
    }
    // The extern helper's own entry point: the previous native's returned
    // object stops being a root.
    cratonvm_jit::jit_ic_quiescent_point();
    thread.native_pending_return = None;
    // A callee that may reach Java (a non-critical upcall, an allocator or a
    // minted segment) gets what the MIC prologue gives every such call: the
    // SATB flush and the dispatch depth guard.
    let _depth_guard = if site.stub.critical() && !site.stub.returns_object() {
        None
    } else {
        vm.mem.heap.flush_thread_satb();
        match enter_jit_dispatch(vm, constructor_receiver(info, args)) {
            Ok(g) => Some(g),
            Err(sentinel) => return Some(sentinel),
        }
    };
    let info_key = jit_site_key(vm.vm_identity, info_addr);
    let bits = ffm_downcall_direct_call_with(
        vm,
        thread,
        info,
        info_key,
        args,
        site.entry.native_id,
        site.entry.desc.ret,
        &site.stub,
        vm.jit_ffm_door_trim,
    )?;
    // Compiled code holds the result from here
    // (`native_return_handed_to_compiled_code`).
    thread.native_pending_return = None;
    Some(bits)
}

/// JIT round 20 (lane fixLN): how [`try_jit_site_cached_native_dispatch_mode`]
/// treats a site-cache fill. A fill (`resolve_native_site`) can load and
/// initialise classes, which runs Java: a GC point. The ordinary callers hand
/// the door their own frame words, which the collector keeps current, so they
/// `Serve` across a fill. A caller whose words are a Rust local (the direct
/// lambda arm's native target, `lambda_site_native_call`) must never hold them
/// across one: it asks `WarmOnly` (decline instead of resolving, raising the
/// flag), then `FillOnly` with nothing but the receiver word (resolve, never
/// serve), re-materialises its words from the caller's current ones and asks
/// `WarmOnly` again.
pub enum NativeSiteDoorMode<'a> {
    Serve,
    WarmOnly(&'a mut bool),
    FillOnly,
}

impl NativeSiteDoorMode<'_> {
    /// `true` (and the `WarmOnly` flag raised) when this call must not run a
    /// resolution.
    pub fn refuses_resolution(&mut self) -> bool {
        match self {
            Self::WarmOnly(cold) => {
                **cold = true;
                true
            }
            Self::Serve | Self::FillOnly => false,
        }
    }
}

