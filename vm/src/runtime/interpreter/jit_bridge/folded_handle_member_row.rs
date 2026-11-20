#[allow(unused_imports)]
use super::*;

/// Shared core of [`resolve_inline_site`] and [`resolve_receiver_inline_site`].
///
/// `receiver_class_id` selects which of the two contracts applies: `None`
/// resolves from `callee_class` (constant-pool resolution), `Some` starts the
/// selection walk at that runtime class and applies the override-legality
/// checks documented on [`resolve_receiver_inline_site`].
///
/// `nest_depth` is how many splices already enclose this one: `0` for a site
/// spliced directly into a compiled method, `1` for a body spliced into that
/// body, and so on. It bounds the RECURSION this function performs on its own
/// callee's calls (`cratonvm_jit::MAX_INLINE_NEST_DEPTH`) — see the nested-site
/// resolution near the end.
/// The member body for a nested `MethodHandle.invokeExact` / `invoke` call
/// whose handle the IR builder folds from the constant-handle table
/// (`ConstFoldTables::const_mh_invoke_infos`): among the instance members of
/// the rows with the call's descriptor, the one body that resolves for the
/// exact class of the call's first argument (the member's receiver). The
/// plan is a candidate: the builder splices it only where its own fold at
/// this call names that member, the receiver is proven that class and the
/// body cannot raise (`IrBuilder::folded_member_splice_admitted`); anywhere
/// else the call stays. `(body, receiver class)`, or `None`.
fn folded_handle_member_row(
    shared: &SharedVm,
    declaring_id: ClassId,
    target: &cratonvm_jit::InlineInvokeTarget,
    operand_classes: &[Option<u32>],
    nest_depth: usize,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<(cratonvm_jit::InlineSite, u32)> {
    if target.class_name != "java/lang/invoke/MethodHandle"
        || !matches!(target.method_name.as_str(), "invokeExact" | "invoke")
    {
        return None;
    }
    let note = |why: &str| {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] nest-mh-member {}{} depth={} -> {why}",
                target.method_name,
                target.descriptor,
                nest_depth + 1
            );
        }
    };
    // Slot 0 is the handle.
    let Some(receiver) = operand_classes.get(1).copied().flatten().filter(|&c| c != 0) else {
        note("refused: receiver class unknown");
        return None;
    };
    let members: Vec<(&'static str, &'static str, &'static str)> = {
        let folds = shared.jit.bridge.const_folds.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<(&'static str, &'static str, &'static str)> = Vec::new();
        for ((_, descriptor), fold) in &folds.const_mh_invoke_infos {
            if *descriptor != target.descriptor || fold.info_ptr == 0 {
                continue;
            }
            // SAFETY: a fold row's `info_ptr` is an interned `JitInvokeInfo`
            // the VM keeps alive for the process (`ConstMhFold`).
            let info = unsafe { &*(fold.info_ptr as *const cratonvm_jit::JitInvokeInfo) };
            let row = (info.class_name, info.method_name, info.descriptor);
            if matches!(info.invoke_kind, 0 | 2) && !rows.contains(&row) {
                rows.push(row);
            }
        }
        rows
    };
    let mut found: Option<(cratonvm_jit::InlineSite, (&str, &str, &str))> = None;
    for member in members.iter().copied() {
        cratonvm_jit::ir::set_splice_arg_classes(vec![Some(receiver)]);
        let site = resolve_inline_site_from(
            shared,
            declaring_id,
            Some(ClassId::new(receiver)),
            member.0,
            member.1,
            member.2,
            nest_depth + 1,
            direct_bind,
            true,
            None,
            false,
        )
        .filter(|s| !s.callee_is_static && s.callee_code_len <= cratonvm_jit::MAX_INLINE_SIZE_COLD);
        if let Some(site) = site {
            if found.is_some() {
                note("refused: two members resolve for the receiver class");
                return None;
            }
            found = Some((site, member));
        }
    }
    let Some((site, member)) = found else {
        note(&format!("refused: none of {} fold rows resolves for class {receiver}", members.len()));
        return None;
    };
    note(&format!("{}.{}{} for class {receiver} (spliced through the fold)", member.0, member.1, member.2));
    Some((site, receiver))
}

pub(in crate::runtime::interpreter::jit_bridge) fn resolve_inline_site_from_body(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver_class_id: Option<ClassId>,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    nest_depth: usize,
    direct_bind: Option<InlineDirectBind<'_>>,
    // Resolve for the optimizing (IR) tier rather than the single-pass emitter.
    // See [`resolve_ir_inline_site`] for what the two admission sets differ on
    // and why. Threaded into the nested resolution below so a nested body is
    // held to the same contract as the one enclosing it.
    ir_mode: bool,
    // The owners the compile substituted for a constant-pool class (see
    // [`resolve_inline_site`]); consulted for a constant-pool resolution at
    // `requesting_class_id`'s own sites only. The nested resolutions pass
    // `None`: a splice's own sites are named by its constant pool.
    substituted: Option<&SubstitutedOwners>,
    // Resolve only body eligibility and metadata; never inspect artifacts or recurse.
    planning_only: bool,
) -> Option<cratonvm_jit::InlineSite> {
    let gates = inline_gates(shared);
    let direct_bind = if planning_only { None } else { direct_bind };
    use cratonvm_reader::constant_pool::ConstantPoolEntry;

    // Name every refusal. This function has two dozen `return None`s and a
    // caller that can only see "refused"; two build cycles were spent today
    // guessing which one fired.
    // JIT round 21 wave 3 (lane splice): the line ends with WHICH resolution
    // refused -- `[tier=ir|sp route=cp|receiver=<id>]`. A census of these
    // lines read single-pass refusals (`new/anewarray/multianewarray`,
    // `checkcast/instanceof`, `ldc-non-numeric-constant`: arms the optimizing
    // tier admits) and the constant-pool attempt the planner makes BEFORE its
    // exact / guarded routes (`ir-splice-target-not-provably-monomorphic`) as
    // optimizing-tier losses. The suffix comes after the reason, so
    // `jit-native-shadow-rank.py`'s pattern still matches.
    macro_rules! no {
        ($why:expr) => {{
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] inline-resolve REFUSED {}.{}{} depth={}: {} [tier={} route={}]",
                    callee_class,
                    callee_method,
                    callee_desc,
                    nest_depth,
                    $why,
                    if ir_mode { "ir" } else { "sp" },
                    match receiver_class_id {
                        Some(r) => format!("receiver={}", r.as_u32()),
                        None => "cp".to_string(),
                    },
                );
            }
            return None;
        }};
    }

    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the exact classes this
    // body's arguments carry, by local slot, as the caller left them for this
    // resolution (taken first, so no early return can leave them for another).
    // A body selected for an exact receiver has that class in slot 0.
    let mut splice_arg_classes = cratonvm_jit::ir::take_splice_arg_classes();
    // `CRATONVM_FROZEN_PAGES`: and the frozen objects they are expected to be.
    // A body of the compiling method's own call site reads that site's;
    // a body resolved for an exact receiver class nobody named an object for
    // takes the one that class's methods are nearly always called on.
    let mut splice_arg_objects = if nest_depth == 0 {
        cratonvm_jit::ir::splice_site_objects()
    } else {
        cratonvm_jit::ir::take_splice_arg_objects()
    };
    if let Some(receiver) = receiver_class_id {
        if splice_arg_objects.first().copied().flatten().is_none() {
            if let Some(addr) = crate::runtime::frozen_pages::dominant_receiver(shared, receiver.as_u32()) {
                if splice_arg_objects.is_empty() {
                    splice_arg_objects.push(None);
                }
                splice_arg_objects[0] = Some(addr);
            }
        }
    }
    // The r23 L8 heap-buffer close: this resolution is an `invokespecial` of
    // the requesting body's direct superclass (`IR_SUPER_SPECIAL_SITE`, taken
    // first for the same reason).
    let super_special_site = IR_SUPER_SPECIAL_SITE.with(|c| c.replace(false));
    // The caller's body-size cap (`set_exact_row_size_cap`), taken for the same
    // reason.
    let exact_size_cap = INLINE_RESOLVE_STATE.with(|s| s.borrow_mut().exact_size_cap.take());
    if let Some(receiver) = receiver_class_id {
        if splice_arg_classes.is_empty() {
            splice_arg_classes.push(None);
        }
        if splice_arg_classes[0].is_none() {
            splice_arg_classes[0] = Some(receiver.as_u32());
        }
    }

    // Interpreter round i7 wave 136, lane L2
    // (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, default OFF): a body
    // spliced at an interface sender's unproven `invokespecial` would skip the
    // dispatch helper's receiver check. One per-VM bool read with it off.
    if iface_special_guarded_target(
        shared,
        requesting_class_id,
        callee_class,
        callee_method,
        callee_desc,
    ) {
        no!("invokespecial-interface-receiver-guarded");
    }

    // A registered native shadows the classfile body. Inlining that bytecode
    // would bypass the native completely, just as compiling the method itself
    // would. This must precede even the "tiny constructor" path below:
    // ConcurrentHashMap.<init>()V is only five bytes, but CratonVM's native
    // constructor installs the segmented backing store. Background C1 used to
    // inline the empty-looking JDK body, so a JIT-created map silently dropped
    // every put after tier-up. The callee compiler already has this own-class
    // guard; keep the inline resolver aligned with it.
    //
    // CONSTANT-POOL RESOLUTION ONLY. `callee_class` is the DECLARED class, and
    // for a receiver-resolved site that is a supertype which may own no body at
    // all — `java/lang/Object` for an `equals` call site, say. Refusing there
    // refuses every override too, including the plain-bytecode one the receiver
    // actually dispatches to.
    //
    // Measured 2026-08-18: this is what stopped devirtualisation inside a
    // splice from ever firing. The MIC evidence named the receiver class,
    // resolution found its real `equals` override, and the site was still
    // refused — `inline-resolve REFUSED java/lang/Object.equals: native-shadow`
    // — because `java/lang/Object.equals` has a registered native and that is
    // the name the constant pool carries. `objectsAreEqual` then refused in
    // turn, because a call in it was "neither spliced nor direct-bound".
    //
    // The SELECTED method is checked below against its own declaring class,
    // unconditionally, which is the precise form of this rule.
    //
    // nio21 wave 5, lane L4 (`CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY`, default
    // ON): one exception, a reviewed `Intrinsic` over a pure-leaf JDK body
    // (`cratonvm_jit::splice_intrinsic_jdk_body_admitted`, whose doc is the
    // review): its splice runs the JDK's own bytecode, which is what HotSpot
    // runs, and no VM state lives in the native. `StringLatin1.getChar` was a
    // native-funnel crossing per character of every case-insensitive compare
    // (no native is registered over it since 2026-10-13).
    // Asked of the exact class the native is registered on, at all three
    // screens below.
    //
    // nio21 wave 14, lane L2 (W12L4-1): a nested-only row (`Math.abs(J)J`, the
    // callee `multiplyExact(JJ)J` needs) is admitted only for a call the
    // optimizing tier meets INSIDE a body it splices (`nest_depth > 0`). A
    // compiling method's own `Math.abs(long)` (depth 0) keeps its scalar
    // `AbsL`; the single-pass tier never takes the row.
    let nested_in_ir_splice = ir_mode && nest_depth > 0;
    let intrinsic_body_spliceable = |class: &str| -> bool {
        cratonvm_jit::splice_intrinsic_jdk_body_admitted_at(
            class,
            callee_method,
            callee_desc,
            nested_in_ir_splice,
        ) && native_is_intrinsic(shared, class, callee_method, callee_desc)
    };
    // JIT round 20 wave 19 (lane yHttp): the three screens ask whether the
    // registration RUNS, not whether one exists (`inline_screen_native_shadows`).
    if receiver_class_id.is_none()
        && inline_screen_native_shadows(shared, None, callee_class, callee_method, callee_desc)
        && !intrinsic_body_spliceable(callee_class)
    {
        // j27 wave 1 (lane JNINAT, REFLECT2-21-3): the line names the
        // registration's kind and the call-site intrinsic that still serves
        // the call, so an intrinsified refusal is not read as an inlining loss.
        no!(native_shadow_refusal_reason(
            shared,
            callee_class,
            callee_method,
            callee_desc
        ));
    }

    // nio20 wave 2, lane L1 -- MINIMAL GUARD in L3's resolver
    // (`r20-shared-arena-close-does-not-wait-for-compiled-scoped-access-bodies`):
    // the optimizing tier never splices `Reference.reachabilityFence`'s empty
    // JDK body. The fence's whole contract is a USE of its referent, and the
    // IR computes liveness from the graph: a splice of `{}` deletes that use,
    // so the referent can leave every safepoint map right after its last real
    // read. Kept as an `Op::Call`, it lowers to no code under
    // `CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE` (nio20 wave 1 L2) and holds
    // the referent mapped up to the fence. Load-bearing for memory safety: a
    // shared arena's blocks are freed only once the closed session is
    // unreachable (`panama::shared_close_defer_free`), and a scoped `*Internal`
    // body is between its `checkValidStateRaw()` and its raw access exactly
    // while its fenced session is live; a `DirectByteBuffer` accessor's
    // `reachabilityFence(this)` is what keeps the buffer's cleaner from freeing
    // the block under the access. Since nio20 wave 1 (L4, `jdk-references`)
    // the fence has no registered native in either mode, so the
    // `native-shadow` refusal above no longer covered it. Rides the keep-alive
    // switch: `CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE=0` restores the
    // splice (the A/B arm; unsound for the reason above).
    if ir_mode
        && cratonvm_jit::is_reachability_fence_site(callee_class, callee_method, callee_desc)
        && true
    {
        no!("reachability-fence (kept as a keep-alive call: a splice of the empty body drops the referent's use)");
    }

    // j27 wave 5 (lane JMMHUNT, `CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER`, default
    // ON): nor `Thread.onSpinWait()`'s empty body. HotSpot's C2 compiles the
    // call as a `MemBar` (a compiler barrier that matches to `PAUSE`), so a
    // `while (!plainFlag) Thread.onSpinWait();` loop re-reads its flag and
    // ends. A splice of `{}` left the loop with no memory node at all, and the
    // optimizing tier hoisted the read out of it: the loop spun forever. Kept
    // as an `Op::Call` it is that barrier, and `ir_lower::on_spin_wait` lowers
    // it to one `PAUSE`. The compiling method's own call only (`nest_depth ==
    // 0`): inside a splice an unspliced static call needs a bound entry, or the
    // planner's unbindable-call / never-run-exit rules refuse or trap the
    // enclosing body, so a nested `onSpinWait` keeps its splice (the page's
    // residual). j27 wave 8 (lane W8SPL,
    // `CRATONVM_JIT_IR_ON_SPIN_WAIT_NESTED_BARRIER`, default ON since
    // 2026-10-28, j27 WARNING flip; =0 restores): at any depth,
    // the planner then counting the surviving row as bound
    // (`on_spin_wait::counts_as_barrier_row`).
    if ir_mode
        && (nest_depth == 0
            || gates.ir_on_spin_wait_nested_barrier_enabled)
        && cratonvm_jit::ir_lower::on_spin_wait::is_on_spin_wait_site(
            callee_class,
            callee_method,
            callee_desc,
        )
        && gates.ir_on_spin_wait_barrier_enabled
    {
        no!("on-spin-wait (kept as a barrier call: a splice of the empty body lets the loop's reads hoist)");
    }

    // JIT round 20 wave 11, lane pAtom (W6-L3-3 / AT-5,
    // `CRATONVM_JIT_UNSAFE_RMW_BODY_NO_SPLICE`, default ON): the optimizing
    // tier keeps `Unsafe.getAndAdd*` / `getAndSet*` / `getAndBitwise*` as a
    // call wherever its RMW prefix (`try_emit_ir_unsafe_rmw_prefix`) would
    // serve that call as one locked instruction under this VM's admission
    // word; a splice of the JDK body leaves a volatile-load + `LOCK CMPXCHG`
    // retry loop instead. `--real-jdk` only in practice: under `--compatible`
    // the registered natives already refuse the splice (`native-shadow`).
    // Asked before the class-manager guard: the admission word reads the
    // registry and the `--real-jdk` gates, never under that lock.
    if ir_mode
        && callee_class == "jdk/internal/misc/Unsafe"
        && (cratonvm_jit::direct_helpers::unsafe_inline_rmw_site(
            callee_class,
            callee_method,
            callee_desc,
        )
        .is_some()
            // JIT round 23 wave 1, lane L9: and the `getAndSetReference*`
            // bodies the reference getAndSet field arm serves
            // (`CRATONVM_JIT_UNSAFE_REF_GET_AND_SET_FIELD`).
            || cratonvm_jit::direct_helpers::unsafe_inline_ref_volatile_site(
                callee_class,
                callee_method,
                callee_desc,
            )
            .is_some_and(|access| access.exchange))
        && cratonvm_jit::direct_helpers::unsafe_inline_rmw_body_splice_loses_prefix(
            callee_class,
            callee_method,
            callee_desc,
            crate::jit::helpers::unsafe_inline_admitted_word_for_compile(shared),
        )
    {
        no!("unsafe-rmw-body-keeps-its-inline-prefix");
    }

    // Taken BEFORE the class-manager guard. Holding two of this subsystem's
    // locks at once is a lock-order obligation, and every other reader of
    // `lambda_proxies` here takes it without `class_manager` held; a proxy's
    // dispatch is synthesised elsewhere entirely, so one is never inlineable.
    if let Some(receiver_id) = receiver_class_id {
        if shared.classes.is_lambda_proxy_class(receiver_id) {
            // JIT round 23 wave 2d, lane L2 (L2W1-3,
            // `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_NESTED`): a nested optimizing
            // resolution for a proxy receiver -- every nested route passes the
            // class its builder will prove (an exact-receiver row, a nested
            // guarded or trap-guarded row) -- is answered by the proxy's
            // forwarder, which the builder splices only on that proof and
            // resumes a trap in through the enclosing body's `invoke`
            // (`ir::IrBuilder::forwarder_reexecute_chain`).
            if ir_mode
                && nest_depth > 0
                && gates.ir_lambda_sam_splice_enabled
                && gates.ir_lambda_sam_splice_nested_enabled
            {
                return resolve_ir_lambda_sam_site(
                    shared,
                    requesting_class_id,
                    receiver_id,
                    callee_class,
                    callee_method,
                    callee_desc,
                    direct_bind,
                    nest_depth,
                    planning_only,
                    splice_arg_objects.first().copied().flatten(),
                );
            }
            // JIT round 23 wave 1, lane L2 (INLINE22-4's census): one line per
            // refusal naming the CALLER's class, so `grep lambda-proxy-census`
            // groups them by caller (the REFUSED line below names only the
            // site). A route that reaches here is one the lambda SAM splice
            // does not serve (`resolve_ir_lambda_sam_site`: the single-pass
            // tier, a nested resolution, the switch off).
            if crate::runtime::env_cache::dbg_jitc() {
                note_lambda_proxy_census(
                    shared,
                    requesting_class_id,
                    receiver_id,
                    callee_class,
                    callee_method,
                    callee_desc,
                    if ir_mode { "ir" } else { "sp" },
                    nest_depth,
                    "refused (receiver-is-lambda-proxy)",
                );
            }
            no!("receiver-is-lambda-proxy");
        }
    }

    let cm = shared.classes.class_manager.read();
    // A name the compile SUBSTITUTED for this site's constant-pool class is
    // resolved by the id the substitution recorded, under the direct binder's
    // rule: not a class constant of the caller, so neither looked up in its
    // loader nor access-checked as one; only the member's own access is asked
    // below (`DirectBindClass::admits`). Every other name is the constant
    // pool's, resolved and §5.4.4-checked as before (i13-L2).
    let substituted_id = match (receiver_class_id, substituted) {
        (None, Some(owners)) => owners.owner_id(
            requesting_class_id,
            callee_class,
            callee_method,
            callee_desc,
        ),
        _ => None,
    };
    let owner_class = match substituted_id {
        Some(id) => DirectBindClass {
            id,
            inaccessible_owner: !substituted_owner_accessible(&cm, requesting_class_id, id),
        },
        None => {
            // A constant-pool resolution asks the loader-faithful answer
            // for a JDK-global name; a receiver resolution does not (see
            // `loader_faithful_global_name_answer`).
            let known = if receiver_class_id.is_none() {
                jit_known_class_constant(shared, &cm, callee_class, requesting_class_id)
            } else {
                jit_known_class(shared, &cm, callee_class, requesting_class_id)
            };
            let Some(id) = known else {
                no!("cp-class-not-loaded");
            };
            DirectBindClass {
                id,
                inaccessible_owner: false,
            }
        }
    };
    let cp_class_id = owner_class.id;
    let store = cm.class_store();
    // JVMS 11 §6.5 invokeinterface/invokevirtual: when the method the constant
    // pool RESOLVES to is private, it is the selected method, and no walk from
    // the receiver may pick a same-named method of the receiver's class. See
    // cha-binds-a-private-interface-method-to-the-receivers-public-namesake-20260918.md.
    if receiver_class_id.is_some()
        && crate::classloading::invokevirtual_private_declaring_class(
            cp_class_id,
            callee_method,
            callee_desc,
            store,
        )
        .is_some()
    {
        no!("cp-resolved-method-is-private");
    }
    // Where the JVMS method-selection walk starts. For a guarded site that is
    // the RUNTIME receiver class; `find_method_recursive` performs the
    // maximally-specific default-method selection only when handed the
    // receiver, which is the same reason the interpreter's own dispatch
    // redirects to the receiver id for interface calls.
    let search_start = receiver_class_id.unwrap_or(cp_class_id);
    if let Some(receiver_id) = receiver_class_id {
        let Some(receiver) = store.get(receiver_id) else {
            no!("receiver-class-not-in-store");
        };
        // A guard admits an EXACT class, so an interface or an array class is
        // never a class a receiver can have here.
        if receiver.is_interface() || receiver.name.starts_with('[') {
            no!("receiver-is-interface-or-array");
        }
    }
    let Some((method, declaring_id)) =
        crate::classloading::find_method_recursive(search_start, callee_method, callee_desc, store)
    else {
        no!("method-not-found-from-search-start");
    };
    // Held to the end of this resolution, so every nested resolution below sees
    // this method on its path ([`InlinePathEntry`]).
    let Some(_inline_path) =
        InlinePathEntry::enter(nest_depth, declaring_id, callee_method, callee_desc)
    else {
        no!("recursive-inline-level");
    };
    // Interpreter round i1 wave 39 (lane L1, cross-lane): no splice of a
    // method a debugger's breakpoint sits in; the site keeps its call, whose
    // door refuses the method while the breakpoint stays
    // (`interpreter::breakpoint_bars_compiling`).
    if super::breakpoint_bars_compiling(shared, declaring_id, callee_method, callee_desc) {
        no!("debugger-breakpoint");
    }
    // JIT round 21 wave 11 (lane finalize): no splice of a finalizable class's
    // constructor that can throw before its `super(..)`: its throw must leave
    // an interpreted frame (`exception_dispatch::finalizer_bars_compiling`;
    // the class store is the one this resolution already holds).
    if callee_method == "<init>"
        && super::finalizer_bars_compiling_in(&shared.config, store, declaring_id, callee_desc)
    {
        no!("finalizable-ctor-may-throw-before-init");
    }
    // j27 wave 5 (lane INVHUNT): a spliced `super.m()` runs only the body
    // JVMS 6.5 `invokespecial` selects; the walk above steps past an abstract
    // re-declaration (HotSpot: `AbstractMethodError`) and breaks a default
    // conflict by order (`IncompatibleClassChangeError`). The call raises them.
    if super_special_site
        && !crate::runtime::resolve::selection::super_special_body_agrees(
            store,
            requesting_class_id,
            cp_class_id,
            callee_method,
            callee_desc,
            declaring_id,
        )
    {
        no!("super-special-selection-is-not-the-walk");
    }
    // `find_method_recursive` steps past an abstract re-declaration to a
    // concrete ancestor, so from the constant-pool class it can answer a
    // PRIVATE ancestor method that is not the resolved one (JVMS §5.4.3.3:
    // the first declaration on the superclass chain) -- and `private` below
    // would then splice it unguarded for every receiver. Refuse unless it is
    // the resolved method (interpreter round i1 wave 29, lane L4;
    // `L4W29PrivateAncestorShadow.java`).
    if receiver_class_id.is_none()
        && method
            .access_flags
            .contains(cratonvm_reader::class_access_flags::MethodAccessFlags::PRIVATE)
        && crate::runtime::resolve::selection::resolve_declaring(
            store,
            cp_class_id,
            callee_method,
            callee_desc,
        ) != Some(declaring_id)
    {
        no!("private-ancestor-is-not-the-resolved-method");
    }
    // A substituted owner the caller could not name itself: only a public or
    // private instance member is spliced through it, as it is bound.
    if !owner_class.admits(method) {
        no!("substituted-owner-member-not-accessible");
    }
    // t1 wave 3, lane P: the member half of JVMS §5.4.4 for a constant-pool
    // name (`jit_known_class_constant` asks only the class half). A splice's
    // own call sites reach here without the compile door's peek, so a body
    // spliced into a compiled method ran its `super.pkgM()` on a superclass of
    // another run-time package (a child loader's subclass of an app class)
    // where HotSpot throws `IllegalAccessError` (`T1PRuntimePackageAccess`
    // `AccRecv.superCall`). Refused: the call stays and resolves at run time.
    // Not for an array class (`[B.clone()`: an array's `clone` is public,
    // JLS 10.7; the resolution core skips the checks for an array owner too).
    if substituted_id.is_none()
        && !callee_class.starts_with('[')
        && super::field_access::member_access_enforced(shared)
        && super::field_access::method_member_access_refusal(
            shared,
            &cm,
            requesting_class_id,
            cp_class_id,
            callee_method,
            callee_desc,
        )
        .is_some()
    {
        no!("member-access-refused");
    }
    if let Some(receiver_id) = receiver_class_id {
        if !receiver_resolution_is_dispatch_faithful(
            store,
            receiver_id,
            cp_class_id,
            declaring_id,
            method,
            callee_method,
            callee_desc,
        ) {
            no!(format!(
                "receiver-resolution-not-dispatch-faithful (selected on class id {}; {})",
                declaring_id.as_u32(),
                receiver_resolution_unfaithful_clause(
                    store,
                    receiver_id,
                    cp_class_id,
                    declaring_id,
                    method,
                    callee_method,
                    callee_desc,
                )
            ));
        }
        // JVMS §5.4.6 against the resolved method: the check above lets a
        // public method through as an override of anything, but a public
        // method in another package does not override a package-private
        // resolved one (§5.4.5). Same answer the JIT dispatch helpers use.
        if crate::runtime::resolve::selection::walk_selection_moves_to(
            store,
            receiver_id,
            cp_class_id,
            callee_method,
            callee_desc,
            Some((method, declaring_id)),
        )
        .is_some()
        {
            no!("receiver-walk-is-not-the-jvms-selection");
        }
    }
    // `--real-jdk`: JVMS §5.4.6 ends in an ERROR where the walk found a body —
    // an abstract re-declaration between the receiver and that body
    // (`AbstractMethodError`), two concrete maximally-specific defaults
    // (`IncompatibleClassChangeError`), or a non-public class method an
    // `invokeinterface` selects (`IllegalAccessError`). The check above keeps
    // the walk for every error answer, and the dispatch helpers raise the
    // error on a guard MISS (`jit_selection_failure`), but a body spliced
    // behind a guard that admits this receiver ran on every HIT. Asked of the
    // exact class the splice is for: the guarded or exact receiver, or, for a
    // constant-pool resolution, the constant-pool class itself (a `final`
    // one is how the optimizing tier splices an `invokevirtual` unguarded).
    // Compile time only; `strict_selection_error` answers the common shapes
    // without a walk, and `select` is memoized (interpreter round i1 wave 38,
    // lane L6; `L6W38InlineSelectionErrorsHot.java`).
    if shared.classes.selection_checks && !method.is_static() && !callee_method.starts_with('<') {
        if let Some(error) = crate::runtime::resolve::selection::strict_selection_error(
            store,
            receiver_class_id.unwrap_or(cp_class_id),
            cp_class_id,
            callee_method,
            callee_desc,
            Some((method, declaring_id)),
        ) {
            no!(format!("receiver-selection-error ({error:?})"));
        }
    }
    // The same rule, applied to the method actually SELECTED rather than the
    // one the constant pool names. Resolution may start at a subclass while the
    // executable override is registered on the declaring class, and it may
    // equally start at a supertype whose own method is native while the
    // receiver's override is ordinary bytecode.
    //
    // UNCONDITIONAL, where it used to be guarded by
    // `declaring_class_name != callee_class`. That guard was load-bearing only
    // because the early check above covered the equal case; now that the early
    // check runs for constant-pool resolution only, this one has to cover both
    // — otherwise a receiver-resolved site whose selection lands back on the
    // declared class would splice bytecode a native shadows. Checking exactly
    // the declaring class still permits a real bytecode override on an
    // intermediate subclass, matching `try_jit_compile_callee_slow`.
    let Some(declaring_class_name) = store.get(declaring_id).map(|c| &*c.name) else {
        no!("declaring-class-not-in-store");
    };
    if method.is_static()
        && cratonvm_jit::offload_hook::keeps_dispatch_helper(
            declaring_class_name,
            callee_method,
            callee_desc,
        )
    {
        no!("gpu-offload-keeps-dispatch-helper");
    }
    // IR-tier splices are UNGUARDED: `IrBuilder` has no class-id compare and no
    // miss edge to send a wrong receiver down, so the target must be the only
    // body a call at this site can ever reach. `invokespecial`/`invokestatic`
    // are that by definition; a virtual site qualifies only when the language
    // has already ruled out an override.
    //
    // Deliberately NOT a CHA-style "no loaded subclass overrides it" answer:
    // that is true until the next class loads, so it needs an invalidation
    // dependency the IR path does not record. `final` is monotone.
    //
    // A `final` CONSTANT-POOL class proves it too: a receiver at such a site
    // is exactly that class, and the walk above started there. That is the
    // `invokevirtual_final_declaring_class` rule the single-pass tier binds
    // `StringBuilder.length()` by, whose body `AbstractStringBuilder` declares
    // (neither that class nor the method is `final`); without it the
    // optimizing tier refused the splice the same compile binds (i13-L2).
    if ir_mode {
        use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
        let class_is_final = |id: ClassId| {
            store
                .get(id)
                .is_some_and(|c| c.access_flags.contains(ClassAccessFlags::FINAL))
        };
        let monomorphic = method.is_static()
            || callee_method == "<init>"
            || method
                .access_flags
                .intersects(MethodAccessFlags::PRIVATE | MethodAccessFlags::FINAL)
            || class_is_final(declaring_id)
            || (receiver_class_id.is_none() && class_is_final(cp_class_id))
            // Round 12 wave 4 (lane iropt3): in `ir_mode` a receiver class is
            // passed only by `resolve_ir_exact_receiver_inline_site`, whose
            // consumer splices the body only for a receiver PROVEN to be exactly
            // that class (`IrBuilder::exact_receiver_proof`). The nested
            // resolutions pass `None`, leave `ir_mode` false, or (round 13
            // wave 10, HS-2, `resolve_ir_nested_exact_receiver_site`) pass the
            // constant-pool class for an `invokevirtual` whose row the IR
            // consumer splices only for a receiver it proves exactly that
            // class. Round 13 wave 12 (lane guardsplice, W7-1):
            // `resolve_ir_profile_receiver_inline_site` passes the profile's
            // class, whose consumer splices only behind `ExactClassIs` with the
            // site's own call as the miss edge.
            || receiver_class_id.is_some()
            // The r23 L8 heap-buffer close: a super call (`invokespecial` of
            // the requesting body's direct superclass, not `<init>`) selects
            // from that superclass by JVMS 6.5 `invokespecial` -- the walk
            // this resolution made from it -- whatever the receiver's class.
            || super_special_site;
        if !monomorphic {
            no!("ir-splice-target-not-provably-monomorphic");
        }
    }
    if inline_screen_native_shadows(
        shared,
        Some(&*cm),
        declaring_class_name,
        callee_method,
        callee_desc,
    ) && !intrinsic_body_spliceable(declaring_class_name)
        && !selected_bridge_masked_by_retired_row(
            shared,
            &cm,
            (cp_class_id, receiver_class_id),
            declaring_id,
            declaring_class_name,
            callee_method,
            callee_desc,
        )
    {
        no!("native-shadow-on-selected-method");
    }
    // The bisect levers (`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY`),
    // asked of the body ACTUALLY being spliced — N8.
    //
    // `jit/src/lib.rs`'s `plan_inline` ladder already asks them, and says in
    // its own comment that it asks the CONSTANT-POOL name: *"guarded
    // virtual/interface speculation resolves its body per receiver class …
    // so a deny aimed at a speculated receiver's own class name is NOT covered
    // by this and would need the receiver's name, which this site does not
    // have."* This site has it. While guarded speculation was default-off that
    // gap was latent; turning it on makes it live, and a lever that cannot stop
    // a splice makes a bisect LIE — which `compile_gate`'s module doc calls out
    // as worse than a missing feature, having paid for it once already (every
    // bisect step on the annotation-scan SIGSEGV read "no effect" while eleven
    // methods were still compiling).
    //
    // Asked of both names a reader would type. The DECLARING class is the one
    // whose source the spliced bytecode came from, and the RECEIVER class is
    // the one a bisect would name after reading a stack trace or a profile —
    // for `TreeMap$EntryIterator.hasNext` those are two different classes, and
    // denying either has to work.
    if cratonvm_jit::jit_force_interpret(declaring_class_name, callee_method) {
        no!("force-interpreted (declaring class)");
    }
    if let Some(receiver_id) = receiver_class_id {
        if let Some(receiver_name) = store.get(receiver_id).map(|c| &*c.name) {
            if receiver_name != declaring_class_name
                && cratonvm_jit::jit_force_interpret(receiver_name, callee_method)
            {
                no!("force-interpreted (receiver class)");
            }
        }
    }
    // ...and the same rule again, over the whole receiver-to-declaring chain.
    //
    // # Why the declaring class alone is not enough
    //
    // A native is registered on the class the RECEIVER actually has, and the
    // method it shadows is very often DECLARED on a superclass. The two
    // screens above ask about the constant-pool class and the declaring class,
    // and a guarded virtual site has neither: it starts the selection walk at
    // the runtime receiver, `find_method_recursive` returns the first concrete
    // body it meets, and that body's declaring class is where the screen then
    // looks — one or more classes ABOVE the one carrying the native.
    //
    // Measured 2026-09-04, `probes/TreeTailIterProbe.java` with
    // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1`: a compiled
    // `for (e : treeMap.tailMap(k).entrySet())` iterates ZERO entries while
    // `entrySet().size()` on the same object answers 6. The single spliced
    // site is `java/util/Iterator.hasNext()Z`, guarded on
    // `java/util/TreeMap$EntryIterator` -- which has
    // `native_al_itr_has_next` registered on it by the `VALUES_ITR_CARRIERS`
    // loop. But `hasNext` is DECLARED on `java/util/TreeMap$PrivateEntryIterator`,
    // which carries no native, so `declaring_class_name` above cleared the
    // screen and the splice ran the real JDK body -- `return next != null` over
    // a `next` field a natively-managed iterator never populates. False, every
    // time, from the first compiled call.
    //
    // Walking the chain is the precise form of the rule the two screens above
    // state, because it asks the question DISPATCH asks: not "does the class
    // that wrote this method have a native" but "does any class this receiver
    // IS have one". Bounded by `declaring_id` -- past it the body is not the
    // one being spliced -- and short in practice.
    if let Some(receiver_id) = receiver_class_id {
        let mut walk = Some(receiver_id);
        while let Some(cid) = walk {
            let Some(class) = store.get(cid) else { break };
            if inline_screen_native_shadows(
                shared,
                Some(&*cm),
                &class.name,
                callee_method,
                callee_desc,
            ) && !(cid == declaring_id && intrinsic_body_spliceable(&*class.name))
                && !(cid == declaring_id
                    && selected_bridge_masked_by_retired_row(
                        shared,
                        &cm,
                        (receiver_id, Some(receiver_id)),
                        declaring_id,
                        declaring_class_name,
                        callee_method,
                        callee_desc,
                    ))
            {
                no!(format!(
                    "native-shadow-on-receiver-chain (registered on {}, declared on {})",
                    class.name, declaring_class_name
                ));
            }
            if cid == declaring_id {
                break;
            }
            walk = class.superclass;
        }
    }
    // The class the SPLICED BODY belongs to, which is what an invalidation
    // dependency must name. For a constant-pool resolution this stays the
    // declared name (unchanged behaviour); for a receiver resolution the
    // declared name is a supertype that may own no body at all, so naming it
    // would record a dependency on a class whose redefinition cannot affect
    // the code, and miss the one whose redefinition can.
    //
    // JIT round 21 wave 14 (lane trace,
    // `CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME`, default ON): a
    // constant-pool resolution names the DECLARING class too when the
    // selection walk left the constant-pool class. That name and id are what
    // every spliced level's frame is reported as (the inline-frame label and
    // its class id), so an inherited body spliced through `K3.d1` -- `A.d1`,
    // declared on interface `A` -- printed `K3.d1:-1` (K3 declares no `d1`,
    // so no line table either) where HotSpot prints `A.d1:110`
    // (`tools/probes/jit/r21/fuzz/R21Fz3SpliceIfaceThrow1.java`). The body IS
    // the declaring class's, so the invalidation dependency names the class
    // whose redefinition can change it, as for a receiver resolution.
    let names_declaring = splice_names_declaring_class(
        receiver_class_id.is_some(),
        declaring_id.as_u32(),
        cp_class_id.as_u32(),
        splice_declaring_class_frame_enabled,
    );
    let inlined_body_class_name = if names_declaring {
        declaring_class_name.to_string()
    } else {
        callee_class.to_string()
    };
    // ...and its id, chosen by the SAME branch so the two can never name
    // different classes. A stack walk that has to answer in `ClassId` -- the
    // JEP 403 deep-reflection gate, `Class.forName`'s caller loader -- can then
    // see a spliced frame without resolving a JIT label by name, which would be
    // a guess in a security-relevant path. `cp_class_id` is the id
    // `callee_class` was looked up under; `declaring_id` is the class that
    // declares the body a receiver resolution selected.
    let inlined_body_class_id = if names_declaring {
        declaring_id.as_u32()
    } else {
        cp_class_id.as_u32()
    };

    // Lifting this alone would splice a body with no monitor at all; adding
    // one needs a monitor on every exit of the splice, including a trap, whose
    // resume re-executes the caller's invoke (which locks again). It would also
    // not reach `R11X64genSyncCost.incSync`: its `putstatic`s are refused
    // next, by `ir-splice-putstatic` (optimizing tier) and by
    // `inline_no_static` (single-pass tier, default on). A compiled caller
    // reaches such a callee through a caller-held direct CALL instead; see
    // `direct_bind_method_refusal`.
    //
    // Round 14 wave 2 (lane syncsplice, SR-1): one admission. The optimizing
    // tier's constant-pool resolution at depth 0, of a method the Methodref's
    // own class declares, is answered with `callee_is_synchronized` set; the
    // IR planner splices it only between a `MonitorEnter`/`MonitorExit` pair
    // and only when the body cannot trap (`cratonvm_jit`'s
    // `ir_sync_splice_site_refusal`, re-checked on the built graph and on the
    // final schedule), and keeps the call otherwise.
    // `CRATONVM_JIT_IR_SYNC_SPLICE=0` restores the refusal.
    let sync_splice = method.is_synchronized();
    // Round 14 wave 6 (lane sync6, SS8-1 a): also an INSTANCE body at depth 1,
    // which the parent's nested loop below keeps only under a synchronized
    // instance body at depth 0 -- the JIT splices it with no pair, inside the
    // parent's window, on the window's own receiver only
    // (`ir_sync_splice_nested_same_receiver_enabled`).
    let sync_depth_ok = nest_depth == 0
        || (nest_depth == 1
            && !method.is_static()
            && gates.ir_sync_splice_nested_same_receiver_enabled);
    if sync_splice
        && !(ir_mode
            && sync_depth_ok
            && receiver_class_id.is_none()
            && substituted_id.is_none()
            && declaring_id == cp_class_id
            && gates.ir_sync_splice_enabled)
    {
        no!("synchronized");
    }
    let Some(code_attr) = method.code() else {
        no!("selected-method-has-no-code");
    };
    let code_len = code_attr.code.len();
    let (early_live_pcs, early_always_taken_pcs, early_never_taken_pcs, early_pruned_switch_edges): (
        Option<Vec<bool>>,
        Vec<usize>,
        Vec<usize>,
        Vec<(usize, usize)>,
    ) = if ir_mode && gates.ir_splice_prune_feed_enabled {
        let frozen = store.get(declaring_id).is_some_and(|c| {
            shared
                .jit
                .jit_cache
                .read()
                .get(
                    &Arc::from(&*c.name),
                    &Arc::from(callee_method),
                    &Arc::from(callee_desc),
                    declaring_id,
                )
                .is_some()
        });
        let raw_code = code_attr.code.as_bytes();
        let always = callee_pruned_branch_rows(
            shared,
            declaring_id.as_u32(),
            callee_method,
            callee_desc,
            raw_code,
            code_len,
            frozen,
            true,
        );
        let never = callee_pruned_branch_rows(
            shared,
            declaring_id.as_u32(),
            callee_method,
            callee_desc,
            raw_code,
            code_len,
            frozen,
            false,
        );
        let switch_edges = callee_pruned_switch_edges(
            shared,
            declaring_id.as_u32(),
            callee_method,
            callee_desc,
            raw_code,
            code_len,
            frozen,
        );
        if let Some((live, live_bytes)) =
            cratonvm_jit::splice_live_pcs_without_pruned_and_switch(
                raw_code,
                code_len,
                &always,
                &never,
                &switch_edges,
            )
        {
            if code_len > cratonvm_jit::MAX_INLINE_BYTECODE_SIZE
                && live_bytes > cratonvm_jit::MAX_INLINE_BYTECODE_SIZE
            {
                no!("too-large");
            }
            (Some(live), always, never, switch_edges)
        } else {
            if code_len > cratonvm_jit::MAX_INLINE_BYTECODE_SIZE {
                no!("too-large");
            }
            (None, always, never, switch_edges)
        }
    } else {
        if code_len > cratonvm_jit::MAX_INLINE_BYTECODE_SIZE {
            no!("too-large");
        }
        (None, Vec::new(), Vec::new(), Vec::new())
    };
    // Round nio20 wave 1, lane L1 (NIO19-1): one exception table is admitted
    // to the optimizing tier's splice -- a javac `finally {
    // Reference.reachabilityFence(s..); }` and nothing else
    // (`cratonvm_jit::ir::strip_fence_finally_handler`, whose doc is the
    // argument). Running that handler is propagating the exception, so the
    // spliced COPY drops it (cut off, or `nop`ped together with the `goto`
    // around it) and a throw in the spliced range takes the route of every
    // spliced throw: a call's exception exit is answered by the caller's own
    // table at its `invoke`, a guard deopts through the chain into this
    // method's frame and the interpreter runs the real handler. A synchronized
    // body keeps the refusal (its window owns every exceptional exit).
    // `CRATONVM_JIT_IR_SPLICE_FENCE_FINALLY=0` restores the refusal.
    //
    // The PUBLIC `ScopedMemoryAccess` accessors (`putIntUnaligned`, ..) are
    // not that shape: they `catch (ScopedAccessError e) { throw
    // e.newRuntimeException(); }`, a handler that runs code (a closed session
    // reaches it through the `*Internal` intrinsic's bytecode fallback), so
    // they stay refused -- and their call site is the IR scoped fold's
    // (`cratonvm_jit::ir::try_ir_scoped_unbox`), which lowers it raw. The
    // refusal names that, so a `CRATONVM_DBG_JITC` census does not read an
    // expected refusal as a missed splice.
    //
    // Round nio21 wave 4, lane L2 (`CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS`,
    // default ON since JIT r24, `=0` off; j27 SMALLCALL fixed this comment,
    // which still said OFF): a table that is not the fence shape may still be admitted
    // when every row is `try { calls } catch (X e) { throw wrap(e); }`
    // (`cratonvm_jit::ir::splice_callee_handler_plan`, whose doc is the
    // argument; `CharsetDecoder.decode(BB,CB,Z)` / `CharsetEncoder.encode`).
    // The copy masks each handler; the builder covers the `try` ranges with
    // call-site exception edges, and an entered handler resumes in the
    // interpreter. Never for a scoped accessor (the scoped fold owns its call
    // site), a synchronized body, a catch type a `StackOverflowError` could
    // match (the stack-guard exits keep the caller's route), a catch-all, a
    // catch type not loaded yet, or a body whose loader is user-defined (the
    // run-time catch resolution may decline there, and a declining spliced
    // site has no correct exit).
    let mut handler_rows: Vec<(usize, usize, usize, String, u32)> = Vec::new();
    let mut handler_masked: Option<Vec<u8>> = None;
    // Round rx27 wave 1 (lane matcher, `CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM`,
    // opt-in): a table with a catch-all row (a `synchronized` block, a
    // `finally`), which the handler splice below refuses, may still be
    // admitted when every row lies inside one branch's fall-through arm that
    // the callee's profile never entered -- `Pattern.matcher`'s `if
    // (!compiled) { synchronized (this) { .. } }` compile guard. The copy masks
    // the arm to a cold throw arm (`cratonvm_jit::ir::cold_handler_arm_mask`,
    // whose doc is the argument), the cold-throw pass below takes it, and the
    // builder turns its branch into a guard that deopts AT the branch
    // (`IrBuilder::prune_cold_throw_arm`): the interpreter, resumed in the
    // callee's own frame before the branch, runs the arm with the method's own
    // table, so compiled code never holds the monitor nor enters the handler.
    // The arm's coldness is asked of the profile once `cm` is dropped.
    let mut cold_handler_arm: Option<cratonvm_jit::ir::ColdHandlerArm> = None;
    // rx27 (lane matcher): `InlineSite::callee_original_handler_rows`.
    let callee_original_handler_rows: Vec<(usize, usize, usize, u16)> = code_attr
        .exception_table
        .iter()
        .map(|e| {
            (
                usize::from(e.start_pc),
                usize::from(e.end_pc),
                usize::from(e.handler_pc),
                e.catch_type,
            )
        })
        .collect();
    let live_handler_rows: Vec<(usize, usize, usize, u16)> = callee_original_handler_rows
        .iter()
        .copied()
        .filter(|&(start, end, _, _)| {
            !early_live_pcs.as_ref().is_some_and(|live| {
                !(start..end.min(code_len))
                    .any(|p| live.get(p).copied().unwrap_or(false))
            })
        })
        .collect();
    let fence_strip = if live_handler_rows.is_empty() {
        None
    } else {
        let scoped_accessor = ir_mode
            && cratonvm_jit::ir::try_ir_scoped_unbox(callee_class, callee_method, callee_desc)
                .is_some();
        let exception_table_refusal = if scoped_accessor {
            "callee-exception-table (ScopedMemoryAccess accessor: the IR scoped fold lowers the call site)"
        } else {
            "callee-exception-table"
        };
        let fence_on = gates.ir_splice_fence_finally_enabled;
        // JIT r23 wave 11 (lane L5): or `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLER_TRAPS`
        // (`cratonvm_jit::ir::ir_splice_callee_handlers_admitted`).
        let handlers_on = !scoped_accessor && cratonvm_jit::ir::ir_splice_callee_handlers_admitted();
        // rx27 (lane matcher): see `cold_handler_arm` above. Only for a table
        // the handler splice refuses outright (a catch-all row), so no body it
        // admits today changes route.
        let cold_arm_on = !scoped_accessor
            && code_attr.exception_table.iter().any(|e| e.catch_type == 0)
            && gates.ir_splice_cold_handler_arm_enabled
            && gates.ir_inline_cold_athrow_enabled;
        if !ir_mode || sync_splice || !(fence_on || handlers_on || cold_arm_on) {
            no!(exception_table_refusal);
        }
        let Some(pool_class) = cm.get_class(declaring_id) else {
            no!(exception_table_refusal);
        };
        let pool = &pool_class.constant_pool;
        let is_fence = |cp_idx: u16| -> bool {
            let Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) = pool.get(cp_idx)
            else {
                return false;
            };
            pool.get_class_name(*class_index) == Some("java/lang/ref/Reference")
                && pool.get_name_and_type(*name_and_type_index)
                    == Some(("reachabilityFence", "(Ljava/lang/Object;)V"))
        };
        let rows: Vec<(usize, usize, usize, u16)> = live_handler_rows;
        let strip = if fence_on {
            cratonvm_jit::ir::strip_fence_finally_handler(
                code_attr.code.as_bytes(),
                code_len,
                &rows,
                &is_fence,
            )
        } else {
            None
        };
        // rx27 (lane matcher): the masked copy and its arm, when the shape
        // holds (the profile is asked after `drop(cm)`).
        let cold_arm = if strip.is_none() && cold_arm_on {
            let num_params = cratonvm_jit::count_param_slots_jvm_spec(callee_desc)
                + usize::from(!method.is_static());
            let ranges: Vec<(usize, usize, usize)> =
                rows.iter().map(|&(s, e, h, _)| (s, e, h)).collect();
            match cratonvm_jit::ir::cold_handler_arm_mask(
                code_attr.code.as_bytes(),
                code_len,
                &ranges,
                num_params,
                usize::from(code_attr.max_locals),
            ) {
                Ok(found) => Some(found),
                Err(why) => {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] splice-cold-handler-arm REFUSED {callee_class}.{callee_method}{callee_desc} \
                             depth={nest_depth}: {why}"
                        );
                    }
                    None
                }
            }
        } else {
            None
        };
        match (strip, cold_arm) {
            (Some(strip), _) => Some(strip),
            (None, Some((masked, arm))) => {
                handler_masked = Some(masked);
                cold_handler_arm = Some(arm);
                None
            }
            (None, None) if handlers_on => {
                // Round nio21 wave 4, lane L2: see above.
                if matches!(
                    cm.get_loader_id(declaring_id),
                    None | Some(cratonvm_types::ClassLoaderId::UserDefined(_))
                ) {
                    no!("callee-exception-table (handlers: user-defined loader)");
                }
                let mut named: Vec<(usize, usize, usize, String, u32)> =
                    Vec::with_capacity(rows.len());
                // JIT round 21 wave 3 (lane keepset;
                // `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE`, opt-in): the
                // `CompletableFuture.uniApplyNow` shape -- a `Throwable` /
                // `Error` catch whose handler runs code and reads pre-`try`
                // locals (`cratonvm_jit::ir::splice_callee_handler_plan_with`).
                // An entered handler only ever resumes in the interpreter (it
                // deopts at its first instruction), which runs it with the
                // method's own table; the chain-snapshot liveness keeps what it
                // reads. A catch-all row stays refused.
                // JIT r23 wave 11 (lane L5): or the trap switch
                // (`cratonvm_jit::ir::ir_splice_callee_handlers_wide_admitted`).
                let wide = cratonvm_jit::ir::ir_splice_callee_handlers_wide_admitted();
                for &(start, end, handler, catch_type) in &rows {
                    let name = if catch_type == 0 {
                        if !wide {
                            no!("callee-exception-table (handlers: catch-all)");
                        }
                        "java/lang/Throwable"
                    } else {
                        let Some(name) = pool.get_class_name(catch_type) else {
                            no!("callee-exception-table (handlers: unnamed catch type)");
                        };
                        name
                    };
                    if !wide
                        && matches!(
                            name,
                            "java/lang/Throwable"
                                | "java/lang/Error"
                                | "java/lang/VirtualMachineError"
                                | "java/lang/StackOverflowError"
                        )
                    {
                        no!("callee-exception-table (handlers: catches StackOverflowError)");
                    }
                    named.push((start, end, handler, name.to_string(), declaring_id.as_u32()));
                }
                let ranges: Vec<(usize, usize, usize)> =
                    rows.iter().map(|&(s, e, h, _)| (s, e, h)).collect();
                let Some(plan) = cratonvm_jit::ir::splice_callee_handler_plan_with(
                    code_attr.code.as_bytes(),
                    code_len,
                    &ranges,
                    wide,
                ) else {
                    no!("callee-exception-table (handlers: not the try-call/catch-wrap-throw shape)");
                };
                handler_rows = named;
                handler_masked = Some(plan.code);
                None
            }
            (None, None) => no!(exception_table_refusal),
        }
    };
    let is_static = method.is_static();
    // jit-inline-clinit-gap fix (2026-07-17): inlining a static method's
    // bytecode splices it directly into the caller with NO call boundary at
    // all -- not even the `direct_calls` raw CALL that `callee_compiler`
    // (this file, the sibling closure guarding `direct_calls`) gates on
    // class-init state. A callee whose OWN body never touches its class's
    // statics (no getstatic/putstatic/new of its own -- the existing
    // "conservative default: do NOT inline getstatic/putstatic-bearing
    // callees" gate a few lines below only excludes the OPPOSITE shape)
    // gives the JIT no code-level opportunity whatsoever to run `<clinit>`
    // before the inlined body executes, silently violating the same JVMS
    // §5.5 "class must be initialized before first invocation of any of its
    // static methods" requirement `callee_compiler`'s gate enforces for the
    // direct-call path. A class's initialized state is monotonic (JVMS:
    // once Initialized, always Initialized), so -- exactly like
    // `callee_compiler`'s check -- checking ONCE, here, at inline-planning
    // time is sound forever for this call site: only admit a static
    // callee for inlining when its declaring class is ALREADY initialized;
    // otherwise return `None`, which drops the site to the ordinary
    // dispatch/direct-call resolution below (both of which independently
    // enforce initialization). `cm`'s read guard is still live here.
    if is_static {
        let declaring_class_initialized = store
            .get(declaring_id)
            .map(crate::vm::is_class_initialized_fast)
            .unwrap_or(false);
        if !declaring_class_initialized {
            no!("static-declaring-class-not-initialized");
        }
    }
    // The constructor twin of the rule above. A spliced `<init>` leaves the
    // caller's `new` with no call its receiver escapes into, so escape analysis
    // may scalar-replace it -- and a scalar-replaced `new` runs no `<clinit>`
    // and registers no finalizer. An uninitialised class here means the `new`
    // has never run (cold); a finalizable one must keep its allocation.
    //
    // al16 (2026-10-05): for the optimizing tier, a SETTLED class (no
    // `<clinit>` owed in its chain, linked: `jit_new_target_init_pending`)
    // counts as initialised, as at the `new` it constructs (see the splice
    // resolver's `ir-splice-new-site-class-not-initialized` arm). Skipping its
    // initialisation is unobservable. `CRATONVM_JIT_IR_SPLICE_NEW_SETTLED_CLASS=0`
    // restores the stricter rule.
    if callee_method == "<init>" {
        let settled_uninitialized = ir_mode
            && !jit_new_target_init_pending(shared, &cm, declaring_id);
        let ctor_class_ok = store.get(declaring_id).is_some_and(|c| {
            (crate::vm::is_class_initialized_fast(c) || settled_uninitialized) && !c.has_finalizer
        });
        if !ctor_class_ok {
            // JIT round 18 wave 9 (lane eagercold): only "not initialized YET"
            // can lift; a finalizer is a standing property of the class.
            // `CRATONVM_JIT_LATE_BIND_UNINIT_CTOR` (default OFF).
            let not_yet_initialized = store
                .get(declaring_id)
                .is_some_and(|c| !crate::vm::is_class_initialized_fast(c) && !c.has_finalizer);
            if not_yet_initialized {
                note_uninit_ctor_late_bind_waiter(
                    shared,
                    requesting_class_id,
                    nest_depth,
                    callee_class,
                    callee_desc,
                );
            }
            no!("ctor-class-not-initialized-or-finalizable");
        }
    }
    // Round 13 wave 5 (lane hashsplice): the INSTANCE twin of the static rule
    // above, for the optimizing tier only, and a budget rule rather than a
    // soundness one. A class is
    // initialized before its first instance exists, and before any subclass is
    // (JVMS §5.5), so no receiver of an instance method whose declaring CLASS
    // is uninitialized has ever reached this site: the body is cold by
    // construction. Splicing it anyway spent the planner's shared byte budget
    // (`IR_INLINE_MAX_TOTAL_BYTES`) and build nodes on it -- the
    // `HashMap$TreeNode` paths nested in `putVal` / `getNode`
    // (`putTreeVal`, `getTreeNode`, and `find` recursively) on a map that never
    // treeified. The site stays the call it was, which is correct whenever a
    // receiver does turn up. HotSpot's C2 declines the same inlines ("method
    // holder not initialized"). Interfaces are not asked: one declaring a default
    // method is initialized with its implementors, but one that does not is
    // never initialized at all. `CRATONVM_JIT_IR_SPLICE_SKIP_UNINIT_CLASS=0` is
    // the kill switch.
    //
    // al16 (2026-10-05): the premise fails for a SETTLED class asked through a
    // concrete receiver class (the profile or factory route): the optimizing
    // tier's spliced `new` of such a class skips its unobservable
    // initialisation (`CRATONVM_JIT_IR_SPLICE_NEW_SETTLED_CLASS`), so live
    // instances exist while it reads uninitialised -- `ArrayList$Itr`, made
    // only by the spliced `iterator()`, kept `hasNext`/`next` calls forever.
    // Likewise a nested call a spliced body of that class makes on its own
    // class (`Itr.next` -> `checkForComodification`); a call into ANOTHER
    // uninitialised class (`HashMap` -> `TreeNode`) is still refused.
    // And at depth 0: a method compiled before its class is initialised (an
    // eager callee compile, `Matcher.group` before the first `Matcher`) is as
    // hot as its own-class callees (`checkMatch`, `checkGroup`).
    if ir_mode
        && !is_static
        && callee_method != "<init>"
        && store
            .get(declaring_id)
            .is_some_and(|c| !c.is_interface() && !crate::vm::is_class_initialized_fast(c))
        && !((receiver_class_id.is_some() || declaring_id == requesting_class_id)
            && !jit_new_target_init_pending(shared, &cm, declaring_id))
    {
        no!("ir-splice-instance-method-of-uninitialized-class");
    }
    let callee_max_locals = code_attr.max_locals as usize; // Widening: u16 to usize
    let code_view = code_attr.code.clone();
    // For the nested operand-source scan below, after the guard is gone.
    let body_handler_pcs: Vec<usize> = code_attr
        .exception_table
        .iter()
        .map(|e| usize::from(e.handler_pc))
        .collect();
    // Round 16 wave 7 (lane arraylist, AL16-1;
    // `CRATONVM_JIT_IR_INLINE_COLD_ATHROW`, default ON since `139a504029`,
    // `=0` the kill switch): for the optimizing
    // tier, a body whose every `athrow` ends a cold throw arm
    // (`cratonvm_jit::ir::cold_throw_arms`, the javac shape of `if (bad) throw
    // new X(..)`) is admitted with each arm MASKED in this spliced copy to
    // `aconst_null; nop..; athrow`. The scan below, every planner walk and the
    // builder then see no exception construction to resolve, and the builder
    // turns the arm's branch into a guard that deopts into the interpreter at
    // that branch, which throws from the callee's own frame
    // (`IrBuilder::prune_cold_throw_arm`). The interpreter runs the method's
    // own, unmasked code. A body with any other `athrow` is not masked and is
    // refused below as before.
    // JIT round 19 wave 4, lane throws4 (THROWS3-4,
    // `CRATONVM_JIT_IR_SELF_SPLICE_BASE_ARM_KEEPS_CALL`, default OFF): a
    // self-recursive body whose cold throw arm tests the recursion's own
    // stepped parameter (its base case) is entered by every call that reaches
    // the bottom, so masked and spliced it deopts per throw until the per-bci
    // de-spec withdraws the site. Refuse it here, so the site is the call it
    // ends as from the first compile.
    if ir_mode
        && code_view.as_bytes().contains(&0xbf)
        && gates.ir_inline_cold_athrow_enabled
        && gates.ir_self_splice_base_arm_keeps_call_enabled
    {
        let param_slots =
            cratonvm_jit::count_param_slots_jvm_spec(callee_desc) + usize::from(!is_static);
        let callee_pool = cm.get_class(declaring_id);
        let self_invoke = |cp_idx: u16| -> bool {
            let Some(class) = callee_pool.as_ref() else {
                return false;
            };
            let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::MethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                })
                | Some(ConstantPoolEntry::InterfaceMethodReference {
                    class_index,
                    name_and_type_index,
                    ..
                }) => (*class_index, *name_and_type_index),
                _ => return false,
            };
            let named_class = class.constant_pool.get_class_name(class_idx);
            class
                .constant_pool
                .get_name_and_type(nat_idx)
                .is_some_and(|(mn, desc)| {
                    mn == callee_method
                        && desc == callee_desc
                        && named_class.is_some_and(|cn| cn == callee_class || cn == &*class.name)
                })
        };
        if cratonvm_jit::ir::cold_throw_arm_tests_recursion_parameter(
            code_view.as_bytes(),
            code_len,
            param_slots,
            &self_invoke,
        ) {
            no!("ir-self-splice-recursion-base-throw-arm");
        }
    }
    //
    // Round nio20 wave 1, lane L1 (NIO19-1): a body whose fence `finally` was
    // stripped above is masked (and scanned, and spliced) from the stripped
    // copy, whose length is `code_len` from here on; `method_code_len` stays
    // the method's own, for what is asked of the method rather than the copy.
    let method_code_len = code_len;
    let code_len = fence_strip.as_ref().map_or(code_len, |s| s.code_len);
    // The caller of an exact-receiver row filters the answer by this cap
    // (`!callee_is_static && callee_code_len <= cap`): refuse here, before the
    // body's nested sites are resolved only to be thrown away. `code_len` is
    // the final `InlineSite::callee_code_len`; nothing below changes it.
    if exceeds_exact_row_size_cap(exact_size_cap, is_static, code_len) {
        no!("over-the-exact-row-size-cap-or-static");
    }
    // Round nio21 wave 4, lane L2: or the copy with its handlers masked
    // (same length; `handler_rows` set), scanned, masked and spliced alike.
    let stripped_code: Option<Vec<u8>> = fence_strip.map(|s| s.code).or(handler_masked);
    let unmasked: &[u8] = stripped_code.as_deref().unwrap_or(code_view.as_bytes());
    let mut cold_athrow_pcs: Vec<usize> = Vec::new();
    let masked_code: Option<Vec<u8>> =
        if ir_mode && unmasked.contains(&0xbf) && gates.ir_inline_cold_athrow_enabled
        {
            let mut copy = unmasked.to_vec();
            // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: under a top-level site
            // in a `try`, an arm that throws a `getstatic` value stays
            // unmasked, for the builder's spliced throw edge.
            if let Some(athrows) = cratonvm_jit::ir::mask_cold_throw_arms_keeping(
                &mut copy,
                code_len,
                cratonvm_jit::ir::ir_root_site_in_try(),
            ) {
                cold_athrow_pcs = athrows;
                Some(copy)
            } else {
                None
            }
        } else {
            None
        };
    let code_bytes: &[u8] = masked_code.as_deref().unwrap_or(unmasked);

    let code = code_bytes;
    let mut scan_pc = 0;
    let mut has_field_ops = false;
    let mut has_static_field_ops = false;
    let mut has_newarray = false;
    let mut has_ldc = false;
    let mut has_ldc2w = false;
    // Conservative default: do NOT inline getstatic/putstatic-bearing callees.
    // A decisive bisect showed that excluding them makes main-path inlining
    // correct (Spring `S01_Context` 15/15, EnumSet.noneOf clean), where leaving
    // them in miscompiled `EnumSet.getUniverse` (inlined
    // `SharedSecrets.getJavaLangAccess()` getstatic feeding `getEnumConstantsShared`
    // → null → `ClassCastException: … not an enum`). The isolated getstatic-inline
    // patterns (cross-class, non-zero field index, interface-typed, lazy-set) all
    // compile correctly, so the failing interaction is narrower than "all
    // getstatic" — but excluding them is the safe conservative fix until it is
    // isolated. `CRATONVM_INLINE_ALLOW_STATIC=1` re-enables them for debugging.
    let inline_no_static = !crate::runtime::env_cache::inline_allow_static();
    // invokespecial sites deferred for elidability validation once the
    // callee's constant pool is available below: (callee_pc, cp_idx). Only
    // no-op super-constructor calls (`java/lang/Object.<init>()V` directly,
    // or a target passing `is_elidable_construction`) survive — anything
    // else rejects the whole site. This is what admits CONSTRUCTOR bodies
    // (which always begin `aload_0; invokespecial super.<init>`) to
    // inlining; the blanket 0xb7 rejection made every ctor un-inlineable,
    // so each `new C(args)` paid a full dispatch round trip per allocation.
    let mut special_sites: Vec<(usize, u16)> = Vec::new();
    // Calls the callee body makes, deferred exactly like `special_sites`
    // because resolving them needs the CALLEE's constant pool, which is only
    // reachable once `callee_class_info` is fetched below:
    // `(callee_pc, cp_idx, opcode)`.
    //
    // Gated: with `CRATONVM_JIT_INLINE_CALLS` off every `invoke*` still
    // rejects the whole site outright, which is the behaviour every release
    // before this one had. The gate is read ONCE here rather than per-opcode so
    // a site cannot be admitted under one answer and emitted under another.
    let inline_calls_allowed = crate::runtime::env_cache::jit_inline_calls();
    let mut invoke_sites: Vec<(usize, u16, u8)> = Vec::new();
    // Interpreter round i6 wave 120, lane L2 (`CRATONVM_JIT_INVOKE_SITE_CLASSIFIER`,
    // default ON since the i6 wave-126 flip): the spliced `invokespecial`s the classifier bound to their
    // JVMS §6.5 selection start, `(callee pc, owner name, owner id)`, applied
    // where the call row is built below (as a pinned private method's owner
    // is). Empty with the switch off.
    let mut special_owners: Vec<(usize, String, u32)> = Vec::new();
    // IR-tier only: `new` sites in the body, `(callee_pc, cp_idx)`, resolved to
    // `(class_id, num_fields)` below once the callee's constant pool is in hand.
    // This is the row whose absence bailed `VolumeShort2.loadFromArray` out of
    // the IR tier altogether, and with it out of escape analysis.
    let mut ir_new_sites: Vec<(usize, u16)> = Vec::new();
    // JIT round 25 wave 10 (W10-6, `CRATONVM_JIT_IR_SPLICE_ANEWARRAY`): IR-tier
    // only, under the switch: `anewarray` sites, `(callee_pc, cp_idx)`,
    // resolved to component class ids below with the resolver the caller's own
    // `anewarray` rows use.
    let mut ir_anewarray_sites: Vec<(usize, u16)> = Vec::new();
    let ir_anewarray_admitted = ir_mode && gates.ir_splice_anewarray_enabled;
    // IR-tier only: `(pc, cp_idx, is_checkcast)` for the body's type checks.
    let mut ir_typecheck_sites: Vec<(usize, u16, bool)> = Vec::new();
    // IR-tier only: pcs of the body's return opcodes. A relocated body is walked
    // straight through with no merge bookkeeping, so exactly one return, at the
    // end, is the shape the splice can honour.
    let mut ir_return_pcs: Vec<usize> = Vec::new();
    // nio21 wave 11, lane L3 (W10L3-1): IR-tier only: `(callee_pc, cp_idx)`
    // of the body's `invokedynamic`s, resolved to constant-word rows below.
    let mut ir_indy_sites: Vec<(usize, u16)> = Vec::new();
    // Round nio21 wave 10 (lane L2): IR-tier only. The pcs of the body's
    // divisions that must prove a non-zero constant divisor below, once the
    // `ldc` rows are resolved (nio21 wave 14: one decoder,
    // `cratonvm_jit::ir::splice_division_const_divisor`).
    let mut ir_const_div_pcs: Vec<usize> = Vec::new();
    let mut dead_typecheck_sites: Vec<(usize, u16, bool)> = Vec::new();
    let mut dead_new_sites: Vec<(usize, u16)> = Vec::new();
    // The calls at pcs the prune feed called dead. The builder prunes only the
    // branches it can (`IrBuilder::prune_spliced_never_taken_branch` declines
    // without chain state, in a sync splice, behind a replay fence), and walks
    // the rest: a call there with no row bailed the whole build
    // (`Method.invoke`'s caller-sensitive arm, `checkAccess`'s slow path).
    // Resolved below as plain call rows that never refuse, nest or bind.
    let mut dead_invoke_sites: Vec<(usize, u16, u8)> = Vec::new();
    while scan_pc < code_len {
        if early_live_pcs
            .as_ref()
            .is_some_and(|live| !live.get(scan_pc).copied().unwrap_or(false))
        {
            if ir_mode && scan_pc + 2 < code_len {
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16;
                match code[scan_pc] {
                    0xc0 | 0xc1 if gates.ir_splice_typecheck_enabled => {
                        dead_typecheck_sites.push((scan_pc, cp_idx, code[scan_pc] == 0xc0));
                    }
                    0xbb => dead_new_sites.push((scan_pc, cp_idx)),
                    0xb6 | 0xb8 | 0xb9
                        if inline_calls_allowed
                            && !sync_splice
                            && scan_pc + (if code[scan_pc] == 0xb9 { 5 } else { 3 }) <= code_len =>
                    {
                        dead_invoke_sites.push((scan_pc, cp_idx, code[scan_pc]));
                    }
                    // The row loops below already resolve dead pcs softly
                    // (`pc_is_live`); these flags only let them run.
                    0xb2 | 0xb3 => has_static_field_ops = true,
                    0xb4 | 0xb5 => has_field_ops = true,
                    0x12 | 0x13 => has_ldc = true,
                    0x14 => has_ldc2w = true,
                    _ => {}
                }
            } else if ir_mode && code[scan_pc] == 0x12 {
                has_ldc = true;
            }
            scan_pc += cratonvm_jit::bytecode_insn_step(code, scan_pc);
            continue;
        }
        match code[scan_pc] {
            // IR verifies and walks each callee in its original bytecode
            // coordinates, then rebases switch targets. The single-pass
            // emitter has no switch arm. Keep synchronized splices conservative.
            0xaa | 0xab
                if ir_mode && !sync_splice && gates.ir_splice_branch_enabled =>
            {
                let next = cratonvm_reader::verified_code(&code[..code_len])
                    .ok()
                    .and_then(|verified| {
                        verified
                            .instruction_at(scan_pc)
                            .map(|insn| insn.next_pc as usize)
                    });
                let Some(next) = next else {
                    no!("invalid-switch-payload")
                };
                scan_pc = next;
                continue;
            }
            0xaa | 0xab => no!("tableswitch/lookupswitch"),
            // `new` — refused for the single-pass emitter, which has no arm for
            // it; admitted (and resolved) for the IR builder, which lowers it
            // to `Op::New` for escape analysis to delete.
            0xbb if ir_mode => {
                if scan_pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                ir_new_sites.push((scan_pc, cp_idx));
                scan_pc += 3;
                continue;
            }
            // W10-6: `anewarray`, for the IR builder under its switch.
            0xbd if ir_anewarray_admitted => {
                if scan_pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                ir_anewarray_sites.push((scan_pc, cp_idx));
                scan_pc += 3;
                continue;
            }
            0xbb | 0xbd | 0xc5 => no!("new/anewarray/multianewarray"),
            // L1W1-1 (`CRATONVM_JIT_SP_SPLICE_NEWARRAY`, default ON; `=0`
            // refuses in single-pass): `newarray` (`0xbc`).
            0xbc if ir_mode || gates.sp_splice_newarray_enabled => {
                if scan_pc + 1 >= code_len {
                    return None;
                }
                has_newarray = true;
                scan_pc += 2;
                continue;
            }
            0xbc => no!("newarray"),
            // A masked cold throw arm's `athrow` (see `masked_code` above).
            0xbf if cold_athrow_pcs.contains(&scan_pc) => {
                scan_pc += 1;
                continue;
            }
            // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: any other `athrow` under
            // a top-level site in a `try`; the builder takes it by a spliced
            // throw edge or refuses the splice, which keeps the call.
            0xbf if ir_mode && cratonvm_jit::ir::ir_root_site_in_try() => {
                scan_pc += 1;
                continue;
            }
            0xbf => no!("athrow"),
            // `checkcast` / `instanceof`. Refused for both tiers until
            // 2026-09-09, and for the optimizing tier the refusal was the same
            // shape as `ir-splice-static-field`'s: `IrBuilder` has had both
            // arms since cov-05, keyed by pc off `checkcast_info` /
            // `instanceof_info`, and nothing rebased a spliced body's rows into
            // them. The survey that motivated those arms counted 306 events on
            // this pair -- the largest single whole-method refusal, more than
            // every opcode gap combined -- because every typed read out of an
            // untyped container is a `checkcast`.
            //
            // Unlike `getstatic`, the rows are RESOLVED here rather than
            // already carried: `InlineSite` had no typecheck field, and the
            // target class can only be named through the CALLEE's constant
            // pool. See the resolution below, which refuses the body when a
            // target is not loaded rather than admitting it with rows missing:
            // a missing row bails the whole METHOD, so the caller would lose
            // its optimizing compile over a callee it merely wanted inlined.
            //
            // The single-pass emitter has the same rows since 2026-10-27
            // (`CRATONVM_JIT_SP_SPLICE_TYPECHECK`): its splice emits the
            // top-level arm's sequence at each, so the refusal below is now
            // that switch's `=0`.
            0xc0 | 0xc1 if ir_mode || gates.sp_splice_typecheck_enabled => {
                if ir_mode && !gates.ir_splice_typecheck_enabled {
                    no!("ir-splice-typecheck");
                }
                if scan_pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                ir_typecheck_sites.push((scan_pc, cp_idx, code[scan_pc] == 0xc0));
                scan_pc += 3;
                continue;
            }
            0xc0 | 0xc1 => no!("checkcast/instanceof"),
            0xc2 | 0xc3 => no!("monitorenter/monitorexit"),
            // A relocated body's control flow would need the merge/loop-header
            // bookkeeping `IrBuilder` computes over the CALLER's code alone.
            // Straight-line only, v1.
            // `jsr` / `ret` / `jsr_w` — subroutines. Refused unconditionally
            // and separately from the ordinary branches below: `IrBuilder` has
            // no lowering for a return address, and the verifier's CFG for one
            // is not the shape `normally_reachable_pcs` walks. No JDK-9+
            // compiler emits them.
            0xa8 | 0xa9 | 0xc9 if ir_mode => no!("ir-splice-subroutine"),
            // Ordinary intra-body control flow — every `if`, `goto` and
            // `goto_w`. Refused until 2026-09-09 with the note "straight-line
            // only, v1", and the missing piece was one pre-scan: the builder
            // ran the verifier's CFG analysis over the CALLER's bytecode alone,
            // so a relocated body's merge targets and loop headers were invisible
            // to it. `IrBuilder::build` now runs the same analysis over each
            // spliced body and rebases the result — see its `ir_splice_branch_enabled`
            // block, which reads the SAME switch this arm does.
            //
            // The cost of the refusal was not marginal: a callee with an `if`
            // is most callees. On a four-callee probe it refused the one
            // remaining body after the `ldc` refusal was lifted.
            //
            // Still refused, by the check further down and independently of
            // this: a body with more than one `return`, or a `return` that is
            // not its last instruction. Branching bodies that funnel to a
            // single trailing return — a ternary, an accumulate-then-return, a
            // loop — are what this admits.
            0x99..=0xa7 | 0xc6 | 0xc7 | 0xc8
                if ir_mode && !gates.ir_splice_branch_enabled =>
            {
                no!("ir-splice-branch")
            }
            // A division's zero-divisor guard inside a spliced region raises
            // the builder's `splice_guard_seen`, which refuses the whole build;
            // so a division whose divisor may be zero stays out.
            //
            // Round nio21 wave 10 (lane L2): a division by a CONSTANT other
            // than 0 owes no guard -- `IrBuilder::add_div_zero_guard` builds
            // none for an `Op::Const` divisor (-1 and MIN included; the
            // lowering answers both without a trap) -- so it is admitted when
            // the instruction immediately before it pushes that constant and
            // no branch lands on the division (checked after the scan: a join
            // there would make the divisor a phi). An `ldc` divisor's value is
            // checked once the constant pool is in hand, below.
            // `CRATONVM_JIT_IR_SPLICE_CONST_DIVISION=0` refuses every one again.
            //
            // A reviewed body (`cratonvm_jit::splice_division_by_variable_admitted`:
            // the `Math.floorDiv` / `floorMod` JDK bodies, W9L2-3) is admitted
            // with ANY divisor: the builder lets its guard stand inside the
            // splice (`CRATONVM_JIT_IR_SPLICE_DIV_GUARD`), resuming in the
            // callee's own frame so the interpreter raises `/ by zero`.
            0x6c | 0x6d | 0x70 | 0x71 if ir_mode => {
                if !cratonvm_jit::splice_division_by_variable_admitted(
                    callee_class,
                    callee_method,
                    callee_desc,
                ) {
                    if !gates.ir_splice_const_division_enabled {
                        no!("ir-splice-division");
                    }
                    ir_const_div_pcs.push(scan_pc);
                } else if crate::runtime::env_cache::dbg_jitc() {
                    // JIT round 20 wave 11 (lane pSplice, W10L2-1): the
                    // engagement line of a division admitted with whatever
                    // divisor (a reviewed row, or every body under
                    // `CRATONVM_JIT_IR_PSPLICE_VARIABLE_DIVISOR`).
                    eprintln!(
                        "[cratonvm-jitc] inline-resolve variable-divisor {}.{}{} depth={} pc={} any-body={}",
                        callee_class,
                        callee_method,
                        callee_desc,
                        nest_depth,
                        scan_pc,
                        cratonvm_jit::splice_division_by_variable_any_body_admitted()
                    );
                }
                scan_pc += 1;
                continue;
            }
            // `ldc` / `ldc_w` / `ldc2_w` were refused here until 2026-09-09,
            // and the reason was plumbing rather than modelling: `InlineSite`
            // recorded a raw `i64` and dropped the float/double tag the builder
            // needs to choose between `Op::Const` and `Op::ConstF`. The tag now
            // rides along in `InlineSite::ldc_fp_pcs` and
            // `append_ir_inline_site` rebases both tables into
            // `IrInlineTables`, so the builder's own `0x12 | 0x13` and `0x14`
            // arms resolve a spliced constant exactly as they resolve one in
            // the caller's own code.
            //
            // The refusal was expensive out of all proportion to its cause: a
            // constant wider than `sipush` is ordinary Java, and on a
            // four-callee probe this term alone refused two of the four. What
            // is still refused is what the RESOLVER refuses — an `ldc` naming a
            // String, a Class, a MethodHandle, a MethodType or a condy site,
            // each of which has resolution side effects (interning, class
            // loading, `<clinit>`) that a spliced immediate would skip.
            0x12 | 0x13 | 0x14 if ir_mode && !gates.ir_splice_ldc_enabled => no!("ir-splice-ldc"),
            0xac..=0xb1 if ir_mode => {
                ir_return_pcs.push(scan_pc);
                scan_pc += 1;
                continue;
            }
            // Array element access and `arraylength` are refused below because
            // `try_emit_inline_body` bails on them. `IrBuilder` lowers all three
            // with their own bounds checks, and `Short2` keeps its two shorts in
            // a `short[2]`, so a splice that could not read one would stop at
            // the accessor it exists to delete.
            0x2e..=0x35 | 0x4f..=0x56 | 0xbe if ir_mode => {
                scan_pc += 1;
                continue;
            }
            // `getstatic` was refused here until 2026-09-09, with the note
            // "no `static_field_info` is rebased into the builder's tables, so
            // a spliced `getstatic` would find no row and bail the whole
            // method AFTER the walk had committed to the body". That was an
            // accurate description of the plumbing and not of any modelling
            // problem: `static_field_info` below has resolved these rows for
            // the single-pass inliner since it existed, and
            // `append_ir_inline_site` now rebases them into
            // `IrInlineTables::static_field_info`, which the builder's own
            // `0xb2` arm reads exactly as it reads a caller's site.
            //
            // The refusal was expensive out of proportion to its cause, in the
            // same way the `ldc` one was: `getstatic` is the single largest
            // opcode in the ir-coverage survey (92 of 273 events), because a
            // static-table read behind an accessor is what framework code is
            // mostly made of.
            //
            // `putstatic` (0xb3) stays refused inside a splice. Since round 11
            // wave 10 the builder lowers a top-level `putstatic` to
            // `Op::StoreStatic`, which calls the `jit_putstatic_*` helpers (they
            // own the SATB pre-barrier a static reference write needs — statics
            // are a Rust-side table, not the heap), but its arm bails inside a
            // splice, so admitting the callee here would only fail the build.
            // Round 14 wave 2 (lane syncsplice): a `static synchronized`
            // body may write its OWN class's statics (checked below, once the
            // row resolves): nothing in its splice can trap, so no replay
            // re-runs the write, and the class is initialised (above).
            //
            // JIT round 21 (lane staticfwd, FG16-3): under
            // `CRATONVM_JIT_IR_SPLICE_PUTSTATIC` (default ON) any body may
            // carry a plain `putstatic`; the row loop below admits only a
            // non-volatile field of a class initialised now, and the builder
            // commits the write behind the replay fence.
            0xb3 if ir_mode
                && !(sync_splice && is_static)
                && !gates.ir_splice_putstatic_enabled =>
            {
                no!("ir-splice-putstatic")
            }
            0xb3 if ir_mode => {
                has_static_field_ops = true;
                scan_pc += 3;
                continue;
            }
            //
            // Handled here rather than by falling through to the ordinary
            // `0xb2 | 0xb3` arm below, because that arm is guarded by
            // `inline_no_static` — `CRATONVM_INLINE_ALLOW_STATIC`, which is
            // OFF by default. That switch belongs to the single-pass inline
            // mini-emitter (`x64/inlining.rs`), which materialises a static
            // read as a baked address and is the reason the gate exists. The
            // optimizing tier does not go through that emitter at all: its
            // `0xb2` arm builds an `Op::LoadStatic` whose lowering picks
            // between the direct load and `helpers.getstatic` on the
            // resolver's own already-initialised answer. Falling through would
            // have made this feature a no-op under its own default and left
            // the refusal UNNAMED, which is the failure the `no!` macro at the
            // top of this function exists to prevent.
            0xb2 if ir_mode => {
                if !gates.ir_splice_getstatic_enabled {
                    no!("ir-splice-static-field");
                }
                has_static_field_ops = true;
                scan_pc += 3;
                continue;
            }
            // invokevirtual / invokestatic / invokeinterface inside the
            // spliced body. These used to reject the site outright — the
            // emitter had no arm for them and, more fundamentally, nothing
            // resolved them: every other piece of callee metadata is keyed by
            // callee pc against the CALLEE's constant pool, and invokes had no
            // such entry. Both halves exist now (`invoke_targets` here,
            // `try_emit_inline_body`'s invoke arm there), so record the site
            // and resolve it below.
            //
            // `invokeinterface` is FIVE bytes (cp_idx, count, 0); the other two
            // are three. Advancing by the wrong width would desync the scan and
            // read operands as opcodes, which is the same class of bug the
            // `wide`-aware length below exists to prevent.
            0xb6 | 0xb8 | 0xb9 => {
                if !inline_calls_allowed {
                    return None;
                }
                let width = if code[scan_pc] == 0xb9 { 5 } else { 3 };
                if scan_pc + width > code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                invoke_sites.push((scan_pc, cp_idx, code[scan_pc]));
                scan_pc += width;
                continue;
            }
            0xb7 => {
                // invokespecial — defer: elidable no-op super-ctor calls are
                // allowed (validated below), everything else rejects.
                if scan_pc + 2 >= code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                special_sites.push((scan_pc, cp_idx));
                scan_pc += 3;
                continue;
            }
            // nio21 wave 11, lane L3 (W10L3-1, `CRATONVM_JIT_IR_SPLICE_INDY`):
            // the optimizing tier splices a body whose `invokedynamic`s are
            // constant-word lambda sites (resolved below, after the scan: no
            // arguments, a reference result, linked, singleton published).
            // Not inside a synchronized splice window. Every other shape, and
            // the single-pass inliner, keep the refusal.
            0xba if ir_mode && !sync_splice && gates.ir_splice_indy_enabled => {
                if scan_pc + 5 > code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                ir_indy_sites.push((scan_pc, cp_idx));
                scan_pc += 5;
                continue;
            }
            // JIT round 20 wave 19, lane ySplice (SPLICE20-7,
            // `CRATONVM_JIT_SP_SPLICE_INDY_CONSTANT`): the single-pass tier
            // splices the same constant-word lambda sites (resolved below to
            // the same rows; `x64::inlining`'s `0xba` arm emits the probe).
            0xba if !ir_mode
                && !sync_splice
                && gates.sp_splice_indy_constant_enabled =>
            {
                if scan_pc + 5 > code_len {
                    return None;
                }
                let cp_idx = ((code[scan_pc + 1] as u16) << 8) | code[scan_pc + 2] as u16; // Cast: bytecode operand decoding
                ir_indy_sites.push((scan_pc, cp_idx));
                scan_pc += 5;
                continue;
            }
            0xba => {
                // JIT round 19 wave 2 (lane splice2, SPLICE19-2 census):
                // which refused bodies a nested indy plan would admit.
                if ir_mode && gates.ir_indy_splice_census_enabled {
                    ir_indy_splice_census_line(
                        shared,
                        &cm,
                        requesting_class_id,
                        declaring_id,
                        callee_class,
                        callee_method,
                        callee_desc,
                        nest_depth,
                        code,
                    );
                }
                no!("invokedynamic")
            }
            // Array loads/stores + arraylength need a bounds check (and AIOOBE
            // path) that the inline codegen (`x64::try_emit_inline_body`) does
            // NOT emit — it bails on these. Rejecting them HERE keeps the
            // resolver and codegen consistent: a method that would only bail
            // mid-inline instead stays on the cheaper direct-call path rather
            // than being planned, rolled back, and downgraded to the
            // dispatch-helper fallback. (Array-load inlining is a follow-up.)
            0x2e..=0x35 => no!("array-load"),
            0x4f..=0x56 => no!("array-store"),
            0xbe => no!("arraylength"),
            0xb4 | 0xb5 => {
                has_field_ops = true;
                scan_pc += 3;
                continue;
            }
            0xb2 | 0xb3 => {
                if inline_no_static {
                    return None;
                }
                has_static_field_ops = true;
                scan_pc += 3;
                continue;
            }
            0x12 => {
                has_ldc = true;
                scan_pc += 2;
                continue;
            }
            0x13 => {
                has_ldc = true;
                scan_pc += 3;
                continue;
            }
            0x14 => {
                has_ldc2w = true;
                scan_pc += 3;
                continue;
            }
            _ => {}
        }
        // `wide`-aware length so the walk stays in sync over a `wide iinc`
        // (6 bytes) — the bare opcode-length table treats every `wide` form as
        // 4 bytes, which would desync the scan and mis-read the widened
        // operands as opcodes (finding 5).
        scan_pc += inline_instr_length(code, scan_pc);
    }

    // The relocated body is walked from its first byte through to `code_len`
    // with the caller's frame parked in `SpliceFrame`, so the LAST instruction
    // must be a `return` — otherwise the walk runs off the end of the body into
    // whatever `lib.rs` appended next.
    //
    // More than one return was refused outright until 2026-09-09, on the
    // grounds that the walk leaves the splice at the first one and would never
    // reach the code after it. It no longer leaves: `IrBuilder::splice_return`
    // turns each return into an edge into a continuation built at the body's
    // end (`finish_multi_return_splice`), which is the ordinary
    // several-predecessors join the builder already performs for a caller's own
    // branches. Lifted by `ir_splice_multi_return_enabled`: default ON since
    // round 16 through the chain arm (`CRATONVM_JIT_IR_CHAIN_ARM`);
    // `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=0` keeps the refusal. The builder recognises a
    // multi-return body from its own decode (round 11 wave 11), so this is the
    // only half that decides.
    //
    // Round 11 wave 11 (lane `calls`): one narrower admission, under
    // `ir_recursive_inline_enabled` (default on). A body that CALLS ITSELF is
    // admitted with several returns, because that is the base case of every
    // recursive method and the shape where splicing pays (a whole frame per
    // copy). Decided below, once the body's invoke targets are resolved; and
    // `lib.rs`'s `append_ir_inline_site` then splices such a body only as a
    // bounded copy of the method being compiled, never into another caller.
    //
    // Every `return` opcode is one byte, so "last instruction" is exactly
    // `last + 1 == code_len` with no length table needed.
    let multi_return_ok = ir_mode && gates.ir_splice_multi_return_enabled;
    // Round 14 wave 5 (lane sync5, SS8-1 b): a synchronized body -- answered
    // only at depth 0 in IR mode (`sync_splice` above) -- may keep several
    // returns: the planner splices it only as a synchronized splice whose scan
    // admits them (`ir_sync_splice_body_scan`), and the builder closes the
    // monitor window after the returns' join.
    let multi_return_ok = multi_return_ok
        || (ir_mode && sync_splice && gates.ir_sync_splice_multi_return_enabled);
    let multi_return_if_self_recursive = ir_mode
        && !multi_return_ok
        && ir_return_pcs.len() > 1
        && gates.ir_splice_branch_enabled
        && gates.ir_recursive_inline_enabled;
    // Internal returns join after the trailing latch, even with one exit.
    let trailing_back_goto = ir_mode
        && multi_return_ok
        && !ir_return_pcs.is_empty()
        && cratonvm_jit::ir::splice_ends_in_backward_goto(code, code_len);
    // A masked TAIL throw arm (`cratonvm_jit::ir::COLD_THROW_TAIL_ARM`:
    // `if (x != null) return x; throw ...`) may follow the last return: the
    // walk leaves the body at that return, and the arm's only ways in are the
    // side entries the builder turns into guards. Its first pc, when it runs
    // to the end of the body.
    let trailing_tail_arm_start = (ir_mode && !cold_athrow_pcs.is_empty())
        .then(|| cratonvm_jit::ir::cold_throw_arms(code, code_len))
        .flatten()
        .and_then(|arms| {
            arms.into_iter()
                .find(|a| {
                    a.branch_pc == cratonvm_jit::ir::COLD_THROW_TAIL_ARM
                        && a.athrow_pc + 1 == code_len
                })
                .map(|a| a.start)
        });
    // j27 wave 5 (lane DECLOOP, `CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER`): or a
    // masked handler run that ends the body (`ByteBuffer.wrap([BII)`'s
    // `catch (IllegalArgumentException) { throw new IOOBE(); }`); the builder
    // walks past the return into its trap
    // (`cratonvm_jit::ir::splice_tail_handler_follows_return`).
    let trailing_masked_handler = ir_mode
        && ir_return_pcs.last().is_some_and(|&last| {
            cratonvm_jit::ir::splice_tail_handler_follows_return(
                code,
                code_len,
                last,
                handler_rows.iter().map(|r| r.2),
            )
        });
    let trailing_pruned_dead = ir_mode
        && early_live_pcs.as_ref().is_some_and(|live| {
            ir_return_pcs.last().is_some_and(|&last| {
                (last + 1..code_len).all(|p| !live.get(p).copied().unwrap_or(false))
            })
        });
    if ir_mode
        && (ir_return_pcs.is_empty()
            || (ir_return_pcs[ir_return_pcs.len() - 1] + 1 != code_len
                && !trailing_back_goto
                && !trailing_masked_handler
                && !trailing_pruned_dead
                && Some(ir_return_pcs[ir_return_pcs.len() - 1] + 1) != trailing_tail_arm_start)
            || (!multi_return_ok && !multi_return_if_self_recursive && ir_return_pcs.len() != 1))
    {
        no!("ir-splice-not-single-trailing-return");
    }
    // Round 14 wave 6 (lane sync6, SS8-1 a): a synchronized body answered at
    // depth 1 is spliced only as a call-free, single-return same-receiver body
    // (`cratonvm_jit` `ir_sync_splice_body_scan`); refuse any other here,
    // before its calls are resolved, nested or direct-bound for nothing.
    if sync_splice
        && nest_depth > 0
        && (!invoke_sites.is_empty() || !special_sites.is_empty() || ir_return_pcs.len() != 1)
    {
        no!("synchronized-nested-body-shape");
    }

    // Resolve the body's `new` sites against the CALLEE's constant pool. A
    // `Deferred` site — the target class not loaded from this holder's loader
    // yet — refuses the whole splice rather than being dropped: the builder's
    // `0xbb` arm bails the METHOD on a missing row, so admitting the body
    // without the row would cost the caller its IR compile entirely.
    let mut ir_new_info: Vec<(usize, u32, usize, bool)> = Vec::new();
    // W10-6: the `anewarray` rows. An unresolved component class refuses the
    // body: the builder would bail the whole METHOD at a spliced `anewarray`
    // with no row. An `anewarray` initialises nothing (JVMS 6.5: it only
    // resolves its component class), so no class-init or finalizer rule of
    // the `new` rows applies.
    let mut ir_anewarray_info: Vec<(usize, u32)> = Vec::new();
    // JIT round 25 wave 13 (W13-6): a callee that is a loop root keeps its
    // call -- and its own optimizing-tier compile, with the whole splice
    // budget for its in-loop sites -- unless
    // `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES` restores wave 10's
    // admission. See `cratonvm_jit::ir::splice_anewarray_refuses_loop_callee`.
    if let Some(edges) = cratonvm_jit::ir::splice_anewarray_refuses_loop_callee(
        code,
        code_len,
        !ir_anewarray_sites.is_empty(),
    ) {
        no!(format!(
            "ir-splice-anewarray-loop-callee ({} anewarray site(s), {edges} back edge(s); \
             kept as a call, compiled as its own root)",
            ir_anewarray_sites.len()
        ));
    }
    for &(apc, cp_idx) in &ir_anewarray_sites {
        match resolve_jit_new_site(shared, &cm, declaring_id, cp_idx) {
            Some(cratonvm_jit::JitNewSite::Resolved { class_id, .. }) => {
                // ClassId(0) is a real component (java/lang/Object), not a
                // missing row. Deferred sites remain refused below.
                ir_anewarray_info.push((apc, class_id));
            }
            _ => no!("ir-splice-anewarray-unresolved"),
        }
    }
    // SPLICE20-2 (lane ySplice): `(callee pc, class name)` of the held
    // unloaded `new`s, decided after `drop(cm)`.
    let mut cold_unresolved_news: Vec<(usize, String)> = Vec::new();
    let unresolved_new_exit_admitted = ir_mode
        && !sync_splice
        && gates.ir_splice_unresolved_new_exit_enabled
        && gates.ir_site_trap_enabled;
    if ir_mode && !ir_new_sites.is_empty() {
        for &(npc, cp_idx) in &ir_new_sites {
            match resolve_jit_new_site(shared, &cm, declaring_id, cp_idx) {
                // A spliced `new` is lowered like the caller's own: an inline
                // bump with no class-init check (wave 4, L7).
                Some(cratonvm_jit::JitNewSite::Resolved { class_id, .. })
                    if jit_new_target_init_pending(shared, &cm, ClassId::new(class_id)) =>
                {
                    no!("ir-splice-new-site-class-init-pending")
                }
                Some(cratonvm_jit::JitNewSite::Resolved {
                    class_id,
                    num_fields,
                    has_finalizer,
                    ..
                }) => {
                    // The IR tier's `Op::New` (inline TLAB bump or scalar
                    // replacement) runs no `<clinit>`; a spliced body must not
                    // carry a `new` of a class that has not been initialised.
                    //
                    // al16 (2026-10-05): ...unless initialising it is
                    // unobservable. The arm above already refused a target
                    // that is not SETTLED (`jit_new_target_init_pending`: no
                    // `<clinit>` anywhere in the chain `new` initialises, no
                    // erroneous or unlinked class), which is the rule the
                    // caller's own resolved `new` rows compile under. A
                    // settled class that nothing has instantiated yet --
                    // `ArrayList$Itr` while an OSR compile runs in the loop
                    // that FILLS a list, before the for-each that iterates it
                    // ever ran -- was refused here, and with `iterator()` the
                    // whole for-each stayed calls. `CRATONVM_JIT_IR_SPLICE_NEW_SETTLED_CLASS=0`
                    // restores the stricter rule.
                    if !cm
                        .get_class(ClassId::new(class_id))
                        .is_some_and(crate::vm::is_class_initialized_fast)
                        && false
                    {
                        no!("ir-splice-new-site-class-not-initialized");
                    }
                    // `ir_new_info` carries no finalizer flag, and a replay of
                    // the splice (a deopt re-runs it from the outer invoke)
                    // would leave a finalizable object the program never made
                    // -- `finalize()` would run on it (r11-fib F15-1). A hot
                    // allocation of a finalizable class is rare: refuse.
                    //
                    // al16: `jit_new_site_flags` answers `has_finalizer` for
                    // every class not initialised yet (a load-bearing `true`
                    // that keeps the caller's own `new` rows on the
                    // initialising helper). A settled class admitted above has
                    // its real answer read here instead.
                    let has_finalizer = if cm
                        .get_class(ClassId::new(class_id))
                        .is_some_and(crate::vm::is_class_initialized_fast)
                    {
                        has_finalizer
                    } else {
                        cm.get_class(ClassId::new(class_id))
                            .is_none_or(|c| c.has_finalizer)
                            || crate::memory::ref_discovery::reference_kind_in(
                                &cm.class_store,
                                ClassId::new(class_id),
                            )
                            .is_some()
                    };
                    if has_finalizer
                    {
                        no!("ir-splice-new-site-finalizable");
                    }
                    // JIT round 20 wave 12 (lane hFin): the row carries the
                    // answer, so the builder's graph keeps such an allocation
                    // off scalar replacement (`ir::Graph::finalizable_new_classes`)
                    // even when the switch above is `0` and the body is spliced.
                    ir_new_info.push((npc, class_id, num_fields, has_finalizer))
                }
                // JIT round 20 wave 19, lane ySplice (SPLICE20-2,
                // `CRATONVM_JIT_IR_SPLICE_UNRESOLVED_NEW_EXIT`): a class not
                // loaded from this holder's loader yet is HELD here and, once
                // `cm` is dropped, admitted only behind a branch edge the
                // callee's own profile says never ran (the unloaded type
                // check's rule, `cold_unloaded_typechecks` below). Its row
                // then carries class id 0, which the builder's `0xbb` arm
                // turns into a precise exit at this `new` (the interpreter
                // resolves, loads and initializes there). Never inside a
                // synchronized window (the exit would resume inside it).
                Some(cratonvm_jit::JitNewSite::Deferred { .. }) if unresolved_new_exit_admitted => {
                    match cm
                        .get_class(declaring_id)
                        .and_then(|c| c.constant_pool.get_class_name(cp_idx))
                    {
                        Some(name) => cold_unresolved_news.push((npc, name.to_string())),
                        None => no!("ir-splice-new-site-unresolved"),
                    }
                }
                _ => no!("ir-splice-new-site-unresolved"),
            }
        }
    }

    // nio21 wave 11, lane L3 (W10L3-1): each `invokedynamic` of the body must
    // be a CONSTANT-WORD lambda site, or the whole splice is refused (the
    // builder refuses a spliced indy without a row, which would cost the
    // caller its compile). The site is the one the compile door interns for
    // the instruction -- keyed by the CALLEE's own method and bci, exactly
    // the key the interpreter links and mints the singleton under -- and the
    // VM is asked to learn its slot now (`prime_jit_indy_constant_word`,
    // tables only: no heap read, so the background worker may ask). A site
    // whose singleton nothing has minted yet (the callee never ran the
    // instruction) is refused; its next compile asks again.
    let mut ir_indy_rows: Vec<(usize, usize, u8, usize)> = Vec::new();
    // JIT round 20 wave 19, lane ySplice (SPLICE20-1,
    // `CRATONVM_JIT_IR_SPLICE_INDY_CAPTURE`): the body's linked CAPTURING
    // lambda sites, `(callee pc, proxy class id, capture tags)`, built by the
    // builder as the proxy's `new` plus one store per capture
    // (`IrBuilder::try_indy_capture_new`, the method-own sites' path).
    let mut ir_indy_capture_rows: Vec<(usize, u32, Vec<u8>)> = Vec::new();
    // JIT round 21 wave 15, lane lambda3 (LAMBDACAP21-2): each capture row's
    // bridge site, `(callee pc, site)`, for the compile door's impl question.
    let mut ir_indy_capture_sites: Vec<(usize, usize)> = Vec::new();
    // `ir_indy_sites` is filled only where the scan admitted the opcode: the
    // optimizing tier's arm, or (SPLICE20-7) the single-pass tier's.
    if !ir_indy_sites.is_empty() {
        let Some(indy_class) = cm.get_class(declaring_id) else {
            no!("ir-splice-indy-class-unavailable");
        };
        for &(ipc, cp_idx) in &ir_indy_sites {
            let descriptor = match indy_class.constant_pool.get(cp_idx) {
                Some(ConstantPoolEntry::InvokeDynamic {
                    name_and_type_index,
                    ..
                }) => indy_class
                    .constant_pool
                    .get_name_and_type(*name_and_type_index)
                    .map(|(_, d)| d),
                _ => None,
            };
            let Some(descriptor) = descriptor else {
                no!("ir-splice-indy-unresolved");
            };
            if !descriptor.starts_with("()L") {
                // j27 wave 4 (lane SPLICECAT, `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE`):
                // a concat bridge site of any arity, the site the callee's own
                // compiled body calls, in both tiers.
                if let Some(row) = splice_concat_rows::splice_concat_row(
                    shared,
                    &cm,
                    indy_class,
                    declaring_id,
                    cp_idx,
                    descriptor,
                    (callee_method, callee_desc, ipc),
                ) {
                    ir_indy_rows.push(row);
                    continue;
                }
                // SPLICE20-1: a capturing site whose proxy the VM names now
                // (linked, a VM-spun proxy, one capture per argument), in an
                // optimizing splice only. Every other shape keeps the refusal.
                let capture_row = if ir_mode && gates.ir_splice_indy_capture_enabled {
                    let tags = cratonvm_jit::indy_arg_type_tags(descriptor);
                    let site = if cratonvm_jit::return_type(descriptor) == b'L'
                        && !tags.is_empty()
                        && tags.len() == count_method_params(descriptor)
                    {
                        interned_jit_indy_bridge_site(
                            shared,
                            &cm,
                            indy_class,
                            declaring_id,
                            cp_idx,
                            Some((callee_method, callee_desc, ipc)),
                        )
                    } else {
                        0
                    };
                    (site != 0)
                        .then(|| {
                            crate::runtime::invokedynamic::jit_indy_capture_alloc_class(
                                shared, site,
                            )
                        })
                        .flatten()
                        .filter(|&(proxy, n)| proxy != 0 && n == tags.len())
                        .map(|(proxy, _)| (proxy, tags, site))
                } else {
                    None
                };
                let Some((proxy, tags, site)) = capture_row else {
                    no!("invokedynamic (capturing or non-reference site: no constant word)");
                };
                ir_indy_capture_rows.push((ipc, proxy, tags));
                ir_indy_capture_sites.push((ipc, site));
                continue;
            }
            let site = interned_jit_indy_bridge_site(
                shared,
                &cm,
                indy_class,
                declaring_id,
                cp_idx,
                Some((callee_method, callee_desc, ipc)),
            );
            if site == 0
                || !crate::runtime::invokedynamic::prime_jit_indy_constant_word(shared, site)
            {
                no!("invokedynamic (no-argument site not linked yet: no constant word)");
            }
            ir_indy_rows.push((ipc, 0, b'L', site));
        }
    }

    let Some(callee_class_info) = cm.get_class(declaring_id) else {
        no!("declaring-class-info-unavailable");
    };

    // Resolve the body's `checkcast` / `instanceof` targets against the
    // CALLEE's constant pool -- the only pool that can name them.
    //
    // Admitted only when the target class is already RESOLVED AND LOADED, which
    // is the same bar the caller's own sites are held to and for the same
    // reason: the not-yet-loaded path runs `jit_typecheck_resolve`, which can
    // call a user classloader's `loadClass`, arbitrary Java this tier does not
    // host inside a helper call. `resolve_jit_new_site` answers exactly that
    // question for a `CONSTANT_Class` entry, which is the shape both opcodes
    // take, so this reuses it rather than adding a second resolver; the field
    // count and init flags it also carries are irrelevant here.
    //
    // An unresolved target refuses the whole CALLEE, the same trade as
    // `ir-splice-new-site-unresolved`: a missing row bails the METHOD, so
    // admitting the body without one costs the CALLER its optimizing compile
    // over a callee it merely wanted inlined. Refusing costs the site its
    // inline and nothing else.
    //
    // The builder's `0xc0`/`0xc1` arms appear to be gentler -- a missing row
    // reaches `plant_uncommon_trap`. It is gated off by default
    // (`ir_unresolved_class_trap_enabled`), so the plant refuses and the arm
    // bails; and with it ON the trap fires and, inside a splice, deopts to
    // re-execute the invoke on every call. Neither setting makes admitting an
    // unresolved body the right move.
    //
    // Round nio20 wave 1, lane L1 (NIO19-3): one exception, under
    // `CRATONVM_JIT_IR_SPLICE_COLD_UNLOADED_TYPECHECK` (default on). An
    // unloaded target is held here and, once `cm` is dropped, admitted when
    // the callee's own branch profile proves its instruction sits behind an
    // edge that never ran (`cratonvm_jit::ir::splice_typecheck_arm_unexecuted`)
    // -- `Buffer.session()`'s cast on a heap buffer. Its row carries class id
    // 0, and the builder plants a PRECISE exit there (the chain snapshot at
    // the type check, so the interpreter resolves, loads and checks; nothing
    // re-executes the invoke), not the default-off trap described above.
    let mut ir_typecheck_info: Vec<(usize, u32, String, bool)> = Vec::new();
    // JIT r23 w2c close: the rows whose target is a final, non-interface class,
    // the verdict the compiling method's own sites intern with.
    let mut ir_typecheck_final: Vec<usize> = Vec::new();
    let mut cold_unloaded_typechecks: Vec<(usize, String, bool)> = Vec::new();
    let cold_unloaded_admitted =
        ir_mode && gates.ir_splice_cold_unloaded_typecheck_enabled;
    for &(tpc, cp_idx, is_checkcast) in &ir_typecheck_sites {
        let resolved = resolve_jit_new_site(shared, &cm, declaring_id, cp_idx);
        let Some(name) = callee_class_info.constant_pool.get_class_name(cp_idx) else {
            no!("ir-splice-typecheck-target-unnamed");
        };
        match resolved {
            Some(cratonvm_jit::JitNewSite::Resolved {
                class_id: target_id,
                ..
            }) if target_id != 0 => {
                if jit_class_is_final_for_typecheck_in(shared, &cm, target_id) {
                    ir_typecheck_final.push(tpc);
                }
                ir_typecheck_info.push((tpc, target_id, name.to_string(), is_checkcast));
            }
            _ if cold_unloaded_admitted => {
                cold_unloaded_typechecks.push((tpc, name.to_string(), is_checkcast));
            }
            _ => no!("ir-splice-typecheck-target-not-loaded"),
        }
    }
    for &(tpc, cp_idx, is_checkcast) in &dead_typecheck_sites {
        let Some(name) = callee_class_info.constant_pool.get_class_name(cp_idx) else {
            continue;
        };
        if let Some(cratonvm_jit::JitNewSite::Resolved { class_id: target_id, .. }) =
            resolve_jit_new_site(shared, &cm, declaring_id, cp_idx)
        {
            if target_id != 0 {
                if jit_class_is_final_for_typecheck_in(shared, &cm, target_id) {
                    ir_typecheck_final.push(tpc);
                }
                ir_typecheck_info.push((tpc, target_id, name.to_string(), is_checkcast));
            }
        }
    }

    // Validate the deferred invokespecial sites: every one must be a
    // resolver-PROVEN no-op super-constructor call, or the whole callee is
    // rejected. Proven means the target is `java/lang/Object.<init>()V`
    // directly, or a `<init>()V` whose body is exactly
    // `aload_0; invokespecial Object.<init>; return`
    // (`is_elidable_construction` — the same predicate the elidable-ctor
    // call-site rewrite uses). The surviving PCs are recorded so the inline
    // body emitter pops the receiver and emits nothing at those sites.
    let mut elided_invoke_pcs: Vec<usize> = Vec::new();
    for &(spc, cp_idx) in &special_sites {
        let (ref_class_idx, nat_idx) = match callee_class_info.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let target_class = callee_class_info
            .constant_pool
            .get_class_name(ref_class_idx)?;
        let (target_name, target_desc) =
            callee_class_info.constant_pool.get_name_and_type(nat_idx)?;
        // An invokespecial that is NOT a provable no-op super-constructor call
        // used to reject the whole site, because the emitter's only 0xb7 arm is
        // the elision. With call splicing on there is a second arm — the
        // dispatch helper — so route it there instead of losing the site.
        let is_noop_ctor_shape = target_name == "<init>" && target_desc == "()V";
        let elidable = is_noop_ctor_shape
            && (target_class == "java/lang/Object" || {
                match jit_known_class_at(
                    shared,
                    &cm,
                    target_class,
                    declaring_id,
                    Some(ref_class_idx),
                ) {
                    Some(tid) => is_elidable_construction(shared, &cm, tid),
                    None => false,
                }
            });
        if !elidable {
            if !inline_calls_allowed {
                return None;
            }
            // The spliced call keeps its constant-pool class (kind 1, no
            // owner substitution, bound on the body's behalf by
            // `bind_spliced_static_calls`): refused where the interpreter
            // selects another body or raises a selection error (interpreter
            // round i1 wave 37, lane L2). The callee is then called, through
            // a door that asks the same question.
            // `is_interface_ref: false` is exact, not a default: only a
            // `MethodReference` reaches here (the match above refuses the
            // whole site for an `InterfaceMethodReference`, the one form an
            // `invokespecial` of an interface method can take).
            let refusal = if gates.invoke_site_classifier_enabled {
                match classify_invoke_site(
                    shared,
                    &cm,
                    declaring_id,
                    target_class,
                    false,
                    target_name,
                    target_desc,
                ) {
                    SiteBinding::ConstantPoolClass => None,
                    SiteBinding::Owner(owner, owner_id) => {
                        if crate::runtime::env_cache::dbg_jitc() {
                            eprintln!(
                                "[cratonvm-jitc] invokespecial-selection SUBSTITUTED splice: \
                                 {target_class}.{target_name}{target_desc} at callee pc {spc} \
                                 binds {owner}"
                            );
                        }
                        special_owners.push((spc, owner.to_string(), owner_id.as_u32()));
                        None
                    }
                    SiteBinding::Refused(why) => Some(why),
                }
            } else {
                special_site_bind_refusal(
                    shared,
                    &cm,
                    declaring_id,
                    target_class,
                    false,
                    target_name,
                    target_desc,
                    true,
                )
            };
            if let Some(why) = refusal {
                no!(format!(
                    "{why} (invokespecial {target_class}.{target_name}{target_desc} at callee pc {spc})"
                ));
            }
            invoke_sites.push((spc, cp_idx, 0xb7));
            continue;
        }
        elided_invoke_pcs.push(spc);
    }

    // Resolve the callee's own calls against the CALLEE's constant pool.
    //
    // Everything needed is textual — the name triple, the argument count and
    // the return byte — so this stays inside the `cm` guard with the field and
    // ldc scans; unlike `resolve_field_ref` it re-locks nothing. What it does
    // NOT do is find a body: `jit_invoke_dispatch` resolves the target at run
    // time from exactly these four fields, which is what makes a spliced call
    // behave identically to the same call in an un-spliced body (same handler,
    // same `<clinit>` barrier, same loader identity via `declaring_class_id`).
    //
    // `declaring_class_id` is the CALLEE's declaring class, not the enclosing
    // method's: a class NAME is not a class identity
    // (BUG-JIT-INVOKESPECIAL-LOADER-20260726), and the pool that named this
    // target belongs to the callee. Handing the dispatcher the enclosing
    // method's id would resolve a two-loader duplicate through the wrong copy.
    //
    // A site with even ONE unresolvable invoke is refused whole — the same
    // contract `ldc` follows ("the callee still compiles and is still CALLED,
    // it just is not spliced"), and the only alternative would be to splice a
    // body with a hole in it.
    let mut invoke_targets: Vec<(usize, cratonvm_jit::InlineInvokeTarget)> = Vec::new();
    // JIT round 24 B3: set when an optimizing splice admits a body with a
    // `MemorySegment` element access (see the refusal it replaces below).
    let mut ffm_element_site = false;
    let mut unloaded_static_call_pcs = Vec::new();
    // nio21 wave 6, lane L4: `(callee pc, callee's declaring class)` of every
    // static call to a loaded, plain-bytecode callee, for the never-run exit
    // applied after the binds (`splice_never_run_static_exit_enabled`).
    let never_run_static_exit = ir_mode && gates.splice_never_run_static_exit_enabled;
    // JIT round 21 wave 16 (lane jit-selfcopy): fill each static row's
    // resolved declaring class (`InlineInvokeTarget::resolved_class_id`) for
    // the self-copy admission's id compare. Opt-in, read once per resolution.
    let self_copy_by_id = ir_mode && gates.ir_self_copy_by_id_enabled;
    let mut never_run_static_candidates: Vec<(usize, ClassId)> = Vec::new();
    // JIT round 16 (lane callbind): the `invokevirtual`s of this body whose
    // resolved method is unoverridable (`invokevirtual_site_final_owner`, the
    // top-level door's rule, native-shadow screen included), as
    // `(callee pc, declaring class name, its id)`. Bound after `cm` is dropped
    // (`bind_spliced_final_calls`); only in IR mode, under
    // `CRATONVM_JIT_IR_SPLICE_FINAL_DIRECT`.
    let mut final_pins: Vec<(usize, String, u32)> = Vec::new();
    let splice_final_direct = ir_mode && gates.ir_splice_final_direct_enabled;
    // A dead row is skipped where a live one refuses the body.
    macro_rules! no_live {
        ($dead:expr, $why:expr) => {{
            if $dead {
                continue;
            }
            no!($why)
        }};
    }
    let mut dead_invoke_targets: Vec<(usize, cratonvm_jit::InlineInvokeTarget)> = Vec::new();
    for (&(ipc, cp_idx, opcode), dead) in invoke_sites
        .iter()
        .map(|s| (s, false))
        .chain(dead_invoke_sites.iter().map(|s| (s, true)))
    {
        let (ref_class_idx, nat_idx) = match callee_class_info.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let target_class = callee_class_info
            .constant_pool
            .get_class_name(ref_class_idx)?;
        let (target_name, target_desc) =
            callee_class_info.constant_pool.get_name_and_type(nat_idx)?;
        let mut invoke_kind: u8 = match opcode {
            0xb6 => 0,
            0xb7 => 1,
            0xb9 => 2,
            0xb8 => 3,
            _ => return None,
        };
        // JVMS 5.4.6 / §6.5: an `invokevirtual` or `invokeinterface` whose
        // RESOLVED method is private selects exactly that method. A call left
        // at kind 0/2 here is dispatched from the receiver by
        // `jit_invoke_dispatch`, which for a private interface method `I.m`
        // lands on the receiver's unrelated public `m` (and for a private
        // `invokevirtual` on the most-derived same-named private method). Pin
        // it as a direct bind at the declaring class, as every top-level
        // compile door already does. See
        // cha-binds-a-private-interface-method-to-the-receivers-public-namesake-20260918.md.
        //
        // The owner carries its id (`InlineInvokeTarget::owner_class_id`,
        // interpreter round i1 wave 17, L3): like a top-level substituted
        // owner, the dispatch helper then takes the class found here rather
        // than resolving the declaring class's NAME through the body's loader.
        let mut private_owner: Option<(String, u32)> = None;
        if matches!(invoke_kind, 0 | 2) {
            if let Some(cp_class_id) =
                jit_known_class_at(shared, &cm, target_class, declaring_id, Some(ref_class_idx))
            {
                let store = cm.class_store();
                if let Some(owner_id) = crate::classloading::invokevirtual_private_declaring_class(
                    cp_class_id,
                    target_name,
                    target_desc,
                    store,
                ) {
                    // `--real-jdk`: a pinned private INTERFACE method has no
                    // compiled receiver check (wave 37, lane L2).
                    if opcode == 0xb9 && shared.classes.selection_checks {
                        no_live!(dead, format!(
                            "invokeinterface-private-receiver-unchecked \
                             ({target_class}.{target_name}{target_desc} at callee pc {ipc})"
                        ));
                    }
                    private_owner = store
                        .get(owner_id)
                        .map(|c| (c.name.to_string(), owner_id.as_u32()));
                    if private_owner.is_some() {
                        invoke_kind = 1;
                    }
                }
            }
        }
        // JIT round 16 (lane callbind): an `invokevirtual` of a `final` method
        // (or of any method of a `final` class) selects that one body for
        // every receiver, which is why the top-level door binds it directly
        // (`invokevirtual_site_final_owner`). Inside a spliced body the row
        // stayed a virtual call through an inline cache: `HashMap.get`'s
        // `getNode`, whenever its nested splice is pruned back to a call. Only
        // a CANDIDATE here; the row changes only if the bind below succeeds,
        // so no unbindable statically bound row is ever created.
        if splice_final_direct && !dead && opcode == 0xb6 && invoke_kind == 0 {
            if let Some((owner, owner_id)) = super::invoke::invokevirtual_site_final_owner(
                shared,
                &cm,
                declaring_id,
                target_class,
                target_name,
                target_desc,
            ) {
                final_pins.push((ipc, owner, owner_id.as_u32()));
            }
        }
        let invoke_kind = invoke_kind;
        // JIT round 20 (lane qMisc, correctness;
        // `jr20-getclass-name-only-predicate-misses-a-private-getclass-FIXED-20261012.md`):
        // a virtual `getClass()Ljava/lang/Class;` row that does not resolve to
        // `Object.getClass` (a class's own STATIC one: `invokevirtual` raises
        // `IncompatibleClassChangeError`; a private one was pinned to kind 1
        // above) would take the optimizing tier's by-name `getClass()` prefix
        // inside the splice. Keep the callee a call.
        if invoke_kind == 0
            && cratonvm_jit::is_object_get_class_site(target_name, target_desc)
            && !super::invoke::invokevirtual_site_resolves_to_object(
                &cm,
                declaring_id,
                target_class,
                target_name,
                target_desc,
            )
        {
            no_live!(dead, "getclass-site-not-object-getclass");
        }
        // JIT round 20 wave 7 (lane huntDisp): a call the interpreter answers
        // with a linkage error on every execution is not spliced either; the
        // callee is then called, and its own compile leaves it interpreted
        // (`CpResolvers::invoke`). Correctness, unconditional.
        if invoke_site_linkage_error(
            shared,
            &cm,
            declaring_id,
            cp_idx,
            target_class,
            target_name,
            target_desc,
            opcode,
        ) {
            no_live!(dead, format!(
                "invoke-site-linkage-error ({target_class}.{target_name}{target_desc} at callee pc {ipc})"
            ));
        }
        // REFUSE the whole splice when the callee makes an FFM element access.
        //
        // `try_emit_inline_body`'s invoke arm lowers every call in a spliced
        // body to an ordinary dispatch — the intrinsic ladder does NOT run
        // inside an inlined body. So splicing a method that contains a
        // `MemorySegment.getAtIndex`/`setAtIndex` silently converts that site
        // from the ~11 ns/element fast path back to the ~1158 ns/element native
        // dispatch, and those accessors live in exactly the one-line wrappers
        // (`TornadoMemorySegment.getShortAtIndex`, `ShortArray.get`) an inliner
        // takes every time.
        //
        // Measured: with these still spliced, kfusion showed `fast_hits=0` and
        // no `getAtIndex` emitted at all; refusing the splice took the same run
        // to 40.4M hits against 6.0M misses. The trade is ONE compiled-to-
        // compiled call per element — a few nanoseconds — to keep a ~100x fast
        // path, and it is only taken for a descriptor the fast path will
        // actually emit (`ffm_kind_for_descriptor`, the same gate the emitter
        // asks), so a float carrier keeps being inlined as before.
        if matches!(opcode, 0xb6 | 0xb9)
            && target_class == "java/lang/foreign/MemorySegment"
            && cratonvm_jit::ffm_element_accessor_name(target_name)
            && cratonvm_jit::ffm_kind_for_descriptor(target_desc).is_some()
        {
            // JIT round 24 B3 (`CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT`, opt-in):
            // an OPTIMIZING splice expands the access inline from the callee's
            // own receiver row (`cratonvm_jit::plan_spliced_ffm_element_sites`,
            // fed by the rows `callee_receiver_profile_rows` reads below), and
            // keeps this call row for a site it cannot expand.
            // j27 wave 4 (lane SPLICEFFM): lifted only for a call the planner
            // will expand -- the one admission both sides ask
            // (`spliced_ffm_element_site_admitted`). Lifted for every splice,
            // a thin or mixed row left a dispatched accessor inside the splice
            // (`R26SegLongLoops` native-put 928 -> 295,708 ns/op) where the
            // callee's own compiled body has its own expansion.
            if !(ir_mode
                && spliced_ffm_element_site_admitted(
                    shared,
                    &cm,
                    declaring_id,
                    (callee_method, callee_desc, ipc),
                    (target_class, target_name, target_desc),
                    code,
                ))
            {
                no_live!(dead, "ffm-accessor-keeps-its-own-fast-path");
            }
            if dead {
                continue;
            }
            ffm_element_site = true;
        }
        // THE SAME REFUSAL, THE SAME REASON, FOR EVERY OTHER CALL THAT HAS ITS
        // OWN THIN DIRECT BIND.
        //
        // `try_compile_inner`'s direct-native ladder does not run inside a
        // spliced body — `try_emit_inline_body`'s invoke arm lowers every call
        // in one to an ordinary dispatch. The calls below are exactly the
        // one-line wrappers an inliner takes every time
        // (`PooledDirectByteBuf._setByte` is `memory.put(idx(index), value)`;
        // `AbstractByteBuf.ensureAccessible` bottoms out in one `VarHandle`
        // read), so splicing them converted each site from a ~10 ns helper back
        // to the ~160 ns generic native funnel.
        //
        // MEASURED, on `JdkZlibIntegrationTest#testHugeDecompress`
        // (`--dump-native-registry`, 369 s wall): **1.07 BILLION** funnel calls,
        // every one of them a spliced site whose bind never got the chance to
        // fire —
        //
        //     536 870 912  java/security/MessageDigest.update(B)V
        //     269 768 030  java/lang/invoke/VarHandle.get(...)
        //     268 435 456  java/nio/DirectByteBuffer.put(IB)...
        //
        // — against a `perf` profile that puts the funnel and its receiver
        // validation at ~45 % of the whole run. The trade is ONE
        // compiled-to-compiled call per operation for a ~16x funnel, and it is
        // taken only for the exact triples the ladder actually binds, so every
        // other call to these classes is inlined as before.
        //
        // Netty compression wall (2026-10-14; `CRATONVM_JIT_SPLICE_THIN_NIO_MD_ASKS_BIND`,
        // default ON): only where that bind can still happen. Since JDK bodies
        // beat `Bridge`s, neither the `DirectByteBuffer` element helper (bound
        // only for a receiver class the funnel has SERVED) nor the
        // `MessageDigest.update(byte)` helper (refused by the `--real-jdk`
        // admission) binds in a real-JDK run, and the refusal kept
        // `UnpooledDirectByteBuf._setByte` / `_getByte` out of every caller for
        // nothing -- their own optimizing bodies then carried no evidence and
        // were discarded, leaving the single-pass bodies on every netty byte.
        if matches!(opcode, 0xb6 | 0xb9)
            && ((target_class == "java/nio/ByteBuffer"
                && ((target_name == "put" && target_desc == "(IB)Ljava/nio/ByteBuffer;")
                    || (target_name == "get" && target_desc == "(I)B"))
                && crate::jit::helpers::nio_byte_splice_keeps_thin_bind(
                    shared,
                    &cm,
                    target_name == "put",
                ))
                || (target_class == "java/security/MessageDigest"
                    && target_name == "update"
                    && target_desc == "(B)V"
                    && crate::jit::helpers::md_update_splice_keeps_thin_bind(shared))
                // Round 15 wave 4 (lane vh5; VH3-5): only a site the callee's
                // own compile will bind, `compareAndSet` included.
                || (target_class == "java/lang/invoke/VarHandle"
                    && crate::jit::helpers::varhandle_splice_keeps_thin_bind(
                        shared,
                        target_name,
                        target_desc,
                        ir_mode,
                    )))
        {
            no_live!(dead, "thin-bound-native-keeps-its-own-fast-path");
        }
        // THE SAME REFUSAL FOR A STATIC CALL-SITE INTRINSIC (round 13 wave 1,
        // lane callcost). `try_emit_inline_body` lowers this call as an
        // ordinary call and `bind_spliced_static_calls` direct-binds the
        // callee's COMPILED JDK BODY -- for `Math.fma` the BigDecimal
        // fallback, ~1000x the `VFMADD` the caller's own compile would emit.
        // Refusing keeps the callee a compiled-to-compiled call whose own
        // body gets the intrinsic.
        //
        // nio21 wave 7, lane L4 (`CRATONVM_JIT_IR_SPLICE_SCALAR_ROW_BODY`,
        // default ON): NOT for an optimizing-tier splice whose row the builder
        // lowers as arithmetic (`cratonvm_jit::ir_splice_lowers_static_scalar_row`:
        // `Math.min(II)I`, `Integer.compare`, the surrogate tests ...). There
        // the row never becomes a call -- `ir_plan_spliced_scalar_intrinsics`
        // hands it to the builder as a `ScalarOp`, consulted before the call
        // row -- so the splice keeps the intrinsic instead of losing it. This
        // refusal was what kept `StringLatin1.compareToCI` (its `Math.min`)
        // out of every caller, not the `Character` case natives (those are
        // registered in synthetic-JDK builds only).
        let ir_scalar_row = ir_mode
            && opcode == 0xb8
            && cratonvm_jit::ir_splice_lowers_static_scalar_row(
                target_class,
                target_name,
                target_desc,
            );
        // JIT round 23 wave 2f, lane L4 (INTRINSICS21-8 / W10L2-2 follow-up,
        // `CRATONVM_JIT_SPLICE_NESTED_ROW_PAST_STATIC_INTRINSIC`, default ON,
        // `0` kill): NOT for a nested-only row either
        // (`cratonvm_jit::splice_intrinsic_jdk_body_nested_only_admitted`:
        // `Math.abs(J)J`, the callee `multiplyExact(JJ)J`'s body needs). This
        // refusal ran first and refused it as a `MathAbsLong` call-site
        // intrinsic, so the whole `multiplyExact(JJ)J` splice of nio21 wave
        // 14 never engaged: `R23W2fRows multiplyexact-long` blind-dispatched
        // the call (66-113 ns/op, HotSpot ~0). Inside a splice the call has no
        // scalar lowering to lose (`ir_spliced_scalar_row` refuses `J`), and
        // the row runs the JDK's own leaf bytecode.
        let nested_only_row = ir_mode
            && crate::runtime::env_cache::jit_splice_nested_row_past_static_intrinsic()
            && cratonvm_jit::splice_intrinsic_jdk_body_nested_only_admitted(
                target_class,
                target_name,
                target_desc,
            );
        // JIT r25, lane regex rows (`CRATONVM_JIT_IR_SPLICE_PAST_ARRAYCOPY`,
        // default ON since R25-FLIP): NOT for `System.arraycopy` in an optimizing-tier splice,
        // which binds every arraycopy call node (relocated ones included) to
        // the same direct helper and inline prefixes as the caller's own site
        // (`cratonvm_jit::ir_splice_keeps_arraycopy_bind`). This refusal is
        // what kept `Arrays.copyOfRange([BII)` out of `StringLatin1.newString`.
        let ir_arraycopy_row = ir_mode
            && opcode == 0xb8
            && cratonvm_jit::ir_splice_keeps_arraycopy_bind(target_class, target_name, target_desc);
        if ir_mode
            && opcode == 0xb8
            && target_class == "java/lang/System"
            && target_name == "arraycopy"
            && crate::runtime::env_cache::dbg_jitc()
        {
            // Printed whether or not the exemption applies, so a run can tell
            // "never reached" (no line) from "reached and declined".
            eprintln!(
                "[cratonvm-jitc] ir-splice-past-arraycopy {} in {callee_class}.{callee_method}{callee_desc} \
                 depth={nest_depth} callee_pc={ipc}",
                if ir_arraycopy_row {
                    "ADMITTED (kept past static-intrinsic-keeps-its-own-fast-path)"
                } else {
                    "DECLINED (switch off, or IR direct calls / CRATONVM_JIT_IR_ARRAYCOPY off)"
                },
            );
        }
        // j27 wave 2 (lane NATMISC, REFLECT2-21-2): NOT for an int/long
        // `Math.min/max` row in a single-pass splice either: the emitter keeps
        // its CMOV there (`sp_splice_emits_minmax_inline`).
        let sp_minmax_row = sp_splice_emits_minmax_inline(
            ir_mode,
            invoke_kind,
            target_class,
            target_name,
            target_desc,
        );
        if !ir_mode
            && opcode == 0xb8
            && crate::runtime::env_cache::dbg_jitc()
            && cratonvm_jit::try_resolve_intrinsic(target_class, target_name, target_desc).is_some()
        {
            // Printed whether or not the exemption applies, as the arraycopy
            // line above: no line at all means this code never ran.
            eprintln!(
                "[cratonvm-jitc] sp-splice-minmax {} {target_class}.{target_name}{target_desc} \
                 in {callee_class}.{callee_method}{callee_desc} depth={nest_depth} callee_pc={ipc} \
                 invoke_kind={invoke_kind}",
                if sp_minmax_row {
                    "ADMITTED (kept past static-intrinsic-keeps-its-own-fast-path)"
                } else {
                    "DECLINED (CRATONVM_JIT_SPLICE_STATIC_MINMAX=0, or not an int/long min/max row)"
                },
            );
        }
        if opcode == 0xb8
            && !ir_scalar_row
            && !nested_only_row
            && !ir_arraycopy_row
            && !sp_minmax_row
            && crate::runtime::env_cache::jit_splice_keeps_static_intrinsic()
            && cratonvm_jit::try_resolve_intrinsic(target_class, target_name, target_desc).is_some()
        {
            no_live!(dead, "static-intrinsic-keeps-its-own-fast-path");
        }
        // Receiver-included, one slot per parameter regardless of category —
        // the count `JitInvokeInfo::num_jit_args` carries and the count the
        // emitter pops, since the JIT operand stack holds one i64 per value.
        let num_jit_args = count_method_params(target_desc) + if opcode == 0xb8 { 0 } else { 1 };
        // Wave 120 (L2): or the classifier's §6.5 owner of a spliced
        // `invokespecial` (`special_owners`, filled above under
        // `CRATONVM_JIT_INVOKE_SITE_CLASSIFIER`): carried with its id, as a
        // pinned private method's owner is.
        let private_owner = private_owner.or_else(|| {
            (opcode == 0xb7)
                .then(|| {
                    special_owners
                        .iter()
                        .find(|(at, _, _)| *at == ipc)
                        .map(|(_, name, id)| (name.clone(), *id))
                })
                .flatten()
        });
        let (class_name, owner_class_id) =
            private_owner.unwrap_or_else(|| (target_class.to_string(), 0));
        // JIT round 20 wave 19, lane ySplice (SPLICE20-2): the constructor
        // call of a held unloaded `new` (`cold_unresolved_news`). The verifier
        // ties an `<init>` of a class this loader has not loaded to a `new` of
        // that class in this body, which becomes a precise exit, so the call
        // is never reached; listed so the planner's unbindable-call rule does
        // not refuse the body for it. Still a real call row (a by-name
        // dispatch), so a site whose `new` resolved after all stays correct.
        if opcode == 0xb7
            && target_name == "<init>"
            && cold_unresolved_news
                .iter()
                .any(|(_, n)| n.as_str() == target_class)
            && jit_known_class_at(shared, &cm, target_class, declaring_id, Some(ref_class_idx))
                .is_none()
        {
            unloaded_static_call_pcs.push(ipc);
        }
        // The target's constant-pool class id and the selected method's
        // declaring class id. Late static cells need both so inherited static
        // calls cannot install a cell under the wrong class identity. These
        // are filled from lookup-only resolution; no class is loaded here.
        let mut target_class_id = 0u32;
        let mut resolved_class_id = 0u32;
        let late_static_cell_metadata = gates.jit_late_static_cell_enabled
            && gates.jit_cycle_edge_cell_enabled
            && gates.jit_cycle_direct_bind_enabled
            && gates.direct_jit_callee_calls_enabled;
        // Compilation must not load a class that Java has not reached.
        // Carry this loader-faithful absence to the precise splice exit.
        if ir_mode && opcode == 0xb8 {
            match jit_known_class_at(shared, &cm, target_class, declaring_id, Some(ref_class_idx)) {
                None => unloaded_static_call_pcs.push(ipc),
                // nio21 wave 6, lane L4: a loaded static callee with a plain
                // bytecode body is a candidate for the never-run exit
                // (`splice_never_run_static_exit_enabled`), decided once the
                // binds below have run.
                // A scalar row is never a call (see `ir_scalar_row`), so it is
                // never an exit either.
                Some(cp_class_id) => {
                    target_class_id = cp_class_id.as_u32();
                    if (never_run_static_exit || self_copy_by_id || late_static_cell_metadata)
                        && !ir_scalar_row
                        && !dead
                    {
                        let store = cm.class_store();
                        // r21 flip review #6 (residual): JVMS 5.4.3.3 resolution
                        // through the facade, not the dispatch walk
                        // `find_method_recursive`, which prefers a non-abstract
                        // method and falls back to interface defaults. That walk
                        // answered `C.f` (static) where `Sub` declares an abstract
                        // instance `f` (JVMS: `Sub.f`, `invokestatic` throws ICCE),
                        // and could reach an interface static, which is not
                        // inherited (NSME). The RESOLVED method must itself be
                        // static, else neither the self-copy id nor the never-run
                        // exit is filed and the ordinary call raises the error.
                        let resolved = crate::runtime::resolve::selection::resolve_declaring(
                            store,
                            cp_class_id,
                            target_name,
                            target_desc,
                        )
                        .and_then(|decl| {
                            store
                                .get(decl)
                                .and_then(|c| c.find_method(target_name, target_desc))
                                .map(|m| (m, decl))
                        });
                        if let Some((method, callee_decl)) = resolved {
                            if (self_copy_by_id || late_static_cell_metadata) && method.is_static() {
                                resolved_class_id = callee_decl.as_u32();
                            }
                            if never_run_static_exit {
                                let registered = store.get(callee_decl).is_some_and(|c| {
                                    shared
                                        .natives
                                        .native_methods
                                        .find(&*c.name, target_name, target_desc)
                                        .is_some()
                                }) || shared
                                    .natives
                                    .native_methods
                                    .find(target_class, target_name, target_desc)
                                    .is_some();
                                if method.is_static()
                                    && !method.is_native()
                                    && !method.is_abstract()
                                    && method.code().is_some()
                                    && !registered
                                {
                                    never_run_static_candidates.push((ipc, callee_decl));
                                }
                            }
                        }
                    }
                }
            }
        }
        (if dead { &mut dead_invoke_targets } else { &mut invoke_targets }).push((
            ipc,
            cratonvm_jit::InlineInvokeTarget {
                class_name,
                owner_class_id,
                method_name: target_name.to_string(),
                descriptor: target_desc.to_string(),
                num_jit_args,
                return_type: cratonvm_jit::return_type(target_desc),
                invoke_kind,
                declaring_class_id: declaring_id.as_u32(),
                target_class_id,
                resolved_class_id,
                // Resolved below, once `cm` is dropped: the direct-bind
                // resolver takes `class_manager` itself (and may COMPILE the
                // callee), so calling it under this read guard is the same
                // self-deadlock the field-resolution phase is split out for.
                direct_entry: None,
            },
        ));
    }

    // A callee that makes an `AtomicInteger` / `AtomicLong` intrinsic call
    // keeps its own compile: the ladders that turn `getAndIncrement()` & co.
    // into one locked instruction run at the compiled method's OWN sites, and
    // a spliced body's calls are ordinary calls, so splicing it would copy the
    // JDK's `Unsafe.getAndAddInt` retry loop in place of the instruction
    // (`cratonvm_jit::splice_loses_atomic_intrinsic`). `FastThreadLocal.<init>`
    // -> `InternalThreadLocalMap.nextVariableIndex()` in a loop: 8-9 ns per
    // iteration kept as a call, 135-142 ns spliced. Both tiers, as the
    // thin-bound refusal above.
    if invoke_targets.iter().any(|(_, t)| {
        matches!(t.invoke_kind, 0 | 1)
            && cratonvm_jit::splice_loses_atomic_intrinsic(
                &t.class_name,
                &t.method_name,
                &t.descriptor,
            )
    }) {
        no!("atomic-intrinsic-callee-keeps-its-own-fast-path");
    }

    // Round 11 wave 11 (lane `calls`): the other half of the self-recursive
    // multi-return admission above. Only a body that makes a STATIC call to
    // itself -- the name triple being resolved -- keeps its several returns;
    // that is the one shape `ir::IrSelfRecursion` can splice (its kind-4 leaf
    // is static-only), and anything else is the general case the default-off
    // `ir_splice_multi_return_enabled` owns.
    //
    // JIT round 21 wave 16 (lane jit-selfcopy): under
    // `CRATONVM_JIT_IR_SELF_COPY_BY_ID` a row whose RESOLVED declaring class
    // is this body's class also counts, so `C.f`'s `invokestatic Sub.f` (with
    // `Sub extends C` not hiding `f`) keeps the admission. Only a gate: the
    // planner's `IrSelfRecursion::classify` and the copy admission decide.
    if multi_return_if_self_recursive
        && !invoke_targets.iter().any(|(_, t)| {
            t.invoke_kind == 3
                && (t.class_name == callee_class
                    || (self_copy_by_id
                        && t.resolved_class_id != 0
                        && t.resolved_class_id == declaring_id.as_u32()))
                && t.method_name == callee_method
                && t.descriptor == callee_desc
        })
    {
        no!("ir-splice-not-single-trailing-return");
    }

    // Lock-order discipline (audit follow-up to the H2 ABBA fix): collect
    // the constant-pool facts for field ops HERE (they borrow `cm`), but
    // DELAY every `resolve_field_ref` call until `cm` is dropped below —
    // resolve_field_ref re-locks class_manager with a plain read() (wedges
    // behind any queued writer while we hold this read), can take
    // class_manager.write() via load_class_concurrent on a cold field
    // class (same-thread self-deadlock), and writes resolution_cache (the
    // cm→resolution_cache inversion).
    if ir_mode {
        for &(npc, cp_idx) in &dead_new_sites {
            if let Some(cratonvm_jit::JitNewSite::Resolved {
                class_id,
                num_fields,
                has_finalizer,
                ..
            }) = resolve_jit_new_site(shared, &cm, declaring_id, cp_idx)
            {
                if !jit_new_target_init_pending(shared, &cm, ClassId::new(class_id)) {
                    ir_new_info.push((npc, class_id, num_fields, has_finalizer));
                }
            }
        }
    }
    let pc_is_live = |p: usize| {
        early_live_pcs
            .as_ref()
            .is_none_or(|live| live.get(p).copied().unwrap_or(false))
    };
    let mut field_sites: Vec<(usize, u16, u8, bool)> = Vec::new();
    let mut byte_array_field_candidates: Vec<usize> = Vec::new();
    if has_field_ops {
        let mut fpc = 0;
        while fpc < code_len {
            if matches!(code[fpc], 0xb4 | 0xb5) && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let nat_idx = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::FieldReference {
                        name_and_type_index,
                        ..
                    }) => *name_and_type_index,
                    _ => return None,
                };
                if let Some((name, desc)) =
                    callee_class_info.constant_pool.get_name_and_type(nat_idx)
                {
                    let type_tag = *desc.as_bytes().first().unwrap_or(&b'L');
                    field_sites.push((fpc, cp_idx, type_tag, pc_is_live(fpc)));
                    // nio21 wave 7, lane L4 (W5L4-3): a read the pool names
                    // `hb:[B`, confirmed against the resolved field in phase 2.
                    if ir_mode && code[fpc] == 0xb4 && byte_array_field_row(name, desc) {
                        byte_array_field_candidates.push(fpc);
                    }
                }
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }

    let mut static_sites: Vec<(usize, u16, u8, bool)> = Vec::new();
    if has_static_field_ops {
        let mut fpc = 0;
        while fpc < code_len {
            if matches!(code[fpc], 0xb2 | 0xb3) && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let nat_idx = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::FieldReference {
                        name_and_type_index,
                        ..
                    }) => *name_and_type_index,
                    _ => return None,
                };
                if let Some((_, desc)) = callee_class_info.constant_pool.get_name_and_type(nat_idx)
                {
                    let type_tag = *desc.as_bytes().first().unwrap_or(&b'L');
                    static_sites.push((fpc, cp_idx, type_tag, pc_is_live(fpc)));
                }
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }

    // `ldc` / `ldc_w` in an INLINE CANDIDATE.
    //
    // The inline mini-emitter (`x64/inlining.rs`) materialises these as bare
    // x86 immediates and has no path to `helpers.ldc_string` /
    // `helpers.ldc_class_cp`. So only the two constant kinds that ARE an
    // immediate — `Integer` and `Float` — can be recorded. Every other kind
    // (String, Class, MethodHandle, MethodType, condy) names a *reference*
    // materialised at run time, and there is no i64 that stands for it.
    //
    // This used to end `_ => 0`, which recorded a perfectly well-formed entry
    // claiming the constant's value was zero. The emitter then trusted it and
    // pushed `null`. A one-line `Dialect.extractPattern(unit) { return
    // "extract(?1 from ?2)"; }` spliced into `H2Dialect.extractPattern`
    // compiled to `xor eax,eax; ret`, and every Hibernate HQL `extract()` /
    // `cast()` / `str()` query then died in `PatternRenderer.<init>` with
    // `NullPointerException: ... because "pattern" is null` (2026-08-11 Linux
    // full suite: 17 of 34 method failures across four HQL classes, all green
    // under `--nojit`).
    //
    // Refuse the whole inline site instead. The callee still compiles and is
    // still CALLED — it just is not spliced — which is what "cannot model it"
    // has to mean.
    let mut ldc_info = Vec::new();
    // Callee PCs whose constant is a float or a double. The optimizing tier
    // cannot splice a body without this — see `InlineSite::ldc_fp_pcs` — and
    // recording it here rather than re-deriving it downstream is what keeps the
    // tag and the value from disagreeing: both come out of the same
    // constant-pool match.
    let mut ldc_fp_pcs: Vec<usize> = Vec::new();
    // Round 14 wave 7 (lane sync7, SS8-5 through S6-1's rows): the optimizing
    // tier's `static synchronized` body at depth 0 may carry `ldc C` of its
    // OWN class (already loaded; the typecheck rows' no-loading resolution),
    // which the JIT admits only as the first half of `holdsLock(C.class)`
    // inside that body's window (`cratonvm_jit::InlineSite::ir_ldc_class_info`).
    // `CRATONVM_JIT_IR_SYNC_SPLICE_SELF_CLASS_HOLDSLOCK=0` refuses it as before.
    let ldc_self_class_rows = ir_mode
        && nest_depth == 0
        && sync_splice
        && is_static
        && gates.ir_sync_splice_self_class_holds_lock_enabled;
    let mut ir_ldc_class_info: Vec<(usize, u32)> = Vec::new();
    let own_class_ldc = |cp_idx: u16| -> bool {
        ldc_self_class_rows
            && matches!(
                resolve_jit_new_site(shared, &cm, declaring_id, cp_idx),
                Some(cratonvm_jit::JitNewSite::Resolved { class_id, .. })
                    if class_id == declaring_id.as_u32()
            )
    };
    // JIT r20 wave 19 (lane zLdc, THROW20-4 `date`,
    // `cratonvm_jit::ir::ir_splice_ref_ldc_enabled`): an OPTIMIZING splice of
    // a non-synchronized body may carry an `ldc` of a String or Class entry
    // of its own pool, built as the same `ConstString` / `ConstClass` node a
    // top-level `ldc` builds with the CALLEE's `(holder, cp index)`. Collected
    // here under `cm` as `(callee pc, cp index, is_class)`; admitted below,
    // after `drop(cm)`, only with a non-zero GC-maintained slot (a RESOLVED
    // constant: its loading, access check and interning already ran, so no
    // resolution side effect moves). `InlineSite::ir_ldc_ref_rows`.
    let ref_ldc_rows = ir_mode && !sync_splice && gates.ir_splice_ref_ldc_enabled;
    let mut ir_ldc_ref_pending: Vec<(usize, u16, bool)> = Vec::new();
    // `(invoke pc, invoke cp index, class id, class name)` of each
    // `invokevirtual getClass()Ljava/lang/Class;` right before a class row,
    // followed by `if_acmpeq`/`if_acmpne`; screened after `drop(cm)`
    // (`InlineSite::ir_getclass_compares`). The class id is the entry's
    // resolution without loading (`resolve_jit_new_site`, as `own_class_ldc`).
    let mut ir_getclass_pending: Vec<(usize, u16, u32, String)> = Vec::new();
    // `Some(is_class)` for an entry the scan may record, `None` otherwise.
    let ref_ldc_kind = |cp_idx: u16| -> Option<bool> {
        if !ref_ldc_rows {
            return None;
        }
        match jit_ldc_constant_for(&cm, declaring_id, cp_idx)? {
            cratonvm_jit::JitLdcConstant::String { .. } => Some(false),
            cratonvm_jit::JitLdcConstant::ClassMirror { .. } => Some(true),
            _ => None,
        }
    };
    let getclass_candidate =
        |fpc: usize, width: usize, cp_idx: u16| -> Option<(usize, u16, u32, String)> {
            if fpc < 3 || fpc + width >= code_len || code[fpc - 3] != 0xb6 {
                return None;
            }
            if !matches!(code[fpc + width], 0xa5 | 0xa6) {
                return None;
            }
            let invoke_cp = ((code[fpc - 2] as u16) << 8) | code[fpc - 1] as u16; // Cast: bytecode operand decoding
            let nat_idx = match callee_class_info.constant_pool.get(invoke_cp)? {
                ConstantPoolEntry::MethodReference {
                    name_and_type_index,
                    ..
                } => *name_and_type_index,
                _ => return None,
            };
            let (name, desc) = callee_class_info.constant_pool.get_name_and_type(nat_idx)?;
            if name != "getClass" || desc != "()Ljava/lang/Class;" {
                return None;
            }
            let class_name = callee_class_info.constant_pool.get_class_name(cp_idx)?;
            match resolve_jit_new_site(shared, &cm, declaring_id, cp_idx)? {
                cratonvm_jit::JitNewSite::Resolved { class_id, .. } if class_id != 0 => {
                    Some((fpc - 3, invoke_cp, class_id, class_name.to_string()))
                }
                _ => None,
            }
        };
    if has_ldc {
        let mut fpc = 0;
        while fpc < code_len {
            if code[fpc] == 0x12 && fpc + 1 < code_len {
                let cp_idx = code[fpc + 1] as u16; // Cast: bytecode operand decoding
                let val = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::Integer(v)) => *v as i64, // JVM spec: bounded float-to-long conversion
                    Some(ConstantPoolEntry::Float(v)) => {
                        ldc_fp_pcs.push(fpc);
                        (*v as f32).to_bits() as i32 as i64 // Cast: JIT ABI -- float bits to i64
                    }
                    Some(ConstantPoolEntry::ClassReference { .. }) if own_class_ldc(cp_idx) => {
                        ir_ldc_class_info.push((fpc, declaring_id.as_u32()));
                        fpc += 2;
                        continue;
                    }
                    Some(_) if ref_ldc_kind(cp_idx).is_some() => {
                        if pc_is_live(fpc) {
                            let is_class = ref_ldc_kind(cp_idx) == Some(true);
                            ir_ldc_ref_pending.push((fpc, cp_idx, is_class));
                            if is_class {
                                ir_getclass_pending.extend(getclass_candidate(fpc, 2, cp_idx));
                            }
                        }
                        fpc += 2;
                        continue;
                    }
                    _ if !pc_is_live(fpc) => {
                        fpc += 2;
                        continue;
                    }
                    _ => no!("ldc-non-numeric-constant"),
                };
                ldc_info.push((fpc, val));
                fpc += 2;
            } else if code[fpc] == 0x13 && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let val = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::Integer(v)) => *v as i64, // JVM spec: bounded float-to-long conversion
                    Some(ConstantPoolEntry::Float(v)) => {
                        ldc_fp_pcs.push(fpc);
                        (*v as f32).to_bits() as i32 as i64 // Cast: JIT ABI -- float bits to i64
                    }
                    Some(ConstantPoolEntry::ClassReference { .. }) if own_class_ldc(cp_idx) => {
                        ir_ldc_class_info.push((fpc, declaring_id.as_u32()));
                        fpc += 3;
                        continue;
                    }
                    Some(_) if ref_ldc_kind(cp_idx).is_some() => {
                        if pc_is_live(fpc) {
                            let is_class = ref_ldc_kind(cp_idx) == Some(true);
                            ir_ldc_ref_pending.push((fpc, cp_idx, is_class));
                            if is_class {
                                ir_getclass_pending.extend(getclass_candidate(fpc, 3, cp_idx));
                            }
                        }
                        fpc += 3;
                        continue;
                    }
                    _ if !pc_is_live(fpc) => {
                        fpc += 3;
                        continue;
                    }
                    _ => no!("ldc-non-numeric-constant"),
                };
                ldc_info.push((fpc, val));
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }

    // Same contract for `ldc2_w`: `Long` and `Double` are the only entries the
    // JVMS permits here, so a third kind means the constant pool disagrees with
    // the bytecode — refuse rather than splice a zero.
    let mut ldc2w_info = Vec::new();
    if has_ldc2w {
        let mut fpc = 0;
        while fpc < code_len {
            if code[fpc] == 0x14 && fpc + 2 < code_len {
                let cp_idx = ((code[fpc + 1] as u16) << 8) | code[fpc + 2] as u16; // Cast: bytecode operand decoding
                let val = match callee_class_info.constant_pool.get(cp_idx) {
                    Some(ConstantPoolEntry::Long(v)) => v.get(),
                    Some(ConstantPoolEntry::Double(v)) => {
                        ldc_fp_pcs.push(fpc);
                        v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                    }
                    _ if !pc_is_live(fpc) => {
                        fpc += 3;
                        continue;
                    }
                    _ => return None,
                };
                ldc2w_info.push((fpc, val));
                fpc += 3;
            } else {
                fpc += inline_instr_length(code, fpc);
            }
        }
    }
    // Round nio21 wave 10 (lane L2): every division the scan recorded must
    // divide by a non-zero constant pushed by the instruction right before it,
    // with no branch landing on the division (`a / (c ? x : 7)` joins a phi
    // there, which the builder guards -- and that guard would refuse the
    // build). An `ldc` divisor is read from the rows just resolved: an
    // `Integer` for `idiv` / `irem`, a `Long` for `ldiv` / `lrem`, never a
    // float. Nio21 wave 14 (lane L2, W12L4-2): judged by the one decoder the
    // optimizing tier's own screens use, `ir::splice_division_const_divisor`.
    let ldc_integral = |p: usize| -> Option<i64> {
        if ldc_fp_pcs.contains(&p) {
            return None;
        }
        ldc_info
            .iter()
            .chain(ldc2w_info.iter())
            .find(|&&(q, _)| q == p)
            .map(|&(_, v)| v)
    };
    if ir_const_div_pcs.iter().any(|&pc| {
        cratonvm_jit::ir::splice_division_const_divisor(code, code_len, pc, &ldc_integral).is_none()
    }) {
        no!("ir-splice-division");
    }

    let num_params = count_method_params(callee_desc);
    let callee_num_args = num_params + if is_static { 0 } else { 1 };
    let return_type = cratonvm_jit::return_type(callee_desc);
    // A spliced call goes through `jit_invoke_dispatch`, whose first argument
    // is the VM context the emitter loads from `heap_local_offset`. Without
    // this the enclosing compile may not establish that slot at all and the
    // dispatch reads garbage. `needs_heap` is ORed across every spliced site
    // by the planner, so one call-carrying callee is enough to turn it on for
    // the whole method — the same way one field-touching callee already does.
    // JIT round 20 wave 19, lane ySplice (SPLICE20-7): a single-pass splice's
    // constant-word lambda site calls the bridge on its cold miss, which loads
    // the VM context the same way. JIT r20 wave 19 (lane zLdc): a spliced
    // String / Class `ldc`'s helper miss arm takes the VM context too, and so
    // does a single-pass splice's `checkcast` / `instanceof` helper call
    // (`CRATONVM_JIT_SP_SPLICE_TYPECHECK`).
    let needs_heap = has_field_ops
        || has_static_field_ops
        || has_newarray
        || !invoke_targets.is_empty()
        || !ir_indy_rows.is_empty()
        || !ir_ldc_ref_pending.is_empty()
        || !ir_typecheck_sites.is_empty();

    let padded = crate::runtime::frame::padded_bytecode(code_bytes);

    // This body is about to be spliced into another method's code, which
    // records no dependency on it that a scoped redefinition could find (the
    // IR tier none, the single-pass plan its outer sites only). Marked under
    // the guard its bytecode was read under: a redefinition of the class asks
    // under the writer (`JitRealm::scoped_redefinition_admitted`, interpreter
    // round i1 wave 20, lane L1), so it sees this mark or this read saw its
    // new bytecode.
    if !planning_only {
        shared.jit.jit_cache.note_bytecode_copied(declaring_id);
    }

    // Interpreter round i6 wave 122, lane L2 (step 3 of
    // `i37-L6-proposal-hidden-method-bit-at-link-time`): the callee's
    // `Method::is_hidden`, stamped into every inline-frame level the splice
    // records. `None` unless the store keeps hidden-method bits
    // (`CRATONVM_HIDDEN_METHOD_BITS`).
    let callee_hidden = crate::runtime::stackwalker::inline_callee_hidden_stamp(
        cm.class_store(),
        declaring_id,
        callee_method,
        callee_desc,
    );

    drop(cm);

    // Round rx27 wave 1 (lane matcher, `CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM`):
    // the masked handler arm is admitted only when the callee's own profile
    // saw its branch jump and never fall into it
    // (`cratonvm_jit::ir::splice_cold_handler_arm_unexecuted`). Asked with
    // `cm` dropped, as the cold type checks below are.
    if let Some(arm) = cold_handler_arm {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: declaring_id.as_u32(),
            method_name: Arc::from(callee_method),
            descriptor: Arc::from(callee_desc),
        };
        let cold = shared
            .jit
            .profile_store
            .get_profile(&profile_key)
            .as_ref()
            .is_some_and(|p| {
                cratonvm_jit::ir::splice_cold_handler_arm_unexecuted(&arm, method_code_len, p)
            });
        if !cold {
            no!(format!(
                "callee-exception-table (cold-handler-arm: no profile proves the branch at callee pc {} \
                 never fell into {}..{})",
                arm.branch_pc, arm.start, arm.end
            ));
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] splice-cold-handler-arm {callee_class}.{callee_method}{callee_desc} \
                 branch_pc={} arm={}..{} depth={nest_depth}: masked, the branch deopts into the arm",
                arm.branch_pc, arm.start, arm.end
            );
        }
    }

    // JIT r20 wave 19 (lane zLdc): the String / Class `ldc` rows collected in
    // the scan, each admitted only with the VM's GC-maintained slot for the
    // CALLEE's `(declaring class, cp index)` -- non-zero only for a site whose
    // constant is already resolved and recorded (JVMS 5.4.3; a failed
    // resolution is never recorded). Asked with `cm` dropped, as
    // `compiled_ldc_slot_for` requires. Any other answer keeps the body
    // refused as before.
    let mut ir_ldc_ref_rows: Vec<(usize, u32, u16, bool, usize)> = Vec::new();
    for &(lpc, cp_idx, is_class) in &ir_ldc_ref_pending {
        let slot_addr = crate::jit::helpers::compiled_ldc_slot_for(shared, declaring_id, cp_idx);
        // JIT r23 wave 9, lane L6 (`CRATONVM_JIT_IR_SPLICE_UNRESOLVED_LDC_EXIT`):
        // an unresolved String constant on an arm the callee's own profile
        // never ran (`StringLatin1.newString`'s `len == 0 -> ""`) is sent as a
        // slot-0 row. The builder plants a precise exit there and the
        // interpreter resolves the constant. Same profile rule as the
        // unresolved `new`s below.
        if slot_addr == 0
            && gates.ir_splice_unresolved_ldc_exit_enabled
            && {
                let profile_key = crate::jit::profile::MethodKey {
                    class_id: declaring_id.as_u32(),
                    method_name: Arc::from(callee_method),
                    descriptor: Arc::from(callee_desc),
                };
                shared
                    .jit
                    .profile_store
                    .get_profile(&profile_key)
                    .as_ref()
                    .is_some_and(|p| {
                        cratonvm_jit::ir::splice_typecheck_arm_unexecuted(
                            code_bytes,
                            code_len,
                            method_code_len,
                            lpc,
                            p,
                        )
                    })
            }
        {
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] splice-unresolved-ldc-exit {callee_class}.{callee_method}{callee_desc} \
                     at callee_pc={lpc} cp=#{cp_idx} depth={nest_depth}"
                );
            }
            ir_ldc_ref_rows.push((lpc, declaring_id.as_u32(), cp_idx, is_class, 0));
            continue;
        }
        if slot_addr == 0 {
            no!(format!(
                "ldc-non-numeric-constant (callee pc {lpc}, cp #{cp_idx}: not resolved or no slot)"
            ));
        }
        ir_ldc_ref_rows.push((lpc, declaring_id.as_u32(), cp_idx, is_class, slot_addr));
    }
    // The `x.getClass() == C.class` pairs over those rows the builder may
    // answer for a fresh allocation: the invoke must resolve to the method
    // `java/lang/Object` declares (a class's own private / static
    // `getClass()` is not it), and `getClass()` must report `C` exactly
    // (native-builtins' remap tables, the qMisc fold's predicate).
    let ir_getclass_compares: Vec<(usize, u32)> = ir_getclass_pending
        .iter()
        .filter(|(_, invoke_cp, _, name)| {
            cp_method_ref_declaring_class_name(shared, declaring_id, *invoke_cp).as_deref()
                == Some("java/lang/Object")
                && cratonvm_native_builtins::getclass_reports_only_itself(name)
        })
        .map(|&(ipc, _, class_id, _)| (ipc, class_id))
        .collect();


    // Round nio20 wave 1, lane L1 (NIO19-3): the unloaded type checks held
    // above, admitted only behind a branch edge this method's own profile says
    // never ran. Asked with `cm` dropped (the profile store takes its own
    // shard and slot locks). The profile is keyed by the method's own bcis,
    // which the spliced copy keeps (a stripped fence handler moves nothing).
    if !cold_unloaded_typechecks.is_empty() {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: declaring_id.as_u32(),
            method_name: Arc::from(callee_method),
            descriptor: Arc::from(callee_desc),
        };
        let profile = shared.jit.profile_store.get_profile(&profile_key);
        for (tpc, name, is_checkcast) in cold_unloaded_typechecks {
            let cold = profile.as_ref().is_some_and(|p| {
                cratonvm_jit::ir::splice_typecheck_arm_unexecuted(
                    code_bytes,
                    code_len,
                    method_code_len,
                    tpc,
                    p,
                )
            });
            // r23 L8 (`CRATONVM_JIT_IR_SPLICE_UNPROFILED_NONNULL_TYPECHECK`):
            // without that proof, a check whose operand a null test already
            // proved non-null traps at most once (the interpreter's check
            // loads the class).
            let nonnull = !cold
                && gates.ir_splice_unprofiled_nonnull_typecheck_enabled
                && cratonvm_jit::ir::typecheck_operand_nonnull_by_test(code_bytes, code_len, tpc);
            if !cold && !nonnull {
                no!(format!(
                    "ir-splice-typecheck-target-not-loaded (callee pc {tpc}: no profile proves its arm cold{})",
                    if profile.is_none() { "; no profile" } else { "" }
                ));
            }
            if nonnull && crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] splice-unprofiled-nonnull-typecheck {callee_class}.{callee_method}{callee_desc} \
                     at callee_pc={tpc} depth={nest_depth}"
                );
            }
            ir_typecheck_info.push((tpc, cratonvm_jit::UNLOADED_SPLICE_CLASS_ID, name, is_checkcast));
        }
    }

    // JIT round 20 wave 19, lane ySplice (SPLICE20-2): the unloaded `new`s
    // held above, by the same rule and from the same profile as the type
    // checks: admitted only behind a branch edge that never ran, as a row
    // with class id 0 (a precise exit at the `new`; `append_ir_inline_site`
    // rebases it, the builder's `0xbb` arm plants it). The class not being
    // loaded already proves no execution reached this `new`; the edge is what
    // keeps a site that will run on every call from becoming an exit per
    // call. The exit is `DeoptReason::ClassLoading`: its first firing asks a
    // recompile, which by then sees the class loaded and initialized (a
    // resolved row), or erroneous (`ir-splice-new-site-class-init-pending`:
    // the callee stays a call).
    if !cold_unresolved_news.is_empty() {
        let profile_key = crate::jit::profile::MethodKey {
            class_id: declaring_id.as_u32(),
            method_name: Arc::from(callee_method),
            descriptor: Arc::from(callee_desc),
        };
        let profile = shared.jit.profile_store.get_profile(&profile_key);
        for (npc, _) in &cold_unresolved_news {
            let cold = profile.as_ref().is_some_and(|p| {
                cratonvm_jit::ir::splice_typecheck_arm_unexecuted(
                    code_bytes,
                    code_len,
                    method_code_len,
                    *npc,
                    p,
                )
            });
            if !cold {
                no!(format!(
                    "ir-splice-new-site-unresolved (callee pc {npc}: no profile proves its arm cold)"
                ));
            }
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] splice-unresolved-new-exit {callee_class}.{callee_method}{callee_desc} \
                     at callee_pc={npc} depth={nest_depth}"
                );
            }
            ir_new_info.push((*npc, cratonvm_jit::UNLOADED_SPLICE_CLASS_ID, 0, false));
        }
    }

    // Phase 2 — resolve field refs with NO class_manager guard held (see
    // the lock-order comment above).
    let mut field_info = Vec::new();
    // Round 15 wave 3 (lane chain3, CHN15-2 producer): `(callee pc, field
    // index, declaring class id)` beside each `field_info` row, from the same
    // resolution (`InlineSite::field_decl_info`).
    let mut field_decl_info: Vec<(usize, usize, u32)> = Vec::new();
    let mut compact_field_info = Vec::new();
    // Callee pcs of the volatile `getfield`s (round 9 wave 10, irl10; landed
    // by wave 10b, wire10b). See `InlineSite::volatile_field_pcs`.
    let mut volatile_field_pcs: Vec<usize> = Vec::new();
    // nio21 wave 2 (lane L3): callee pcs of the `getfield`s whose resolved
    // field is `final`. See `InlineSite::final_field_pcs`.
    let mut final_field_pcs: Vec<usize> = Vec::new();
    // nio21 wave 7 (lane L4): see `InlineSite::byte_array_field_pcs`.
    let mut byte_array_field_pcs: Vec<usize> = Vec::new();
    // JIT r20 wave 18 (lane xLib): see `InlineSite::referent_load_pcs`.
    let mut referent_load_pcs: Vec<usize> = Vec::new();
    for (fpc, cp_idx, type_tag, is_live) in field_sites {
        // Round 15 wave 4 (lane fieldowner): a site whose owner the
        // interpreter would resolve through the loader to another class
        // refuses the body in both modes (`jit_field_owner_refusal`).
        let site = jit_resolve_field_site(shared, declaring_id, cp_idx);
        if let Err(Some(why)) = site {
            if !is_live { continue; }
            no!(why);
        }
        if let Ok(resolved) = site {
            // JVMS §6.5 linking ICCE: a `getfield`/`putfield` naming a static
            // field is left to the interpreter, which throws it.
            if !jit_field_kind_agrees(&resolved, false) {
                if !is_live { continue; }
                no!("splice-field-kind-mismatch");
            }
            // Round 15 wave 4 (lane fieldowner): the slot's own descriptor
            // must be the one the pool names, as every other door asks
            // (`jit_field_tag_agrees`); `type_tag` is the pool's.
            if gates.jit_field_site_faithful_enabled
                && !jit_field_tag_agrees(&resolved, type_tag, cp_idx)
            {
                if !is_live { continue; }
                no!("splice-field-tag-disagrees");
            }
            // SBR4 satb: a spliced body cannot carry the SATB keep-alive of a
            // `Reference.referent` load (see `jit_load_of_referent`) -- in the
            // single-pass inliner. JIT r20 wave 18 (lane xLib, THROW20-4;
            // `CRATONVM_JIT_IR_REFERENT_SPLICE`): an OPTIMIZING splice admits
            // it as a load that always takes the checked `jit_getfield` helper
            // (which logs the referent) -- `InlineSite::referent_load_pcs`,
            // with no compact row and no declaring-class identity below, so
            // no inline arm and no field-forwarding pass can serve it.
            let referent_load =
                jit_load_of_referent(shared, code_bytes.get(fpc).copied(), &resolved);
            if referent_load && !(ir_mode && gates.jit_ir_referent_splice_enabled) {
                if !is_live { continue; }
                no!("splice-reference-referent-load");
            }
            // ...and a final-field store the interpreter would reject
            // (`IllegalAccessError`) likewise.
            if code_bytes.get(fpc).copied() == Some(0xb5)
                && final_field_put_illegal(shared, declaring_id, callee_method, &resolved)
            {
                if !is_live { continue; }
                no!("splice-final-field-store");
            }
            // A VOLATILE instance field's `field_info` row has no volatility of
            // its own. A volatile STORE refuses the body in both modes: the
            // single-pass splice would store with no StoreLoad fence and the
            // optimizing tier has no fenced spliced store. A volatile LOAD is
            // admitted and its pc recorded in `volatile_field_pcs`: the IR tier
            // builds it as an ordered `Op::Load` (Acquire, never hoisted or
            // merged), and a single-pass splice reads it with one plain load,
            // which x86-64 orders as an acquire. With the IR's volatile
            // modelling switched off (`CRATONVM_JIT_IR_VOLATILE_FIELDS=0`) an IR
            // splice is still refused, as before
            // (`volatile-instance-fields-are-plain-accesses-in-compiled-code`).
            //
            // JIT round 21 wave 13 (lane cf, `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE`,
            // opt-in): the optimizing tier HAS a fenced store. Its builder's
            // `putfield` arm builds a volatile pc as an `Op::Store` marked
            // `volatile_access` (`IrBuilder::add_spliced_volatile_field_pcs` sets
            // the store admission bit beside the load's), and `ir_lower` follows
            // every such store with `LOCK ADD [RSP], 0`, spliced or not. So under
            // the switch an IR-mode body keeps its volatile store, recorded in
            // `volatile_field_pcs` like a load; the single-pass refusal stands.
            // `CompletableFuture.uniApplyNow`'s `d.result = d.encodeValue(..)` is
            // the row (`docs/feature-designs/real-jdk-mode.md`).
            if resolved.is_volatile {
                if code_bytes.get(fpc).copied() == Some(0xb5)
                    // JIT r23 wave 11b (lane L5): or the fresh-receiver form
                    // (`CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE_FRESH`; the builder
                    // proves the receiver).
                    && !(ir_mode && cratonvm_jit::ir::ir_splice_volatile_store_admitted())
                {
                    if !is_live { continue; }
                    no!("splice-volatile-field-store");
                }
                if ir_mode && !gates.ir_volatile_fields_enabled {
                    if !is_live { continue; }
                    no!("ir-splice-volatile-field");
                }
                // Recorded in BOTH modes, so a site resolved for one tier and
                // read by the other still names its volatile reads.
                volatile_field_pcs.push(fpc);
            }
            // nio21 wave 2 (lane L3): a plain read of a `final` field. Never
            // a volatile one (a field is not both, JVMS 4.5; the builder
            // refuses to forward any volatile access regardless).
            if resolved.is_final
                && !resolved.is_volatile
                && code_bytes.get(fpc).copied() == Some(0xb4)
            {
                final_field_pcs.push(fpc);
            }
            // nio21 wave 7, lane L4 (W5L4-3): the pool's `hb:[B` read is the
            // bootstrap `ByteBuffer`'s own field (the resolution found an
            // array-typed field declared by that class), so it loads a
            // `byte[]` or null. See `InlineSite::byte_array_field_pcs`.
            if byte_array_field_candidates.contains(&fpc)
                && resolved.desc_byte == b'['
                && !resolved.is_static
                && byte_array_field_declarer_admitted(shared, &resolved)
            {
                byte_array_field_pcs.push(fpc);
            }
            // JIT r20 wave 18 (lane xLib): the admitted referent load (IR
            // only, see above) gets its `field_info` row and nothing else.
            if referent_load {
                referent_load_pcs.push(fpc);
                field_info.push((fpc, resolved.field_index, type_tag));
                continue;
            }
            if gates.compact_ref_fields_enabled {
                if let Some(layout) =
                    cratonvm_types::class_layout(resolved.declaring_class_id.as_u32())
                {
                    if let (Some(off), Some(is_ref)) = (
                        layout.field_disp(resolved.field_index),
                        layout.field_is_ref(resolved.field_index),
                    ) {
                        compact_field_info.push((fpc, off, is_ref));
                    }
                }
            }
            field_info.push((fpc, resolved.field_index, type_tag));
            if let Some(decl) = jit_field_decl_identity(shared, declaring_id, cp_idx, &resolved) {
                field_decl_info.push((fpc, resolved.field_index, decl));
            }
        } else if ir_mode
        {
            if !is_live { continue; }
            // Round 15 wave 3 (lane chain3): the static loop's rule for an
            // instance field. With no `field_info` row the builder's
            // `getfield` / `putfield` arm bails the whole METHOD at this pc
            // after the splice was committed to (a W5-2 rebuild then drops
            // the TOP-LEVEL site, nest and all); refusing this body keeps
            // its call and the rest of the nest.
            no!("ir-splice-instance-field-unresolved");
        }
    }
    let mut static_field_info = Vec::new();
    for (fpc, cp_idx, type_tag, is_live) in static_sites {
        // Round 15 wave 4 (lane fieldowner): as the instance loop above.
        let site = jit_resolve_field_site(shared, declaring_id, cp_idx);
        if let Err(Some(why)) = site {
            if !is_live { continue; }
            no!(why);
        }
        if let Ok(resolved) = site {
            if !jit_field_kind_agrees(&resolved, true) {
                if !is_live { continue; }
                no!("splice-field-kind-mismatch");
            }
            if gates.jit_field_site_faithful_enabled
                && !jit_field_tag_agrees(&resolved, type_tag, cp_idx)
            {
                if !is_live { continue; }
                no!("splice-field-tag-disagrees");
            }
            if code_bytes.get(fpc).copied() == Some(0xb3)
                && final_field_put_illegal(shared, declaring_id, callee_method, &resolved)
            {
                if !is_live { continue; }
                no!("splice-final-field-store");
            }
            // Round 14 wave 2 (lane syncsplice): the only `putstatic` an
            // optimizing splice carried was a `static synchronized` body's
            // write to its own class (the scan above), non-volatile.
            //
            // JIT round 21 (lane staticfwd, FG16-3): under
            // `CRATONVM_JIT_IR_SPLICE_PUTSTATIC` also a plain write of any
            // class, when it is non-volatile (a spliced store carries no
            // StoreLoad fence) and the field's declaring class -- the one
            // `putstatic` initialises (JVMS 6.5) -- is initialised NOW. That
            // state is monotone, so the `jit_putstatic_*` helper's class-init
            // guard answers from its memo forever: the write runs no
            // `<clinit>` and cannot trap, which is what lets the builder
            // commit it and admit the class's later accesses behind the
            // replay fence.
            if ir_mode && code_bytes.get(fpc).copied() == Some(0xb3) {
                let sync_own = sync_splice
                    && resolved.declaring_class_id == declaring_id
                    && !resolved.is_volatile;
                if !sync_own {
                    if !gates.ir_splice_putstatic_enabled {
                        if !is_live { continue; }
                        no!("ir-sync-splice-putstatic-not-own-class");
                    }
                    if resolved.is_volatile {
                        if !is_live { continue; }
                        no!("ir-splice-putstatic-volatile");
                    }
                    let initialized = shared
                        .classes
                        .class_manager
                        .read()
                        .class_store()
                        .get(resolved.declaring_class_id)
                        .is_some_and(crate::vm::is_class_initialized_fast);
                    if !initialized {
                        if !is_live { continue; }
                        no!("ir-splice-putstatic-class-not-initialized");
                    }
                }
            }
            static_field_info.push((
                fpc,
                resolved.declaring_class_id.as_u32(),
                resolved.field_index,
                type_tag,
                resolved.is_volatile,
            ));
        } else if ir_mode {
            if !is_live { continue; }
            // A `getstatic` whose owner is not loaded, behind a branch edge
            // the callee's profile says never ran (`CharacterData.of`'s
            // non-Latin-1 arms, which kept `Character.digit` a call per
            // digit): a class-id-0 row, the unloaded `new`'s precise exit
            // (same rule and switch, `cold_unresolved_news` above).
            if unresolved_new_exit_admitted
                && code_bytes.get(fpc).copied() == Some(0xb2)
                && field_site_owner_unloaded(shared, declaring_id, cp_idx)
                && shared
                    .jit
                    .profile_store
                    .get_profile(&crate::jit::profile::MethodKey {
                        class_id: declaring_id.as_u32(),
                        method_name: Arc::from(callee_method),
                        descriptor: Arc::from(callee_desc),
                    })
                    .is_some_and(|p| {
                        cratonvm_jit::ir::splice_typecheck_arm_unexecuted(
                            code_bytes,
                            code_len,
                            method_code_len,
                            fpc,
                            &p,
                        )
                    })
            {
                if crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] splice-unresolved-getstatic-exit {callee_class}.{callee_method}{callee_desc} at callee_pc={fpc} depth={nest_depth}"
                    );
                }
                static_field_info.push((fpc, cratonvm_jit::UNLOADED_SPLICE_CLASS_ID, 0, type_tag, false));
                continue;
            }
            // An unresolvable site is DROPPED for the single-pass emitter,
            // which bails that one site and keeps the rest of the body. The
            // optimizing tier has no such fallback: a `getstatic` with no row
            // bails the whole METHOD, after the splice has been committed to.
            // Refuse the body instead — the callee is still compiled and still
            // called, it is just not spliced. Same trade, and the same
            // sentence, as `ir-splice-new-site-unresolved`.
            no!("ir-splice-static-field-unresolved");
        }
    }

    // Nesting: a call inside the spliced body that is itself worth splicing.
    //
    // Resolved AFTER `drop(cm)`, and it must be: this recurses into
    // `resolve_inline_site_from`, which takes its own `class_manager.read()`.
    // parking_lot's RwLock is not reentrant, and a writer queued between the
    // two reads deadlocks the thread against itself — the same lock-order
    // obligation the field-resolution phase above was split out for.
    //
    // Only STATICALLY BOUND calls are candidates. `invokestatic` and
    // `invokespecial` name their target through the constant pool, which is
    // what `resolve_inline_site` resolves; `invokevirtual` / `invokeinterface`
    // select on the runtime receiver, and this planning context has no receiver
    // profile for a callee-internal pc (the profile is keyed by the enclosing
    // method's bci). Those keep the dispatch helper — which is step 4's whole
    // point, and is why nesting does not need them.
    //
    // A nested site is ADDITIVE: the pc keeps its `invoke_targets` entry too,
    // so a nested splice that bails mid-body inside the emitter falls back to
    // the ordinary call rather than failing the outer splice.
    // IR-tier nesting is deeper and is gated on nothing but `ir_mode`. Deeper
    // because the accessor chains it exists for are four levels on their own
    // (`get` → `loadFromArray` → `Short2.<init>()V` → `Short2.<init>([S)V`),
    // and stopping one short leaves the allocation being passed to an opaque
    // call — which is an escape, so the whole splice buys nothing. Ungated
    // because `CRATONVM_JIT_INLINE_NEST` is the single-pass emitter's switch and
    // the IR path is already behind its own.
    // r25 W11-7 review fix: below a chain body (a top-level site never is).
    let in_chain_tree = nest_depth > 0 && chain_tree_flag();
    let nest_budget = if ir_mode {
        cratonvm_jit::ir_inline_max_nest_depth_in(in_chain_tree)
    } else {
        cratonvm_jit::MAX_INLINE_NEST_DEPTH
    };
    let mut nested_sites: Vec<cratonvm_jit::NestedInlineSite> = Vec::new();
    // rx27 lane bimodal: the in-body pcs whose `CharSequence.charAt` call the
    // inline String prefix answers in this copy (`chain-prefix-kept`), for the
    // keep-loop-splice-caller rule below.
    let mut string_prefix_pcs: Vec<usize> = Vec::new();
    // rx27 wave 3 (lane bimodal, `CRATONVM_JIT_IR_NESTED_HOT_CAP_BY_FREQUENCY`):
    // this body's interpreted invocation count, the denominator the
    // nested hot cap reads its rows' frequency with
    // (`cratonvm_jit::ir_nested_guarded_body_cap_in`). Asked exactly as
    // `InlineSite::callee_invocations` is below, so the planner's
    // `ir_nested_row_hot_cap` reads the same number.
    // Round rx27 wave 7 (lane chain): the receiver rows the chain entry took
    // from the callee's own artifact, carried to the planner with the
    // interpreted rows (`callee_receiver_profile` below).
    let mut entry_hot_rows: Vec<(usize, cratonvm_jit::profile::ReceiverCounts)> = Vec::new();
    let cap_invocations =
        if ir_mode && gates.ir_nested_chain_splice_enabled {
            shared.jit.profile_store.invocation_count(cratonvm_jit_api::invoc_key_parts(
                declaring_id.as_u32(),
                callee_method,
                callee_desc,
            ))
        } else {
            0
        };
    // `CRATONVM_JIT_IR_SPLICE_LEAF_PAST_DEPTH`: at the cap, static leaf bodies
    // only (`ir_leaf_splice_body`), which nest nothing further.
    let sun_unsafe_bridge = ir_mode
        && (callee_class == "sun/misc/Unsafe"
            || invoke_targets
                .iter()
                .any(|(_, t)| t.class_name == "sun/misc/Unsafe"));
    let leaf_tier = ir_mode
        && !sun_unsafe_bridge
        && nest_depth + 1 == nest_budget
        && gates.ir_splice_leaf_past_depth_enabled;
    if !planning_only && (nest_depth + 1 < nest_budget || leaf_tier)
        && (ir_mode || crate::runtime::env_cache::jit_inline_nest())
    {
        // The CALLEE's own receiver profile, fetched once for the whole body.
        //
        // This is the piece that makes devirtualising inside a splice possible
        // at all, and it needed no new profiling: receiver types are recorded
        // against the bci of the method that is EXECUTING, so a virtual call
        // inside `objectsAreEqual` is already profiled under
        // `objectsAreEqual`'s own `MethodKey` at its own bci — exactly the
        // (method, pc) pair a nested site names. The enclosing method's profile
        // never had this and never could, which is why re-keying it by
        // (caller pc, callee pc) was the wrong shape to reach for.
        // Fetched on the first site that reads it (`LazyCalleeProfile`): the
        // snapshot clones every profile map, and a body whose calls are all
        // static or already refused never reads it.
        let callee_profile = LazyCalleeProfile {
            shared,
            wanted: crate::runtime::env_cache::jit_inline_splice_devirt()
                .then_some((declaring_id, callee_method, callee_desc)),
            cell: std::cell::OnceCell::new(),
        };
        // JIT r25 wave 7 (W7-B, `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`, default
        // ON since the wave-10 flip, `=0` kills it):
        // a body that calls its own selector virtually (`next.match` inside
        // `Pattern$X.match`) is a dispatch-chain link. Its chain sites take
        // the profiled route past a contradicted nest-exact row, their bodies
        // the chain admission (`cratonvm_jit::r25_w7b_chain_splice`), and its
        // interface sites (`predicate.is`, `seq.charAt`) the profiled route
        // too, so a link spliced one level up keeps no call in its loops.
        let chain_on =
            ir_mode && gates.ir_nested_chain_splice_enabled;
        // W7-B fix (round 2): a chain call is of the body's own class family
        // as well: never on `java/lang/Object` (`is_chain_target`), and this
        // body's class is a subclass of the call's constant-pool class
        // (`Pattern$GroupHead` of `Pattern$Node`). `AbstractSet.hashCode`'s
        // element `hashCode()` is no chain. Asked under a short read of the
        // class manager, dropped before any nested resolution.
        let chain_family = |t: &cratonvm_jit::InlineInvokeTarget| -> bool {
            if !cratonvm_jit::r25_w7b_chain_splice::is_chain_target(t, callee_method, callee_desc) {
                return false;
            }
            // W7-B fix 2: and the constant-pool class is a class, not an
            // interface (`Pattern$Node`; a decorator's `c.iterator()` on a
            // `Collection` is delegation, not a dispatch chain).
            let cm = shared.classes.class_manager.read();
            jit_known_class(shared, &cm, &t.class_name, declaring_id).is_some_and(|cp| {
                cm.get_class(cp).is_some_and(|c| !c.is_interface())
                    && cm.is_subclass_of(declaring_id, cp)
            })
        };
        let chain_body = chain_on && invoke_targets.iter().any(|(_, t)| chain_family(t));
        // r25 W11-7 review fix: this body's nested sites resolve inside a
        // chain tree when it is one, a chain body, or a lambda body whose
        // interface rows W10-4 opens.
        let thin_forwarding_spine = ir_mode
            && code_len <= 32
            && !invoke_targets.is_empty()
            && invoke_targets.len() <= 2
            && invoke_targets
                .iter()
                .all(|(_, t)| t.method_name != "<init>" && t.class_name != "java/lang/Object");
        let children_in_chain_tree = in_chain_tree
            || chain_body
            || thin_forwarding_spine
            || (ir_mode
                && cratonvm_jit::r25_w10_4_lambda_iface::admits_body(callee_method)
                && invoke_targets.iter().any(|(_, t)| t.invoke_kind == 2));
        let enclosing_has_run_for_cold_static = never_run_static_exit
            && !never_run_static_candidates.is_empty()
            && eager_static_callee_has_run(shared, declaring_id, callee_method, callee_desc);
        let has_run_sibling_static = enclosing_has_run_for_cold_static
            && never_run_static_candidates.iter().any(|&(p, decl)| {
                invoke_targets
                    .iter()
                    .find(|(ip, _)| *ip == p)
                    .is_some_and(|(_, t)| {
                        eager_static_callee_has_run(shared, decl, &t.method_name, &t.descriptor)
                    })
            });
        let early_return_without_prior_static = enclosing_has_run_for_cold_static
            .then(|| {
                ir_return_pcs
                    .first()
                    .copied()
                    .filter(|&ret_pc| !invoke_targets.iter().any(|(ip, _)| *ip < ret_pc))
            })
            .flatten();
        let _chain_tree = ChainTreeScope::enter(children_in_chain_tree);
        // W7-B fix 2: this body's invocation count, the denominator of the
        // chain frequency rule (`chain_frequency_verdict`); 0 is no evidence.
        let chain_invocations = if chain_body {
            shared
                .jit
                .profile_store
                .invocation_count(cratonvm_jit_api::invoc_key_parts(
                    declaring_id.as_u32(),
                    callee_method,
                    callee_desc,
                ))
        } else {
            0
        };
        for (ipc, target) in &invoke_targets {
            // In IR mode every kind takes the statically-bound path: the
            // monomorphism gate on the selected method is what decides whether
            // a virtual target is spliceable, and it refuses the ones a guard
            // would otherwise have to cover. There is no guarded arm to fall
            // into, so routing `0`/`2` to the profile-driven branch below would
            // only produce sites the IR consumer must throw away.
            let kind = if ir_mode { 3 } else { target.invoke_kind };
            if leaf_tier && target.invoke_kind != 3 {
                continue;
            }
            // JIT r25 wave 7 (W7-B): this call is the chain's next link.
            let chain_site = chain_body && chain_family(target);
            // A scalar row stays a call row in this body: the planner lowers it as
            // arithmetic (`ir_plan_spliced_scalar_intrinsics`), where nesting its
            // JDK body (`Integer.compress`, ~100 bytes of bit loop) would win instead.
            if ir_mode
                && target.invoke_kind == 3
                && cratonvm_jit::ir_splice_lowers_static_scalar_row(
                    &target.class_name,
                    &target.method_name,
                    &target.descriptor,
                )
            {
                continue;
            }
            match kind {
                // Statically bound: one body, no guard.
                1 | 3 => {
                    // Round 11 wave 11 (lane `calls`): a multi-return body's
                    // call to ITSELF (admitted only for bounded self-recursive
                    // splicing, see `multi_return_if_self_recursive`) is nested
                    // only as deep as that splice can use:
                    // `IR_RECURSIVE_INLINE_MAX_COPIES` copies, plus the level
                    // whose self-calls become the direct self-call. Past it every
                    // level multiplies the tree by the method's fan-out -- 126
                    // resolutions for `fib` at the full depth against 2 + 4 + 8
                    // here -- for bodies `append_ir_inline_site` never splices.
                    let max_rec = cratonvm_jit::ir::ir_recursive_inline_max_copies(code_len);
                    if ir_mode
                        && ir_return_pcs.len() > 1
                        && nest_depth >= max_rec
                        && target.invoke_kind == 3
                        && target.class_name == callee_class
                        && target.method_name == callee_method
                        && target.descriptor == callee_desc
                        && gates.ir_recursive_inline_enabled
                    {
                        continue;
                    }
                    if enclosing_has_run_for_cold_static
                        && target.invoke_kind == 3
                        && target.direct_entry.is_none()
                        && (has_run_sibling_static
                            || early_return_without_prior_static.is_some_and(|ret_pc| *ipc > ret_pc))
                        && never_run_static_candidates.iter().any(|&(p, decl)| {
                            p == *ipc
                                && !eager_static_callee_has_run(
                                    shared,
                                    decl,
                                    &target.method_name,
                                    &target.descriptor,
                                )
                        })
                    {
                        continue;
                    }
                    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: what this
                    // call's operands are known to be, handed to the body's
                    // own resolution (and keyed on below for a virtual call).
                    let mut operand_objects: Vec<Option<usize>> = Vec::new();
                    let operand_classes = nested_operand_classes(
                        shared,
                        code,
                        code_len,
                        *ipc,
                        &target.descriptor,
                        target.invoke_kind != 3,
                        &body_handler_pcs,
                        &splice_arg_classes,
                        &static_field_info,
                        &splice_arg_objects,
                        &field_info,
                        &|at| {
                            invoke_targets
                                .iter()
                                .find(|(p, _)| *p == at)
                                .map(|(_, t)| {
                                    (t.num_jit_args, usize::from(!t.descriptor.ends_with(")V")))
                                })
                        },
                        &mut operand_objects,
                    );
                    // `CRATONVM_FROZEN_PAGES`, stage 2: a virtual call's
                    // receiver nothing names is the object this site is nearly
                    // always called on, and its class the snapshot's: an exact
                    // row the builder proves with an identity guard at the call
                    // (`IrBuilder::frozen_prove_site_receiver`).
                    let mut operand_classes = operand_classes;
                    if matches!(target.invoke_kind, 0 | 2) && operand_objects.first().is_none_or(Option::is_none) {
                        if let Some(addr) = crate::runtime::frozen_pages::site_receiver(
                            shared,
                            declaring_id.as_u32(),
                            callee_method,
                            callee_desc,
                            *ipc,
                        ) {
                            if operand_objects.is_empty() {
                                operand_objects.push(None);
                            }
                            operand_objects[0] = Some(addr);
                            let class = shared.classes.frozen.facts.read().as_ref().and_then(|facts| {
                                facts
                                    .object_id(addr)
                                    .and_then(|id| facts.objects.get(id as usize))
                                    .map(|o| o.class_id)
                            });
                            if let Some(class) = class.filter(|&c| c != 0) {
                                if operand_classes.is_empty() {
                                    operand_classes.push(None);
                                }
                                operand_classes[0].get_or_insert(class);
                            }
                        }
                    }
                    cratonvm_jit::ir::set_splice_arg_classes(operand_classes.clone());
                    cratonvm_jit::ir::set_splice_arg_objects(operand_objects.clone());
                    // The r23 L8 heap-buffer close: an `invokespecial` of the
                    // body's direct superclass is bound by the language
                    // (`ir_super_special_splice_enabled`).
                    let super_special = ir_mode
                        && target.invoke_kind == 1
                        && target.method_name != "<init>"
                        && gates.ir_super_special_splice_enabled
                        && {
                            let cm = shared.classes.class_manager.read();
                            cm.get_class(declaring_id)
                                .and_then(|c| c.superclass)
                                .and_then(|s| cm.get_class(s))
                                .is_some_and(|s| *s.name == *target.class_name)
                        };
                    IR_SUPER_SPECIAL_SITE.with(|c| c.set(super_special));
                    let nested = resolve_inline_site_from(
                        shared,
                        declaring_id,
                        None,
                        &target.class_name,
                        &target.method_name,
                        &target.descriptor,
                        nest_depth + 1,
                        direct_bind,
                        ir_mode,
                        None,
                        false,
                    );
                    IR_SUPER_SPECIAL_SITE.with(|c| c.set(false));
                    // Round 14 wave 6 (lane sync6, SS8-1 a): a synchronized
                    // nested body only under a synchronized INSTANCE body at
                    // depth 0 (the gate above admits it at depth 1); elsewhere
                    // the call stays, as before.
                    let nested = nested.filter(|s| {
                        !s.callee_is_synchronized || (sync_splice && !is_static && nest_depth == 0)
                    });
                    // JIT r25 wave 10 (W10-3): under the chain switch the
                    // leaf tier takes up to 24 bytes (`ASCII.getType`, 17).
                    let nested = nested.filter(|s| {
                        !leaf_tier
                            || (s.callee_is_static
                                && cratonvm_jit::r25_w7b_chain_splice::chain_leaf_splice_body_in(
                                    &s.callee_code,
                                    s.callee_code_len,
                                    children_in_chain_tree,
                                ))
                    });
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] nest-static {}.{}{} at callee_pc={} depth={} -> {}",
                            target.class_name,
                            target.method_name,
                            target.descriptor,
                            ipc,
                            nest_depth + 1,
                            if nested.is_some() {
                                "SPLICED"
                            } else {
                                "refused"
                            },
                        );
                    }
                    // JIT r23 wave 5, lane L2
                    // (`CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER`, opt-in): this
                    // body, spliced at depth 0, holds a SAM call whose own
                    // receiver profile names one lambda proxy, and that call
                    // stays a call here (the forwarder splices only in the
                    // body's own compile, as its depth-0 guarded site). Spliced,
                    // every execution pays the compiled lambda arm -- ~600 ns
                    // for an adapting forwarder (`BoxRef`), which installs no
                    // machine thunk. Kept as a call, the caller reaches the
                    // body's own optimizing artifact, where the forwarder and
                    // its implementation are spliced. Asked only for a body
                    // whose forwarder the depth-0 resolver admits.
                    // A receiver the frozen snapshot names takes the exact row
                    // below instead: no class test, and its captures known.
                    if nested.is_none()
                        && ir_mode
                        && nest_depth == 0
                        && matches!(target.invoke_kind, 0 | 2)
                        && gates.keep_lambda_site_caller_enabled
                        && operand_objects.first().copied().flatten().is_none()
                    {
                        let proxy = one_lambda_proxy_receiver(
                            callee_profile.as_ref().and_then(|p| p.receivers.get(ipc)),
                        );
                        if let Some(proxy) = proxy {
                            // JIT r23 wave 9, lane L5
                            // (`CRATONVM_JIT_IR_NESTED_LAMBDA_SAM_GUARDED`,
                            // default ON): before keeping the whole body as a
                            // call, splice the SAM call's forwarder one level
                            // down behind a class test on that proxy (a nested
                            // guarded row, call miss edge), which the nested
                            // lambda route serves
                            // (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_NESTED`). The
                            // general interface rows for this are opt-in
                            // (`CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE`,
                            // `_SPLICE_NESTED_PROFILE_TRAP`), so without this
                            // `Pattern$BmpCharPropertyGreedy.match` (its
                            // `CharPredicate.is` names one proxy) was never
                            // spliced into `Pattern$Start.match`'s
                            // per-position loop: a full compiled call per text
                            // position on `split` with a `\s+` pattern.
                            // Refused here: the rule below, unchanged.
                            if gates.nested_lambda_sam_guarded_enabled
                                && gates.ir_nested_guarded_splice_enabled
                                && gates.ir_lambda_sam_splice_nested_enabled
                                && gates.ir_splice_chain_fences_enabled
                                && gates.ir_splice_nested_exact_receiver_enabled
                                && shared.classes.is_lambda_proxy_class(ClassId::new(proxy))
                            {
                                let cap = cratonvm_jit::ir_nested_guarded_body_cap_in(
                                    callee_profile.as_ref().and_then(|p| p.receivers.get(ipc)),
                                    cap_invocations,
                                    code,
                                    code_len,
                                    *ipc,
                                    1,
                                );
                                let guarded = resolve_inline_site_from(
                                    shared,
                                    declaring_id,
                                    Some(ClassId::new(proxy)),
                                    &target.class_name,
                                    &target.method_name,
                                    &target.descriptor,
                                    nest_depth + 1,
                                    direct_bind,
                                    true,
                                    None,
                                    false,
                                )
                                .filter(|s| {
                                    !s.callee_is_static
                                        && cratonvm_jit::ir_nested_hot_body_admitted(s, cap)
                                });
                                if crate::runtime::env_cache::dbg_jitc() {
                                    eprintln!(
                                        "[cratonvm-jitc] nest-lambda-guarded {}.{}{} at callee_pc={} depth={} -> proxy {} {}",
                                        target.class_name,
                                        target.method_name,
                                        target.descriptor,
                                        ipc,
                                        nest_depth + 1,
                                        proxy,
                                        if guarded.is_some() {
                                            "(forwarder spliced behind a class test)"
                                        } else {
                                            "refused (the keep-the-caller rule decides)"
                                        },
                                    );
                                }
                                if let Some(site) = guarded {
                                    // As the nest-profiled rows note theirs:
                                    // a redefinition of the guard's class
                                    // withdraws this compile.
                                    shared.jit.jit_cache.note_bytecode_copied(ClassId::new(proxy));
                                    nested_sites.push(cratonvm_jit::NestedInlineSite {
                                        callee_pc: *ipc,
                                        guard_class_id: proxy,
                                        site,
                                        bimorphic_second: None,
                                    });
                                    continue;
                                }
                            }
                            if resolve_ir_lambda_sam_site(
                                shared,
                                declaring_id,
                                ClassId::new(proxy),
                                &target.class_name,
                                &target.method_name,
                                &target.descriptor,
                                direct_bind,
                                0,
                                planning_only,
                                None,
                            )
                            .is_some()
                            {
                                no!(format!(
                                    "keep-lambda-site-caller: the SAM call at callee_pc={ipc} \
                                     splices only in this body's own compile"
                                ));
                            }
                        }
                    }
                    // Round 13 wave 10 (lane iropt8, HS-2): an `invokevirtual`
                    // the language rule refused is resolved again for a
                    // receiver of exactly its constant-pool class, and the row
                    // is marked with that class. The IR consumer splices it
                    // only for a receiver it proves exactly that class (the
                    // exact `this` of an enclosing exact-receiver splice) and
                    // lowers the kept call row otherwise.
                    // JIT round 20 wave 11 (lane pSplice, W5L4-5): an
                    // `invokeinterface` on `this` in a forwarder body resolved
                    // for an exact receiver is resolved for that receiver
                    // (`resolve_ir_nested_iface_forwarder_site`); `None` keeps
                    // the arms below exactly as before.
                    let iface_forwarder = match receiver_class_id {
                        Some(receiver)
                            if nested.is_none()
                                && ir_mode
                                && !is_static
                                && target.invoke_kind == 2 =>
                        {
                            resolve_ir_nested_iface_forwarder_site(
                                shared,
                                declaring_id,
                                receiver,
                                &target.class_name,
                                &target.method_name,
                                &target.descriptor,
                                nest_depth + 1,
                                direct_bind,
                                code,
                                code_len,
                                *ipc,
                                target.num_jit_args,
                            )
                        }
                        _ => None,
                    };
                    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: an
                    // `invokevirtual` whose receiver is a trusted `static
                    // final` (`HttpStatusClass.UNKNOWN.contains(code)`) or an
                    // argument the caller proved (`obj1.equals(obj2)` in
                    // `AssertionUtils.objectsAreEqual`) is resolved for that
                    // exact class, by JVMS selection from it, and marked with
                    // it: a nest-exact row the builder splices only where it
                    // proves the same receiver, and keeps the call otherwise.
                    let mut static_final_row: Option<(cratonvm_jit::InlineSite, u32)> = None;
                    // An `invokeinterface` row too: the builder splices an
                    // exact row there only for a proven receiver, as here.
                    if nested.is_none()
                        && iface_forwarder.is_none()
                        && ir_mode
                        && matches!(target.invoke_kind, 0 | 2)
                        && gates.ir_splice_chain_fences_enabled
                        && gates.ir_splice_nested_exact_receiver_enabled
                    {
                        if let Some(class_id) = operand_classes.first().copied().flatten() {
                            let receiver = ClassId::new(class_id);
                            {
                                // Under the guard, as the other receiver
                                // resolvers mark theirs.
                                let _cm = shared.classes.class_manager.read();
                                shared.jit.jit_cache.note_bytecode_copied(receiver);
                            }
                            let mut cap = cratonvm_jit::ir_psplice_nested_exact_row_cap(
                                callee_profile.as_ref().and_then(|p| p.receivers.get(ipc)),
                                code,
                                code_len,
                                *ipc,
                                target.num_jit_args,
                            );
                            // A receiver the frozen snapshot names is one object the
                            // builder guards once per compile: its body replaces the call
                            // with no class test, so it takes the hot cap.
                            // shortcut: code-size policy, not measured on code-cache-bound
                            // rows; revisit if a frozen chain bloats a compiled body.
                            if operand_objects.first().copied().flatten().is_some() {
                                cap = cap.max(cratonvm_jit::MAX_INLINE_BYTECODE_SIZE);
                            }
                            cratonvm_jit::ir::set_splice_arg_classes(operand_classes.clone());
                            cratonvm_jit::ir::set_splice_arg_objects(operand_objects.clone());
                            // The filter below, asked before the body's nested
                            // sites resolve.
                            set_exact_row_size_cap(cap);
                            let site = resolve_inline_site_from(
                                shared,
                                declaring_id,
                                Some(receiver),
                                &target.class_name,
                                &target.method_name,
                                &target.descriptor,
                                nest_depth + 1,
                                direct_bind,
                                true,
                                None,
                                false,
                            );
                            let body_len = site.as_ref().map(|s| s.callee_code_len);
                            let site = site
                                .filter(|s| !s.callee_is_static && s.callee_code_len <= cap);
                            if crate::runtime::env_cache::dbg_jitc() {
                                eprintln!(
                                    "[cratonvm-jitc] nest-static-final {}.{}{} at callee_pc={} depth={} -> class {} {} (body {:?}, cap {})",
                                    target.class_name,
                                    target.method_name,
                                    target.descriptor,
                                    ipc,
                                    nest_depth + 1,
                                    class_id,
                                    if site.is_some() {
                                        "(spliced where the receiver is proven)"
                                    } else {
                                        "refused"
                                    },
                                    body_len,
                                    cap,
                                );
                            }
                            static_final_row = site.map(|s| (s, class_id));
                        }
                    }
                    let (nested, exact_class) = match nested {
                        Some(site) => (Some(site), 0u32),
                        None if iface_forwarder.is_some() => match iface_forwarder {
                            Some((site, class_id)) => (Some(site), class_id),
                            None => (None, 0),
                        },
                        None if static_final_row.is_some() => match static_final_row.take() {
                            Some((site, class_id)) => (Some(site), class_id),
                            None => (None, 0),
                        },
                        None if ir_mode
                            && target.invoke_kind == 0
                            && target.class_name != "java/lang/Object"
                            && gates.ir_splice_chain_fences_enabled
                            && gates.ir_splice_nested_exact_receiver_enabled =>
                        {
                            // JIT r25 wave 7 (W7-B): the drop below also at a
                            // chain site.
                            // rx27 wave 3 (lane bimodal,
                            // `CRATONVM_JIT_IR_NESTED_EXACT_YIELDS_TO_PROFILE`):
                            // and at any site, so the profiled route below
                            // plans the class the evidence names
                            // (`Matcher.match` pc 121's `matchRoot.match`).
                            let drop_contradicted =
                                gates.ir_splice_nested_exact_trap_enabled
                                    || chain_site
                                    || gates.nested_exact_yields_to_profile_enabled
                                    // rx27 wave 7 (lane chain): the chain
                                    // entry needs the profiled route there.
                                    || cratonvm_jit::rx27_chain_entry::enabled();
                            match resolve_ir_nested_exact_receiver_site(
                                shared,
                                declaring_id,
                                &target.class_name,
                                &target.method_name,
                                &target.descriptor,
                                nest_depth + 1,
                                direct_bind,
                                // nio21 wave 5, lane L4: the planner's cap for
                                // this row (hot for a forwarder body). JIT
                                // round 20 wave 11 (lane pSplice, W7L4-5):
                                // or the nested hot cap of this pc, the body
                                // judged below by the planner's own rule.
                                cratonvm_jit::ir_psplice_nested_exact_row_cap(
                                    callee_profile.as_ref().and_then(|p| p.receivers.get(ipc)),
                                    code,
                                    code_len,
                                    *ipc,
                                    target.num_jit_args,
                                ),
                            )
                            .filter(|(site, _)| {
                                cratonvm_jit::ir_psplice_nested_exact_row_admitted(
                                    site,
                                    callee_profile.as_ref().and_then(|p| p.receivers.get(ipc)),
                                    code,
                                    code_len,
                                    *ipc,
                                    target.num_jit_args,
                                )
                            }) {
                                // Round nio20 wave 3, lane L4 (L3W2-3): under
                                // `CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP`
                                // the builder may splice this row behind a
                                // trap guard where the receiver is NOT proven
                                // (`IrBuilder::nested_trap_guard_plan`), so a
                                // row whose evidence names another receiver
                                // class at this bci is dropped (the call
                                // stays) rather than trap on every subclass.
                                // JIT r25 wave 7 (W7-B): a chain site's row is
                                // dropped on the same evidence, so the
                                // profiled route below plans the link the
                                // receiver profile names (`Pattern$Node`'s own
                                // body never arrives at `next.match`).
                                Some((_, class_id))
                                    if drop_contradicted
                                        && ir_nested_receiver_evidence_contradicts(
                                            shared,
                                            callee_profile.as_ref(),
                                            &inlined_body_class_name,
                                            callee_method,
                                            callee_desc,
                                            declaring_id,
                                            *ipc,
                                            class_id,
                                        ) =>
                                {
                                    if crate::runtime::env_cache::dbg_jitc() {
                                        eprintln!(
                                            "[cratonvm-jitc] nest-exact {}.{}{} at callee_pc={} depth={} -> class {} dropped (the receiver evidence names another class)",
                                            target.class_name,
                                            target.method_name,
                                            target.descriptor,
                                            ipc,
                                            nest_depth + 1,
                                            class_id,
                                        );
                                    }
                                    (None, 0)
                                }
                                Some((site, class_id)) => {
                                    if crate::runtime::env_cache::dbg_jitc() {
                                        eprintln!(
                                            "[cratonvm-jitc] nest-exact {}.{}{} at callee_pc={} depth={} -> class {} (spliced where the receiver is proven)",
                                            target.class_name,
                                            target.method_name,
                                            target.descriptor,
                                            ipc,
                                            nest_depth + 1,
                                            class_id,
                                        );
                                    }
                                    // JIT round 19 wave 3 (lane splice3,
                                    // SPLICE2-4,
                                    // `CRATONVM_JIT_IR_NESTED_BIMORPHIC_EXACT_ROW`):
                                    // a callee profile naming a bimorphic
                                    // pair that contains the constant-pool
                                    // class resolves the pair's other class
                                    // as this row's second body, exactly as
                                    // `nest-profiled` resolves a pair
                                    // (SPLICE19-4). The builder tests it only
                                    // on the guarded form's miss edge; a
                                    // receiver proven exact keeps the
                                    // unguarded splice. Refused: the one-class
                                    // HS-2 row, as with the switch off.
                                    let second =
                                        if gates.ir_nested_bimorphic_exact_row_enabled
                                        {
                                            callee_profile
                                            .as_ref()
                                            .and_then(|p| p.receivers.get(ipc))
                                            .and_then(cratonvm_jit::ir::nested_bimorphic_receiver_pair)
                                            .and_then(|(a, b)| {
                                                if a == class_id {
                                                    Some(b)
                                                } else if b == class_id {
                                                    Some(a)
                                                } else {
                                                    None
                                                }
                                            })
                                            .and_then(|other| {
                                                let body = resolve_inline_site_from(
                                                    shared,
                                                    declaring_id,
                                                    Some(ClassId::new(other)),
                                                    &target.class_name,
                                                    &target.method_name,
                                                    &target.descriptor,
                                                    nest_depth + 1,
                                                    direct_bind,
                                                    true,
                                                    None,
                                                    false,
                                                )
                                                .filter(|s| {
                                                    !s.callee_is_static
                                                        && s.callee_code_len
                                                            <= cratonvm_jit::MAX_INLINE_SIZE_COLD
                                                })?;
                                                // As the guard classes of
                                                // `nest-profiled`: a
                                                // redefinition withdraws this
                                                // compile.
                                                shared
                                                    .jit
                                                    .jit_cache
                                                    .note_bytecode_copied(ClassId::new(other));
                                                Some((body, other))
                                            })
                                        } else {
                                            None
                                        };
                                    if let Some((body, other)) = second {
                                        if crate::runtime::env_cache::dbg_jitc() {
                                            eprintln!(
                                                "[cratonvm-jitc] nest-exact {}.{}{} at callee_pc={} depth={} -> second class {other} (bimorphic exact row)",
                                                target.class_name,
                                                target.method_name,
                                                target.descriptor,
                                                ipc,
                                                nest_depth + 1,
                                            );
                                        }
                                        nested_sites.push(cratonvm_jit::NestedInlineSite {
                                            callee_pc: *ipc,
                                            guard_class_id: class_id,
                                            site,
                                            bimorphic_second: Some(Box::new(
                                                cratonvm_jit::NestedInlineSite {
                                                    callee_pc: *ipc,
                                                    guard_class_id: other,
                                                    site: body,
                                                    bimorphic_second: None,
                                                },
                                            )),
                                        });
                                        continue;
                                    }
                                    (Some(site), class_id)
                                }
                                None => (None, 0),
                            }
                        }
                        // Round nio20 wave 2, lane L3 (`utfencode`): an
                        // `invokeinterface` inside a spliced body whose
                        // receivers were ONE class only (the callee's own
                        // profile at that bci, else its compiled artifact's
                        // inline cache with no miss) -- `StringCharBuffer.get`'s
                        // `str.charAt(..)`, a `String` on every call -- is
                        // resolved for that class and marked with it, like a
                        // nest-exact row. The builder splices it behind an
                        // exact class test whose miss edge DEOPTS through the
                        // chain snapshot at the call
                        // (`IrBuilder::nested_trap_guard_plan`), and keeps the
                        // call where it cannot. An `invokeinterface` row is
                        // never a nest-exact one (those are `invokevirtual`
                        // only), which is how the consumer tells them apart.
                        // `CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP`.
                        None if ir_mode
                            && target.invoke_kind == 2
                            && gates.ir_splice_chain_fences_enabled
                            && gates.ir_splice_nested_exact_receiver_enabled
                            && gates.ir_splice_nested_profile_trap_enabled =>
                        {
                            // The artifact is cached under the body's own
                            // class: the declaring class for a receiver
                            // resolution (`StringCharBuffer` for a site
                            // declared on `CharBuffer`).
                            match ir_nested_trap_receiver_class(
                                shared,
                                callee_profile.as_ref(),
                                &inlined_body_class_name,
                                callee_method,
                                callee_desc,
                                declaring_id,
                                *ipc,
                            ) {
                                Some(class_id) => {
                                    let receiver = ClassId::new(class_id);
                                    {
                                        // Under the guard, as the other
                                        // receiver resolvers mark theirs.
                                        let _cm = shared.classes.class_manager.read();
                                        shared.jit.jit_cache.note_bytecode_copied(receiver);
                                    }
                                    let site = resolve_inline_site_from(
                                        shared,
                                        declaring_id,
                                        Some(receiver),
                                        &target.class_name,
                                        &target.method_name,
                                        &target.descriptor,
                                        nest_depth + 1,
                                        direct_bind,
                                        true,
                                        None,
                                        false,
                                    )
                                    .filter(|s| {
                                        !s.callee_is_static
                                            && s.callee_code_len
                                                <= cratonvm_jit::MAX_INLINE_SIZE_COLD
                                    });
                                    if crate::runtime::env_cache::dbg_jitc() {
                                        eprintln!(
                                            "[cratonvm-jitc] nest-profile-trap {}.{}{} at callee_pc={} depth={} -> class {} {}",
                                            target.class_name,
                                            target.method_name,
                                            target.descriptor,
                                            ipc,
                                            nest_depth + 1,
                                            class_id,
                                            if site.is_some() { "(spliced behind a trap guard)" } else { "refused" },
                                        );
                                    }
                                    match site {
                                        Some(site) => (Some(site), class_id),
                                        None => (None, 0),
                                    }
                                }
                                None => (None, 0),
                            }
                        }
                        None => (None, 0),
                    };
                    // JIT round 19 (lane splice19, CH6W5-1 stage B): an
                    // `invokevirtual` with neither a monomorphic body nor an
                    // exact constant-pool row (`Object.equals` / `hashCode`
                    // on a map key, an abstract declaration) is resolved for
                    // the class the CALLEE's own receiver profile -- or its
                    // compiled body's inline cache -- says dominates this pc,
                    // and marked with that class. The IR consumer splices it
                    // for a receiver proven exactly that class, and otherwise
                    // behind an exact class test whose miss edge is the call
                    // row kept here (`IrBuilder::add_nested_guarded_receiver_site`,
                    // same switch). Off: nothing here runs.
                    // JIT round 19 proposal wave 5 (lane mega5, MEGAOSR4-1,
                    // `CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE`): an
                    // `invokeinterface` takes the same profile-driven row.
                    // The builder's `0xb9` arm asks the same exact proof and
                    // guarded plan as `0xb6` (W7-1 one level down), and the
                    // resolver below selects the receiver class's body by
                    // JVMS selection (default methods included). Off: kind 0
                    // only, exactly as before.
                    // JIT r25 wave 7 (W7-B): and every interface site of a
                    // dispatch-chain body.
                    // JIT r25 wave 10 (W10-4,
                    // `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE`): and every
                    // interface site of a lambda implementation body
                    // (`CharPredicate.lambda$union$2`'s `is(ch) || p.is(ch)`).
                    let lambda_body_iface = ir_mode
                        && target.invoke_kind == 2
                        && cratonvm_jit::r25_w10_4_lambda_iface::admits_body(callee_method);
                    if lambda_body_iface && crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] nest-lambda-iface {}.{}{} at callee_pc={} depth={} in {}.{}{} -> the profiled interface route is open (see the nest-profiled-iface line)",
                            target.class_name,
                            target.method_name,
                            target.descriptor,
                            ipc,
                            nest_depth + 1,
                            inlined_body_class_name,
                            callee_method,
                            callee_desc,
                        );
                    }
                    // JIT r25 wave 10 (W10-3b, `CRATONVM_JIT_IR_CHAIN_STRING_PREFIX_KEPT`):
                    // a chain / lambda-body interface site the inline String
                    // prefix answers as a call (`CharSequence.charAt` on a
                    // String: one class test, the coder byte, a bounds compare
                    // and a load) keeps the call; its bytecode splice
                    // (`String.charAt` -> `StringLatin1.charAt` -> `checkIndex`)
                    // ran about twice the instructions per character.
                    let prefix_kept = target.invoke_kind == 2
                        && gates.prefix_kept_enabled
                        && cratonvm_jit::r25_w10_3b::site_answered_by_string_prefix(
                            &target.class_name,
                            &target.method_name,
                            &target.descriptor,
                            target.invoke_kind,
                        );
                    if prefix_kept && (chain_body || lambda_body_iface) {
                        string_prefix_pcs.push(*ipc);
                    }
                    if prefix_kept
                        && (chain_body || lambda_body_iface)
                        && crate::runtime::env_cache::dbg_jitc()
                    {
                        eprintln!(
                            "[cratonvm-jitc] chain-prefix-kept {}.{}{} at callee_pc={} depth={} in {}.{}{} -> kept as a call (the inline String prefix answers it)",
                            target.class_name,
                            target.method_name,
                            target.descriptor,
                            ipc,
                            nest_depth + 1,
                            inlined_body_class_name,
                            callee_method,
                            callee_desc,
                        );
                    }
                    let profiled_kind = cratonvm_jit::r25_w10_3b::nested_profiled_kind(
                        target.invoke_kind,
                        gates.ir_nested_guarded_interface_enabled,
                        chain_body || lambda_body_iface,
                        prefix_kept,
                    );
                    let profiled_tag = if target.invoke_kind == 2 {
                        "nest-profiled-iface"
                    } else {
                        "nest-profiled"
                    };
                    let (nested, exact_class) = match (nested, exact_class) {
                        (None, _)
                            if ir_mode
                                && profiled_kind
                                && gates.ir_nested_guarded_splice_enabled
                                && gates.ir_splice_chain_fences_enabled
                                && gates.ir_splice_nested_exact_receiver_enabled =>
                        {
                            // JIT round 19 wave 2 (lane splice2, SPLICE19-4,
                            // `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE`): a
                            // callee profile that names a bimorphic pair (the
                            // top-level GS-1 shares) resolves BOTH classes;
                            // the second body rides on the first row
                            // (`NestedInlineSite::bimorphic_second`) and the
                            // builder tests it on the first test's miss edge.
                            // Either body refused: the one-class rule below,
                            // exactly as with the switch off.
                            // JIT round 20 (lane splice): a hot site's two
                            // bodies share the hot cap
                            // (`CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP`).
                            let pair_cap = cratonvm_jit::ir_nested_guarded_body_cap_in(
                                callee_profile.as_ref().and_then(|p| p.receivers.get(ipc)),
                                cap_invocations,
                                code,
                                code_len,
                                *ipc,
                                2,
                            );
                            let resolve_for = |d: u32| {
                                // The caller's facts about the call's operands
                                // hold on both edges of the class test; the
                                // body's own exact rows still need the builder's
                                // proof. Without them every class below this
                                // body was unknown.
                                cratonvm_jit::ir::set_splice_arg_classes(operand_classes.clone());
                                resolve_inline_site_from(
                                    shared,
                                    declaring_id,
                                    Some(ClassId::new(d)),
                                    &target.class_name,
                                    &target.method_name,
                                    &target.descriptor,
                                    nest_depth + 1,
                                    direct_bind,
                                    true,
                                    None,
                                    false,
                                )
                                .filter(|s| {
                                    !s.callee_is_static
                                        && (cratonvm_jit::ir_nested_hot_body_admitted(s, pair_cap)
                                            // JIT r25 wave 7 (W7-B): or a
                                            // chain link's share of the pair.
                                            || (chain_site
                                                && false
                                                // W7-B fix 2: and frequent.
                                                && cratonvm_jit::r25_w7b_chain_splice::chain_frequency_verdict(
                                                    cratonvm_jit::ir_nested_row_samples(
                                                        callee_profile
                                                            .as_ref()
                                                            .and_then(|p| p.receivers.get(ipc)),
                                                    ),
                                                    chain_invocations,
                                                )
                                                .is_ok()))
                                })
                            };
                            // Round rx27 wave 3 (lane chain,
                            // `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS`): or a
                            // chain site's pair (`GroupTail` pc 70).
                            // Round rx27 wave 4 (lane chain,
                            // `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES`): or
                            // an interface site of a chain body (`predicate.is`
                            // in `BmpCharPropertyGreedy.match`).
                            let pair_asked = cratonvm_jit::r25_w7b_chain_splice::nested_pair_admitted(
                                chain_site,
                            ) || cratonvm_jit::r25_w7b_chain_splice::chain_predicate_pair_site(
                                chain_body,
                                target.invoke_kind,
                            );
                            let pair = if pair_asked {
                                callee_profile
                                    .as_ref()
                                    .and_then(|p| p.receivers.get(ipc))
                                    .and_then(cratonvm_jit::ir::nested_bimorphic_receiver_pair)
                            } else {
                                None
                            };
                            let bimorphic = pair.and_then(|(a, b)| {
                                let first = resolve_for(a)?;
                                let second = resolve_for(b)?;
                                // As below: a redefinition of either guard's
                                // class withdraws this compile.
                                shared.jit.jit_cache.note_bytecode_copied(ClassId::new(a));
                                shared.jit.jit_cache.note_bytecode_copied(ClassId::new(b));
                                Some((first, a, second, b))
                            });
                            // Round rx27 wave 9 (lane chain): why a pair the
                            // chain switches asked for did not form (the
                            // `regex-matches` `predicate.is` pc 26 row read
                            // `no dominant class` in some processes).
                            if pair_asked && bimorphic.is_none() && crate::runtime::env_cache::dbg_jitc() {
                                let row = callee_profile.as_ref().and_then(|p| p.receivers.get(ipc));
                                let why = match (pair, row) {
                                    (Some((a, b)), _) => {
                                        format!("a body was refused (classes {a}+{b})")
                                    }
                                    (None, None) => "no receiver row".to_string(),
                                    (None, Some(counts)) => {
                                        let ev = cratonvm_jit::profile::ReceiverEvidence::of(counts);
                                        let s = ev.summary();
                                        format!(
                                            "the row names no pair (obs={} types={} top={:?} second={:?})",
                                            s.observations, s.types, s.top, s.second
                                        )
                                    }
                                };
                                eprintln!(
                                    "[cratonvm-jitc] nest-pair-declined {}.{}{} at callee_pc={} depth={} in {}.{}{}: {why}",
                                    target.class_name,
                                    target.method_name,
                                    target.descriptor,
                                    ipc,
                                    nest_depth + 1,
                                    inlined_body_class_name,
                                    callee_method,
                                    callee_desc,
                                );
                            }
                            if let Some((first, a, second, b)) = bimorphic {
                                if crate::runtime::env_cache::dbg_jitc() {
                                    eprintln!(
                                        "[cratonvm-jitc] {profiled_tag} {}.{}{} at callee_pc={} depth={} -> guard on classes {a}+{b}",
                                        target.class_name,
                                        target.method_name,
                                        target.descriptor,
                                        ipc,
                                        nest_depth + 1,
                                    );
                                }
                                nested_sites.push(cratonvm_jit::NestedInlineSite {
                                    callee_pc: *ipc,
                                    guard_class_id: a,
                                    site: first,
                                    bimorphic_second: Some(Box::new(
                                        cratonvm_jit::NestedInlineSite {
                                            callee_pc: *ipc,
                                            guard_class_id: b,
                                            site: second,
                                            bimorphic_second: None,
                                        },
                                    )),
                                });
                                continue;
                            }
                            // JIT round 19 wave 4 (lane splice4, SPLICE3-1,
                            // `CRATONVM_JIT_IR_NESTED_GUARD_CLASS_EVIDENCE`):
                            // `true` when the class below is the one the
                            // callee's own artifact guarded-spliced at this
                            // pc (its inline cache there is miss-edge only,
                            // SPLICE2-3, so it answered nothing).
                            let mut from_guard_class = false;
                            // JIT round 20 wave 4 (lane splice): a callee
                            // profile row with real evidence that names two
                            // or more classes and no dominant one has ANSWERED
                            // (polymorphic or megamorphic); the compiled
                            // artifact's cache is not asked to overrule it.
                            // Its MIC holds whichever class it installed
                            // first, and a megamorphic-seeded site publishes
                            // nothing to its PIC ways, so the cache read as
                            // "one class" there and the row guarded a 1-in-N
                            // class (`R19Mega5Nested inherited-static-mega-call`
                            // 9 -> 13 ns once package-private overrides were
                            // admitted). `CRATONVM_JIT_IR_NESTED_PROFILE_OVER_CACHE=0`
                            // restores the fallback.
                            let profile_spoke_poly = callee_profile
                                .as_ref()
                                .and_then(|p| p.receivers.get(ipc))
                                .is_some_and(|counts| {
                                    let evidence =
                                        cratonvm_jit::profile::ReceiverEvidence::of(counts);
                                    let summary = evidence.summary();
                                    summary.types > 1
                                        && summary.observations
                                            >= cratonvm_jit::MIN_GUARDED_RECEIVER_EVIDENCE
                                })
                                && true;
                            let dom = callee_profile
                                .as_ref()
                                .and_then(|p| p.receivers.get(ipc))
                                .and_then(|counts| crate::jit::profile::dominant_receiver(counts, 80))
                                .or_else(|| {
                                    if profile_spoke_poly {
                                        return None;
                                    }
                                    let artifact = shared.jit.jit_cache.read().get(
                                        &Arc::from(callee_class),
                                        &Arc::from(callee_method),
                                        &Arc::from(callee_desc),
                                        declaring_id,
                                    )?;
                                    artifact.dominant_receiver_at_bci(*ipc).or_else(|| {
                                        let class = gates.ir_nested_guard_class_evidence_enabled
                                            .then(|| artifact.guarded_splice_class_at_bci(*ipc))
                                            .flatten();
                                        from_guard_class = class.is_some();
                                        class
                                    })
                                })
                                .filter(|&d| d != 0);
                            // JIT round 20 (lane splice): a hot site's body
                            // takes the hot cap
                            // (`CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP`).
                            let one_cap = cratonvm_jit::ir_nested_guarded_body_cap_in(
                                callee_profile.as_ref().and_then(|p| p.receivers.get(ipc)),
                                cap_invocations,
                                code,
                                code_len,
                                *ipc,
                                1,
                            );
                            // JIT round 20 wave 2 (lane splice): the refused
                            // body's size, for the DBG line below.
                            let mut refused_len = 0usize;
                            // JIT r25 wave 7 (W7-B): the chain rule's answer
                            // for a chain site's body over the plain rule,
                            // for the `nest-chain` census line below.
                            let mut chain_verdict: Option<
                                Result<(), cratonvm_jit::r25_w7b_chain_splice::ChainRefusal>,
                            > = None;
                            // Round rx27 wave 4: the body was judged as a
                            // chain entry (`rx27_chain_entry`).
                            let mut entry_site = false;
                            let resolved = dom.and_then(|d| {
                                cratonvm_jit::ir::set_splice_arg_classes(operand_classes.clone());
                                let site = resolve_inline_site_from(
                                    shared,
                                    declaring_id,
                                    Some(ClassId::new(d)),
                                    &target.class_name,
                                    &target.method_name,
                                    &target.descriptor,
                                    nest_depth + 1,
                                    direct_bind,
                                    true,
                                    None,
                                    false,
                                )
                                .filter(|s| {
                                    refused_len = s.callee_code_len;
                                    if s.callee_is_static {
                                        return false;
                                    }
                                    if cratonvm_jit::ir_nested_hot_body_admitted(s, one_cap) {
                                        return true;
                                    }
                                    // Round rx27 wave 4 (lane chain,
                                    // `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`): or
                                    // a chain body entered from a non-chain
                                    // site (`matchRoot.match` in
                                    // `Matcher.match`), judged by the link rule.
                                    entry_site = !chain_site
                                        && cratonvm_jit::rx27_chain_entry::entry_body(s);
                                    // JIT r25 wave 7 (W7-B): or a chain link.
                                    if !chain_site && !entry_site {
                                        return false;
                                    }
                                    // Round rx27 wave 7 (lane chain): an entry
                                    // site with no interpreted row whose class
                                    // came from the callee's own artifact,
                                    // which guard-spliced this body over the
                                    // cold cap there: that compile judged the
                                    // site hot. The planner reads the same
                                    // row (`entry_hot_rows`).
                                    let artifact_row = cratonvm_jit::rx27_chain_entry::artifact_hot_row(
                                        entry_site,
                                        from_guard_class,
                                        callee_profile
                                            .as_ref()
                                            .and_then(|p| p.receivers.get(ipc))
                                            .is_some_and(|c| !c.is_empty()),
                                        s,
                                        d,
                                        cap_invocations,
                                    );
                                    let verdict =
                                        { let _ = &artifact_row; Err(cratonvm_jit::r25_w7b_chain_splice::ChainRefusal::SwitchedOff) }
                                        // W7-B fix 2: and frequent.
                                        .and_then(|()| {
                                            cratonvm_jit::r25_w7b_chain_splice::chain_frequency_verdict(
                                                cratonvm_jit::ir_nested_row_samples(
                                                    callee_profile
                                                        .as_ref()
                                                        .and_then(|p| p.receivers.get(ipc)),
                                                ),
                                                chain_invocations,
                                            )
                                        });
                                    chain_verdict = Some(verdict);
                                    if verdict.is_ok() {
                                        if let Some(row) = artifact_row {
                                            entry_hot_rows.push((*ipc, row));
                                        }
                                    }
                                    verdict.is_ok()
                                })?;
                                // As `resolve_ir_nested_exact_receiver_site`
                                // notes its receiver: a redefinition of the
                                // guard's class withdraws this compile.
                                shared.jit.jit_cache.note_bytecode_copied(ClassId::new(d));
                                Some((site, d))
                            });
                            if crate::runtime::env_cache::dbg_jitc() {
                                eprintln!(
                                    "[cratonvm-jitc] {profiled_tag} {}.{}{} at callee_pc={} depth={} -> {}",
                                    target.class_name,
                                    target.method_name,
                                    target.descriptor,
                                    ipc,
                                    nest_depth + 1,
                                    match (&resolved, dom) {
                                        (Some((_, d)), _) if from_guard_class => {
                                            format!("guard on class {d} (from the callee's guard class)")
                                        }
                                        (Some((s, d)), _) => format!(
                                            "guard on class {d} (body {} bytes, cap {one_cap}; \
                                             samples {} invocations {cap_invocations})",
                                            s.callee_code_len,
                                            cratonvm_jit::ir_nested_row_samples(
                                                callee_profile
                                                    .as_ref()
                                                    .and_then(|p| p.receivers.get(ipc))
                                            ),
                                        ),
                                        // `refused_len == 0`: the resolver
                                        // refused the body itself, before
                                        // any size test; its own `inline-resolve
                                        // REFUSED .. depth=N` line names why.
                                        (None, Some(d)) if refused_len == 0 => format!(
                                            "class {d} refused by the resolver (no body: see the \
                                             `inline-resolve REFUSED {}.{}{} depth={}` line; cap \
                                             {one_cap}, samples {})",
                                            target.class_name,
                                            target.method_name,
                                            target.descriptor,
                                            nest_depth + 1,
                                            cratonvm_jit::ir_nested_row_samples(
                                                callee_profile
                                                    .as_ref()
                                                    .and_then(|p| p.receivers.get(ipc))
                                            ),
                                        ),
                                        (None, Some(d)) => format!(
                                            "class {d} refused by the resolver (body {refused_len} \
                                             bytes: over cap {one_cap}, or over 35 and not a leaf; \
                                             samples {} invocations {cap_invocations})",
                                            cratonvm_jit::ir_nested_row_samples(
                                                callee_profile
                                                    .as_ref()
                                                    .and_then(|p| p.receivers.get(ipc))
                                            ),
                                        ),
                                        (None, None) if profile_spoke_poly => {
                                            "no dominant class (the callee profile is polymorphic; \
                                             the compiled cache is not asked)"
                                                .to_string()
                                        }
                                        (None, None) => "no evidence".to_string(),
                                    },
                                );
                            }
                            // JIT r25 wave 7 (W7-B): the chain census, one
                            // line per chain site the plain rule did not
                            // admit: SPLICED (the link rule took it), or why
                            // not.
                            if (chain_site || entry_site) && crate::runtime::env_cache::dbg_jitc() {
                                if entry_site {
                                    eprintln!(
                                        "[cratonvm-jitc] chain-entry {}.{}{} at callee_pc={} depth={} in {}.{}{}",
                                        target.class_name,
                                        target.method_name,
                                        target.descriptor,
                                        ipc,
                                        nest_depth + 1,
                                        inlined_body_class_name,
                                        callee_method,
                                        callee_desc,
                                    );
                                }
                                if let (Some(verdict), Some(d)) = (chain_verdict, dom) {
                                    // W7-B fix (round 2): the row's samples
                                    // and whether the site is in a loop of
                                    // this body (the planner's `ir-chain-tree`
                                    // line then says which links a loop roots
                                    // and what they add to the frame).
                                    eprintln!(
                                        "[cratonvm-jitc] nest-chain {}.{}{} at callee_pc={} depth={} in {}.{}{} samples={} invocations={chain_invocations} freq_permille={} in_loop={} -> class {d} {}",
                                        target.class_name,
                                        target.method_name,
                                        target.descriptor,
                                        ipc,
                                        nest_depth + 1,
                                        inlined_body_class_name,
                                        callee_method,
                                        callee_desc,
                                        cratonvm_jit::ir_nested_row_samples(
                                            callee_profile
                                                .as_ref()
                                                .and_then(|p| p.receivers.get(ipc))
                                        ),
                                        cratonvm_jit::r25_w7b_chain_splice::chain_frequency_permille(
                                            cratonvm_jit::ir_nested_row_samples(
                                                callee_profile
                                                    .as_ref()
                                                    .and_then(|p| p.receivers.get(ipc))
                                            ),
                                            chain_invocations,
                                        ),
                                        cratonvm_jit::bytecode_pc_in_loop(code, code_len, *ipc),
                                        match verdict {
                                            Ok(()) => format!(
                                                "SPLICED (link body {refused_len} bytes behind a class test)"
                                            ),
                                            Err(why) => format!("refused: {why}"),
                                        },
                                    );
                                }
                            }
                            match resolved {
                                Some((site, d)) => (Some(site), d),
                                None => (None, 0),
                            }
                        }
                        other => other,
                    };
                    // A signature-polymorphic `invokeExact` the builder folds
                    // to its constant handle's member: that member's body for
                    // the receiver's exact class, which the builder splices
                    // only through the fold (`IrBuilder::folded_member_splice_admitted`).
                    let (nested, exact_class) = match (nested, exact_class) {
                        (None, _) if ir_mode => {
                            match folded_handle_member_row(shared, declaring_id, target, &operand_classes, nest_depth, direct_bind) {
                                Some((site, class_id)) => (Some(site), class_id),
                                None => (None, 0),
                            }
                        }
                        other => other,
                    };
                    if let Some(nested) = nested {
                        nested_sites.push(cratonvm_jit::NestedInlineSite {
                            callee_pc: *ipc,
                            guard_class_id: exact_class,
                            site: nested,
                            bimorphic_second: None,
                        });
                    }
                }
                // Virtual / interface: one body per receiver class, so a splice
                // needs a guard and the profile has to name the class.
                0 | 2 => {
                    // THE GATE, and it belongs here rather than only on the
                    // profile fetch below. Gating just the profile left the MIC
                    // fallback running with `CRATONVM_JIT_INLINE_SPLICE_DEVIRT`
                    // OFF, so devirtualisation happened whenever nesting did and
                    // the flag's two arms were byte-identical — measured
                    // 2026-08-18, `nested-splice-guarded=1` in both. A switch
                    // that does not switch anything is worse than no switch: it
                    // makes an A/B report "no difference" for a feature that was
                    // on in both arms.
                    if !crate::runtime::env_cache::jit_inline_splice_devirt() {
                        continue;
                    }
                    // TWO sources, in this order, and the second is the one
                    // that actually answers for this workload.
                    //
                    //  1. the CALLEE's own receiver profile, keyed by its own
                    //     bci. The right key — receiver types are recorded
                    //     against the executing method — but measured
                    //     2026-08-18 it is EMPTY at exactly these sites: the
                    //     eager-callee-chain compiles a method like
                    //     `objectsAreEqual` before it ever runs its
                    //     `invokevirtual equals` interpreted, so nothing is
                    //     recorded. It is also gated off entirely unless
                    //     `CRATONVM_TIER_PGO` is set.
                    //  2. the CALLEE's COMPILED ARTIFACT's inline cache at that
                    //     bci. A method that skipped the interpreter has been
                    //     caching its receiver on every compiled call since,
                    //     which is the same evidence one layer down — and it is
                    //     available precisely when (1) is not.
                    //
                    // Both are speculation; the class-id guard is what makes
                    // either safe, and a wrong guess costs the miss edge.
                    let row = callee_profile
                        .as_ref()
                        .and_then(|p| p.receivers.get(ipc))
                        .filter(|counts| !counts.is_empty());
                    // JIT round 19 proposal wave 5 (lane mega5, MEGA5-1,
                    // `CRATONVM_JIT_SP_NESTED_GUARD_STRICT_SHARE`): this
                    // guard's miss edge is `emit_guarded_nested_inline`'s
                    // ordinary call, which inside a single-pass splice is the
                    // BLIND dispatch helper (no MIC / PIC), while refusing the
                    // row costs only the splice: the enclosing body is then
                    // refused as "neither spliced nor direct-bound" and the
                    // call to it reaches the callee's own compiled body, whose
                    // site has its own seeded inline cache. So a row the
                    // callee's profile shows missing more than ~3 % of the
                    // time is refused here, and a profile row that fails the
                    // bar is not overruled by the compiled cache's evidence
                    // below. Off: the 80 % bar, as before.
                    let strict = gates.sp_nested_guard_strict_share_enabled;
                    let bar = if strict {
                        cratonvm_jit::SP_NESTED_GUARD_STRICT_SHARE_PCT
                    } else {
                        // Same 80% bar as the top-level guarded-virtual planner.
                        80
                    };
                    let profile_dom =
                        row.and_then(|counts| crate::jit::profile::dominant_receiver(counts, bar));
                    // "Fails the bar" means the row has enough samples to
                    // answer at all (`dominant_receiver` at a 1 % bar: the
                    // observation floor) and its top class is under `bar`; a
                    // row under the floor is silent, as before.
                    let row_rejects = strict
                        && profile_dom.is_none()
                        && row.is_some_and(|counts| {
                            crate::jit::profile::dominant_receiver(counts, 1).is_some()
                        });
                    if row_rejects {
                        if crate::runtime::env_cache::dbg_jitc() {
                            eprintln!(
                                "[cratonvm-jitc] nest-virtual {}.{}{} at callee_pc={} -> refused: callee row under the {bar}% share (sp-nested-guard-strict-share)",
                                target.class_name, target.method_name, target.descriptor, ipc,
                            );
                        }
                        continue;
                    }
                    let (dom, evidence) = match profile_dom {
                        Some(d) => (d, "profile"),
                        None => {
                            let artifact = shared.jit.jit_cache.read().get(
                                &Arc::from(callee_class),
                                &Arc::from(callee_method),
                                &Arc::from(callee_desc),
                                declaring_id,
                            );
                            let from_cache = artifact
                                .as_ref()
                                .and_then(|cm| cm.dominant_receiver_at_bci(*ipc));
                            match from_cache {
                                Some(d) => (d, "mic"),
                                None => {
                                    if crate::runtime::env_cache::dbg_jitc() {
                                        // Distinguish the three ways this can
                                        // answer nothing — no artifact at all,
                                        // an artifact with no slot at this bci,
                                        // and a slot that failed the dominance
                                        // bar — because they call for three
                                        // different fixes.
                                        let detail = match artifact.as_ref() {
                                            None => "no-artifact".to_string(),
                                            Some(cm) => format!(
                                                "artifact slots=[{}]",
                                                cm.mic_slot_census()
                                                    .iter()
                                                    .map(|(b, c, h, m)| format!(
                                                        "bci{b}:cls{c}:h{h}:m{m}"
                                                    ))
                                                    .collect::<Vec<_>>()
                                                    .join(",")
                                            ),
                                        };
                                        eprintln!(
                                            "[cratonvm-jitc] nest-virtual {}.{}{} at callee_pc={} -> no evidence (profile={} mic: {})",
                                            target.class_name,
                                            target.method_name,
                                            target.descriptor,
                                            ipc,
                                            if callee_profile.is_some() { "empty" } else { "absent" },
                                            detail,
                                        );
                                    }
                                    continue;
                                }
                            }
                        }
                    };
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] nest-virtual {}.{}{} at callee_pc={} -> guard on class {} (from {})",
                            target.class_name, target.method_name, target.descriptor, ipc, dom,
                            evidence,
                        );
                    }
                    // JIT round 19 wave 11, lane tail11 (TAIL9-20-2,
                    // `CRATONVM_JIT_SP_NESTED_MISS_SHARE_REFUSE`, opt-in): the
                    // miss share the guard MEASURED in the bodies that already
                    // spliced it (`JitCache::sp_nested_miss_counts`, keyed by
                    // this enclosing body, this pc and this class). At or above
                    // the bar the row is refused, as MEGA5-1 refuses a profile
                    // row under its share: the enclosing body is then refused
                    // whole ("neither spliced nor direct-bound" below), and the
                    // outer call reaches the enclosing callee's compiled body
                    // and its own inline cache instead of this guard's blind
                    // dispatch miss edge. Off: nothing is asked.
                    if gates.sp_nested_miss_share_refuse_enabled {
                        let enclosing =
                            format!("{inlined_body_class_name}.{callee_method}:{callee_desc}");
                        let counts = shared
                            .jit
                            .jit_cache
                            .read()
                            .sp_nested_miss_counts(&enclosing, *ipc, dom);
                        if let Some((hits, misses)) = counts {
                            let refused = cratonvm_jit::sp_nested_miss_share_refuses(hits, misses);
                            if crate::runtime::env_cache::dbg_jitc() {
                                eprintln!(
                                    "[cratonvm-jitc] nest-virtual {}.{}{} at callee_pc={} in {enclosing} -> measured hits={hits} misses={misses}: {} (sp-nested-miss-share-refuse)",
                                    target.class_name,
                                    target.method_name,
                                    target.descriptor,
                                    ipc,
                                    if refused { "refused" } else { "kept" },
                                );
                            }
                            if refused {
                                continue;
                            }
                        }
                    }
                    // Resolve the body that receiver ACTUALLY dispatches to,
                    // not the constant-pool one: the guard certifies the
                    // subclass, so splicing the superclass's method behind it
                    // is silent wrong code at every overriding site. This is
                    // the same contract `resolve_receiver_inline_site`
                    // documents, and it applies verbatim one level down.
                    if let Some(nested) = resolve_inline_site_from(
                        shared,
                        declaring_id,
                        Some(ClassId::new(dom)),
                        &target.class_name,
                        &target.method_name,
                        &target.descriptor,
                        nest_depth + 1,
                        direct_bind,
                        // Unreachable in IR mode (every kind is routed to the
                        // statically-bound arm above), and a guarded splice is
                        // not something the IR consumer can emit — so `false`
                        // here is a statement, not a default.
                        false,
                        None,
                        false,
                    ) {
                        nested_sites.push(cratonvm_jit::NestedInlineSite {
                            callee_pc: *ipc,
                            guard_class_id: dom,
                            site: nested,
                            bimorphic_second: None,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // A spliced call must not be WORSE than the call it replaced.
    //
    // Measured 2026-08-18 (see `jit_inline_call_dispatch`): the chain this
    // whole line of work targets is already direct-bound, so emitting an
    // admitted call through the blind dispatch helper traded a ~4 ns raw CALL
    // for a ~175 ns name resolution — `assertFull` 47 -> 163-266 ns/iter, with
    // `disp_calls` going from 3 870 to 2 003 361 over 2 000 000 iterations.
    // Splicing away one frame does not pay for downgrading the call inside it.
    //
    // So unless the fallback is explicitly re-enabled, refuse any site with a
    // call that is not itself spliced, and then CLEAR `invoke_targets` — which
    // removes the emitter's fallback as well, so a nested splice that bails
    // during emission bails the enclosing splice instead of quietly becoming a
    // dispatch. Refusing costs the site its inline; admitting it costs 3.5x.
    // (`invoke_targets` is already `let mut` where it is built, ~540 lines up.)

    // Preserve the standalone loop's guarded splices when a nested copy cannot
    // reproduce them. Ask the profile and IR planner, not a published artifact:
    // C1, C2 and an absent callee must give the same answer for the same evidence.
    if !planning_only && ir_mode && nest_depth == 0 && gates.keep_loop_splice_caller_enabled {
        let key = crate::jit::profile::MethodKey {
            class_id: declaring_id.as_u32(), method_name: Arc::from(callee_method),
            descriptor: Arc::from(callee_desc),
        };
        let profile = shared.jit.profile_store.get_profile(&key);
        {
            let lost: Vec<(usize, u32)> = invoke_targets
                .iter()
                .flat_map(|(ipc, target)| {
                    let guards = planned_guarded_splice_classes(
                        shared, declaring_id, *ipc, target,
                        profile.as_ref(),
                        cratonvm_jit::bytecode_pc_in_loop(code, code_len, *ipc),
                    );
                    let nested_guards: Vec<u32> = nested_sites
                        .iter()
                        .filter(|n| n.callee_pc == *ipc)
                        .flat_map(|n| {
                            std::iter::once(n.guard_class_id)
                                .chain(n.bimorphic_second.as_ref().map(|b| b.guard_class_id))
                        })
                        .collect();
                    guards.into_iter().filter_map(move |guard| keep_loop_splice_caller_lost_class(
                        target.invoke_kind,
                        Some(guard),
                        cratonvm_jit::bytecode_pc_in_loop(code, code_len, *ipc),
                        &nested_guards,
                    )
                    .map(|class_id| (*ipc, class_id)))
                })
                .collect();
            // A row answered inside this copy is not lost:
            //  * `CRATONVM_JIT_IR_KEEP_LOOP_STRING_PREFIX`: a `String` row the
            //    copy answers with the inline String prefix (`chain-prefix-kept`,
            //    pc 21's `charAt`);
            //  * `CRATONVM_JIT_IR_KEEP_LOOP_RARE_ROW`: a row at a site that runs
            //    on fewer than C2's `InlineFrequencyRatio` of the body's entries
            //    (pc 70's `next.match` after the greedy run, ~190 per mille): its
            //    call is paid per entry, not per iteration. The same window and
            //    verdict the nested chain rule asks (`nest-chain ... rare site`).
            let rare_window = (!lost.is_empty() && gates.keep_loop_rare_row_enabled).then(|| {
                let key = crate::jit::profile::MethodKey {
                    class_id: declaring_id.as_u32(),
                    method_name: Arc::from(callee_method),
                    descriptor: Arc::from(callee_desc),
                };
                let invocations = shared.jit.profile_store.invocation_count(
                    cratonvm_jit_api::invoc_key_parts(
                        declaring_id.as_u32(),
                        callee_method,
                        callee_desc,
                    ),
                );
                (shared.jit.profile_store.get_profile(&key), invocations)
            });
            let lost = lost.into_iter().find(|&(ipc, class_id)| {
                let answered =
                    keep_loop_row_answered_by_string_prefix(&string_prefix_pcs, ipc, || {
                        jit_class_is_builtin_named(shared, class_id, "java/lang/String")
                    });
                if answered {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] keep-loop-string-prefix {inlined_body_class_name}.\
                             {callee_method}{callee_desc} callee_pc={ipc} class {class_id}: \
                             answered by the inline String prefix, not lost"
                        );
                    }
                    return false;
                }
                let Some((profile, invocations)) = rare_window.as_ref() else {
                    return true;
                };
                let samples = cratonvm_jit::ir_nested_row_samples(
                    profile.as_ref().and_then(|p| p.receivers.get(&ipc)),
                );
                let rare = keep_loop_row_is_rare(samples, *invocations);
                if rare && crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] keep-loop-rare-row {inlined_body_class_name}.\
                         {callee_method}{callee_desc} callee_pc={ipc} class {class_id}: \
                         samples={samples} invocations={invocations}, rare, not lost"
                    );
                }
                !rare
            });
            if let Some((ipc, class_id)) = lost {
                no!(format!(
                    "keep-loop-splice-caller: the in-loop call at callee_pc={ipc} is planned for a guarded splice \
                     (class {class_id}) in this body's standalone compile"
                ));
            }
        }
    }

    // A frequent next-link splice that this nested copy cannot reproduce stays
    // a call into the standalone body. Use the same profile/body prediction as
    // the loop policy above; publication order is not planning evidence.
    if !planning_only && ir_mode
        && nest_depth >= 1
        && keep_loop_splice_caller_enabled()
        && gates.ir_nested_chain_splice_enabled
    {
        let lost_link = {
            let kept_link_pcs: Vec<usize> = invoke_targets
                .iter()
                .filter(|(ipc, t)| {
                    cratonvm_jit::r25_w7b_chain_splice::is_chain_target(
                        t,
                        callee_method,
                        callee_desc,
                    ) && !nested_sites.iter().any(|n| n.callee_pc == *ipc)
                        // The chain family, as the nested loop asks it.
                        && {
                            let cm = shared.classes.class_manager.read();
                            jit_known_class(shared, &cm, &t.class_name, declaring_id).is_some_and(
                                |cp| {
                                    cm.get_class(cp).is_some_and(|c| !c.is_interface())
                                        && cm.is_subclass_of(declaring_id, cp)
                                },
                            )
                        }
                })
                .map(|(ipc, _)| *ipc)
                .collect();
            if kept_link_pcs.is_empty() {
                None
            } else {
                let key = crate::jit::profile::MethodKey {
                    class_id: declaring_id.as_u32(),
                    method_name: Arc::from(callee_method),
                    descriptor: Arc::from(callee_desc),
                };
                let profile = shared.jit.profile_store.get_profile(&key);
                let invoc_key = cratonvm_jit_api::invoc_key_parts(
                    declaring_id.as_u32(),
                    callee_method,
                    callee_desc,
                );
                let invocations = shared.jit.profile_store.invocation_count(invoc_key);
                kept_link_pcs.into_iter().find_map(|ipc| {
                        let target = &invoke_targets.iter().find(|(pc, _)| *pc == ipc)?.1;
                        let class_id = planned_guarded_splice_classes(
                            shared, declaring_id, ipc, target,
                            profile.as_ref(),
                            cratonvm_jit::bytecode_pc_in_loop(code, code_len, ipc),
                        ).into_iter().next()?;
                        let samples = cratonvm_jit::ir_nested_row_samples(
                            profile.as_ref().and_then(|p| p.receivers.get(&ipc)),
                        );
                        cratonvm_jit::r25_w7b_chain_splice::chain_frequency_verdict(
                            samples,
                            invocations,
                        )
                        .is_ok()
                        .then_some((ipc, class_id, samples, invocations))
                })
            }
        };
        if let Some((ipc, class_id, samples, invocations)) = lost_link {
            no!(format!(
                "chain-link-keeps-spliced-call: the next-link call at callee_pc={ipc} is \
                 planned for a guarded splice (class {class_id}) in this body's standalone compile and frequent \
                 (samples={samples} invocations={invocations}); kept as a call"
            ));
        }
    }

    // Direct-bind whatever the same resolver the TOP LEVEL uses will bind.
    //
    // Runs after `drop(cm)` and after the nested resolution, and it must:
    // `callee_compiler` / `direct_callee_lookup` take `class_manager` for
    // reading and may transitively COMPILE the callee, which takes it for
    // writing. Both bounds that recursion themselves (depth, cycle, fan-out),
    // which is why reusing them is better than writing a lookup here.
    //
    // Skipped for a pc that is already NESTED — a spliced body beats a call,
    // and asking would compile a callee whose code this site is not going to
    // emit. Skipped for virtual/interface kinds: a direct bind names one body,
    // and those select on the runtime receiver (that is what
    // `resolve_receiver_inline_site` and the guarded-virtual path are for).
    //
    // nio21 wave 6, lane L4 (`splice_unbindable_nested_to_call_enabled`): first
    // drop each statically bound NESTED body the IR planner is certain to
    // refuse for an unbindable call of its own
    // (`cratonvm_jit::ir_splice_body_has_unbindable_call`), so its pc is bound
    // below like any other call instead of being left unbound -- which made
    // the planner refuse THIS body too.
    if ir_mode && direct_bind.is_some() && gates.splice_unbindable_nested_to_call_enabled {
        nested_sites.retain(|n| {
            let doomed = n.guard_class_id == 0
                && invoke_targets
                    .iter()
                    .any(|(pc, t)| *pc == n.callee_pc && matches!(t.invoke_kind, 1 | 3))
                && cratonvm_jit::ir_splice_body_has_unbindable_call(&n.site);
            if doomed && crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] nest-unbindable-to-call {}.{}{} at callee_pc={} in {}{}: \
                     kept as a bound call",
                    n.site.class_name,
                    n.site.method_name,
                    n.site.descriptor,
                    n.callee_pc,
                    callee_method,
                    callee_desc,
                );
            }
            !doomed
        });
    }
    // JIT round 21, lane ci (`CRATONVM_JIT_IR_LATIN1_CASE_NESTED_TO_CALL`): a
    // nested `StringLatin1.compareToCI` / `toLowerCase` / `toUpperCase` body
    // is the JDK loop the inline prefix replaces, and the prefix sits only on
    // a CALL with an entry -- left nested, its pc is never bound, so the
    // planner's strip cannot degrade it and the JDK body splices. Dropped here
    // whenever the binder answers an entry (bound again just below).
    if ir_mode {
        if let Some(bind) = direct_bind {
            nested_sites.retain(|n| {
                let Some((_, t)) = invoke_targets.iter().find(|(pc, t)| {
                    *pc == n.callee_pc
                        && n.guard_class_id == 0
                        && cratonvm_jit::ir_splice_nested_case_prefix_stays_call(
                            &t.class_name,
                            &t.method_name,
                            &t.descriptor,
                            t.invoke_kind,
                        )
                }) else {
                    return true;
                };
                let bound = bind(
                    ClassId::new(t.declaring_class_id),
                    &t.class_name,
                    &t.method_name,
                    &t.descriptor,
                )
                .is_some();
                if bound && crate::runtime::env_cache::dbg_jitc() {
                    eprintln!(
                        "[cratonvm-jitc] nest-case-prefix-to-call {}.{}{} at callee_pc={} in \
                         {}{}: kept as a bound call",
                        t.class_name,
                        t.method_name,
                        t.descriptor,
                        n.callee_pc,
                        callee_method,
                        callee_desc,
                    );
                }
                !bound
            });
        }
    }
    if let Some(bind) = direct_bind {
        bind_spliced_static_calls(&mut invoke_targets, &nested_sites, bind);
        bind_spliced_final_calls(&mut invoke_targets, &final_pins, &nested_sites, bind);
    }

    // nio21 wave 6, lane L4: a static call the binds left without an entry
    // because its callee has NEVER RUN becomes a precise exit, as an unloaded
    // callee's does (`splice_never_run_static_exit_enabled`). Only when the
    // invocation counter is shown live: this body has run, or the method being
    // compiled has (a reviewed intrinsic's JDK body never runs as bytecode).
    if never_run_static_exit
        && !never_run_static_candidates.is_empty()
        && (eager_static_callee_has_run(shared, declaring_id, callee_method, callee_desc)
            || cratonvm_jit::jit_active_compile_top().is_some_and(|(cid, _, m, d)| {
                cid.as_u32() != 0 && eager_static_callee_has_run(shared, cid, &m, &d)
            }))
    {
        let exits = never_run_static_exit_pcs(
            &invoke_targets,
            &nested_sites,
            &unloaded_static_call_pcs,
            &never_run_static_candidates,
            |decl, method, desc| eager_static_callee_has_run(shared, decl, method, desc),
        );
        if crate::runtime::env_cache::dbg_jitc() {
            for pc in &exits {
                if let Some((_, t)) = invoke_targets.iter().find(|(p, _)| p == pc) {
                    eprintln!(
                        "[cratonvm-jitc] splice-never-run-static-exit {}.{}{} at callee_pc={pc} \
                         in {}{}",
                        t.class_name, t.method_name, t.descriptor, callee_method, callee_desc,
                    );
                }
            }
        }
        unloaded_static_call_pcs.extend(exits);
    }

    // JIT round 21 wave 1, lane updaters (UPDATERS21-3,
    // `CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT`, opt-in): the same exit
    // for a PRIVATE instance call (a pinned private `invokevirtual` /
    // `invokeinterface`, or an `invokespecial` of this body's own class that is
    // not a constructor) the binds left without an entry because its callee has
    // never run. `AtomicIntegerFieldUpdaterImpl.accessCheck`'s cold
    // `throwAccessCheckException(obj)` is the shape: never run, so never
    // compiled, so the background door's lookup-only binder finds no entry and
    // the row refused `accessCheck` (`ir-splice-unbindable-surviving-call`),
    // which every updater access then paid as a call.
    if ir_mode
        && gates.splice_never_run_private_exit_enabled
        && (eager_static_callee_has_run(shared, declaring_id, callee_method, callee_desc)
            || cratonvm_jit::jit_active_compile_top().is_some_and(|(cid, _, m, d)| {
                cid.as_u32() != 0 && eager_static_callee_has_run(shared, cid, &m, &d)
            }))
    {
        let body_class_name = shared
            .classes
            .class_manager
            .read()
            .get_class(declaring_id)
            .map(|c| c.name.to_string());
        let special_pcs: Vec<usize> = special_sites.iter().map(|&(pc, _)| pc).collect();
        let exits = never_run_private_exit_pcs(
            &invoke_targets,
            &nested_sites,
            &unloaded_static_call_pcs,
            &special_pcs,
            body_class_name.as_deref(),
            |decl, method, desc| {
                // A registered native may run uncounted: never an exit.
                let name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(decl)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                shared
                    .natives
                    .native_methods
                    .find(&name, method, desc)
                    .is_some()
                    || eager_static_callee_has_run(shared, decl, method, desc)
            },
        );
        if crate::runtime::env_cache::dbg_jitc() {
            for pc in &exits {
                if let Some((_, t)) = invoke_targets.iter().find(|(p, _)| p == pc) {
                    eprintln!(
                        "[cratonvm-jitc] splice-never-run-private-exit {}.{}{} at callee_pc={pc} \
                         in {}{}",
                        t.class_name, t.method_name, t.descriptor, callee_method, callee_desc,
                    );
                }
            }
        }
        unloaded_static_call_pcs.extend(exits);
    }

    // Interpreter round i1 wave 51, lane L1
    // (`interpreter-L1-a-call-site-of-a-spliced-callee-is-never-recorded-as-resolved-FIXED-20261015`):
    // a call in this body whose owner record would be observable and is not
    // made yet ([`super::invoke::unrecorded_method_ref_owner`], what the
    // compile doors guard at their own sites with a first-call cell) keeps
    // the body out of line. A splice binds the call by name from this pool and
    // never resolves it, so its first execution inside the splice would record
    // nothing; the callee, called, resolves it where it runs (interpreted, or
    // its own compile's guard). Asked with `cm` dropped: the probe takes the
    // class manager and the resolution cache itself. Except a site that is a
    // precise exit (`unloaded_static_call_pcs`, decided above): the splice
    // never executes it, the interpreter it exits to runs the `invoke` and
    // records the owner -- `AssertNotEquals.failEqual`, which no passing
    // `assertNotEquals` ever calls, refused the whole two-instruction body.
    // `CRATONVM_JIT_IR_SPLICE_UNRECORDED_CALL_EXIT`: in the optimizing tier
    // such a site (`invokespecial` aside) becomes that exit itself.
    if splice_refuses_unresolved_site_enabled(shared) {
        for &(ipc, cp_idx, opcode) in &invoke_sites {
            if matches!(opcode, 0xb6..=0xb9)
                && !unloaded_static_call_pcs.contains(&ipc)
                && super::invoke::unrecorded_method_ref_owner(shared, declaring_id, cp_idx)
                    .is_some()
            {
                if ir_mode
                    && opcode != 0xb7
                    && gates.ir_splice_unrecorded_call_exit_enabled
                {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] splice-unrecorded-call-exit {}.{}{} at callee_pc={ipc} (cp #{cp_idx})",
                            callee_class, callee_method, callee_desc,
                        );
                    }
                    nested_sites.retain(|n| n.callee_pc != ipc);
                    unloaded_static_call_pcs.push(ipc);
                    continue;
                }
                no!(format!(
                    "callee-site-unresolved (callee pc {ipc}, cp #{cp_idx})"
                ));
            }
        }
    }

    // A spliced call must not be WORSE than the call it replaced.
    //
    // Measured 2026-08-18 (see `jit_inline_call_dispatch`): the chain this
    // whole line of work targets is already direct-bound, so emitting an
    // admitted call through the blind dispatch helper traded a ~4 ns raw CALL
    // for a ~175 ns name resolution — `assertFull` 47 -> 163-266 ns/iter, with
    // `disp_calls` going from 3 870 to 2 003 361 over 2 000 000 iterations.
    // Splicing away one frame does not pay for downgrading the call inside it.
    //
    // So unless the fallback is explicitly re-enabled, every call in the body
    // must be either SPLICED IN TURN or DIRECT-BOUND; a site with one that is
    // neither is refused whole. Refusing costs the site its inline; admitting
    // it costs 3.5x.
    //
    // NOT applied in IR mode, and the difference is in the emitter, not the
    // policy. That measurement is of `try_emit_inline_body`'s invoke arm, which
    // lowers a spliced call to the BLIND dispatch helper — no MIC, no PIC, a
    // name resolution per call. `IrBuilder` lowers a spliced call to the same
    // `Op::Call` it lowers every other invoke to, and `ir_lower` gives it the
    // ordinary dispatch. That is still one inline cache short of what the
    // caller's own sites get (the cache is keyed by bytecode pc, and every node
    // in a spliced region carries the CALLER's `invoke` pc — see
    // `ir::IrInlineSite`), so it is not free; it is a call, not a resolution.
    //
    // Keeping the rule here would refuse exactly the bodies this lane exists
    // for: `VolumeShort2.loadFromArray` calls `ShortArray.get`, which is an FFM
    // accessor the nested resolver deliberately refuses to splice (it keeps its
    // own fast path), and no direct bind is offered for a virtual kind. One
    // un-spliceable call would cost the site its allocation.
    if !ir_mode && !crate::runtime::env_cache::jit_inline_call_dispatch() {
        let nested_pcs: Vec<usize> = nested_sites.iter().map(|n| n.callee_pc).collect();
        // j27 wave 2 (lane NATMISC, REFLECT2-21-2): an int/long/float/double `Math.min/max`
        // row is bound too -- the emitter serves it as a CMOV/minmax, from this row.
        // Likewise getClass() and Integer/Long.valueOf() are served inline.
        let minmax_row = |t: &cratonvm_jit::InlineInvokeTarget| {
            sp_splice_emits_minmax_inline(
                false,
                t.invoke_kind,
                &t.class_name,
                &t.method_name,
                &t.descriptor,
            ) || sp_splice_emits_getclass_inline(
                false,
                t.invoke_kind,
                &t.method_name,
                &t.descriptor,
            ) || sp_splice_emits_value_of_inline(
                false,
                t.invoke_kind,
                &t.class_name,
                &t.method_name,
                &t.descriptor,
            ) || sp_splice_emits_string_accessor_inline(
                false,
                t.invoke_kind,
                &t.class_name,
                &t.method_name,
                &t.descriptor,
            ) || sp_splice_emits_char_scalar_inline(
                false,
                t.invoke_kind,
                &t.class_name,
                &t.method_name,
                &t.descriptor,
            ) || sp_splice_emits_current_thread_inline(
                false,
                t.invoke_kind,
                &t.class_name,
                &t.method_name,
                &t.descriptor,
            ) || sp_splice_emits_byte_array_view_inline(
                false,
                t.invoke_kind,
                &t.class_name,
                &t.method_name,
                &t.descriptor,
            )
        };
        if let Some((pc, t)) = invoke_targets.iter().find(|(pc, t)| {
            t.direct_entry.is_none() && !nested_pcs.contains(pc) && !minmax_row(t)
        }) {
            no!(format!(
                "call at callee_pc={} to {}.{}{} (kind {}) is neither spliced nor direct-bound",
                pc, t.class_name, t.method_name, t.descriptor, t.invoke_kind
            ));
        }
        // A GUARDED nested splice keeps its dispatch entry no matter what: the
        // guard's miss edge has to go somewhere, and for a virtual site there
        // is no direct bind to send it to. That is the same bargain PGO-02
        // makes one level up — the cold edge pays the helper, the hot edge pays
        // nothing — and it is only a bargain while the guard actually holds,
        // which is what the 80% dominance bar above is for.
        //
        // An UNGUARDED nested pc with no direct bind loses its entry, so a
        // nested splice that bails at emission time bails the enclosing splice
        // rather than degrading to the helper. One that IS direct-bound keeps
        // it: falling back to a raw CALL is not a downgrade.
        let guarded_pcs: Vec<usize> = nested_sites
            .iter()
            .filter(|n| n.guard_class_id != 0)
            .map(|n| n.callee_pc)
            .collect();
        invoke_targets.retain(|(pc, t)| {
            t.direct_entry.is_some() || guarded_pcs.contains(pc) || minmax_row(t)
        });
    }

    // JIT round 19 proposal wave 4 (lane megaosr4, MEGAOSR19-2): the callee's
    // own receiver profile at each of its virtual / interface sites, for the
    // optimizing planner to seed the inline cache a splice leaves there.
    // Optimizing tier only (the single-pass splice lowers a surviving call to
    // dispatch, with no cache to seed), and only under the switch.
    // JIT round 20 (lane splice): also the planner's nested hot cap's
    // evidence (`cratonvm_jit::ir_nested_guarded_body_cap`), which must read
    // the same rows the nested resolution above admitted its bodies by.
    if !dead_invoke_targets.is_empty() {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] splice-dead-rows {} in {callee_class}.{callee_method}{callee_desc} \
                 depth={nest_depth} pcs={:?}",
                dead_invoke_targets.len(),
                dead_invoke_targets.iter().map(|(pc, _)| *pc).collect::<Vec<_>>(),
            );
        }
        invoke_targets.extend(dead_invoke_targets);
    }
    let seed_rows = ir_mode
        && (gates.ir_splice_callee_profile_seed_enabled
            || gates.ir_nested_guarded_hot_cap_enabled
            // JIT round 24 B3: an admitted element site's receiver row.
            || (ffm_element_site && gates.ir_splice_ffm_element_enabled));
    let mut callee_receiver_profile = if seed_rows {
        callee_receiver_profile_rows(
            shared,
            declaring_id.as_u32(),
            callee_method,
            callee_desc,
            &invoke_targets,
        )
    } else {
        Vec::new()
    };
    // Round rx27 wave 7 (lane chain): the chain entry's artifact rows, where
    // the interpreted profile had none, so the planner's link rule
    // (`chain_row_admitted_in`) reads the row this resolution admitted by.
    if ir_mode && !entry_hot_rows.is_empty() {
        for (pc, row) in entry_hot_rows {
            if !callee_receiver_profile.iter().any(|(p, _)| *p == pc) {
                callee_receiver_profile.push((pc, row));
            }
        }
        callee_receiver_profile.sort_unstable_by_key(|&(pc, _)| pc);
    }
    // JIT round 23 wave 4 (lane L1, CF23-2; `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`,
    // opt-in): the callee's own never-taken branches, for the optimizing
    // builder's in-splice prune. Optimizing tier only.
    // JIT round 23 wave 9 (lane W9-L2, CF-W9-2): also under
    // `CRATONVM_JIT_IR_PRUNE_RARE_ARM` (default ON).
    let (callee_never_taken_pcs, callee_always_taken_pcs, callee_pruned_switch_edges) =
        if ir_mode && gates.ir_splice_prune_feed_enabled {
            (
                early_never_taken_pcs,
                early_always_taken_pcs,
                early_pruned_switch_edges,
            )
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
    // JIT round 23 wave 8 (lane W8-L2, CF-W8-3;
    // `CRATONVM_JIT_IR_EVIDENCE_COLD_ARM_WEIGHT`): the callee's rarely taken
    // arms, for the acceptance gate's pricing only. Optimizing tier only.
    let callee_cold_arm_ranges =
        if ir_mode && gates.cold_blind_dispatch_weight_enabled {
            callee_cold_arm_rows(shared, declaring_id.as_u32(), callee_method, callee_desc, &padded, code_len)
        } else {
            Vec::new()
        };
    // JIT round 23 wave 10 (lane L2, CF-W10-2;
    // `CRATONVM_JIT_IR_SPLICE_COLD_NESTED_HOT_PATH`): the callee's profiled
    // dominant path, so the cold-nested rule keeps its hot nested calls
    // spliced. Optimizing tier only.
    let callee_hot_path_pcs =
        if ir_mode && gates.ir_splice_cold_nested_hot_path_enabled {
            callee_hot_path_rows(shared, declaring_id.as_u32(), callee_method, callee_desc, &padded, code_len)
        } else {
            Vec::new()
        };
    // JIT r25 wave 8 (W8-5): the chain cost rule's denominator, only under
    // its switch (0 is "no evidence").
    // rx27 wave 3 (lane bimodal): the count read once above, so the nested
    // hot cap the resolver admitted under and the planner's re-check agree.
    let callee_invocations = cap_invocations;

    if ir_mode {
        mint_const_ref_folds_at_offer(shared, None, ClassId::new(inlined_body_class_id), &padded);
    }
    Some(cratonvm_jit::InlineSite {
        callee_code: padded.to_vec(),
        // The method's own bytes (a stripped fence `finally` included, nio20
        // L1): the chain-snapshot liveness reads what a deopt resumes.
        // rx27 (lane matcher): the method's WHOLE code (`method_code_len`).
        // `code_len` is the copy's, which a stripped tail fence `finally`
        // shortens: cut there, the original lost the handler that reads the
        // fenced locals.
        callee_original_code: (masked_code.is_some() || stripped_code.is_some())
            .then(|| code_view.as_bytes()[..method_code_len].to_vec()),
        callee_original_handler_rows,
        callee_code_len: code_len,
        callee_max_locals: callee_max_locals,
        callee_num_args,
        callee_is_static: is_static,
        return_type,
        field_info,
        field_decl_info,
        compact_field_info,
        static_field_info,
        ldc_info,
        ldc2w_info,
        ldc_fp_pcs,
        needs_heap,
        class_name: inlined_body_class_name.clone(),
        class_id: inlined_body_class_id,
        method_name: callee_method.to_string(),
        descriptor: callee_desc.to_string(),
        elided_invoke_pcs,
        invoke_targets,
        unloaded_static_call_pcs,
        // Interned by `try_compile_inner` right before backend emission; a
        // resolver never fills this, and an `InlineSite` that never reaches a
        // compile keeps it empty, which makes the emitter's invoke arm bail.
        resolved_invoke_infos: Vec::new(),
        nested_sites: nested_sites.into(),
        ir_new_info,
        ir_anewarray_info,
        ir_typecheck_info,
        ir_typecheck_final,
        volatile_field_pcs,
        final_field_pcs,
        byte_array_field_pcs,
        referent_load_pcs,
        // Round 14 wave 2 (lane syncsplice): admitted only by the gate at
        // the `synchronized` refusal above.
        callee_is_synchronized: sync_splice,
        // Round 14 wave 7 (lane sync7, SS8-5): see `ldc_self_class_rows`.
        ir_ldc_class_info,
        // JIT r20 wave 19 (lane zLdc): see `ref_ldc_rows`.
        ir_ldc_ref_rows,
        ir_getclass_compares,
        // Round nio21 wave 4 (lane L2): the admitted handler rows, if any.
        ir_handler_rows: handler_rows,
        ir_indy_rows,
        // JIT round 20 wave 19, lane ySplice (SPLICE20-1).
        ir_indy_capture_rows,
        // JIT round 21 wave 15, lane lambda3 (LAMBDACAP21-2).
        ir_indy_capture_sites,
        // Filled by the compile door only.
        ir_lambda_impl_rows: Vec::new(),
        callee_receiver_profile,
        callee_never_taken_pcs,
        callee_always_taken_pcs,
        callee_pruned_switch_edges,
        callee_cold_arm_ranges,
        callee_hot_path_pcs,
        callee_invocations,
        hidden: callee_hidden,
        // j27 wave 5, lane PROXY: set by the profile-receiver resolver only.
        cha_class_set: Vec::new(),
        ir_security_walk_ignored: is_security_walk_ignored_for_jit(
            shared,
            declaring_id,
            &inlined_body_class_name,
            callee_method,
            callee_hidden.unwrap_or(false),
        ),
        ir_caller_mirror_slot: if is_security_walk_ignored_for_jit(
            shared,
            declaring_id,
            &inlined_body_class_name,
            callee_method,
            callee_hidden.unwrap_or(false),
        ) {
            0
        } else {
            crate::jit::helpers::class_mirror_slot_for(shared, declaring_id)
        },
    })
}



fn sp_splice_emits_current_thread_inline(
    ir_mode: bool,
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    !ir_mode
        && cratonvm_jit::x64::spliced_current_thread_site(invoke_kind, class, name, descriptor)
}

fn sp_splice_emits_byte_array_view_inline(
    ir_mode: bool,
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    !ir_mode
        && cratonvm_jit::x64::spliced_byte_array_view_site(invoke_kind, class, name, descriptor)
}

fn callee_pruned_branch_rows(
    shared: &SharedVm,
    declaring_class_id: u32,
    callee_method: &str,
    callee_desc: &str,
    code: &[u8],
    code_len: usize,
    frozen: bool,
    always: bool,
) -> Vec<usize> {
    if declaring_class_id == 0 {
        return Vec::new();
    }
    let Some(profile) = shared
        .jit
        .profile_store
        .get_profile(&crate::jit::profile::MethodKey {
            class_id: declaring_class_id,
            method_name: callee_method.into(),
            descriptor: callee_desc.into(),
        })
    else {
        return Vec::new();
    };
    if always {
        cratonvm_jit::splice_always_taken_pcs_frozen(&profile, code, code_len, frozen)
    } else {
        cratonvm_jit::splice_never_taken_pcs_frozen(&profile, code, code_len, frozen)
    }
}

fn callee_pruned_switch_edges(
    shared: &SharedVm,
    declaring_class_id: u32,
    callee_method: &str,
    callee_desc: &str,
    code: &[u8],
    code_len: usize,
    frozen: bool,
) -> Vec<(usize, usize)> {
    if declaring_class_id == 0 {
        return Vec::new();
    }
    let Some(profile) = shared
        .jit
        .profile_store
        .get_profile(&crate::jit::profile::MethodKey {
            class_id: declaring_class_id,
            method_name: callee_method.into(),
            descriptor: callee_desc.into(),
        })
    else {
        return Vec::new();
    };
    cratonvm_jit::splice_pruned_switch_edges_frozen(&profile, code, code_len, frozen)
}
